//! The sustain pedals: foot down = ordinary piano sustain (CC 64) into a 58-edo
//! Reaper input. Two hardware choices, picked by a startup question:
//!
//! - *EX-P* (the preferred one): the two M-Audio EX-P expression pedals on the
//!   DOREMiDi MPC-20, read through the host-side bridge's "MPC-20 pedals" port
//!   (`tools/pedals/bridge.sh` -- the MPC-20's CH345 chip makes no ALSA device, so
//!   the bridge is the only path). Pedal 1 sustains output 0 ("58-edo 1", the LEFT
//!   keyboard); pedal 2 sustains output 1 ("58-edo 2"). EVERY pedal CC sends a
//!   sustain event -- raw value at or above `EXP_SUSTAIN_ON_AT` sends on, below
//!   sends off, no edge detection and no window. Stateless on purpose: the
//!   bridge's USB stream can drop messages (observed live 2026-07-25 as
//!   stuck-on/stuck-off sustain under an earlier transitions-only design), and
//!   resending the state on every message means the next pedal movement always
//!   resynchronizes the receiver.
//!
//! - *SoftStep (KMSS)*: pad "0" (the rightmost TOP pad) sustains output 0 only.
//!   The board streams each pad's 4 pressure sensors as CCs in tether mode; pad
//!   "0" owns CC 72..75 (slot 8 -- see `SLOT_LABEL` in the drumkit decoder). A held
//!   LEVEL, not a strike, so it does not go through the drumkit's fire/release
//!   machine. The rule is the window in `rigs/softstep.toml`: a pad-0 CC whose
//!   sum-of-4 is at least `sustain_on_sum` refreshes the window, sustain is on
//!   while the window is fresh, and `sustain_window_ms` without a qualifying
//!   signal ends it. The lapse doubles as the release debounce, so there is no off
//!   threshold -- the tether stream is on-change, and a lifted foot goes silent.
//!
//! Either way the gear is optional and hot-pluggable: absent at startup, a red
//! note prints and discovery retries until it appears or the runtime stops.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use midir::{MidiInput, MidiInputConnection, MidiInputPort};

use edo_surface::drumkit_runtime::{decode, tether};
use edo_surface::expression_pedals;
use edo_surface::midi;
use edo_surface::rig::SoftstepParams;

use super::record::SharedOutputGate;
use super::STOP_REQUESTED;

/// Which pedal hardware sustains, per the startup question.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SustainHardware {
  ExP,
  SoftStep,
}

/// Parse one answer line of the startup question. `None` = unrecognized, ask
/// again. Bare Enter defaults to EX-P, the preferred hardware.
pub(crate) fn parse_hardware_answer(line: &str) -> Option<SustainHardware> {
  match line.trim().to_ascii_lowercase().as_str() {
    "" | "e" | "exp" | "ex-p" | "expression" => Some(SustainHardware::ExP),
    "s" | "ss" | "softstep" | "kmss" => Some(SustainHardware::SoftStep),
    _ => None,
  }
}

/// EX-P: a pedal CC at or above this sustains, below releases (ties to ON). The
/// pedals' reliable travel is ~1..119 (`expression_pedals`), so 60 is mid-travel.
/// A top-of-file const like the travel bounds, not a per-rig knob.
const EXP_SUSTAIN_ON_AT: u8 = 60;

/// Pad printed "0" streams its 4 sensors on CC 72..75 (base = 40 + 4 * slot 8).
const PAD0_BASE_CC: u8 = 72;
const SUSTAIN_CC: u8 = 64;
/// How often pedal state is polled (bounds how late a transition can be).
const TICK: Duration = Duration::from_millis(10);
/// How often to re-probe for gear that wasn't there.
const DISCOVERY_RETRY: Duration = Duration::from_secs(2);

/// The pure pedal state: the pad-0 sensor snapshot and the hold window.
pub(crate) struct SustainWindow {
  sensors: [u8; 4],
  on_sum: u16,
  window: Duration,
  last_signal: Option<Instant>,
}

impl SustainWindow {
  pub(crate) fn new(params: &SoftstepParams) -> Self {
    SustainWindow {
      sensors: [0; 4],
      on_sum: params.sustain_on_sum,
      window: Duration::from_millis(params.sustain_window_ms),
      last_signal: None,
    }
  }

  /// Feed one CC. CCs outside pad 0's four sensors are ignored; a qualifying
  /// sum-of-4 (>= `sustain_on_sum`) refreshes the hold window, anything less is
  /// no signal at all.
  pub(crate) fn on_cc(&mut self, cc: u8, val: u8, now: Instant) {
    if !(PAD0_BASE_CC..PAD0_BASE_CC + 4).contains(&cc) {
      return;
    }
    self.sensors[(cc - PAD0_BASE_CC) as usize] = val;
    let sum: u16 = self.sensors.iter().map(|&v| v as u16).sum();
    if sum >= self.on_sum {
      self.last_signal = Some(now);
    }
  }

