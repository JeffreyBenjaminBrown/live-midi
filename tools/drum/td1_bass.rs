//! TD-1 Bass - hits on the drummer's Roland TD-1 play a percussive, pitched bass
//! tone. Two SoftSteps choose a pitch (laid out like the frets of a bass), and each
//! TD-1 pad plays a fixed interval above it: root, fifth, octave, and so on. It is
//! monophonic: each hit fades out the note before it.
//!
//! # How to run
//!
//! ```sh
//! pw-jack cargo run --bin td1_bass   # start
//! pkill -x td1_bass                  # stop, from another terminal (or Ctrl-C)
//! ```
//!
//! The TD-1 is plain class-compliant USB MIDI: its input port is named "TD-1 MIDI 1"
//! and it sends everything on channel 10. Its note-offs carry nothing (always ~100
//! ms after the hit, velocity 64), so only note-ons count. See the capture in
//! tools/drum/td1-capture-2026-10-07.log. Which pads play which interval, how long a
//! hit takes to fade out the note before it, and whether a kick sounds under each
//! hit are in tools/drum/td1_bass.toml. To listen to some other
//! MIDI port instead (e.g. to test without the kit), set `TD1_BASS_PORT` to a
//! substring of its name; to leave the SoftSteps alone, set `TD1_BASS_NO_SOFTSTEPS`.
//!
//! The SoftSteps are the frets: pressing a pad picks the pitch the NEXT drum hits
//! play, and the last pad pressed stays picked until another is. A note already
//! ringing keeps its pitch, or, with `root-change-cuts-off-bass` in td1_bass.toml,
//! fades out at the press. Two more pads choose whether the "fifth" drums play a
//! perfect or a flat fifth; the choice takes effect at the next fret press (even
//! of the same fret). `FRETS` and `FIFTHS` are the layout;
//! `BOARDS` says which board is which and whether it is turned around. Both boards
//! are switched into tether mode (the drumkit runtime's job) and back to standalone
//! on exit, Ctrl-C or pkill. Either board may be missing; with neither, the bass
//! plays the first fret's pitch.
//!
//! Every note a pad can play is rendered once at startup and played by the drumkit
//! runtime's one-shot sampler. `MIDI_PULSE_NO_AUDIO=1` runs it silently.

use edo_surface::drumkit_runtime::audio::{Sampler, Trigger, VoiceId};
use edo_surface::drumkit_runtime::samples::DrumSample;
use edo_surface::drumkit_runtime::{any_softstep_present, start_with_hook, tether, PedalHook};
use edo_surface::rig::Rig;
use midir::{Ignore, MidiInput};
use serde::Deserialize;
use std::f64::consts::{PI, TAU};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{env, thread};

const PORT_SUBSTRING: &str = "TD-1";
const PORT_ENV: &str = "TD1_BASS_PORT";
/// Set to leave the SoftSteps alone, e.g. to test while another td1_bass is using
/// them: binding them switches them into tether mode, and exiting switches them back
/// to standalone, which silences them for the other run.
const NO_SOFTSTEPS_ENV: &str = "TD1_BASS_NO_SOFTSTEPS";

/// tools/drum/td1_bass.toml as written; `parse_settings` checks it into `Settings`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    fade_ms: u32,
    kick: bool,
    #[serde(rename = "root-change-cuts-off-bass")]
    root_change_cuts_off_bass: bool,
    pads: BTreeMap<String, toml::Value>,
}

/// tools/drum/td1_bass.toml, whose comments say what each setting does.
#[derive(Debug)]
struct Settings {
    fade_ms: u32,
    kick: bool,
    root_change_cuts_off_bass: bool,
    /// TD-1 note -> what that pad plays above the fretted pitch.
    pads: BTreeMap<u8, Interval>,
}

/// What a TD-1 pad plays, above the pitch the SoftSteps picked.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Interval {
    /// A fixed number of semitones (negative is below).
    Semitones(i8),
    /// A fifth, perfect or flat, as the fifth buttons (`FIFTHS`) last chose.
    Fifth,
}

impl Interval {
    /// The semitones it plays while the `fifth`th entry of `FIFTHS` is chosen.
    fn semitones(self, fifth: usize) -> i8 {
        match self {
            Interval::Semitones(semitones) => semitones,
            Interval::Fifth => FIFTHS[fifth].3,
        }
    }

    /// Every number of semitones it can play.
    fn choices(self) -> Vec<i8> {
        (0..FIFTHS.len()).map(|fifth| self.semitones(fifth)).collect()
    }
}

fn settings_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("drum/td1_bass.toml")
}

