//! Pure tether-mode decoding: the SoftStep's hosted sensor stream -> per-pad hits
//! with velocity. No MIDI/audio I/O, so it is unit-testable without hardware.
//!
//! In hosted/tether mode (see `kmss-research.org`) the device streams each pad's 4
//! pressure sensors as Control Change on channel 0: CC number = sensor index, value
//! 0..127 = reading. A pad owns `baseCC..baseCC+3`. We sum a pad's 4 sensors, detect
//! an onset (sum rising across an on-threshold), capture the attack PEAK over a
//! short window, and emit one hit per strike whose velocity is that peak scaled to
//! 1..127. Each pad has its own slot, so two feet landing together yield two hits --
//! the limitation the old Program-Change path could not escape.

use std::time::{Duration, Instant};

pub const NUM_PADS: usize = 10;

/// Printed pedal label (1..9, then 0) for each pad slot. Slot = `(baseCC-40)/4`; the
/// base CCs in slot order are 40,44,48,...,76. The label<->base mapping was measured
/// on the device (`kmss-research.org`): e.g. base 44 = label "1", base 40 = label
/// "6", base 72 = label "0".
const SLOT_LABEL: [u8; NUM_PADS] = [6, 1, 7, 2, 8, 3, 9, 4, 0, 5];

/// dB spread between the softest (vel 1) and hardest (vel 127) hit. Loudness is
/// ~logarithmic, so velocity is spread over a fixed dB range rather than scaling
/// amplitude linearly (linear would put vel 127 ~42 dB above vel 1 -- far too much).
/// Bigger = more dynamic contrast; smaller = flatter. 20 dB => the softest hit is
/// 1/10 the amplitude of the hardest. **This is the knob to turn for velocity feel.**
pub const VELOCITY_DB_RANGE: f32 = 20.0;

/// Sum-of-4 sensor value that maps to full velocity (127). Hard hits measured
/// ~430-460 against a 508 ceiling, so 460 makes a solid strike read as ~127.
const VELOCITY_FULL_SCALE: u16 = 460;

/// Onset/release thresholds on a pad's sum-of-4 (hysteresis: must fall below
/// `OFF_SUM` to re-arm, so a held foot can't chatter), and how long after onset we
/// keep capturing the attack peak before firing.
///
/// `ATTACK` is set from real hard-floor captures: the device refreshes its sensors
/// only ~every 10 ms, and a deliberate strike peaks on the *second* refresh (~10-11
/// ms after onset). 14 ms clears that second frame reliably (a 10 ms window
/// occasionally fired just before the peak landed, registering a hard hit as ~17%).
/// Lower it for less latency at the risk of clipping the peak; raise it if soft hits
/// on a springier surface peak later. (On carpet the peak lagged 20+ ms.)
const ON_SUM: u16 = 40;
const OFF_SUM: u16 = 20;
const ATTACK: Duration = Duration::from_millis(14);

/// A decoded strike: which printed pedal label, and velocity 1..127.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
  pub label: u8,
  pub velocity: u8,
}

/// Linear gain multiplier for a velocity 1..127, spread over `VELOCITY_DB_RANGE`
/// (vel 127 -> 1.0). Multiply a pad's configured base gain by this.
pub fn gain_from_velocity(velocity: u8) -> f32 {
  let v = velocity.clamp(1, 127);
  let t = (v as f32 - 1.0) / 126.0; // 0..1
  let db = (t - 1.0) * VELOCITY_DB_RANGE; // -RANGE..0
  10f32.powf(db / 20.0)
}

