//! `softstep_meter` -- a live per-pad pressure meter for the SoftStep, DRIVING THE
//! SAME tether decoder the drumkit runtime uses (`decode::TetherDecoder`, reused
//! verbatim via `#[path]`), so what you see here is exactly what the drumkit would
//! fire -- same onset/release thresholds, attack window, debounce, de-stick, and
//! pressure->gain mapping. Unlike the drumkit it plays no per-pad sample bank and does no
//! monome/accrete/pedal-hook work; it exists purely to watch sensing, hit detection,
//! pressure, and the ditto pad.
//!
//!   cargo run --bin softstep_meter
//!
//! Displays a live pressure bar per pad (sum of its 4 sensors, 0..508) with a
//! peak-hold marker, and on every detected hit shows the pad, resolved pressure and
//! gain. Pad 3 ("the gap between the feet", mirroring rigs/kmss-drumkit.toml) is
//! wired as DITTO: struck, it repeats the last REAL pad's hit at the ditto press's
//! OWN pressure (pad, ditto, ditto repeats the ORIGINAL, not the ditto -- see
//! `resolve_fire`, which mirrors `drumkit_runtime::mod::resolve_fire`).
//!
//! Restores standalone mode on exit -- Ctrl-C (via `tether::arm`'s signal thread) or
//! `q` + Enter (via `TetherSession`'s `Drop`) -- exactly like the drumkit runtime.
//!
//! Optional audio audition (off by default): set SOFTSTEP_METER_AUDIO=1 to play
//! drum-samples/snare.wav via `pw-play` at the resolved gain on every hit -- one
//! reference sample for every pad, since this is a touch/pressure meter, not the
//! kit (mirrors the Python meter's `audition-sample`). Launch under `pw-jack` if you
//! want that sample to share the sound card via PipeWire.

#[path = "drumkit_runtime/decode.rs"]
mod decode;
// `tether::session()` (an alternate constructor for a host runtime that owns its own
// signal handling) is unused here -- this meter is standalone, so it uses `arm()`
// instead, like the drumkit runtime's own standalone path (`run_from_rig`).
#[path = "drumkit_runtime/tether.rs"]
#[allow(dead_code)]
mod tether;

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use midir::{MidiInput, MidiInputPort};

use midi_pulse::midi;
use midi_pulse::rig::{drum_samples_dir, load_softstep_params, SoftstepParams};

use decode::{collect_control_changes, gain_from_pressure, DrumEvent, TetherDecoder, NUM_PADS};

/// Printed label -> base CC of that pad's 4 sensors, in printed-board order (1..9,
/// then 0). Mirrors decode.rs's private `SLOT_LABEL` / the Python meter's
/// `LABEL_BASE` (measured on the device; see learnings/keith-mcmillen-softstep.org).
/// `TetherDecoder` exposes no accessor for its live per-sensor sums (only
/// Fire/Revise/Release events), so the live pressure BAR -- which needs a
/// continuous 0..508 reading even between hits -- keeps its own shadow of the same
/// raw CC stream, purely for display. Hit/ditto detection always goes through the
/// real decoder below, never this shadow.
const LABEL_BASE: [(u8, u8); NUM_PADS] =
  [(1, 44), (2, 52), (3, 60), (4, 68), (5, 76), (6, 40), (7, 48), (8, 56), (9, 64), (0, 72)];

/// The pad wired as ditto, mirroring rigs/kmss-drumkit.toml's pedal 3 ("the gap
/// between the feet"): struck, it repeats the last REAL pad's hit at ITS OWN
/// pressure.
const DITTO_LABEL: u8 = 3;

const SUM_MAX: f32 = 508.0;
const BAR_WIDTH: usize = 44;
const PEAK_HOLD_SECS: f64 = 0.6;
const PEAK_DECAY_STEP: u16 = 24;
const DRAW_TICK: Duration = Duration::from_millis(30);
const AUDITION_SAMPLE: &str = "snare.wav";