  pub(crate) fn is_on(&self, now: Instant) -> bool {
    matches!(self.last_signal, Some(t) if now.duration_since(t) <= self.window)
  }
}

/// CC 64 to every MIDI channel: the transform spreads octaves over channels (one
/// Pianoteq track per channel on the receiving side), and the pedal must hold all
/// of them -- a sustain that only reached middle C's channel would drop the bass.
pub(crate) fn sustain_messages(on: bool) -> Vec<Vec<u8>> {
  let value = if on { 127 } else { 0 };
  (0u8..16).map(|ch| vec![0xB0 | ch, SUSTAIN_CC, value]).collect()
}

fn send_sustain(gate: &SharedOutputGate, on: bool) {
  for message in sustain_messages(on) {
    gate.send_raw(message);
  }
}

/// Run until the runtime stops: find the chosen pedal gear (retrying while
/// absent) and pedal CC 64 on its target output(s). Releases sustain on the way
/// out; the SoftStep path also restores standalone mode.
pub(crate) fn run_sustain_thread(
  hardware: SustainHardware,
  params: SoftstepParams,
  gates: Vec<SharedOutputGate>,
) {
  match hardware {
    SustainHardware::ExP => run_exp(&gates),
    SustainHardware::SoftStep => run_softstep(&params, &gates[0]),
  }
}

/// The EX-P path: pedal 1 -> output 0 (58-edo 1, left), pedal 2 -> output 1.
/// Stateless: every pedal CC becomes a sustain message for its output.
fn run_exp(gates: &[SharedOutputGate]) {
  let mut announced_absent = false;
  let _connection = loop {
    if STOP_REQUESTED.load(Ordering::Relaxed) {
      return;
    }
    match connect_exp(gates) {
      Ok((connection, port_name)) => {
        println!(
          "sustain pedals: EX-P ({port_name}) -- pedal 1 -> 58-edo 1 (left), pedal 2 -> 58-edo 2",
        );
        break connection;
      }
      Err(e) => {
        if !announced_absent {
          announced_absent = true;
          // Red, like the surfaces runtime's missing-gear report.
          eprintln!("\x1b[31mno EX-P pedal bridge found -- sustain is off. {e}\x1b[0m");
        }
        sleep_responsively(DISCOVERY_RETRY);
      }
    }
  };
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    std::thread::sleep(TICK);
  }
  // Never leave the receiving synth's dampers up after exit. No state is
  // tracked, so release both unconditionally; a redundant off is harmless.
  for gate in gates {
    send_sustain(gate, false);
  }
}

/// Bind the bridge's "MPC-20 pedals" port; the callback turns each pedal CC
/// into an immediate sustain message on that pedal's output.
fn connect_exp(
  gates: &[SharedOutputGate],
) -> Result<(MidiInputConnection<()>, String), String> {
  let midi_in = MidiInput::new("edo-un12-exp-sustain").map_err(|e| e.to_string())?;
  let port = midi_in
    .ports()
    .into_iter()
    .find(|p| {
      midi_in
        .port_name(p)
        .is_ok_and(|n| n.contains(expression_pedals::DEFAULT_PORT))
    })
    .ok_or_else(|| {
      format!(
        "no MIDI input matching {:?} (is the bridge running? See the family readme, \"the bridge\").",
        expression_pedals::DEFAULT_PORT,
      )
    })?;
  let port_name = midi_in.port_name(&port).unwrap_or_else(|_| "<unknown>".to_string());
  let gates = gates.to_vec();
  let connection = midi_in
    .connect(
      &port,
      "exp-sustain",
      move |_timestamp, message, _| {
        if let Some((pedal, on)) = exp_event(message) {
          if pedal < gates.len() {
            send_sustain(&gates[pedal], on);
          }
        }
      },
      (),
    )
    .map_err(|e| format!("connect to {port_name:?}: {e}"))?;
  Ok((connection, port_name))
}

/// The pure EX-P rule: a pedal CC maps to (pedal index, sustain on?), anything
/// else to None. At or above the threshold sustains -- ties go to ON.
pub(crate) fn exp_event(message: &[u8]) -> Option<(usize, bool)> {
  expression_pedals::decode_pedal_cc(message)
    .map(|(pedal, raw)| (pedal, raw >= EXP_SUSTAIN_ON_AT))
}