fn parse_settings(source: &str) -> Result<Settings, String> {
    let file: SettingsFile = toml::from_str(source).map_err(|e| e.to_string())?;
    if file.pads.is_empty() {
        return Err("[pads] names no pads".to_string());
    }
    let mut pads = BTreeMap::new();
    for (key, value) in file.pads {
        let note = key
            .parse::<u8>()
            .ok()
            .filter(|&n| n <= 127)
            .ok_or_else(|| format!("pad {key:?} is not a MIDI note (0..127)"))?;
        let interval = match value {
            toml::Value::Integer(n) => Interval::Semitones(
                i8::try_from(n).map_err(|_| format!("pad {note}: {n} semitones is too many"))?,
            ),
            toml::Value::String(word) if word == "fifth" => Interval::Fifth,
            other => {
                return Err(format!("pad {note}: {other} is neither a number of semitones nor \"fifth\""))
            }
        };
        for semitones in interval.choices() {
            if let Some(&(.., fret)) = FRETS.iter().find(|f| played(f.3, semitones).is_none()) {
                let fret = note_name(fret);
                return Err(format!("pad {note}: {fret} plus {semitones} semitones is not a MIDI note"));
            }
        }
        if pads.insert(note, interval).is_some() {
            return Err(format!("pad {note} is listed twice"));
        }
    }
    Ok(Settings {
        fade_ms: file.fade_ms,
        kick: file.kick,
        root_change_cuts_off_bass: file.root_change_cuts_off_bass,
        pads,
    })
}

fn load_settings() -> Result<Settings, String> {
    let path = settings_path();
    let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_settings(&source).map_err(|e| format!("{}: {e}", path.display()))
}

/// The MIDI note `semitones` above `fret_note`, if that is a MIDI note.
fn played(fret_note: u8, semitones: i8) -> Option<u8> {
    u8::try_from(fret_note as i16 + semitones as i16).ok().filter(|&n| n <= 127)
}

/// A SoftStep. `id` is its rig id (what the pad hook is told); `port` is a substring
/// of its MIDI port name. Upright, a board's printed labels read 1 2 3 4 5 along the
/// near row and 6 7 8 9 0 along the far row; `rotated` means it is turned 180
/// degrees from that. (In the 2-edogrids rigs the newer board was rotated.)
struct Board {
    id: &'static str,
    port: &'static str,
    rotated: bool,
}

const BOARDS: [Board; 2] = [
    // The first board gets ten of the frets in `FRETS`; the second the other two and
    // the fifth buttons (`FIFTHS`).
    Board { id: "new", port: "SoftStep", rotated: false },
    Board { id: "old", port: "SSCOM", rotated: false },
];

/// Where each note sits, as you look down at the boards: (index into `BOARDS`, row,
/// column, MIDI note). Row 0 is the row nearer you, column 0 the leftmost.
///
/// The lowest two strings of a bass in standard tuning, open string through the
/// 4th fret, then two more frets of the A string on the other board: chromatic from
/// the open low E (E1) up to D#2. The first entry is the pitch before any press.
const FRETS: [(usize, usize, usize, u8); 12] = [
    (0, 0, 0, 28), // E1, the open low string
    (0, 0, 1, 29), // F1
    (0, 0, 2, 30), // F#1
    (0, 0, 3, 31), // G1
    (0, 0, 4, 32), // G#1
    (0, 1, 0, 33), // A1, the open A string
    (0, 1, 1, 34), // A#1
    (0, 1, 2, 35), // B1
    (0, 1, 3, 36), // C2
    (0, 1, 4, 37), // C#2
    (1, 0, 0, 38), // D2
    (1, 0, 1, 39), // D#2
];

/// The fifth buttons, which choose what the pads set to "fifth" in td1_bass.toml
/// play: (index into `BOARDS`, row, column, semitones, name), placed like `FRETS`.
/// The last one pressed stays chosen; the first entry is chosen before any press.
const FIFTHS: [(usize, usize, usize, i8, &str); 2] = [
    (1, 1, 4, 7, "5th"), // top right: the perfect fifth
    (1, 0, 4, 6, "b5"),  // bottom right: the flat fifth
];

const SAMPLE_RATE: u32 = 48_000;
const BUFFER_FRAMES: u32 = 128;
const MASTER_AMPLITUDE: f32 = 1.0;

/// Velocity 127 plays at full level, velocity 0 would be this many dB quieter.
const VELOCITY_RANGE_DB: f32 = 24.0;

// The tone is a plucked string heard through a magnetic pickup, built from its
// harmonics. A string plucked at fraction p of its length (from the bridge) and heard
// by a pickup at q has harmonic k at sin(k pi p) * sin(k pi q) / k: the plucked
// shape gives 1/k^2, the pickup hears the string's velocity, which gives back a k,
// and each position notches the spectrum. A tone control rolls off the top, and
// higher harmonics die away faster, which is what makes the start sound plucked.
// (The first version was a sawtooth-like stack with a kick-drum pitch drop and heavy
// clipping, which sounded farty.)