// --------------------------------------------------------------------------------
// Pure, hardware-free logic (unit tested below).
// --------------------------------------------------------------------------------

/// What a pad does when struck. The meter has no per-pad sample bank (unlike the
/// drumkit); a "hit" is identified by its printed label only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PadKind {
  Real,
  Ditto,
}

fn pad_kind(label: u8) -> PadKind {
  if label == DITTO_LABEL {
    PadKind::Ditto
  } else {
    PadKind::Real
  }
}

/// The most recently played REAL pad's label + its static gain trim. The meter
/// carries no per-pad rig, so every real pad's trim is `REAL_GAIN`. Mirrors
/// `drumkit_runtime::mod::LastHit` (minus the sample payload).
#[derive(Clone, Copy, Debug, PartialEq)]
struct LastHit {
  label: u8,
  gain: f32,
}

/// The meter's flat per-pad gain trim (no rig.toml equivalent here, unlike the
/// drumkit's per-pad `gain`).
const REAL_GAIN: f32 = 1.0;

/// Mirrors `drumkit_runtime::mod::resolve_fire`'s ditto semantics exactly, minus the
/// sample payload: a REAL press records itself into `last` and resolves to its own
/// label at its own pressure; a DITTO press resolves to the last REAL press's label,
/// gain-scaled by the DITTO press's OWN pressure (not the original hit's) -- and,
/// deliberately, a Ditto fire never writes `last`, so pad-ditto-ditto repeats the
/// ORIGINAL, not the ditto. Returns `None` only for a ditto struck before anything
/// real has played. Pure -- the unit-testable core of the ditto behavior.
fn resolve_fire(
  kind: PadKind,
  own_label: u8,
  last: &mut Option<LastHit>,
  pressure: f32,
  db_range: f32,
) -> Option<(u8, f32)> {
  match kind {
    PadKind::Real => {
      let resolved = REAL_GAIN * gain_from_pressure(pressure, db_range);
      *last = Some(LastHit { label: own_label, gain: REAL_GAIN });
      Some((own_label, resolved))
    }
    PadKind::Ditto => {
      let hit = last.as_ref()?;
      Some((hit.label, hit.gain * gain_from_pressure(pressure, db_range)))
    }
  }
}

/// Mirrors `drumkit_runtime::mod::resolve_revise`: what a mid-attack-window Revise
/// on this pad should update its displayed gain to (`None` only if a ditto revises
/// before anything real has played, which a Revise implies shouldn't happen).
fn resolve_revise(kind: PadKind, last: &Option<LastHit>, pressure: f32, db_range: f32) -> Option<f32> {
  match kind {
    PadKind::Real => Some(REAL_GAIN * gain_from_pressure(pressure, db_range)),
    PadKind::Ditto => last.as_ref().map(|hit| hit.gain * gain_from_pressure(pressure, db_range)),
  }
}

/// One redraw tick's peak-hold update for a single bar. `pv`/`pt` are the held peak
/// value and the time (seconds, any consistent monotonic origin) it was last set to
/// a fresh high. Mirrors the Python meter's `hold_peak`: the peak snaps to a fresh
/// high immediately (resetting its hold timer); otherwise, once it has sat unbeaten
/// for `PEAK_HOLD_SECS`, it falls `PEAK_DECAY_STEP` per redraw tick -- the hold
/// timer is deliberately NOT reset while decaying, so once the hold elapses the peak
/// keeps falling every tick until `current` catches up (or it reaches 0).
fn peak_hold_tick(current: u16, pv: u16, pt: f64, now: f64) -> (u16, f64) {
  if current >= pv {
    (current, now)
  } else if now - pt > PEAK_HOLD_SECS {
    (pv.saturating_sub(PEAK_DECAY_STEP), pt)
  } else {
    (pv, pt)
  }
}