/// The SoftStep path: find a board, enter tether mode, pedal output 0 from pad 0.
fn run_softstep(params: &SoftstepParams, gate: &SharedOutputGate) {
  let mut announced_absent = false;
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    match find_kmss_input() {
      Some((midi_in, port, port_name, select_substring)) => {
        run_with_board(midi_in, port, &port_name, &select_substring, params, gate);
        return;
      }
      None => {
        if !announced_absent {
          announced_absent = true;
          // Red, like the surfaces runtime's missing-gear report.
          eprintln!(
            "\x1b[31mno KMSS (SoftStep) found -- the pad-0 sustain pedal is off. \
             Plug the board in and it will be picked up automatically.\x1b[0m"
          );
        }
        sleep_responsively(DISCOVERY_RETRY);
      }
    }
  }
}

/// The KMSS's sensor-stream input port: any port whose name contains a SoftStep
/// client name, preferring the DATA port among that board's ports ("MIDI 1" on the
/// older board, "Control Surface" on the newer -- the TRS/CV ports carry no
/// sensors). Returns the `MidiInput` too, since midir consumes it on connect.
fn find_kmss_input() -> Option<(MidiInput, MidiInputPort, String, String)> {
  let midi_in = MidiInput::new("edo-un12-sustain").ok()?;
  let candidates: Vec<(MidiInputPort, String)> = midi_in
    .ports()
    .into_iter()
    .filter_map(|p| midi_in.port_name(&p).ok().map(|n| (p, n)))
    .filter(|(_, n)| n.contains("SSCOM") || n.contains("SoftStep"))
    .collect();
  let names: Vec<String> = candidates.iter().map(|(_, n)| n.clone()).collect();
  let (idx, _guessed) =
    midi::preferred_port_index(&names, &midi::SOFTSTEP_DATA_PORT_PREFERENCE)?;
  let (port, name) = candidates.into_iter().nth(idx)?;
  // The same selector the rigs use, so tether mode targets the right rawmidi card.
  let select = if name.contains("SSCOM") { "SSCOM" } else { "SoftStep" };
  Some((midi_in, port, name, select.to_string()))
}

fn run_with_board(
  midi_in: MidiInput,
  port: MidiInputPort,
  port_name: &str,
  select_substring: &str,
  params: &SoftstepParams,
  gate: &SharedOutputGate,
) {
  // Registered for restore-on-drop BEFORE entering, so a half-taken mode switch
  // still gets restored. The remap runtime owns SIGINT (STOP_REQUESTED), so the
  // no-signal-thread session is the right variant.
  let session = tether::session();
  if let Err(e) = session.enter(select_substring) {
    eprintln!("\x1b[31mcould not put the KMSS in tether mode: {e} -- sustain pedal off\x1b[0m");
    return;
  }

  let window = Arc::new(Mutex::new(SustainWindow::new(params)));
  let window_for_cb = Arc::clone(&window);
  let connection = midi_in.connect(
    &port,
    "sustain",
    move |_timestamp, message, _| {
      let mut ccs = Vec::new();
      decode::collect_control_changes(message, &mut ccs);
      if ccs.is_empty() {
        return;
      }
      let now = Instant::now();
      let mut window = window_for_cb.lock().unwrap();
      for (cc, val) in ccs {
        window.on_cc(cc, val, now);
      }
    },
    (),
  );
  let _connection = match connection {
    Ok(c) => c,
    Err(e) => {
      eprintln!("\x1b[31mcould not bind the KMSS sensor stream ({port_name}): {e}\x1b[0m");
      return;
    }
  };
  println!("sustain pedal: KMSS pad 0 ({port_name}) -> CC 64 on 58-edo 1");

  let mut on = false;
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    std::thread::sleep(TICK);
    let want = window.lock().unwrap().is_on(Instant::now());
    if want != on {
      on = want;
      send_sustain(gate, on);
    }
  }
  // Never leave the receiving synth's dampers up after exit.
  if on {
    send_sustain(gate, false);
  }
  // `_connection` and `session` drop here: stream released, standalone restored.
}