const TONE_SECS: f64 = 2.5;
const ATTACK_SECS: f64 = 0.003;
const FADE_OUT_SECS: f64 = 0.05;
/// Where the string is plucked, as a fraction of its length from the bridge. Nearer
/// the bridge (smaller) is brighter and thinner.
const PLUCK_POSITION: f64 = 0.2;
/// Where the pickup sits, likewise.
const PICKUP_POSITION: f64 = 0.15;
/// The tone control: harmonics above this roll off by 6 dB per octave.
const TONE_CUTOFF_HZ: f64 = 1000.0;
/// Harmonics above this are left out; after the roll-off they are inaudible.
const MAX_HARMONIC_HZ: f64 = 3000.0;
/// How long a harmonic would take to fall by a factor of e if its frequency were 0.
/// The decay time halves at DECAY_HALVING_HZ, thirds at twice that, and so on.
const SUSTAIN_SECS: f64 = 1.2;
const DECAY_HALVING_HZ: f64 = 150.0;
/// Soft-clipping drive (tanh): 0 is clean. More is louder and grittier.
const DRIVE: f64 = 0.0;

// The kick drum, part of every hit's tone when td1_bass.toml says `kick = true`: a
// sine whose pitch falls fast from KICK_START_HZ to KICK_END_HZ (the drum head's
// thump), plus a short burst of noise (the beater's click). It is over within
// KICK_SECS, so it does not sustain like the bass. It does not follow the frets.
const KICK_SECS: f64 = 0.5;
const KICK_START_HZ: f64 = 160.0;
const KICK_END_HZ: f64 = 50.0;
/// The pitch covers all but 1/e of its fall in this long.
const KICK_PITCH_SECS: f64 = 0.035;
/// The thump falls by a factor of e in this long.
const KICK_DECAY_SECS: f64 = 0.12;
const KICK_CLICK_LEVEL: f64 = 0.3;
const KICK_CLICK_SECS: f64 = 0.004;
/// The kick's peak, relative to the bass's.
const KICK_LEVEL: f64 = 1.0;
/// Bass plus kick can peak well above full scale (up to ~1.75), so while the kick
/// sounds the sum is soft-limited: untouched below this level, squeezed smoothly
/// toward 1 above it. The bass alone is never limited.
const LIMIT_KNEE: f64 = 0.7;

/// Render one hit of the bass tone at `hz`, peak-normalized to 1.
fn bass_tone(hz: f64, sample_rate: u32) -> DrumSample {
    let rate = sample_rate as f64;
    let len = (TONE_SECS * rate) as usize;
    let mut sum = vec![0.0f64; len];
    for k in (1..).take_while(|&k| k as f64 * hz < MAX_HARMONIC_HZ) {
        let (k, f) = (k as f64, k as f64 * hz);
        let amplitude = (k * PI * PLUCK_POSITION).sin() * (k * PI * PICKUP_POSITION).sin() / k
            / (1.0 + (f / TONE_CUTOFF_HZ).powi(2)).sqrt();
        let decay_secs = SUSTAIN_SECS / (1.0 + f / DECAY_HALVING_HZ);
        // A phasor that turns by this harmonic's frequency and shrinks by its decay
        // every frame; its imaginary part is the decaying sine.
        let shrink = (-1.0 / (decay_secs * rate)).exp();
        let (turn_cos, turn_sin) = ((TAU * f / rate).cos(), (TAU * f / rate).sin());
        let (mut re, mut im) = (amplitude, 0.0);
        for s in sum.iter_mut() {
            *s += im;
            (re, im) = (shrink * (re * turn_cos - im * turn_sin), shrink * (re * turn_sin + im * turn_cos));
        }
    }
    let mut frames: Vec<f32> = sum
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let attack = (i as f64 / (ATTACK_SECS * rate)).min(1.0);
            let fade_out = ((len - i) as f64 / (FADE_OUT_SECS * rate)).min(1.0);
            (s * attack * fade_out) as f32
        })
        .collect();
    let peak = frames.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-9);
    for s in frames.iter_mut() {
        let x = (*s / peak) as f64;
        *s = if DRIVE > 0.0 { (DRIVE * x).tanh() / DRIVE.tanh() } else { x } as f32;
    }
    DrumSample { frames, sample_rate }
}

/// The kick drum alone, peak-normalized to 1.
fn kick_drum(sample_rate: u32) -> Vec<f64> {
    let rate = sample_rate as f64;
    let len = (KICK_SECS * rate) as usize;
    let mut noise = 0x9E37_79B9_7F4A_7C15u64; // xorshift state, any nonzero seed
    let mut phase = 0.0f64; // in cycles
    let mut frames: Vec<f64> = (0..len)
        .map(|i| {
            let t = i as f64 / rate;
            phase += (KICK_END_HZ + (KICK_START_HZ - KICK_END_HZ) * (-t / KICK_PITCH_SECS).exp()) / rate;
            noise ^= noise << 13;
            noise ^= noise >> 7;
            noise ^= noise << 17;
            let white = noise as f64 / u64::MAX as f64 * 2.0 - 1.0;
            let thump = (TAU * phase).sin() * (-t / KICK_DECAY_SECS).exp();
            let click = KICK_CLICK_LEVEL * white * (-t / KICK_CLICK_SECS).exp();
            let fade_out = ((len - i) as f64 / (FADE_OUT_SECS * rate)).min(1.0);
            (thump + click) * fade_out
        })
        .collect();
    let peak = frames.iter().fold(0.0f64, |m, s| m.max(s.abs())).max(1e-9);
    for s in frames.iter_mut() {
        *s /= peak;
    }
    frames
}