/// Render one pad's bar: `#` filled 0..fill, `-` elsewhere, with a `|` peak-hold
/// marker overlaid at the peak's column (if in range). Mirrors the Python meter's
/// bar (`BARW`=44, `SUM_MAX`=508).
fn render_bar(sum: u16, peak: u16) -> String {
  let fill = ((sum as f32 / SUM_MAX) * BAR_WIDTH as f32) as usize;
  let peak_col = ((peak as f32 / SUM_MAX) * BAR_WIDTH as f32) as usize;
  let mut bar = vec!['-'; BAR_WIDTH];
  for c in bar.iter_mut().take(fill.min(BAR_WIDTH)) {
    *c = '#';
  }
  if peak_col < BAR_WIDTH {
    bar[peak_col] = '|';
  }
  bar.into_iter().collect()
}

/// A sensor's INTERPRETED value for the display shadow: its raw reading, or 0 once
/// it has been silent (no CC) longer than `silence` (de-stick, matching
/// `SoftstepParams::silence_to_zero_ms`; `silence == ZERO` disables it). Mirrors
/// `decode::TetherDecoder::interp`, duplicated here only because the decoder keeps
/// that state private -- this shadow drives the pressure BAR only, never firing.
fn interp_shadow(sensors: &[u8; 40], last_seen: &[Option<Instant>; 40], idx: usize, now: Instant, silence: Duration) -> u16 {
  if silence.is_zero() {
    return sensors[idx] as u16;
  }
  match last_seen[idx] {
    Some(t) if now.duration_since(t) <= silence => sensors[idx] as u16,
    _ => 0,
  }
}

// --------------------------------------------------------------------------------
// Live state + I/O (not unit tested -- needs real MIDI/terminal).
// --------------------------------------------------------------------------------

/// One pad's live display state.
#[derive(Clone, Copy)]
struct PadDisplay {
  peak_val: u16,
  peak_t: f64,
  last_pressure: f32,
  last_gain: f32,
}

impl Default for PadDisplay {
  fn default() -> Self {
    PadDisplay { peak_val: 0, peak_t: 0.0, last_pressure: 0.0, last_gain: 0.0 }
  }
}

/// Everything the MIDI callback, the poll thread, and the redraw loop share.
struct MeterState {
  sensors: [u8; 40],                    // raw last value per CC 40..79 (display shadow only)
  last_seen: [Option<Instant>; 40],     // when each sensor last got a CC (display shadow only)
  pads: [PadDisplay; NUM_PADS],         // indexed by printed label (0..9)
  last_hit: Option<LastHit>,            // for ditto resolution (`resolve_fire`)
  recent: String,                       // last hit line, like the Python meter's `recent`
  messages: VecDeque<String>,           // recent event log, newest last (last 8, like Python)
}

impl MeterState {
  fn new() -> Self {
    MeterState {
      sensors: [0; 40],
      last_seen: [None; 40],
      pads: [PadDisplay::default(); NUM_PADS],
      last_hit: None,
      recent: "(strike a pad)".to_string(),
      messages: VecDeque::new(),
    }
  }

  fn push_message(&mut self, msg: String) {
    self.messages.push_back(msg);
    while self.messages.len() > 8 {
      self.messages.pop_front();
    }
  }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let params = load_softstep_params()?;
  let audio_on = std::env::var_os("SOFTSTEP_METER_AUDIO").is_some();
  let select_substring = std::env::args().nth(1).unwrap_or_else(|| "SSCOM".to_string());

  println!("softstep_meter -- SoftStep pressure meter (Rust; the reference implementation, see the Python meter's startup banner)");
  println!(
    "  on_sum={} off_sum={} attack={}ms debounce={}ms full_scale={} db_range={} de-stick={}ms",
    params.on_sum,
    params.off_sum,
    params.attack_ms,
    params.debounce_ms,
    params.pressure_full_scale,
    params.gain_db_range,
    params.silence_to_zero_ms,
  );
  println!("  pad {DITTO_LABEL} is DITTO (mirrors rigs/kmss-drumkit.toml pedal 3, \"the gap between the feet\")");
  if audio_on {
    println!("  audio audition ON (SOFTSTEP_METER_AUDIO set): plays drum-samples/{AUDITION_SAMPLE} via `pw-play` at the resolved gain");
  } else {
    println!("  audio audition OFF (silent meter). Set SOFTSTEP_METER_AUDIO=1 to hear hits via `pw-play`.");
  }
  println!("  keys: q + Enter quits (Ctrl-C also restores standalone mode)\n");

