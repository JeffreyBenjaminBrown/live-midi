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
//! Displays, for the two most-recently-struck pads (newest on top), each of that pad's
//! 4 pressure sensors AND their sum as its own live bar (0..127 per sensor, 0..508 for
//! the sum) with a peak-hold marker; the top pad also shows a running count of hits
//! since it reached the top (kept as it slides to second). On every detected hit it
//! shows the pad, resolved pressure and gain. Pad 3 ("the gap between the feet",
//! mirroring rigs/kmss-drumkit.toml) is
//! wired as DITTO: struck, it repeats the last REAL pad's hit at the ditto press's
//! OWN pressure (pad, ditto, ditto repeats the ORIGINAL, not the ditto -- see
//! `resolve_fire`, which mirrors `drumkit_runtime::mod::resolve_fire`).
//!
//! Restores standalone mode on exit -- Ctrl-C (via `tether::arm`'s signal thread) or
//! `q` + Enter (via `TetherSession`'s `Drop`) -- exactly like the drumkit runtime.
//!
//! Audio audition (ON by default): plays drum-samples/snare.wav via `pw-play` at the
//! resolved gain on every hit -- one reference sample for every pad, since this is a
//! touch/pressure meter, not the kit (mirrors the Python meter's `audition-sample`).
//! Set SOFTSTEP_METER_SILENT=1 for a silent meter. Launch under `pw-jack` so that
//! sample shares the sound card via PipeWire.

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

/// The base CC of pad `label`'s first sensor (its 4 sensors are `base..=base+3`).
/// `label` is a printed pad label 0..9, always present in `LABEL_BASE`.
fn label_base(label: u8) -> u8 {
  LABEL_BASE.iter().find(|(l, _)| *l == label).map(|(_, b)| *b).unwrap_or(40)
}

/// The pad wired as ditto, mirroring rigs/kmss-drumkit.toml's pedal 3 ("the gap
/// between the feet"): struck, it repeats the last REAL pad's hit at ITS OWN
/// pressure.
const DITTO_LABEL: u8 = 3;

const SUM_MAX: f32 = 508.0;
const SENSOR_MAX: f32 = 127.0; // one sensor's full scale (four of these sum to SUM_MAX)
const BAR_WIDTH: usize = 44;
const PEAK_HOLD_SECS: f64 = 0.6;
const PEAK_DECAY_STEP: u16 = 24; // per-tick peak fall for the 0..508 sum bar
const SENSOR_PEAK_DECAY_STEP: u16 = 6; // the same fall rate scaled to a 0..127 sensor bar
const RECENT_PADS_SHOWN: usize = 2; // how many recently-struck pads meter at once
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
/// for `PEAK_HOLD_SECS`, it falls `decay_step` per redraw tick -- the hold timer is
/// deliberately NOT reset while decaying, so once the hold elapses the peak keeps
/// falling every tick until `current` catches up (or it reaches 0). `decay_step`
/// scales with the bar's full range (see `PEAK_DECAY_STEP` / `SENSOR_PEAK_DECAY_STEP`).
fn peak_hold_tick(current: u16, pv: u16, pt: f64, now: f64, decay_step: u16) -> (u16, f64) {
  if current >= pv {
    (current, now)
  } else if now - pt > PEAK_HOLD_SECS {
    (pv.saturating_sub(decay_step), pt)
  } else {
    (pv, pt)
  }
}

/// Render one bar: `#` filled 0..fill, `-` elsewhere, with a `|` peak-hold marker
/// overlaid at the peak's column (if in range). `max` is the bar's full-scale value
/// (`SUM_MAX` for a pad's sum, `SENSOR_MAX` for a single sensor). Mirrors the Python
/// meter's bar (`BARW`=44).
fn render_bar(value: u16, peak: u16, max: f32) -> String {
  let fill = ((value as f32 / max) * BAR_WIDTH as f32) as usize;
  let peak_col = ((peak as f32 / max) * BAR_WIDTH as f32) as usize;
  let mut bar = vec!['-'; BAR_WIDTH];
  for c in bar.iter_mut().take(fill.min(BAR_WIDTH)) {
    *c = '#';
  }
  if peak_col < BAR_WIDTH {
    bar[peak_col] = '|';
  }
  bar.into_iter().collect()
}