/// Identity below LIMIT_KNEE; above it, a tanh curve that meets it smoothly and
/// approaches 1.
fn soft_limit(x: f64) -> f64 {
    let over = x.abs() - LIMIT_KNEE;
    if over <= 0.0 {
        return x;
    }
    let room = 1.0 - LIMIT_KNEE;
    x.signum() * (LIMIT_KNEE + room * (over / room).tanh())
}

/// `bass` with `kick` under its start, soft-limited while the kick lasts.
fn add_kick(bass: DrumSample, kick: &[f64]) -> DrumSample {
    let mut frames = bass.frames;
    for (s, k) in frames.iter_mut().zip(kick) {
        *s = soft_limit(*s as f64 + KICK_LEVEL * k) as f32;
    }
    DrumSample { frames, sample_rate: bass.sample_rate }
}

fn velocity_gain(velocity: u8) -> f32 {
    let db = -VELOCITY_RANGE_DB * (1.0 - velocity as f32 / 127.0);
    10f32.powf(db / 20.0)
}

/// If a TD-1 message is a hit on one of the pads: what that pad plays above the
/// fretted pitch, and how hard it was hit.
fn pad_hit(message: &[u8], settings: &Settings) -> Option<(Interval, u8)> {
    let [status, note, velocity] = *message else { return None };
    if status & 0xF0 != 0x90 || velocity == 0 {
        return None;
    }
    settings.pads.get(&note).map(|&interval| (interval, velocity))
}

/// A tone for every note some pad can play from some fret, keyed by MIDI note, each
/// with the kick under it if the settings say so.
fn render_tones(sample_rate: u32, settings: &Settings) -> BTreeMap<u8, Arc<DrumSample>> {
    let kick = settings.kick.then(|| kick_drum(sample_rate));
    let notes: BTreeSet<u8> = FRETS
        .iter()
        .flat_map(|f| {
            let choices = settings.pads.values().flat_map(|interval| interval.choices());
            choices.map(move |semitones| played(f.3, semitones))
        })
        .map(|note| note.expect("parse_settings keeps every pad on every fret a MIDI note"))
        .collect();
    let tone = |note: u8| {
        let bass = bass_tone(midi_hz(note), sample_rate);
        Arc::new(match &kick {
            Some(kick) => add_kick(bass, kick),
            None => bass,
        })
    };
    notes.into_iter().map(|note| (note, tone(note))).collect()
}

/// The pads as "note:+semitones" (or "note:fifth"), lowest interval first, for the
/// startup line.
fn describe_pads(settings: &Settings) -> String {
    let mut pads: Vec<(&u8, &Interval)> = settings.pads.iter().collect();
    pads.sort_by_key(|&(note, interval)| (interval.semitones(0), *note));
    let describe = |interval: &Interval| match interval {
        Interval::Semitones(semitones) => format!("{semitones:+}"),
        Interval::Fifth => "fifth".to_string(),
    };
    pads.iter().map(|(note, interval)| format!("{note}:{}", describe(interval))).collect::<Vec<_>>().join(" ")
}

fn midi_hz(note: u8) -> f64 {
    440.0 * 2f64.powf((note as f64 - 69.0) / 12.0)
}

fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[note as usize % 12], note as i32 / 12 - 1)
}

/// The printed label of the pad at (`row`, `column`) as you look down at a board.
fn label_at(rotated: bool, row: usize, column: usize) -> u8 {
    let (row, column) = if rotated { (1 - row, 4 - column) } else { (row, column) };
    ((row * 5 + column + 1) % 10) as u8
}

/// Is the pad at (`board`, `row`, `column`) the one printed `label` on board `id`?
fn is_pad(board: usize, row: usize, column: usize, id: &str, label: u8) -> bool {
    BOARDS[board].id == id && label_at(BOARDS[board].rotated, row, column) == label
}

/// Which entry of `FRETS` the pad printed `label` on board `id` plays, if any.
fn fret_at(id: &str, label: u8) -> Option<usize> {
    FRETS.iter().position(|&(board, row, column, _)| is_pad(board, row, column, id, label))
}

/// Which entry of `FIFTHS` the pad printed `label` on board `id` chooses, if any.
fn fifth_at(id: &str, label: u8) -> Option<usize> {
    FIFTHS.iter().position(|&(board, row, column, ..)| is_pad(board, row, column, id, label))
}

/// What the SoftSteps have chosen, and the bass voice sounding now. Shared by the
/// SoftStep hook and the TD-1 callback; each starts at the first entry of its table.
#[derive(Default)]
struct State {
    /// The fret in effect (index into `FRETS`).
    fret: AtomicUsize,
    /// The fifth in effect (index into `FIFTHS`).
    fifth: AtomicUsize,
    /// The fifth button pressed last, which takes effect at the next fret press.
    next_fifth: AtomicUsize,
    /// The last hit's voice, until the next hit (or a fret press) fades it out.
    sounding: Mutex<Option<VoiceId>>,
}