  // Arm Ctrl-C/SIGTERM restoration FIRST, before any MIDI/poll/keyboard thread
  // spawns, so the signal block is inherited by all of them (see tether::arm).
  let session = tether::arm();
  session
    .enter(&select_substring)
    .map_err(|e| format!("could not enter tether mode (needs alsa-utils `amidi`): {e}"))?;
  println!("device in tether mode; will restore standalone on exit\n");

  let state = Arc::new(Mutex::new(MeterState::new()));
  let decoder = Arc::new(Mutex::new(TetherDecoder::new(params)));

  let midi_in = MidiInput::new("softstep-meter")?;
  let port = select_input_port(&midi_in, &select_substring)?;
  let port_name = midi_in.port_name(&port).unwrap_or_else(|_| "<unknown>".to_string());
  println!("bound MIDI input {port_name:?}\n");

  // MIDI callback: feed every CC into BOTH the display shadow and the real decoder.
  let state_cb = Arc::clone(&state);
  let decoder_cb = Arc::clone(&decoder);
  let mut ccs: Vec<(u8, u8)> = Vec::with_capacity(16);
  let conn = midi_in
    .connect(
      &port,
      "softstep-meter-in",
      move |_timestamp, message, _| {
        ccs.clear();
        collect_control_changes(message, &mut ccs);
        if ccs.is_empty() {
          return;
        }
        let now = Instant::now();
        {
          let mut st = state_cb.lock().unwrap_or_else(|e| e.into_inner());
          for &(cc, val) in &ccs {
            if (40..=79).contains(&cc) {
              let idx = (cc - 40) as usize;
              st.sensors[idx] = val & 0x7F;
              st.last_seen[idx] = Some(now);
            }
          }
        }
        let mut dec = decoder_cb.lock().unwrap_or_else(|e| e.into_inner());
        for &(cc, val) in &ccs {
          dec.on_cc(cc, val, now);
        }
      },
      (),
    )
    .map_err(|e| format!("connect to {port_name:?}: {e}"))?;

  // Poll thread: advance the decoder every ~1 ms (same cadence as the drumkit
  // runtime's `run_voice_timer`), resolve ditto, and update the display state.
  let stop = Arc::new(AtomicBool::new(false));
  let poll_handle = {
    let state = Arc::clone(&state);
    let decoder = Arc::clone(&decoder);
    let stop = Arc::clone(&stop);
    thread::Builder::new()
      .name("softstep-meter-poll".to_string())
      .spawn(move || run_poll_loop(decoder, state, stop, params.gain_db_range, audio_on))?
  };

  // A background line-reader so `q` + Enter can quit; Ctrl-C also works (via the
  // signal thread `tether::arm` installed above).
  let quit = Arc::new(AtomicBool::new(false));
  {
    let quit = Arc::clone(&quit);
    thread::spawn(move || {
      let stdin = io::stdin();
      for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if matches!(line.trim(), "q" | "Q" | "quit") {
          quit.store(true, Ordering::SeqCst);
          break;
        }
      }
    });
  }

  let start = Instant::now();
  while !quit.load(Ordering::SeqCst) {
    draw(&state, start, params);
    thread::sleep(DRAW_TICK);
  }

  // Tear down in the same order as `DrumSession`'s `Drop`: stop the poll thread,
  // release the MIDI connection, THEN restore standalone mode (the `session` drop
  // at the end of this function, after `conn` has already gone out of scope).
  stop.store(true, Ordering::Relaxed);
  let _ = poll_handle.join();
  drop(conn);
  drop(session);
  Ok(())
}