fn sleep_responsively(total: Duration) {
  let step = Duration::from_millis(100);
  let mut slept = Duration::ZERO;
  while slept < total && !STOP_REQUESTED.load(Ordering::Relaxed) {
    std::thread::sleep(step);
    slept += step;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn params(on_sum: u16, window_ms: u64) -> SoftstepParams {
    SoftstepParams {
      sustain_on_sum: on_sum,
      sustain_window_ms: window_ms,
      ..SoftstepParams::default()
    }
  }

  #[test]
  fn a_qualifying_pad0_cc_turns_sustain_on() {
    let mut w = SustainWindow::new(&params(40, 150));
    let t0 = Instant::now();
    assert!(!w.is_on(t0));
    w.on_cc(72, 50, t0);
    assert!(w.is_on(t0));
  }

  #[test]
  fn sums_below_the_threshold_are_no_signal_at_all() {
    let mut w = SustainWindow::new(&params(40, 150));
    let t0 = Instant::now();
    w.on_cc(72, 20, t0);
    w.on_cc(73, 19, t0);
    assert!(!w.is_on(t0), "sum 39 is below the 40 threshold");
    w.on_cc(74, 1, t0);
    assert!(w.is_on(t0), "the sensors sum across the pad: 20+19+1 = 40");
  }

  #[test]
  fn ccs_of_other_pads_are_ignored() {
    let mut w = SustainWindow::new(&params(40, 150));
    let t0 = Instant::now();
    w.on_cc(71, 127, t0); // pad "4"'s last sensor
    w.on_cc(76, 127, t0); // pad "5"'s first sensor
    w.on_cc(64, 127, t0); // some other pad entirely
    assert!(!w.is_on(t0));
  }

  #[test]
  fn sustain_lapses_when_the_window_empties() {
    let mut w = SustainWindow::new(&params(40, 150));
    let t0 = Instant::now();
    w.on_cc(72, 50, t0);
    assert!(w.is_on(t0 + Duration::from_millis(150)), "the boundary is inclusive");
    assert!(!w.is_on(t0 + Duration::from_millis(151)));
  }

  #[test]
  fn a_qualifying_signal_restarts_the_window_from_its_own_time() {
    let mut w = SustainWindow::new(&params(40, 150));
    let t0 = Instant::now();
    w.on_cc(72, 50, t0);
    w.on_cc(73, 10, t0 + Duration::from_millis(100)); // sum 60: still pressed
    assert!(w.is_on(t0 + Duration::from_millis(250)));
    assert!(!w.is_on(t0 + Duration::from_millis(251)));
  }

  #[test]
  fn a_lift_is_silence_not_a_low_reading() {
    // The release path in practice: the foot comes off, the sensors send a few
    // low readings, then nothing. The low readings are not signals, so the
    // window lapses from the last QUALIFYING one.
    let mut w = SustainWindow::new(&params(40, 150));
    let t0 = Instant::now();
    w.on_cc(72, 100, t0);
    w.on_cc(72, 10, t0 + Duration::from_millis(50)); // bounce: sum 10, no signal
    w.on_cc(72, 0, t0 + Duration::from_millis(60));
    assert!(w.is_on(t0 + Duration::from_millis(150)));
    assert!(!w.is_on(t0 + Duration::from_millis(151)));
  }

  #[test]
  fn the_hardware_question_takes_e_s_and_defaults_to_exp() {
    assert_eq!(parse_hardware_answer("e\n"), Some(SustainHardware::ExP));
    assert_eq!(parse_hardware_answer("EXP\n"), Some(SustainHardware::ExP));
    assert_eq!(parse_hardware_answer("\n"), Some(SustainHardware::ExP), "bare Enter -> EX-P");
    assert_eq!(parse_hardware_answer("s\n"), Some(SustainHardware::SoftStep));
    assert_eq!(parse_hardware_answer("SoftStep\n"), Some(SustainHardware::SoftStep));
    assert_eq!(parse_hardware_answer("what\n"), None, "unrecognized -> ask again");
  }

  #[test]
  fn every_pedal_cc_maps_to_a_sustain_event_ties_on() {
    // CC 21 on channel 1 = pedal 0; channel 2 = pedal 1.
    assert_eq!(exp_event(&[0xB0, 21, 90]), Some((0, true)));
    assert_eq!(exp_event(&[0xB0, 21, 59]), Some((0, false)));
    assert_eq!(exp_event(&[0xB0, 21, 60]), Some((0, true)), "the tie goes to ON");
    assert_eq!(exp_event(&[0xB1, 21, 1]), Some((1, false)), "full heel releases");
    assert_eq!(exp_event(&[0xB1, 21, 119]), Some((1, true)), "full toe sustains");
  }

  #[test]
  fn non_pedal_messages_are_no_event_at_all() {
    assert_eq!(exp_event(&[0xB0, 22, 90]), None, "wrong controller");
    assert_eq!(exp_event(&[0xB2, 21, 90]), None, "channel 3 is no pedal");
    assert_eq!(exp_event(&[0x90, 60, 90]), None, "a note is not a pedal CC");
  }

  #[test]
  fn sustain_messages_cover_all_16_channels() {
    let on = sustain_messages(true);
    assert_eq!(on.len(), 16);
    for (ch, message) in on.iter().enumerate() {
      assert_eq!(message[0], 0xB0 | ch as u8);
      assert_eq!(message[1], 64);
      assert_eq!(message[2], 127);
    }
    assert!(sustain_messages(false).iter().all(|m| m[2] == 0));
  }
}