impl State {
    /// Fade out the sounding voice, if any, over `fade_secs`.
    fn cut_off(&self, trigger: &Trigger, fade_secs: f32) {
        if let Some(voice) = self.sounding.lock().unwrap_or_else(|e| e.into_inner()).take() {
            trigger.fade_out(voice, fade_secs);
        }
    }
}

/// A SoftStep press on a fret picks that fret, brings in the fifth last chosen, and
/// (given `cut_off`: the trigger and fade in seconds) fades out the ringing bass. A
/// press on a fifth button chooses that fifth for the next fret press. A release,
/// or a press on a pad with neither job, changes nothing. Every press is consumed,
/// so the drumkit plays no sample for it.
fn softstep_hook(state: Arc<State>, cut_off: Option<(Trigger, f32)>) -> PedalHook {
    Arc::new(move |id: &str, label: u8, down: bool| {
        if !down {
            return true;
        }
        if let Some(i) = fret_at(id, label) {
            state.fret.store(i, Ordering::Relaxed);
            let fifth = state.next_fifth.load(Ordering::Relaxed);
            let fifth_changed = state.fifth.swap(fifth, Ordering::Relaxed) != fifth;
            if let Some((trigger, fade_secs)) = &cut_off {
                state.cut_off(trigger, *fade_secs);
            }
            let fifth_news = match fifth_changed {
                true => format!("; fifth pads now play the {}", FIFTHS[fifth].4),
                false => String::new(),
            };
            println!("{id} pad {label}: {}{fifth_news}", note_name(FRETS[i].3));
        } else if let Some(i) = fifth_at(id, label) {
            state.next_fifth.store(i, Ordering::Relaxed);
            let (semitones, name) = (FIFTHS[i].3, FIFTHS[i].4);
            println!("{id} pad {label}: the next fret press brings in the {name} ({semitones:+})");
        }
        true
    })
}

/// Declares the boards to the drumkit runtime, which finds them, switches them into
/// tether mode and reports their presses. Not `parse_rig`: its validation rejects a
/// board that no softstep window uses, and here the hook uses them instead.
fn softstep_rig() -> Result<Rig, toml::de::Error> {
    let mut source = String::from("version = 1\nid = \"td1-bass\"\ntitle = \"TD-1 bass\"\n");
    for board in &BOARDS {
        source += &format!("[[softsteps]]\nid = {:?}\nselect.name_contains = {:?}\n", board.id, board.port);
    }
    toml::from_str(&source)
}