/// The poll loop: advance the decoder, resolve any Fire/Revise into the shared
/// display state (ditto via `resolve_fire`/`resolve_revise`), and optionally
/// audition a hit. One-shot samples ignore Release (like the drumkit's pads absent
/// a `PedalHook`), so this meter does too.
fn run_poll_loop(
  decoder: Arc<Mutex<TetherDecoder>>,
  state: Arc<Mutex<MeterState>>,
  stop: Arc<AtomicBool>,
  db_range: f32,
  audio_on: bool,
) {
  let mut events: Vec<DrumEvent> = Vec::with_capacity(8);
  while !stop.load(Ordering::Relaxed) {
    thread::sleep(Duration::from_millis(1));
    events.clear();
    {
      let mut dec = decoder.lock().unwrap_or_else(|e| e.into_inner());
      dec.poll(Instant::now(), &mut events);
    }
    if events.is_empty() {
      continue;
    }
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    for event in &events {
      match *event {
        DrumEvent::Fire { label, pressure } => {
          let kind = pad_kind(label);
          match resolve_fire(kind, label, &mut st.last_hit, pressure, db_range) {
            Some((src, gain)) => {
              st.pads[label as usize].last_pressure = pressure;
              st.pads[label as usize].last_gain = gain;
              let msg = if kind == PadKind::Ditto {
                format!("pad {label} (ditto): repeats pad {src}'s hit, pressure {pressure:.3} (gain {gain:.3})")
              } else {
                format!("pad {label}: pressure {pressure:.3} (gain {gain:.3})")
              };
              st.recent = msg.clone();
              st.push_message(msg);
              if audio_on {
                play_audition(gain);
              }
            }
            None => {
              let msg = format!("pad {label} (ditto): nothing has played yet -- no-op");
              st.recent = msg.clone();
              st.push_message(msg);
            }
          }
        }
        DrumEvent::Revise { label, pressure } => {
          let kind = pad_kind(label);
          let last_hit = st.last_hit;
          if let Some(gain) = resolve_revise(kind, &last_hit, pressure, db_range) {
            st.pads[label as usize].last_pressure = pressure;
            st.pads[label as usize].last_gain = gain;
          }
        }
        DrumEvent::Release { .. } => {} // one-shot display; the drumkit's samples ignore this too
      }
    }
  }
}

/// Redraw the whole screen in place (ANSI clear + cursor-home, like the Python
/// meter), one line per pad in printed-label order (1..9, then 0).
fn draw(state: &Mutex<MeterState>, start: Instant, params: SoftstepParams) {
  let now = Instant::now();
  let now_secs = now.duration_since(start).as_secs_f64();
  let silence = Duration::from_millis(params.silence_to_zero_ms);
  let mut st = state.lock().unwrap_or_else(|e| e.into_inner());

  let mut lines = vec![
    String::new(),
    "  SoftStep pressure meter (Rust) -- strike a pad: bar=pressure, peak-hold marked; pad 3=ditto".to_string(),
    "  keys: q + Enter quits (Ctrl-C also restores standalone mode)".to_string(),
    String::new(),
  ];
  for &(label, base) in LABEL_BASE.iter() {
    let idx0 = (base - 40) as usize;
    let sum: u16 = (0..4).map(|k| interp_shadow(&st.sensors, &st.last_seen, idx0 + k, now, silence)).sum();
    let pad = &mut st.pads[label as usize];
    let (pv, pt) = peak_hold_tick(sum, pad.peak_val, pad.peak_t, now_secs);
    pad.peak_val = pv;
    pad.peak_t = pt;
    let bar = render_bar(sum, pv);
    let pressure_s = if pad.last_pressure > 0.0 { format!("p={:.2}", pad.last_pressure) } else { "      ".to_string() };
    let ditto = if label == DITTO_LABEL { "*" } else { " " };
    lines.push(format!("  pad {label:<2}{ditto}[{bar}] {sum:3}  {pressure_s}  gain {:.2}", pad.last_gain));
  }
  lines.push(String::new());
  lines.push(format!("  last hit: {}", st.recent));
  lines.push(format!(
    "  (on_sum={} off_sum={} attack={}ms debounce={}ms full_scale={} db_range={} de-stick={}ms audio={})",
    params.on_sum,
    params.off_sum,
    params.attack_ms,
    params.debounce_ms,
    params.pressure_full_scale,
    params.gain_db_range,
    params.silence_to_zero_ms,
    if std::env::var_os("SOFTSTEP_METER_AUDIO").is_some() { "on" } else { "off" },
  ));
  lines.push(String::new());
  for m in &st.messages {
    lines.push(format!("    {m}"));
  }
  drop(st);

  print!("\x1b[2J\x1b[H{}\n", lines.join("\n"));
  let _ = io::stdout().flush();
}