/// The pads shown as full 4-sensor meters: the last `RECENT_PADS_SHOWN` distinct pads
/// struck, newest first. `count` is how many hits the pad has detected since it last
/// reached the top -- reset to 1 the instant it becomes newest, bumped on each further
/// hit while it stays on top, and *kept* as it slides to second (a different pad
/// displacing it never touches its count).
#[derive(Clone, Copy, Debug, PartialEq)]
struct RecentPad {
  label: u8,
  count: u32,
}

/// Record a detected hit on `label` into the recent-pads list (front = newest, the
/// invariant the display relies on). Already on top -> its count bumps; otherwise it
/// jumps to the front with a fresh count of 1, the old top slides to second keeping its
/// own count, and the list is capped at `RECENT_PADS_SHOWN`. Pure -- unit-tested below.
fn note_hit(recent: &mut Vec<RecentPad>, label: u8) {
  if let Some(front) = recent.first_mut() {
    if front.label == label {
      front.count += 1;
      return;
    }
  }
  recent.retain(|p| p.label != label);
  recent.insert(0, RecentPad { label, count: 1 });
  recent.truncate(RECENT_PADS_SHOWN);
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
  sensor_peak_val: [u16; 40],           // per-sensor peak-hold value (one bar each)
  sensor_peak_t: [f64; 40],             // per-sensor peak-hold timer, seconds
  pads: [PadDisplay; NUM_PADS],         // per-pad sum peak-hold + last pressure/gain, by label
  recent_pads: Vec<RecentPad>,          // last two pads struck, newest first (what the meter draws)
  last_hit: Option<LastHit>,            // for ditto resolution (`resolve_fire`)
  recent: String,                       // last hit line, like the Python meter's `recent`
  messages: VecDeque<String>,           // recent event log, newest last (last 8, like Python)
}