fn print_layout() {
    println!("SoftStep pads (printed label: fret, or which fifth), as you look down at them:");
    for (b, board) in BOARDS.iter().enumerate() {
        for row in [1, 0] {
            let cells: Vec<String> = (0..5)
                .map(|column| {
                    let fret = FRETS.iter().find(|f| (f.0, f.1, f.2) == (b, row, column));
                    let fifth = FIFTHS.iter().find(|f| (f.0, f.1, f.2) == (b, row, column));
                    let job = match (fret, fifth) {
                        (Some(f), _) => note_name(f.3),
                        (_, Some(f)) => f.4.to_string(),
                        _ => "-".to_string(),
                    };
                    format!("{}:{job:<4}", label_at(board.rotated, row, column))
                })
                .collect();
            let name = if row == 1 { board.port } else { "" };
            println!("  {name:<9} {} {}", if row == 1 { "far " } else { "near" }, cells.join(" "));
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // First, before any thread spawns: Ctrl-C and pkill then put the SoftSteps back
    // in standalone mode before exiting (see `tether::arm`).
    let tether_session = tether::arm();
    let settings = load_settings()?;

    let sampler = if env::var_os("MIDI_PULSE_NO_AUDIO").is_some() {
        Sampler::start_null(SAMPLE_RATE, MASTER_AMPLITUDE)
    } else {
        Sampler::start(SAMPLE_RATE, BUFFER_FRAMES, MASTER_AMPLITUDE)?
    };
    let rate = sampler.output_rate() as u32;
    let tones = render_tones(rate, &settings);
    let state = Arc::new(State::default());
    let trigger = sampler.trigger();
    let fade_secs = settings.fade_ms as f32 / 1000.0;
    let cut_off = settings.root_change_cuts_off_bass.then(|| (sampler.trigger(), fade_secs));

    let substring = env::var(PORT_ENV).unwrap_or_else(|_| PORT_SUBSTRING.to_string());
    let mut midi_in = MidiInput::new("td1-bass")?;
    midi_in.ignore(Ignore::All);
    let port = midi_in
        .ports()
        .into_iter()
        .find(|p| midi_in.port_name(p).map_or(false, |n| n.contains(&substring)))
        .ok_or_else(|| {
            let available: Vec<String> =
                midi_in.ports().iter().filter_map(|p| midi_in.port_name(p).ok()).collect();
            format!("no MIDI input port matching {substring:?}; is the TD-1 on? available: {available:?}")
        })?;
    let port_name = midi_in.port_name(&port)?;
    let state_for_hits = Arc::clone(&state);
    let pads = describe_pads(&settings);
    let fade_ms = settings.fade_ms;
    let with_kick = if settings.kick { ", each with a kick under it" } else { "" };
    let fret_cuts = match settings.root_change_cuts_off_bass {
        true => "; a fret press cuts off the ringing bass",
        false => "",
    };
    let _connection = midi_in.connect(
        &port,
        "td1-bass-in",
        move |_stamp, message, _| {
            if let Some((interval, velocity)) = pad_hit(message, &settings) {
                let fret = state_for_hits.fret.load(Ordering::Relaxed);
                let semitones = interval.semitones(state_for_hits.fifth.load(Ordering::Relaxed));
                let note = played(FRETS[fret].3, semitones).expect("validated by parse_settings");
                let mut sounding = state_for_hits.sounding.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(previous) = sounding.take() {
                    trigger.fade_out(previous, fade_secs);
                }
                *sounding = Some(trigger.fire(Arc::clone(&tones[&note]), velocity_gain(velocity)));
                println!("hit, velocity {velocity}, {} ({semitones:+})", note_name(note));
            }
        },
        (),
    )?;

    let rig = softstep_rig()?;
    let _softsteps = if env::var_os(NO_SOFTSTEPS_ENV).is_some() {
        eprintln!("{NO_SOFTSTEPS_ENV} is set: leaving the SoftSteps alone");
        None
    } else if any_softstep_present(&rig) {
        Some(start_with_hook(&rig, tether_session, Some(softstep_hook(state, cut_off)))?)
    } else {
        eprintln!(
            "warning: no SoftStep found, so every hit plays {}; plug one in and restart",
            note_name(FRETS[0].3),
        );
        None
    };

    print_layout();
    println!(
        "td1_bass: from {port_name:?}, pads (TD-1 note:semitones above the fret) {pads}{with_kick}; \
         fifth pads start on the {} and a fifth button takes effect at the next fret press{fret_cuts}; \
         each hit fades the last out over {fade_ms} ms. Ctrl-C to stop.",
        FIFTHS[0].4,
    );
    loop {
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_board_reads_as_printed_upright_and_turned_around() {
        let row = |rotated, row| (0..5).map(|c| label_at(rotated, row, c)).collect::<Vec<u8>>();
        assert_eq!(row(false, 0), [1, 2, 3, 4, 5]);
        assert_eq!(row(false, 1), [6, 7, 8, 9, 0]);
        assert_eq!(row(true, 0), [0, 9, 8, 7, 6]);
        assert_eq!(row(true, 1), [5, 4, 3, 2, 1]);
    }

    /// The (board id, printed label) of the pad at a `FRETS`/`FIFTHS` position.
    fn pad(board: usize, row: usize, column: usize) -> (&'static str, u8) {
        (BOARDS[board].id, label_at(BOARDS[board].rotated, row, column))
    }

    #[test]
    fn every_fret_and_fifth_button_is_on_a_real_pad_and_no_two_share_one() {
        let positions = FRETS.iter().map(|f| (f.0, f.1, f.2)).chain(FIFTHS.iter().map(|f| (f.0, f.1, f.2)));
        let mut pads: Vec<(&str, u8)> = Vec::new();
        for (board, row, column) in positions {
            assert!(board < BOARDS.len() && row < 2 && column < 5);
            pads.push(pad(board, row, column));
        }
        let count = pads.len();
        pads.sort();
        pads.dedup();
        assert_eq!(pads.len(), count, "two jobs on one pad");
    }

    fn load(cell: &AtomicUsize) -> usize {
        cell.load(Ordering::Relaxed)
    }

    #[test]
    fn pressing_a_fret_picks_it_and_releasing_keeps_it() {
        let state = Arc::new(State::default());
        let hook = softstep_hook(Arc::clone(&state), None);
        for (i, &(board, row, column, _)) in FRETS.iter().enumerate().rev() {
            let (id, label) = pad(board, row, column);
            assert!(hook(id, label, true));
            assert_eq!(load(&state.fret), i);
            assert!(hook(id, label, false));
            assert_eq!(load(&state.fret), i);
        }
        assert_eq!(load(&state.fifth), 0);
    }

    #[test]
    fn a_fifth_button_takes_effect_at_the_next_fret_press_even_on_the_same_fret() {
        let state = Arc::new(State::default());
        let hook = softstep_hook(Arc::clone(&state), None);
        let (fret_id, fret_label) = pad(FRETS[3].0, FRETS[3].1, FRETS[3].2);
        assert!(hook(fret_id, fret_label, true));
        for (i, &(board, row, column, ..)) in FIFTHS.iter().enumerate().rev() {
            let before = load(&state.fifth);
            let (id, label) = pad(board, row, column);
            assert!(hook(id, label, true));
            assert!(hook(id, label, false));
            assert_eq!((load(&state.fifth), load(&state.next_fifth)), (before, i));
            assert!(hook(fret_id, fret_label, true));
            assert_eq!(load(&state.fifth), i);
            assert_eq!(load(&state.fret), 3);
        }
    }

    #[test]
    fn a_pad_with_no_job_or_an_unknown_board_changes_nothing() {
        let state = Arc::new(State::default());
        let hook = softstep_hook(Arc::clone(&state), None);
        let (last_fret, last_fifth) = (FRETS.len() - 1, FIFTHS.len() - 1);
        state.fret.store(last_fret, Ordering::Relaxed);
        state.fifth.store(last_fifth, Ordering::Relaxed);
        state.next_fifth.store(last_fifth, Ordering::Relaxed);
        let idle = BOARDS
            .iter()
            .flat_map(|b| (0..10).map(move |label| (b.id, label)))
            .filter(|&(id, label)| fret_at(id, label).is_none() && fifth_at(id, label).is_none());
        for (id, label) in idle {
            assert!(hook(id, label, true));
        }
        assert!(hook("not-a-board", 1, true));
        assert_eq!((load(&state.fret), load(&state.fifth)), (last_fret, last_fifth));
        assert_eq!(load(&state.next_fifth), last_fifth);
    }

    #[test]
    fn a_fret_press_cuts_off_the_ringing_bass_only_when_asked() {
        let sampler = Sampler::start_null(SAMPLE_RATE, 1.0);
        let tone = Arc::new(bass_tone(midi_hz(FRETS[0].3), SAMPLE_RATE));
        let fret = pad(FRETS[0].0, FRETS[0].1, FRETS[0].2);
        let fifth = pad(FIFTHS[0].0, FIFTHS[0].1, FIFTHS[0].2);
        for cuts in [false, true] {
            let state = Arc::new(State::default());
            *state.sounding.lock().unwrap() = Some(sampler.trigger().fire(Arc::clone(&tone), 1.0));
            let hook = softstep_hook(Arc::clone(&state), cuts.then(|| (sampler.trigger(), 0.02)));
            assert!(hook(fifth.0, fifth.1, true)); // a fifth button never cuts off
            assert!(state.sounding.lock().unwrap().is_some());
            assert!(hook(fret.0, fret.1, true));
            assert_eq!(state.sounding.lock().unwrap().is_none(), cuts);
        }
    }

    #[test]
    fn the_rig_declares_every_board_by_its_port() {
        let rig = softstep_rig().unwrap();
        let declared: Vec<(&str, &str)> =
            rig.softsteps.iter().map(|s| (s.id.as_str(), s.select.name_substring())).collect();
        let boards: Vec<(&str, &str)> = BOARDS.iter().map(|b| (b.id, b.port)).collect();
        assert_eq!(declared, boards);
        assert!(rig.softstep_windows.is_empty() && rig.sinks.is_empty());
    }

    #[test]
    fn the_shipped_settings_load() {
        load_settings().unwrap();
    }

    /// Every key td1_bass.toml must have above its [pads] table.
    const TOP_KEYS: [&str; 3] = ["fade_ms = 20", "kick = true", "root-change-cuts-off-bass = true"];

    /// A settings file with every top key and these `pads` lines.
    fn settings(pads: &str) -> String {
        format!("{}\n[pads]\n{pads}", TOP_KEYS.join("\n"))
    }

    #[test]
    fn settings_need_every_key_and_pads_that_stay_midi_notes() {
        let parse = |pads: &str| parse_settings(&settings(pads));
        assert!(parse("60 = 0\n72 = 12\n61 = -5\n62 = \"fifth\"").is_ok());
        assert!(parse("60 = \"third\"").is_err());
        assert!(parse("60 = 1.5").is_err());
        assert!(parse("60 = 200").is_err()); // more semitones than an i8 holds
        assert!(parse("").is_err()); // no pads
        assert!(parse("x = 0").is_err());
        assert!(parse("128 = 0").is_err());
        assert!(parse("60 = 127").is_err()); // past note 127 from any fret
        assert!(parse("60 = -128").is_err()); // below note 0
        assert!(parse("60 = 0\n060 = 12").is_err()); // the same note twice
        for missing in 0..TOP_KEYS.len() {
            let mut keys = TOP_KEYS.to_vec();
            keys.remove(missing);
            assert!(parse_settings(&format!("{}\n[pads]\n60 = 0", keys.join("\n"))).is_err(), "{missing}");
        }
        assert!(parse_settings(&format!("fade = 1\n{}", settings("60 = 0"))).is_err());
    }

    #[test]
    fn only_note_ons_of_the_pads_are_hits() {
        let settings = parse_settings(&settings("60 = 0\n72 = 12\n55 = -5\n67 = \"fifth\"")).unwrap();
        let hit = |message: &[u8]| pad_hit(message, &settings);
        assert_eq!(hit(&[0x99, 60, 100]), Some((Interval::Semitones(0), 100)));
        assert_eq!(hit(&[0x90, 72, 1]), Some((Interval::Semitones(12), 1)));
        assert_eq!(hit(&[0x99, 55, 64]), Some((Interval::Semitones(-5), 64)));
        assert_eq!(hit(&[0x99, 67, 64]), Some((Interval::Fifth, 64)));
        assert_eq!(hit(&[0x99, 60, 0]), None); // a note-on at 0 is a note-off
        assert_eq!(hit(&[0x89, 72, 64]), None);
        assert_eq!(hit(&[0x99, 61, 100]), None);
        assert_eq!(hit(&[0xB9, 60, 100]), None);
    }

    #[test]
    fn a_fifth_pad_plays_whichever_fifth_is_chosen() {
        for (i, f) in FIFTHS.iter().enumerate() {
            assert_eq!(Interval::Fifth.semitones(i), f.3);
            assert_eq!(Interval::Semitones(12).semitones(i), 12);
        }
        assert_eq!(Interval::Fifth.choices(), FIFTHS.iter().map(|f| f.3).collect::<Vec<_>>());
    }

    #[test]
    fn midi_notes_name_and_tune_like_a_piano() {
        assert_eq!(midi_hz(69), 440.0);
        assert!((midi_hz(57) - 220.0).abs() < 1e-9);
        assert_eq!(note_name(69), "A4");
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(28), "E1");
    }

    /// The period with the strongest autocorrelation, searched between 0.4 and 1.4
    /// times the expected one (so a tone an octave too high would be caught), over a
    /// stretch after the attack's pitch drop.
    fn measured_hz(tone: &DrumSample, expected_hz: f64) -> f64 {
        let rate = tone.sample_rate as f64;
        let x = &tone.frames[(0.15 * rate) as usize..(0.55 * rate) as usize];
        let expected = rate / expected_hz;
        let lags = (0.4 * expected) as usize..(1.4 * expected) as usize;
        let score = |lag: usize| -> f64 {
            x.iter().zip(&x[lag..]).map(|(a, b)| (a * b) as f64).sum::<f64>() / (x.len() - lag) as f64
        };
        let best = lags.max_by(|&a, &b| score(a).total_cmp(&score(b))).unwrap();
        rate / best as f64
    }

    #[test]
    fn every_pad_plays_its_interval_above_the_fret() {
        // The shipped pads, without the kick, whose thump would blur the pitch.
        let settings = Settings { kick: false, ..load_settings().unwrap() };
        let tones = render_tones(SAMPLE_RATE, &settings);
        let fret = FRETS.iter().map(|f| f.3).min().unwrap();
        let choices: BTreeSet<i8> = settings.pads.values().flat_map(|i| i.choices()).collect();
        for &semitones in &choices {
            let note = played(fret, semitones).unwrap();
            let want = midi_hz(fret) * 2f64.powf(semitones as f64 / 12.0);
            let measured = measured_hz(&tones[&note], want);
            assert!((measured / want - 1.0).abs() < 0.01, "{semitones:+}: {measured} Hz, want {want}");
        }
        for f in &FRETS {
            for &semitones in &choices {
                assert!(tones.contains_key(&played(f.3, semitones).unwrap()));
            }
        }
    }

    #[test]
    fn the_soft_limit_leaves_quiet_samples_alone_and_keeps_loud_ones_under_1() {
        for x in [0.0, 0.3, -0.5, LIMIT_KNEE, -LIMIT_KNEE] {
            assert_eq!(soft_limit(x), x);
        }
        let mut last = LIMIT_KNEE;
        for x in [0.71, 0.8, 1.0, 1.5, 2.0] {
            let y = soft_limit(x);
            assert!(last < y && y < 1.0, "{x} -> {y}");
            assert_eq!(soft_limit(-x), -y);
            last = y;
        }
        assert!(soft_limit(10.0) <= 1.0);
    }

    #[test]
    fn the_kick_thumps_at_the_start_and_leaves_the_rest_of_the_bass_alone() {
        let bass = bass_tone(midi_hz(FRETS[0].3), SAMPLE_RATE);
        let kick = kick_drum(SAMPLE_RATE);
        let both = add_kick(bass.clone(), &kick);
        let end = kick.len();
        assert!(end < bass.frames.len());
        assert_eq!(both.frames[end..], bass.frames[end..]);
        let energy = |f: &[f32]| f.iter().map(|&s| (s * s) as f64).sum::<f64>();
        let thump = end / 8; // the kick's first ~60 ms, while it is about as strong as the bass
        assert!(energy(&both.frames[..thump]) > 1.4 * energy(&bass.frames[..thump]));
        assert!(both.frames.iter().all(|s| s.abs() < 1.0));
    }

    #[test]
    fn the_lowest_and_highest_frets_sound_at_their_pitch() {
        let notes = FRETS.iter().map(|f| f.3);
        for note in [notes.clone().min().unwrap(), notes.max().unwrap()] {
            let hz = midi_hz(note);
            let measured = measured_hz(&bass_tone(hz, SAMPLE_RATE), hz);
            assert!((measured / hz - 1.0).abs() < 0.01, "{}: {measured} Hz, want {hz}", note_name(note));
        }
    }
}