/// Play the (single, reference) audition sample via `pw-play` at `gain` (clamped to
/// [0,1], matching the Python meter's `--volume`). Best-effort: a missing `pw-play`
/// or sample simply means no sound, never a crash.
fn play_audition(gain: f32) {
  let path = drum_samples_dir().join(AUDITION_SAMPLE);
  let vol = gain.clamp(0.0, 1.0);
  let _ = std::process::Command::new("pw-play")
    .arg(format!("--volume={vol:.3}"))
    .arg(path)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn();
}

/// Choose the KMSS input port: any whose name contains `substring`, preferring the
/// data port -- the one that carries the tether sensor stream. Which name that is
/// differs per unit, so the preference order lives in
/// `midi::SOFTSTEP_DATA_PORT_PREFERENCE`.
/// A small standalone re-statement of `drumkit_runtime::select_input_port` (private
/// there, and pulled in a `Rig`-shaped device list this meter has no rig for); the
/// port *preference* itself is shared, so the two cannot drift apart.
fn select_input_port(midi_in: &MidiInput, substring: &str) -> Result<MidiInputPort, String> {
  let mut matches: Vec<(MidiInputPort, String)> = midi_in
    .ports()
    .into_iter()
    .filter_map(|p| midi_in.port_name(&p).ok().map(|n| (p, n)))
    .filter(|(_, name)| name.contains(substring))
    .collect();
  if matches.is_empty() {
    let available: Vec<String> = midi_in.ports().iter().filter_map(|p| midi_in.port_name(p).ok()).collect();
    return Err(format!("no MIDI input port matching {substring:?}; available ports: {available:?}"));
  }
  let names: Vec<String> = matches.iter().map(|(_, n)| n.clone()).collect();
  let (idx, guessed) = midi::preferred_port_index(&names, &midi::SOFTSTEP_DATA_PORT_PREFERENCE)
    .expect("matches is non-empty");
  if guessed {
    eprintln!(
      "warning: {} MIDI input ports match {substring:?} and none looks like a data port \
       ({names:?}); binding {:?}.",
      names.len(),
      names[idx],
    );
  }
  Ok(matches.remove(idx).0)
}

#[cfg(test)]
mod tests {
  use super::*;

  const DB_RANGE: f32 = 20.0;

  #[test]
  fn pad_3_is_ditto_everything_else_is_real() {
    assert_eq!(pad_kind(3), PadKind::Ditto);
    for label in [1, 2, 4, 5, 6, 7, 8, 9, 0] {
      assert_eq!(pad_kind(label), PadKind::Real, "pad {label} should be real");
    }
  }

  #[test]
  fn a_real_fire_resolves_its_own_label_and_records_itself_as_last() {
    let mut last: Option<LastHit> = None;
    let (label, gain) = resolve_fire(PadKind::Real, 1, &mut last, 0.5, DB_RANGE).expect("a real pad always fires");
    assert_eq!(label, 1);
    assert!(gain > 0.0 && gain <= REAL_GAIN, "sub-max pressure resolves within (0, REAL_GAIN]: {gain}");
    assert_eq!(last, Some(LastHit { label: 1, gain: REAL_GAIN }), "records itself as the last real hit");
  }