impl MeterState {
  fn new() -> Self {
    MeterState {
      sensors: [0; 40],
      last_seen: [None; 40],
      sensor_peak_val: [0; 40],
      sensor_peak_t: [0.0; 40],
      pads: [PadDisplay::default(); NUM_PADS],
      recent_pads: Vec::new(),
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

/// The SoftStep boards this meter knows how to name, as (port-name substring, label).
/// The two units present disjointly: SSCOM ports never contain "SoftStep" and vice
/// versa, so a substring match picks exactly one board.
const KNOWN_BOARDS: [(&str, &str); 2] =
  [("SSCOM", "older SoftStep (SSCOM)"), ("SoftStep", "newer SoftStep")];

/// Which known boards are actually plugged in, by scanning the MIDI input ports once.
fn present_boards() -> Vec<(&'static str, &'static str)> {
  let Ok(midi_in) = MidiInput::new("softstep-meter-probe") else {
    return vec![];
  };
  let names: Vec<String> =
    midi_in.ports().iter().filter_map(|p| midi_in.port_name(p).ok()).collect();
  KNOWN_BOARDS
    .into_iter()
    .filter(|(sub, _)| names.iter().any(|n| n.contains(sub)))
    .collect()
}

/// One board's live meter: its display state, its poll thread, and the MIDI connection
/// kept alive for the run.
struct Board {
  label: String,
  state: Arc<Mutex<MeterState>>,
  stop: Arc<AtomicBool>,
  poll: thread::JoinHandle<()>,
  _conn: midir::MidiInputConnection<()>,
}

/// Tether one board and start reading it. Returns an error (rather than aborting the
/// whole meter) if this board cannot be brought up, so one dead board does not hide a
/// working one.
fn start_board(
  substring: &str,
  label: &str,
  params: SoftstepParams,
  audio_on: bool,
  session: &tether::TetherSession,
) -> Result<Board, String> {
  session.enter(substring).map_err(|e| format!("tether {label}: {e}"))?;

  let state = Arc::new(Mutex::new(MeterState::new()));
  let decoder = Arc::new(Mutex::new(TetherDecoder::new(params)));

  let midi_in = MidiInput::new(&format!("softstep-meter-{substring}")).map_err(|e| e.to_string())?;
  let port = select_input_port(&midi_in, substring)?;
  let port_name = midi_in.port_name(&port).unwrap_or_else(|_| "<unknown>".to_string());

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
    .map_err(|e| format!("connect {label} ({port_name:?}): {e}"))?;

  let stop = Arc::new(AtomicBool::new(false));
  let poll = {
    let state = Arc::clone(&state);
    let stop = Arc::clone(&stop);
    thread::Builder::new()
      .name(format!("softstep-meter-poll-{substring}"))
      .spawn(move || run_poll_loop(decoder, state, stop, params.gain_db_range, audio_on))
      .map_err(|e| e.to_string())?
  };

  println!("  bound {label}: {port_name:?}");
  Ok(Board { label: label.to_string(), state, stop, poll, _conn: conn })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let params = load_softstep_params()?;
  let audio_on = std::env::var_os("SOFTSTEP_METER_SILENT").is_none();

  // With an argument, meter just the board whose port name contains it (the old
  // behaviour). With none, meter EVERY connected board -- which is what a two-board
  // rig wants, and what stops the old default of "SSCOM only" from silently hiding
  // the newer board. `q` + Enter quits; Ctrl-C restores standalone on all of them.
  let targets: Vec<(String, String)> = match std::env::args().nth(1) {
    Some(arg) => vec![(arg.clone(), arg)],
    None => {
      let present = present_boards();
      if present.is_empty() {
        return Err("no SoftStep found (looked for SSCOM and SoftStep); is one plugged in?".into());
      }
      present.into_iter().map(|(sub, label)| (sub.to_string(), label.to_string())).collect()
    }
  };

  println!("softstep_meter -- SoftStep pressure meter (Rust; the reference implementation)");
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
  println!("  pad {DITTO_LABEL} is DITTO (mirrors rigs/kmss-drumkit.toml pedal 3, the gap between the feet)");
  if audio_on {
    println!("  audio audition ON (default): plays drum-samples/{AUDITION_SAMPLE} via `pw-play` at the resolved gain on each hit. Set SOFTSTEP_METER_SILENT=1 to silence.");
  } else {
    println!("  audio audition OFF (SOFTSTEP_METER_SILENT set): silent meter.");
  }
  println!("  keys: q + Enter quits (Ctrl-C also restores standalone mode)\n");

  // Arm Ctrl-C/SIGTERM restoration FIRST, before any MIDI/poll thread spawns, so the
  // signal block is inherited by all of them. One session covers every board.
  let session = tether::arm();

  let mut boards: Vec<Board> = Vec::new();
  for (substring, label) in &targets {
    match start_board(substring, label, params, audio_on, &session) {
      Ok(board) => boards.push(board),
      Err(e) => eprintln!("  skipped a board: {e}"),
    }
  }
  if boards.is_empty() {
    return Err("no SoftStep could be brought up".into());
  }
  println!();

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
    draw(&boards, start, params, audio_on);
    thread::sleep(DRAW_TICK);
  }

  // Tear down like `DrumSession`'s `Drop`: stop the poll threads, release the MIDI
  // connections, THEN restore standalone (the `session` drop, after every `_conn`).
  for board in &boards {
    board.stop.store(true, Ordering::Relaxed);
  }
  for board in boards {
    let _ = board.poll.join();
    drop(board._conn);
  }
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
        DrumEvent::Fire { label, pressure, .. } => {
          // Every detected hit brings its pad to the top of the two-pad meter and
          // advances that pad's running count (ditto no-ops included -- the pad WAS
          // struck), before we resolve ditto / message / audition below.
          note_hit(&mut st.recent_pads, label);
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
        DrumEvent::Revise { label, pressure, .. } => {
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

/// Redraw the whole screen in place (ANSI clear + cursor-home, like the Python meter).
/// Every connected board gets its own labelled section; within each, the two
/// most-recently-struck pads, newest first, each as four sensor bars then their sum
/// bar, with the top pad's running hit-count, its last-hit line, and its event log.
fn draw(boards: &[Board], start: Instant, params: SoftstepParams, audio_on: bool) {
  let now = Instant::now();
  let now_secs = now.duration_since(start).as_secs_f64();
  let silence = Duration::from_millis(params.silence_to_zero_ms);

  let mut lines = vec![
    String::new(),
    "  SoftStep pressure meter (Rust) -- the last two pads struck, newest on top;".to_string(),
    "  each shows its 4 sensors then their sum -- bar (peak-hold |), value, recent max; pad 3 = ditto.".to_string(),
    "  keys: q + Enter quits (Ctrl-C also restores standalone mode)".to_string(),
  ];

  for board in boards {
    lines.push(String::new());
    // Only label the board when there is more than one, so a single-board run reads
    // exactly as it always did.
    if boards.len() > 1 {
      lines.push(format!("  == {} ==", board.label));
    }
    let mut st = board.state.lock().unwrap_or_else(|e| e.into_inner());

    if st.recent_pads.is_empty() {
      lines.push("  (strike a pad -- the two most-recently-struck pads meter here)".to_string());
      lines.push(String::new());
    } else {
      // Clone the short recent list so we can mutate the peak-hold state it points into.
      let recent = st.recent_pads.clone();
      for (pos, rp) in recent.iter().enumerate() {
        let which = if pos == 0 { "newest" } else { "previous" };
        let ditto = if rp.label == DITTO_LABEL { " *ditto" } else { "" };
        lines.push(format!("  pad {} ({}){}  --  hits since top: {}", rp.label, which, ditto, rp.count));
        let idx0 = (label_base(rp.label) - 40) as usize;
        // Each of the pad's 4 sensors as its own 0..127 bar.
        for k in 0..4 {
          let idx = idx0 + k;
          let v = interp_shadow(&st.sensors, &st.last_seen, idx, now, silence);
          let (pv, pt) =
            peak_hold_tick(v, st.sensor_peak_val[idx], st.sensor_peak_t[idx], now_secs, SENSOR_PEAK_DECAY_STEP);
          st.sensor_peak_val[idx] = pv;
          st.sensor_peak_t[idx] = pt;
          let bar = render_bar(v, pv, SENSOR_MAX);
          lines.push(format!("    {:<4}[{}] {:3}  max {:3}", format!("s{}", k + 1), bar, v, pv));
        }
        // Their sum as the 0..508 bar the meter used to show alone (with pressure + gain).
        let sum: u16 = (0..4).map(|k| interp_shadow(&st.sensors, &st.last_seen, idx0 + k, now, silence)).sum();
        let pad = &mut st.pads[rp.label as usize];
        let (pv, pt) = peak_hold_tick(sum, pad.peak_val, pad.peak_t, now_secs, PEAK_DECAY_STEP);
        pad.peak_val = pv;
        pad.peak_t = pt;
        let bar = render_bar(sum, pv, SUM_MAX);
        let pressure_s = if pad.last_pressure > 0.0 { format!("p={:.2}", pad.last_pressure) } else { "      ".to_string() };
        lines.push(format!("    {:<4}[{}] {:3}  max {:3}  {}  gain {:.2}", "sum", bar, sum, pv, pressure_s, pad.last_gain));
        lines.push(String::new());
      }
    }

    lines.push(format!("  last hit: {}", st.recent));
    for m in &st.messages {
      lines.push(format!("    {m}"));
    }
  }

  lines.push(String::new());
  lines.push(format!(
    "  (on_sum={} off_sum={} attack={}ms debounce={}ms full_scale={} db_range={} de-stick={}ms audio={})",
    params.on_sum,
    params.off_sum,
    params.attack_ms,
    params.debounce_ms,
    params.pressure_full_scale,
    params.gain_db_range,
    params.silence_to_zero_ms,
    if audio_on { "on" } else { "off" },
  ));
  lines.push(String::new());

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
    let empty = render_bar(0, 0, SUM_MAX);
    assert_eq!(empty, format!("|{}", "-".repeat(BAR_WIDTH - 1)), "silent pad: no fill, peak marker at column 0");
    let full = render_bar(508, 508, SUM_MAX);
    assert_eq!(full.chars().filter(|&c| c == '#').count(), BAR_WIDTH, "max sum fills the whole bar");
    let half = render_bar(254, 254, SUM_MAX); // half of 508
    let fill_count = half.chars().filter(|&c| c == '#' || c == '|').count();
    assert!(fill_count >= BAR_WIDTH / 2 - 1 && fill_count <= BAR_WIDTH / 2 + 1, "roughly half-filled: {half:?}");
  }

  #[test]
  fn render_bar_uses_its_max_so_a_single_sensor_fills_at_127() {
    // A sensor bar is the same shape as the sum bar, just full-scaled to 127 not 508.
    let full = render_bar(127, 127, SENSOR_MAX);
    assert_eq!(full.chars().filter(|&c| c == '#').count(), BAR_WIDTH, "a maxed sensor fills its whole bar");
    let half = render_bar(64, 64, SENSOR_MAX);
    let fill_count = half.chars().filter(|&c| c == '#' || c == '|').count();
    assert!(fill_count >= BAR_WIDTH / 2 - 1 && fill_count <= BAR_WIDTH / 2 + 1, "half a sensor ~ half a bar: {half:?}");
    // The same raw value reads much fuller on the sensor scale than on the sum scale.
    let on_sensor = render_bar(100, 100, SENSOR_MAX).chars().filter(|&c| c == '#').count();
    let on_sum = render_bar(100, 100, SUM_MAX).chars().filter(|&c| c == '#').count();
    assert!(on_sensor > on_sum, "100 fills more of a 127-scale bar ({on_sensor}) than a 508-scale bar ({on_sum})");
  }

  #[test]
  fn render_bar_peak_marker_sits_ahead_of_a_decayed_current_value() {
    let bar = render_bar(50, 300, SUM_MAX); // current has fallen well below the held peak
    let peak_col = ((300.0f32 / SUM_MAX) * BAR_WIDTH as f32) as usize;
    assert_eq!(bar.chars().nth(peak_col), Some('|'), "peak marker at its own column: {bar:?}");
  }

  #[test]
  fn peak_hold_snaps_up_immediately_to_a_fresh_high() {
    let (pv, pt) = peak_hold_tick(300, 100, 0.0, 0.05, PEAK_DECAY_STEP);
    assert_eq!(pv, 300, "a fresh high snaps the peak up immediately");
    assert_eq!(pt, 0.05, "and resets the hold clock to now");
  }

  #[test]
  fn peak_hold_sits_flat_until_the_hold_elapses_then_decays_every_tick() {
    let (pv, pt) = peak_hold_tick(0, 300, 0.0, 0.1, PEAK_DECAY_STEP); // well within the 0.6s hold
    assert_eq!((pv, pt), (300, 0.0), "held peak sits flat inside the hold window");

    let (pv2, pt2) = peak_hold_tick(0, 300, 0.0, 0.7, PEAK_DECAY_STEP); // past the hold
    assert_eq!(pv2, 300 - PEAK_DECAY_STEP, "decays by one step once the hold elapses");
    assert_eq!(pt2, 0.0, "the hold clock is NOT reset while decaying (mirrors the Python meter)");

    // A further tick without a fresh high keeps decaying (pt never advanced).
    let (pv3, _) = peak_hold_tick(0, pv2, pt2, 0.75, PEAK_DECAY_STEP);
    assert_eq!(pv3, 300 - 2 * PEAK_DECAY_STEP);
  }

  #[test]
  fn peak_hold_decay_step_scales_with_the_bar() {
    // A 0..127 sensor bar falls by the smaller SENSOR_PEAK_DECAY_STEP, not the sum's step.
    let (pv, _) = peak_hold_tick(0, 100, 0.0, 0.7, SENSOR_PEAK_DECAY_STEP);
    assert_eq!(pv, 100 - SENSOR_PEAK_DECAY_STEP, "a sensor bar decays by its own, smaller step");
  }

  #[test]
  fn peak_hold_never_underflows_below_zero() {
    let (pv, _) = peak_hold_tick(0, 10, 0.0, 10.0, PEAK_DECAY_STEP); // way past the hold, small peak
    assert_eq!(pv, 0, "saturating_sub floors at 0, never wraps");
  }

  // ---- recent-pads bookkeeping (the two-pad meter + running hit count) ----

  fn labels(recent: &[RecentPad]) -> Vec<u8> {
    recent.iter().map(|p| p.label).collect()
  }

  #[test]
  fn note_hit_first_press_puts_the_pad_on_top_with_a_fresh_count() {
    let mut r = Vec::new();
    note_hit(&mut r, 5);
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].label, r[0].count), (5, 1), "a first press starts its count at 1");
  }

  #[test]
  fn note_hit_bumps_the_count_while_the_same_pad_stays_on_top() {
    let mut r = Vec::new();
    note_hit(&mut r, 5);
    note_hit(&mut r, 5);
    note_hit(&mut r, 5);
    assert_eq!(r.len(), 1, "re-striking the top pad does not add a row");
    assert_eq!(r[0].count, 3, "each hit on the top pad advances its running count");
  }

  #[test]
  fn note_hit_a_new_pad_takes_the_top_and_the_old_top_keeps_its_count_at_second() {
    let mut r = Vec::new();
    note_hit(&mut r, 5); // 5 -> top
    note_hit(&mut r, 5); // 5 count 2
    note_hit(&mut r, 7); // 7 -> top; 5 slides to second, keeping count 2
    assert_eq!(labels(&r), vec![7, 5], "newest on top");
    assert_eq!((r[0].label, r[0].count), (7, 1), "the new top starts fresh at 1");
    assert_eq!((r[1].label, r[1].count), (5, 2), "the demoted pad keeps its count");
  }

  #[test]
  fn note_hit_shows_only_the_two_most_recently_struck_pads() {
    let mut r = Vec::new();
    note_hit(&mut r, 1);
    note_hit(&mut r, 2);
    note_hit(&mut r, 3); // pad 1 falls off the bottom
    assert_eq!(labels(&r), vec![3, 2], "only the two newest distinct pads remain");
  }

  #[test]
  fn note_hit_re_striking_the_second_pad_promotes_it_with_a_fresh_count() {
    let mut r = Vec::new();
    note_hit(&mut r, 5); // top 5
    note_hit(&mut r, 7); // top 7, second 5
    note_hit(&mut r, 7); // top 7 count 2, second 5
    note_hit(&mut r, 5); // 5 was at second -> back to top with a FRESH count; 7 demotes keeping 2
    assert_eq!(labels(&r), vec![5, 7]);
    assert_eq!(r[0].count, 1, "returning to the top resets the count to 1");
    assert_eq!(r[1].count, 2, "the pad it displaced keeps the count it had on top");
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