fn velocity_from_peak(peak: u16) -> u8 {
  let v = (peak as u32 * 127 / VELOCITY_FULL_SCALE as u32).max(1);
  v.min(127) as u8
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PadState {
  Idle,
  Rising { onset: Instant, peak: u16 },
  Held,
}

/// Per-pad onset / attack-peak / velocity state machine over the hosted CC stream.
pub struct TetherDecoder {
  sensors: [u8; 40], // CC 40..79, index = cc - 40
  pads: [PadState; NUM_PADS],
}

impl TetherDecoder {
  pub fn new() -> Self {
    TetherDecoder { sensors: [0; 40], pads: [PadState::Idle; NUM_PADS] }
  }

  /// Feed one Control-Change sensor reading at time `now`. CCs outside the key range
  /// (nav pad 80..83, expression pedal 86, etc.) are ignored.
  pub fn on_cc(&mut self, cc: u8, val: u8, now: Instant) {
    if !(40..=79).contains(&cc) {
      return;
    }
    self.sensors[(cc - 40) as usize] = val & 0x7F;
    let slot = ((cc - 40) / 4) as usize;
    let sum = self.pad_sum(slot);
    self.pads[slot] = match self.pads[slot] {
      PadState::Idle if sum > ON_SUM => PadState::Rising { onset: now, peak: sum },
      PadState::Idle => PadState::Idle,
      // Released before the attack window elapsed: drop the (sub-window) press.
      PadState::Rising { .. } if sum < OFF_SUM => PadState::Idle,
      PadState::Rising { onset, peak } => PadState::Rising { onset, peak: peak.max(sum) },
      PadState::Held if sum < OFF_SUM => PadState::Idle,
      PadState::Held => PadState::Held,
    };
  }

  /// Emit a hit for any pad whose attack window has elapsed since onset, then mark it
  /// Held (one hit per strike until it releases). Call frequently (every few ms).
  pub fn poll(&mut self, now: Instant, out: &mut Vec<Hit>) {
    for slot in 0..NUM_PADS {
      if let PadState::Rising { onset, peak } = self.pads[slot] {
        if now.duration_since(onset) >= ATTACK {
          out.push(Hit { label: SLOT_LABEL[slot], velocity: velocity_from_peak(peak) });
          self.pads[slot] = PadState::Held;
        }
      }
    }
  }

  fn pad_sum(&self, slot: usize) -> u16 {
    let base = slot * 4;
    (0..4).map(|k| self.sensors[base + k] as u16).sum()
  }
}

impl Default for TetherDecoder {
  fn default() -> Self {
    Self::new()
  }
}

/// Extract every Control-Change `(controller, value)` pair from a raw MIDI buffer,
/// handling running status and skipping non-CC messages. (Tether data is CC on
/// channel 0; we accept any channel since the device only uses ch0.)
pub fn collect_control_changes(message: &[u8], out: &mut Vec<(u8, u8)>) {
  let mut i = 0;
  let mut in_cc = false; // last status byte was a Control Change (0xBx)
  while i < message.len() {
    let b = message[i];
    if b & 0x80 != 0 {
      let high = b & 0xF0;
      in_cc = high == 0xB0;
      match high {
        // Two data bytes (note off/on, poly pressure, CC, pitch bend).
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => {
          if i + 2 >= message.len() {
            break;
          }
          if high == 0xB0 {
            out.push((message[i + 1] & 0x7F, message[i + 2] & 0x7F));
          }
          i += 3;
        }
        // One data byte (program change, channel pressure).
        0xC0 | 0xD0 => {
          if i + 1 >= message.len() {
            break;
          }
          i += 2;
        }
        // System common / realtime: step one byte, no running status.
        _ => {
          in_cc = false;
          i += 1;
        }
      }
    } else if in_cc {
      // Running status: another (controller, value) pair under the previous 0xBx.
      if i + 1 < message.len() {
        out.push((message[i] & 0x7F, message[i + 1] & 0x7F));
        i += 2;
      } else {
        break;
      }
    } else {
      i += 1; // stray data byte with no governing status
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ccs(message: &[u8]) -> Vec<(u8, u8)> {
    let mut out = Vec::new();
    collect_control_changes(message, &mut out);
    out
  }

  #[test]
  fn collects_control_changes_with_running_status() {
    assert_eq!(ccs(&[0xB0, 44, 100]), vec![(44, 100)], "one CC");
    assert_eq!(ccs(&[0xB0, 44, 100, 60, 120]), vec![(44, 100), (60, 120)], "running status");
    assert_eq!(ccs(&[0xC0, 5, 0xB0, 44, 100]), vec![(44, 100)], "PC before CC is skipped");
    assert_eq!(ccs(&[0x90, 60, 100]), Vec::<(u8, u8)>::new(), "note-on is not a CC");
    assert_eq!(ccs(&[0xB0, 44]), Vec::<(u8, u8)>::new(), "truncated CC yields nothing");
  }

  /// Drive a pad's 4 sensors to a chosen sum, then fire it after the attack window.
  fn strike(d: &mut TetherDecoder, base: u8, sensor_vals: [u8; 4], t0: Instant) -> Vec<Hit> {
    for (k, v) in sensor_vals.iter().enumerate() {
      d.on_cc(base + k as u8, *v, t0);
    }
    let mut hits = Vec::new();
    d.poll(t0 + ATTACK, &mut hits);
    hits
  }

  #[test]
  fn single_pad_fires_one_hit_mapped_to_its_label() {
    let mut d = TetherDecoder::new();
    let t0 = Instant::now();
    // base 44 = label "1".
    let hits = strike(&mut d, 44, [127, 127, 127, 80], t0);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].label, 1, "base 44 -> printed label 1");
    assert_eq!(hits[0].velocity, 127, "sum 461 >= full scale -> max velocity");
  }

  #[test]
  fn does_not_fire_before_the_attack_window() {
    let mut d = TetherDecoder::new();
    let t0 = Instant::now();
    d.on_cc(44, 100, t0);
    let mut hits = Vec::new();
    d.poll(t0, &mut hits); // window not elapsed
    assert!(hits.is_empty());
    d.poll(t0 + ATTACK, &mut hits);
    assert_eq!(hits.len(), 1);
  }

  #[test]
  fn velocity_scales_with_peak_pressure() {
    let mut d = TetherDecoder::new();
    let t0 = Instant::now();
    // Soft-ish: sum 120 -> ~33; hard: sum 460 -> 127. Distinct, and soft < hard.
    let soft = strike(&mut d, 44, [60, 60, 0, 0], t0)[0].velocity;
    // release and re-arm before a second strike
    d.on_cc(44, 0, t0 + ATTACK * 2);
    d.on_cc(45, 0, t0 + ATTACK * 2);
    let hard = strike(&mut d, 44, [127, 127, 127, 79], t0 + ATTACK * 3)[0].velocity;
    assert!(soft < hard, "soft {soft} should be quieter than hard {hard}");
    assert_eq!(hard, 127);
  }

  #[test]
  fn two_pads_struck_together_both_fire() {
    let mut d = TetherDecoder::new();
    let t0 = Instant::now();
    d.on_cc(44, 120, t0); // label 1
    d.on_cc(60, 120, t0); // base 60 -> label 3
    let mut hits = Vec::new();
    d.poll(t0 + ATTACK, &mut hits);
    let labels: Vec<u8> = hits.iter().map(|h| h.label).collect();
    assert!(labels.contains(&1) && labels.contains(&3), "both feet fire: {labels:?}");
  }

  #[test]
  fn one_hit_per_press_refires_only_after_release() {
    let mut d = TetherDecoder::new();
    let t0 = Instant::now();
    d.on_cc(44, 100, t0);
    let mut hits = Vec::new();
    d.poll(t0 + ATTACK, &mut hits);
    assert_eq!(hits.len(), 1, "first strike fires");
    hits.clear();
    d.on_cc(44, 110, t0 + ATTACK * 2); // still held high
    d.poll(t0 + ATTACK * 3, &mut hits);
    assert!(hits.is_empty(), "held foot does not retrigger");
    d.on_cc(44, 0, t0 + ATTACK * 4); // release below off-threshold
    d.on_cc(44, 100, t0 + ATTACK * 5); // new strike
    d.poll(t0 + ATTACK * 6, &mut hits);
    assert_eq!(hits.len(), 1, "refires after a release");
  }

  #[test]
  fn nav_pad_and_pedal_ccs_are_ignored() {
    let mut d = TetherDecoder::new();
    let t0 = Instant::now();
    d.on_cc(80, 127, t0); // nav pad
    d.on_cc(86, 127, t0); // expression pedal
    let mut hits = Vec::new();
    d.poll(t0 + ATTACK, &mut hits);
    assert!(hits.is_empty(), "non-key CCs produce no drum hits");
  }

  #[test]
  fn gain_spans_the_db_range_and_is_monotonic() {
    assert!((gain_from_velocity(127) - 1.0).abs() < 1e-6, "vel 127 = unity gain");
    let soft = gain_from_velocity(1);
    let expected = 10f32.powf(-VELOCITY_DB_RANGE / 20.0);
    assert!((soft - expected).abs() < 1e-4, "vel 1 = -{VELOCITY_DB_RANGE} dB: {soft} vs {expected}");
    assert!(gain_from_velocity(96) > gain_from_velocity(32), "louder hit -> more gain");
  }
}