  #[test]
  fn ditto_before_anything_played_is_a_silent_no_op() {
    let mut last: Option<LastHit> = None;
    assert_eq!(resolve_fire(PadKind::Ditto, DITTO_LABEL, &mut last, 0.8, DB_RANGE), None);
  }

  #[test]
  fn ditto_repeats_the_last_real_pads_label_at_the_dittos_own_pressure() {
    let mut last: Option<LastHit> = None;
    resolve_fire(PadKind::Real, 6, &mut last, 0.5, DB_RANGE).unwrap(); // pad 6 struck soft-ish
    let (echoed_label, echoed_gain) =
      resolve_fire(PadKind::Ditto, DITTO_LABEL, &mut last, 1.0, DB_RANGE).expect("a hit has played");
    assert_eq!(echoed_label, 6, "ditto echoes pad 6, not itself");
    assert!((echoed_gain - REAL_GAIN).abs() < 1e-6, "pressure 1.0 against REAL_GAIN trim = unity: {echoed_gain}");
  }

  #[test]
  fn a_hard_ditto_press_plays_louder_than_the_soft_hit_it_echoes() {
    let mut last: Option<LastHit> = None;
    let (_, soft_gain) = resolve_fire(PadKind::Real, 9, &mut last, 0.15, DB_RANGE).unwrap();
    let (echoed_label, hard_ditto_gain) =
      resolve_fire(PadKind::Ditto, DITTO_LABEL, &mut last, 1.0, DB_RANGE).expect("a hit has played");
    assert_eq!(echoed_label, 9);
    assert!(hard_ditto_gain > soft_gain, "hard ditto ({hard_ditto_gain}) > soft original ({soft_gain})");
  }

  #[test]
  fn pad_ditto_ditto_repeats_the_original_not_the_ditto() {
    let mut last: Option<LastHit> = None;
    resolve_fire(PadKind::Real, 4, &mut last, 0.8, DB_RANGE).unwrap();
    resolve_fire(PadKind::Ditto, DITTO_LABEL, &mut last, 0.15, DB_RANGE).expect("first ditto");
    resolve_fire(PadKind::Ditto, DITTO_LABEL, &mut last, 0.05, DB_RANGE).expect("second ditto");
    assert_eq!(last, Some(LastHit { label: 4, gain: REAL_GAIN }), "still pad 4 -- a ditto never becomes `last`");
  }

  #[test]
  fn revise_scales_a_real_pad_and_a_ditto_pad_once_something_has_played() {
    let mut last: Option<LastHit> = None;
    let scaled = resolve_revise(PadKind::Real, &last, 0.5, DB_RANGE).expect("a real pad always revises");
    assert!(scaled > 0.0 && scaled < REAL_GAIN, "sub-max pressure revises below unity: {scaled}");

    assert_eq!(resolve_revise(PadKind::Ditto, &last, 1.0, DB_RANGE), None, "nothing to revise against yet");

    resolve_fire(PadKind::Real, 2, &mut last, 0.5, DB_RANGE).unwrap();
    let ditto_scaled = resolve_revise(PadKind::Ditto, &last, 1.0, DB_RANGE).expect("a hit has played");
    assert!((ditto_scaled - REAL_GAIN).abs() < 1e-6, "pressure 1.0 against REAL_GAIN trim = unity: {ditto_scaled}");
  }

  #[test]
  fn render_bar_fills_proportionally_and_marks_the_peak() {
    // At rest (sum=0, peak=0) the bar is all dashes except column 0, which the peak
    // marker ('|') still occupies -- matching the Python meter's `bar[pk]='|'` when
    // `pk` is 0 (an idle peak-hold reads as column 0, not "no marker").
    let empty = render_bar(0, 0);
    assert_eq!(empty, format!("|{}", "-".repeat(BAR_WIDTH - 1)), "silent pad: no fill, peak marker at column 0");
    let full = render_bar(508, 508);
    assert_eq!(full.chars().filter(|&c| c == '#').count(), BAR_WIDTH, "max sum fills the whole bar");
    let half = render_bar(254, 254); // half of 508
    let fill_count = half.chars().filter(|&c| c == '#' || c == '|').count();
    assert!(fill_count >= BAR_WIDTH / 2 - 1 && fill_count <= BAR_WIDTH / 2 + 1, "roughly half-filled: {half:?}");
  }

  #[test]
  fn render_bar_peak_marker_sits_ahead_of_a_decayed_current_value() {
    let bar = render_bar(50, 300); // current has fallen well below the held peak
    let peak_col = ((300.0f32 / SUM_MAX) * BAR_WIDTH as f32) as usize;
    assert_eq!(bar.chars().nth(peak_col), Some('|'), "peak marker at its own column: {bar:?}");
  }

  #[test]
  fn peak_hold_snaps_up_immediately_to_a_fresh_high() {
    let (pv, pt) = peak_hold_tick(300, 100, 0.0, 0.05);
    assert_eq!(pv, 300, "a fresh high snaps the peak up immediately");
    assert_eq!(pt, 0.05, "and resets the hold clock to now");
  }

  #[test]
  fn peak_hold_sits_flat_until_the_hold_elapses_then_decays_every_tick() {
    let (pv, pt) = peak_hold_tick(0, 300, 0.0, 0.1); // well within the 0.6s hold
    assert_eq!((pv, pt), (300, 0.0), "held peak sits flat inside the hold window");

    let (pv2, pt2) = peak_hold_tick(0, 300, 0.0, 0.7); // past the hold
    assert_eq!(pv2, 300 - PEAK_DECAY_STEP, "decays by one step once the hold elapses");
    assert_eq!(pt2, 0.0, "the hold clock is NOT reset while decaying (mirrors the Python meter)");

    // A further tick without a fresh high keeps decaying (pt never advanced).
    let (pv3, _) = peak_hold_tick(0, pv2, pt2, 0.75);
    assert_eq!(pv3, 300 - 2 * PEAK_DECAY_STEP);
  }

  #[test]
  fn peak_hold_never_underflows_below_zero() {
    let (pv, _) = peak_hold_tick(0, 10, 0.0, 10.0); // way past the hold, small peak
    assert_eq!(pv, 0, "saturating_sub floors at 0, never wraps");
  }

  #[test]
  fn interp_shadow_desticks_after_silence() {
    let now0 = Instant::now();
    let mut sensors = [0u8; 40];
    let mut last_seen: [Option<Instant>; 40] = [None; 40];
    sensors[0] = 100;
    last_seen[0] = Some(now0);
    let silence = Duration::from_millis(25);
    assert_eq!(interp_shadow(&sensors, &last_seen, 0, now0, silence), 100);
    let later = now0 + Duration::from_millis(30);
    assert_eq!(interp_shadow(&sensors, &last_seen, 0, later, silence), 0, "de-sticks after silence");
  }

  #[test]
  fn interp_shadow_disabled_keeps_the_raw_value() {
    let now0 = Instant::now();
    let mut sensors = [0u8; 40];
    sensors[3] = 42;
    let last_seen: [Option<Instant>; 40] = [None; 40]; // never seen, but silence=0 disables de-stick
    assert_eq!(interp_shadow(&sensors, &last_seen, 3, now0, Duration::ZERO), 42);
  }

  #[test]
  fn label_base_matches_the_measured_device_map() {
    // Cross-check against learnings/keith-mcmillen-softstep.org's measured table.
    let expect: [(u8, u8); NUM_PADS] =
      [(1, 44), (2, 52), (3, 60), (4, 68), (5, 76), (6, 40), (7, 48), (8, 56), (9, 64), (0, 72)];
    assert_eq!(LABEL_BASE, expect);
  }
}
