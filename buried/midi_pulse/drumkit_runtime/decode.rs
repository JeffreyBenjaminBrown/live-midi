//! Pure tether-mode decoding: the SoftStep's hosted sensor stream -> per-pad hits
//! with a strike pressure. No MIDI/audio I/O, so it is unit-testable without hardware.
//!
//! In hosted/tether mode (see `learnings/keith-mcmillen-softstep.org`) the device streams each pad's 4
//! pressure sensors as Control Change on channel 0: CC number = sensor index, value
//! 0..127 = reading. A pad owns `baseCC..baseCC+3`. We sum a pad's 4 sensors and, the
//! instant the sum crosses an on-threshold, FIRE a hit (so even a one-sample tap counts).
//! For a short attack window after that we keep watching: if a higher peak arrives we emit
//! a REVISE telling the audio to ramp that pad's playing voice up to the louder pressure.
//! Each pad has its own slot, so two feet landing together yield two hits -- the
//! limitation the old Program-Change path could not escape.

use std::time::{Duration, Instant};

use midi_pulse::rig::SoftstepParams;

pub const NUM_PADS: usize = 10;

/// Printed pedal label (1..9, then 0) for each pad slot. Slot = `(baseCC-40)/4`; the
/// base CCs in slot order are 40,44,48,...,76. The label<->base mapping was measured
/// on the device (`learnings/keith-mcmillen-softstep.org`): e.g. base 44 = label "1", base 40 = label
/// "6", base 72 = label "0".
const SLOT_LABEL: [u8; NUM_PADS] = [6, 1, 7, 2, 8, 3, 9, 4, 0, 5];

// The detection & pressure/gain knobs -- onset/release thresholds, the attack WATCH window,
// the pressure full-scale, and the dB spread -- are NOT constants here. They live in the shared
// rigs/softstep.toml (`SoftstepParams`) and are handed to `TetherDecoder::new`. A hit
// fires the instant the sum crosses `on_sum` (even a one-sample tap); for `attack_ms` after
// that we keep watching, and a higher peak ramps the playing voice up to the louder pressure
// (see `audio`). So the attack window is *watch-and-adjust*, not "signal must persist this
// long". Its ~14 ms size comes from real captures: the device scans only ~every 10 ms and a
// deliberate strike peaks on the 2nd refresh (~10-11 ms). `off_sum` (release) re-arms the pad
// and, once a stuck sensor has de-sticked to 0, closes it out.

/// What `poll` emits for a pad. A hit fires at ONSET (`Fire`, so even a one-sample tap
/// counts); while the attack window is still open, a higher peak emits a `Revise` telling
/// the audio to ramp that pad's playing voice up to the louder pressure (`audio::VoicePool`).
///
/// `pressure` is a CONTINUOUS strike strength in 0.0..1.0 (peak pressure / full scale),
/// not a MIDI velocity: the audio path consumes the float directly, so there is no lossy
/// 0..127 quantization step anywhere (this decoder never speaks MIDI). (No `Eq`: f32.)
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DrumEvent {
  /// Start a voice for `label` at `pressure` (0.0..1.0). `hard` is the quantized
  /// light/hard call (see [`is_hard`]) for the peak seen SO FAR.
  Fire { label: u8, pressure: f32, hard: bool },
  /// The pad's current voice should get louder -- ramp it toward `pressure` (0.0..1.0).
  /// `hard` re-reports the light/hard call for the new, higher peak: a press that
  /// starts light and grows past the threshold arrives as `Fire{hard:false}` then
  /// `Revise{hard:true}`, exactly as the loudness does.
  Revise { label: u8, pressure: f32, hard: bool },
  /// The pad's pressure fell below the off threshold (or de-sticked to zero): the
  /// foot lifted. One-shot samples ignore this; the feet-accrete mirror needs it
  /// (holding a pedal = holding the accrete button). A debounce-suppressed press
  /// releases without ever having Fired -- consumers must tolerate an unpaired
  /// Release.
  Release { label: u8 },
}

/// Is this peak sum-of-4 a HARD strike rather than a light one? The "one pad, two
/// related purposes" trick: tap lightly for one job, stomp for the other.
///
/// Compares the RAW sum (not a pressure), so `threshold` is directly comparable to
/// `on_sum` / `pressure_full_scale` and readable off the meter's `sum` column.
///
/// This necessarily reads the ATTACK PEAK, i.e. how *fast* you stomped, not how hard
/// you are standing once down. That is right for a tap and meaningless for a pedal you
/// HOLD (accrete) -- sustained pressure would need the decoder to keep tracking past
/// the attack window, which nothing asks for yet.
pub fn is_hard(peak_sum: u16, threshold: u16) -> bool {
  peak_sum >= threshold
}

/// Linear gain multiplier for a `pressure` 0.0..1.0, spread over `db_range` dB
/// (pressure 1.0 -> unity gain, 0.0 -> -`db_range` dB). Multiply a pad's configured base
/// gain by this. `db_range` is `SoftstepParams::gain_db_range`.
pub fn gain_from_pressure(pressure: f32, db_range: f32) -> f32 {
  let t = pressure.clamp(0.0, 1.0);
  let db = (t - 1.0) * db_range; // -db_range..0
  10f32.powf(db / 20.0)
}

/// Continuous strike pressure in 0.0..1.0: peak pressure (sum-of-4) over the configured
/// full scale, clamped. No quantization -- this is the whole point of not going through a
/// 0..127 velocity.
fn pressure_from_peak(peak: u16, full_scale: u16) -> f32 {
  (peak as f32 / full_scale.max(1) as f32).clamp(0.0, 1.0)
}

/// How a pad guards against contact bounce, chosen per pad (see `set_debounce_by_label`).
///
/// The pad KIND picks the mode: momentary TIMING pedals (the tempo-factor x2/x3/div2/div3
/// pedals) want `Settle`; drum pads and HELD pedals (accrete) want `Standard`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DebounceMode {
  /// Fire on onset, watch the attack window for a higher peak (`Revise`), re-arm on
  /// release, and suppress a re-press within the decoder's `debounce` min-gap. Right for
  /// drum pads (a fast double-tap is two real hits) and for HELD pedals like accrete
  /// (every genuine press counts, and lifting must register at once).
  Standard,
  /// Aggressive settle debounce for a momentary TIMING pedal (the tempo-factor pedals).
  /// Fire once on onset, emit no `Revise`, then accept no further fire until the pad's
  /// sum has held perfectly steady for `stable_for` AND fallen back below `off_sum`.
  ///
  /// A single stomp on a bare fire-on-onset pad reads as many presses -- its sum dips and
  /// re-crosses the threshold as the foot rocks, so `x2` is heard as `x2^10`. A fixed
  /// min-gap does not fix this (a slow bounce outlasts the gap); requiring the SIGNAL to
  /// go quiet does, because the bounce keeps the sum changing and so keeps resetting the
  /// clock. One stomp then yields exactly one event, at the cost of not being able to
  /// re-fire for `stable_for` after releasing -- fine for a timing control.
  Settle { stable_for: Duration },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PadState {
  Idle,
  /// Fired at onset; still watching for a higher peak until `onset + ATTACK`. `sent_peak`
  /// is the highest peak already emitted (Fire, then any Revises); we ramp up only when a
  /// strictly higher peak arrives -- comparing raw peaks, not a quantized velocity.
  Watching { onset: Instant, peak: u16, sent_peak: u16 },
  /// `DebounceMode::Settle` hold after a fire: emit nothing until the sum goes quiet.
  /// `stable_since` is when the sum last stopped changing; `last_sum` is the previous
  /// poll's sum (a change resets `stable_since`). Re-arm to `Idle` only once the sum has
  /// been unchanged for `stable_for` AND is below `off_sum` (released) -- so holding the
  /// pad down, however steadily, never retriggers it.
  Settling { stable_since: Instant, last_sum: u16, stable_for: Duration },
  Held,
}

/// Per-pad onset / attack-peak / pressure state machine over the hosted CC stream.
/// Its thresholds come from the shared [`SoftstepParams`] (rigs/softstep.toml).
pub struct TetherDecoder {
  sensors: [u8; 40], // CC 40..79, index = cc - 40
  pads: [PadState; NUM_PADS],
  on_sum: u16,               // fire when a pad's sum-of-4 exceeds this
  off_sum: u16,              // re-arm when it falls below this
  attack: Duration,          // watch-and-adjust window after onset
  pressure_full_scale: u16,  // sum-of-4 mapping to full pressure (1.0)
  /// Sum-of-4 at or above which a strike reports `hard` (see `is_hard`). Plumbing
  /// only: nothing binds light/hard to behavior yet.
  pressure_threshold_sum: u16,
  /// Minimum gap between two hits on the SAME pad, on top of requiring a release.
  debounce: Duration,
  /// A sensor with no CC for longer than this reads 0 (de-stick); `ZERO` disables it.
  /// The tether stream is on-change, so a physically stuck sensor stops sending and its
  /// last value would otherwise linger, pinning a pad Held and blocking its next hit.
  silence: Duration,
  /// When each sensor (CC 40..79, index = cc - 40) last received a CC (`None` = never).
  last_seen: [Option<Instant>; 40],
  /// When each pad last fired, for the debounce gate (`None` = never).
  last_fire: [Option<Instant>; NUM_PADS],
  /// Per-pad bounce guard (index = slot). Every pad starts `Standard`; the runtime
  /// switches the timing pedals to `Settle` from the rig via `set_debounce_by_label`.
  debounce_mode: [DebounceMode; NUM_PADS],
}

impl TetherDecoder {
  /// Build a decoder from the shared [`SoftstepParams`] (rigs/softstep.toml).
  pub fn new(params: SoftstepParams) -> Self {
    TetherDecoder {
      sensors: [0; 40],
      pads: [PadState::Idle; NUM_PADS],
      on_sum: params.on_sum,
      off_sum: params.off_sum,
      attack: Duration::from_millis(params.attack_ms),
      pressure_full_scale: params.pressure_full_scale,
      pressure_threshold_sum: params.pressure_threshold_sum,
      debounce: Duration::from_millis(params.debounce_ms),
      silence: Duration::from_millis(params.silence_to_zero_ms),
      last_seen: [None; 40],
      last_fire: [None; NUM_PADS],
      debounce_mode: [DebounceMode::Standard; NUM_PADS],
    }
  }

  /// Set one pad's bounce guard by its printed label (1..9, then 0 -- the same labels
  /// `[[softstep_windows]]` uses in its `pedal` field). An unknown label is ignored, so
  /// a rig that names a pedal this board does not have is harmless.
  pub fn set_debounce_by_label(&mut self, label: u8, mode: DebounceMode) {
    if let Some(slot) = SLOT_LABEL.iter().position(|&l| l == label) {
      self.debounce_mode[slot] = mode;
    }
  }

  /// Feed one Control-Change sensor reading at time `now`. CCs outside the key range
  /// (nav pad 80..83, expression pedal 86, etc.) are ignored.
  pub fn on_cc(&mut self, cc: u8, val: u8, now: Instant) {
    if !(40..=79).contains(&cc) {
      return;
    }
    let idx = (cc - 40) as usize;
    self.sensors[idx] = val & 0x7F;
    self.last_seen[idx] = Some(now);
    // The onset / fire / revise / release state machine lives in `poll` (driven by the
    // interpreted sum), so a sensor going silent (de-stick) is handled with no new CC.
  }

  /// Advance every pad's state machine off its interpreted sum and emit events. A pad
  /// FIRES the instant its sum crosses `ON_SUM` (even a one-sample tap); while the attack
  /// window is still open a higher peak emits a `Revise` (ramp the voice up); after the
  /// window it holds until the sum falls below `OFF_SUM` -- which also re-arms it and
  /// de-sticks a stuck sensor. Call frequently (every ~1 ms).
  pub fn poll(&mut self, now: Instant, out: &mut Vec<DrumEvent>) {
    for slot in 0..NUM_PADS {
      let sum = self.pad_sum(slot, now);
      let label = SLOT_LABEL[slot];
      self.pads[slot] = match self.pads[slot] {
        PadState::Idle if sum > self.on_sum => {
          // Fire immediately, unless this pad fired less than `debounce` ago.
          let debounced =
            matches!(self.last_fire[slot], Some(t) if now.duration_since(t) < self.debounce);
          if debounced {
            PadState::Held // suppress the too-soon retrigger; it must release first
          } else {
            let pressure = pressure_from_peak(sum, self.pressure_full_scale);
            let hard = is_hard(sum, self.pressure_threshold_sum);
            out.push(DrumEvent::Fire { label, pressure, hard });
            self.last_fire[slot] = Some(now);
            match self.debounce_mode[slot] {
              // Timing pedal: no attack watch, no Revise -- go straight to settling and
              // stay mute until the sum goes quiet and releases (see `DebounceMode`).
              DebounceMode::Settle { stable_for } => {
                PadState::Settling { stable_since: now, last_sum: sum, stable_for }
              }
              DebounceMode::Standard => PadState::Watching { onset: now, peak: sum, sent_peak: sum },
            }
          }
        }
        PadState::Idle => PadState::Idle,
        // Settling (a fired timing pedal): emit nothing. A changing sum keeps resetting
        // the stability clock; re-arm only once it has been steady for `stable_for` and
        // has fallen below `off_sum`. So one stomp is one event, and holding never repeats.
        PadState::Settling { stable_since, last_sum, stable_for } => {
          let (stable_since, last_sum) =
            if sum != last_sum { (now, sum) } else { (stable_since, last_sum) };
          if sum < self.off_sum && now.duration_since(stable_since) >= stable_for {
            PadState::Idle
          } else {
            PadState::Settling { stable_since, last_sum, stable_for }
          }
        }
        // Released (or de-sticked to 0) before the window closed: the voice plays out.
        PadState::Watching { .. } if sum < self.off_sum => {
          out.push(DrumEvent::Release { label });
          PadState::Idle
        }
        PadState::Watching { onset, peak, sent_peak } => {
          let peak = peak.max(sum);
          let sent_peak = if peak > sent_peak {
            // A strictly higher peak arrived -> ramp the voice up to the new pressure.
            let pressure = pressure_from_peak(peak, self.pressure_full_scale);
            let hard = is_hard(peak, self.pressure_threshold_sum);
            out.push(DrumEvent::Revise { label, pressure, hard });
            peak
          } else {
            sent_peak
          };
          if now.duration_since(onset) >= self.attack {
            PadState::Held // window closed; final pressure locked in
          } else {
            PadState::Watching { onset, peak, sent_peak }
          }
        }
        PadState::Held if sum < self.off_sum => {
          out.push(DrumEvent::Release { label });
          PadState::Idle
        }
        PadState::Held => PadState::Held,
      };
    }
  }

  /// A pad's sum-of-4 in INTERPRETED units: each sensor reads its raw value, or 0 once it
  /// has been silent (no CC) longer than `silence` (de-stick; `silence == ZERO` = off).
  fn pad_sum(&self, slot: usize, now: Instant) -> u16 {
    let base = slot * 4;
    (0..4).map(|k| self.interp(base + k, now)).sum()
  }

  fn interp(&self, i: usize, now: Instant) -> u16 {
    if self.silence.is_zero() {
      return self.sensors[i] as u16;
    }
    match self.last_seen[i] {
      Some(t) if now.duration_since(t) <= self.silence => self.sensors[i] as u16,
      _ => 0, // silent too long (or never seen) -> treated as released to 0
    }
  }
}

impl Default for TetherDecoder {
  fn default() -> Self {
    Self::new(SoftstepParams::default())
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

  // Defaults match rigs/softstep.toml (on/off 20, full-scale 460); the tests build
  // decoders with a chosen debounce/silence and otherwise the defaults.
  const ATTACK: Duration = Duration::from_millis(14);

  fn decoder(debounce_ms: u64, silence_to_zero_ms: u64) -> TetherDecoder {
    TetherDecoder::new(SoftstepParams { debounce_ms, silence_to_zero_ms, ..SoftstepParams::default() })
  }

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

  fn fires(events: &[DrumEvent]) -> Vec<(u8, f32)> {
    events
      .iter()
      .filter_map(|e| match e {
        DrumEvent::Fire { label, pressure, .. } => Some((*label, *pressure)),
        _ => None,
      })
      .collect()
  }

  fn revises(events: &[DrumEvent]) -> Vec<(u8, f32)> {
    events
      .iter()
      .filter_map(|e| match e {
        DrumEvent::Revise { label, pressure, .. } => Some((*label, *pressure)),
        _ => None,
      })
      .collect()
  }

  /// Drive a pad's 4 sensors to a chosen sum, then poll -- which fires it on onset.
  fn strike(d: &mut TetherDecoder, base: u8, sensor_vals: [u8; 4], t0: Instant) -> Vec<DrumEvent> {
    for (k, v) in sensor_vals.iter().enumerate() {
      d.on_cc(base + k as u8, *v, t0);
    }
    let mut ev = Vec::new();
    d.poll(t0, &mut ev);
    ev
  }

  #[test]
  fn single_pad_fires_one_hit_mapped_to_its_label() {
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    // base 44 = label "1"; sum 461 >= full scale -> max pressure 1.0.
    assert_eq!(fires(&strike(&mut d, 44, [127, 127, 127, 80], t0)), vec![(1, 1.0)]);
  }

  #[test]
  fn fires_immediately_on_onset() {
    // No attack-window wait anymore -- a pad fires the instant its sum crosses ON_SUM.
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    d.on_cc(44, 100, t0);
    let mut ev = Vec::new();
    d.poll(t0, &mut ev);
    assert_eq!(fires(&ev), vec![(1, pressure_from_peak(100, 460))], "fires on onset, immediately");
  }

  #[test]
  fn a_one_sample_tap_still_fires() {
    // The behavior this change adds: a tap on for a single sample, released before the
    // window elapses, now COUNTS (the old model dropped it).
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    d.on_cc(44, 40, t0); // sum 40 > ON_SUM
    let mut ev = Vec::new();
    d.poll(t0, &mut ev);
    assert_eq!(fires(&ev).len(), 1, "single-sample tap fires");
    // Released well before ATTACK elapses -- it already fired, and does not double-fire.
    ev.clear();
    d.on_cc(44, 0, t0 + Duration::from_millis(3));
    d.poll(t0 + Duration::from_millis(3), &mut ev);
    assert!(fires(&ev).is_empty(), "release does not double-fire");
  }

  #[test]
  fn fires_on_onset_then_revises_up_to_a_later_peak() {
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    // A modest first reading fires immediately.
    d.on_cc(44, 60, t0);
    let mut ev = Vec::new();
    d.poll(t0, &mut ev);
    assert_eq!(fires(&ev), vec![(1, pressure_from_peak(60, 460))], "fires on the first sample");
    // Within the window the press grows -> a Revise ramps the voice up (no second Fire).
    ev.clear();
    d.on_cc(44, 127, t0 + Duration::from_millis(10)); // sum 254
    d.on_cc(45, 127, t0 + Duration::from_millis(10));
    d.poll(t0 + Duration::from_millis(10), &mut ev);
    assert!(fires(&ev).is_empty(), "no second Fire");
    assert_eq!(revises(&ev), vec![(1, pressure_from_peak(254, 460))], "Revise up to the higher peak");
  }

  #[test]
  fn pressure_scales_with_peak_pressure() {
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    // Soft-ish sum 120 vs hard sum 460, each fired on onset. Distinct, soft < hard.
    let soft = fires(&strike(&mut d, 44, [60, 60, 0, 0], t0))[0].1;
    // Release (sum 0) and let a poll re-arm before the second strike.
    d.on_cc(44, 0, t0 + ATTACK);
    d.on_cc(45, 0, t0 + ATTACK);
    d.poll(t0 + ATTACK, &mut Vec::new());
    let hard = fires(&strike(&mut d, 44, [127, 127, 127, 79], t0 + ATTACK * 2))[0].1;
    assert!(soft < hard, "soft {soft} should be quieter than hard {hard}");
    assert_eq!(hard, 1.0, "sum 460 = full scale -> pressure 1.0");
  }

  #[test]
  fn two_pads_struck_together_both_fire() {
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    d.on_cc(44, 120, t0); // label 1
    d.on_cc(60, 120, t0); // base 60 -> label 3
    let mut ev = Vec::new();
    d.poll(t0, &mut ev);
    let labels: Vec<u8> = fires(&ev).iter().map(|(l, _)| *l).collect();
    assert!(labels.contains(&1) && labels.contains(&3), "both feet fire: {labels:?}");
  }

  #[test]
  fn one_hit_per_press_refires_only_after_release() {
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    let mut ev = Vec::new();
    d.on_cc(44, 100, t0);
    d.poll(t0, &mut ev);
    assert_eq!(fires(&ev).len(), 1, "first strike fires");
    // Held high across the window and beyond: at most a Revise, never a second Fire.
    ev.clear();
    d.on_cc(44, 110, t0 + ATTACK / 2);
    d.poll(t0 + ATTACK, &mut ev);
    d.poll(t0 + ATTACK * 2, &mut ev);
    assert!(fires(&ev).is_empty(), "held foot does not retrigger");
    // Release (a poll catches the sub-OFF_SUM sum), then a new strike fires.
    d.on_cc(44, 0, t0 + ATTACK * 3);
    d.poll(t0 + ATTACK * 3, &mut ev);
    ev.clear();
    d.on_cc(44, 100, t0 + ATTACK * 4);
    d.poll(t0 + ATTACK * 4, &mut ev);
    assert_eq!(fires(&ev).len(), 1, "refires after a release");
  }

  #[test]
  fn debounce_suppresses_a_too_soon_retrigger_on_the_same_pad() {
    let mut d = decoder(50, 0); // 50 ms minimum gap between hits on a pad
    let t0 = Instant::now();
    let mut ev = Vec::new();
    d.on_cc(44, 100, t0);
    d.poll(t0, &mut ev);
    assert_eq!(fires(&ev).len(), 1, "first hit fires");
    // Release, then re-press ~10 ms after the first hit (inside the debounce window).
    d.on_cc(44, 0, t0 + Duration::from_millis(2));
    d.poll(t0 + Duration::from_millis(2), &mut ev);
    let onset2 = t0 + Duration::from_millis(10);
    d.on_cc(44, 100, onset2);
    ev.clear();
    d.poll(onset2, &mut ev);
    assert!(fires(&ev).is_empty(), "a re-press within debounce is suppressed");
    // Release, then re-press well past the debounce window from the first hit.
    d.on_cc(44, 0, onset2 + Duration::from_millis(2));
    d.poll(onset2 + Duration::from_millis(2), &mut ev);
    let onset3 = t0 + Duration::from_millis(120);
    d.on_cc(44, 100, onset3);
    ev.clear();
    d.poll(onset3, &mut ev);
    assert_eq!(fires(&ev).len(), 1, "a re-press after debounce fires");
    // A different pedal is on its own debounce clock -- unaffected.
    d.on_cc(60, 100, onset3); // label 3
    ev.clear();
    d.poll(onset3, &mut ev);
    assert_eq!(fires(&ev).len(), 1, "debounce is per-pad");
  }

  // ---- Settle debounce for timing pedals (DebounceMode::Settle) ----

  const SETTLE: Duration = Duration::from_millis(150);

  /// Drive one sensor of a pad to `val` at `t`, then poll, returning the events.
  fn feed(d: &mut TetherDecoder, cc: u8, val: u8, t: Instant) -> Vec<DrumEvent> {
    d.on_cc(cc, val, t);
    let mut ev = Vec::new();
    d.poll(t, &mut ev);
    ev
  }

  #[test]
  fn a_settle_pad_fires_once_through_a_bouncing_press() {
    // The x2 -> x(2^10) bug: one physical stomp whose sum rocks up and down. A Standard
    // pad would re-fire on every up-crossing; a Settle pad fires exactly once.
    let mut d = decoder(0, 0);
    d.set_debounce_by_label(1, DebounceMode::Settle { stable_for: SETTLE }); // base 44 = label 1
    let t0 = Instant::now();
    let mut fires_seen = 0;
    // A jittery ~120 ms press: sum crosses on/off_sum (20) up and down many times, each
    // reading 2 ms apart -- never quiet for 150 ms, so it stays one event.
    for (i, &v) in [80u8, 5, 90, 3, 100, 0, 70, 4, 88, 2, 95, 6, 77].iter().enumerate() {
      let ev = feed(&mut d, 44, v, t0 + Duration::from_millis(i as u64 * 2));
      fires_seen += fires(&ev).len();
    }
    assert_eq!(fires_seen, 1, "a bouncing single press fires exactly once under Settle");
  }

  #[test]
  fn a_settle_pad_does_not_retrigger_while_held_steady() {
    let mut d = decoder(0, 0);
    d.set_debounce_by_label(1, DebounceMode::Settle { stable_for: SETTLE });
    let t0 = Instant::now();
    assert_eq!(fires(&feed(&mut d, 44, 100, t0)).len(), 1, "fires on onset");
    // Held perfectly steady well past the settle window: still no second fire (the sum is
    // quiet, but it never fell below off_sum, so the pad stays disarmed).
    let mut later = Vec::new();
    for ms in [200u64, 400, 800] {
      let mut ev = Vec::new();
      d.poll(t0 + Duration::from_millis(ms), &mut ev);
      later.extend(ev);
    }
    assert!(fires(&later).is_empty(), "a held Settle pad never repeats: {later:?}");
    assert!(revises(&later).is_empty(), "Settle emits no Revise: {later:?}");
  }

  #[test]
  fn a_settle_pad_re_arms_only_after_it_goes_quiet_and_releases() {
    let mut d = decoder(0, 0);
    d.set_debounce_by_label(1, DebounceMode::Settle { stable_for: SETTLE });
    let t0 = Instant::now();
    assert_eq!(fires(&feed(&mut d, 44, 100, t0)).len(), 1, "first stomp fires");
    // Release, then stomp again just 50 ms later -- still inside the settle window, so it
    // is swallowed (aggressive debounce, as asked for the timing pedals).
    d.on_cc(44, 0, t0 + Duration::from_millis(10));
    d.poll(t0 + Duration::from_millis(10), &mut Vec::new());
    assert!(
      fires(&feed(&mut d, 44, 100, t0 + Duration::from_millis(50))).is_empty(),
      "a stomp within the settle window is suppressed",
    );
    // Let it go quiet and stay released. The first poll after the release observes the
    // drop to zero (resetting the clock); a later poll finds it unchanged past the window
    // (real polling runs every ~1 ms, so quiet genuinely accumulates). Then a fresh stomp fires.
    d.on_cc(44, 0, t0 + Duration::from_millis(60));
    d.poll(t0 + Duration::from_millis(60), &mut Vec::new()); // observe the release
    d.poll(t0 + Duration::from_millis(250), &mut Vec::new()); // still 0, quiet past 150 ms -> re-arm
    assert_eq!(
      fires(&feed(&mut d, 44, 100, t0 + Duration::from_millis(300))).len(),
      1,
      "after quiet + release past the window, the next stomp fires",
    );
  }

  #[test]
  fn settle_is_per_pad_and_leaves_standard_pads_alone() {
    // Only label 1 is Settle; label 3 stays Standard and behaves as before.
    let mut d = decoder(0, 0);
    d.set_debounce_by_label(1, DebounceMode::Settle { stable_for: SETTLE });
    let t0 = Instant::now();
    // Standard pad (label 3, base 60): fires, then a higher peak Revises within the window.
    assert_eq!(fires(&feed(&mut d, 60, 60, t0)).len(), 1, "standard pad fires");
    d.on_cc(60, 127, t0 + Duration::from_millis(5));
    d.on_cc(61, 127, t0 + Duration::from_millis(5));
    let mut ev = Vec::new();
    d.poll(t0 + Duration::from_millis(5), &mut ev);
    assert!(!revises(&ev).is_empty(), "a standard pad still Revises to a higher peak");
  }

  #[test]
  fn nav_pad_and_pedal_ccs_are_ignored() {
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    d.on_cc(80, 127, t0); // nav pad
    d.on_cc(86, 127, t0); // expression pedal
    let mut ev = Vec::new();
    d.poll(t0, &mut ev);
    assert!(fires(&ev).is_empty(), "non-key CCs produce no drum hits");
  }

  #[test]
  fn a_release_event_follows_the_foot_lifting() {
    // Fire on press; a Release fires once the pad's sum falls below the off
    // threshold -- whether that happens during the attack window or after it. The
    // feet-accrete mirror relies on this down/up pairing (sensor base 44 = label 1).
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    let mut ev = Vec::new();
    d.on_cc(44, 100, t0);
    d.poll(t0, &mut ev);
    assert!(ev.iter().any(|e| matches!(e, DrumEvent::Fire { label: 1, .. })), "fires on press");
    // Still held past the attack window: no Release yet.
    ev.clear();
    d.on_cc(44, 100, t0 + ATTACK * 2);
    d.poll(t0 + ATTACK * 2, &mut ev);
    assert!(!ev.iter().any(|e| matches!(e, DrumEvent::Release { .. })), "held: no Release");
    // Lift the foot (sum to zero): a Release for the same label.
    ev.clear();
    d.on_cc(44, 0, t0 + ATTACK * 3);
    d.poll(t0 + ATTACK * 3, &mut ev);
    assert!(
      ev.iter().any(|e| matches!(e, DrumEvent::Release { label: 1 })),
      "the lift emits a Release: {ev:?}",
    );
    // A quick tap released DURING the window also pairs Fire with Release.
    ev.clear();
    d.on_cc(44, 100, t0 + ATTACK * 5);
    d.poll(t0 + ATTACK * 5, &mut ev);
    d.on_cc(44, 0, t0 + ATTACK * 5 + Duration::from_millis(2));
    d.poll(t0 + ATTACK * 5 + Duration::from_millis(2), &mut ev);
    assert!(ev.iter().any(|e| matches!(e, DrumEvent::Fire { label: 1, .. })));
    assert!(
      ev.iter().any(|e| matches!(e, DrumEvent::Release { label: 1 })),
      "an in-window lift also emits a Release: {ev:?}",
    );
  }

  #[test]
  fn a_stuck_sensor_re_arms_after_silence() {
    // silence-to-zero = 20 ms: a sensor with no CC for 20 ms reads 0.
    let mut d = decoder(0, 20);
    let t0 = Instant::now();
    assert_eq!(fires(&strike(&mut d, 44, [127, 127, 127, 79], t0)).len(), 1, "the strike fires");
    // The foot "sticks": no further CCs. After the silence window a poll de-sticks it.
    let mut ev = Vec::new();
    d.poll(t0 + Duration::from_millis(50), &mut ev);
    assert!(fires(&ev).is_empty(), "the silence pass emits no spurious hit");
    // A fresh strike now fires again -- the pad re-armed with no explicit release CC.
    let again = strike(&mut d, 44, [127, 127, 127, 79], t0 + Duration::from_millis(60));
    assert_eq!(fires(&again).len(), 1, "re-armed by silence -> the next strike is heard");
  }

  #[test]
  fn silence_disabled_keeps_a_stuck_sensor_held() {
    // silence-to-zero = 0 disables de-stick: a stuck sensor pins the pad Held (old behavior).
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    assert_eq!(fires(&strike(&mut d, 44, [127, 127, 127, 79], t0)).len(), 1);
    let mut ev = Vec::new();
    d.poll(t0 + Duration::from_secs(5), &mut ev); // long silence, but de-stick is off
    assert!(fires(&ev).is_empty());
    // Still Held (never released below OFF_SUM), so a same-pad strike does NOT re-fire.
    let again = strike(&mut d, 44, [127, 127, 127, 79], t0 + Duration::from_secs(6));
    assert!(fires(&again).is_empty(), "without de-stick, a stuck pad stays Held, cannot re-fire");
  }

  #[test]
  fn gain_spans_the_db_range_and_is_monotonic() {
    let db = 20.0f32; // = default gain_db_range
    assert!((gain_from_pressure(1.0, db) - 1.0).abs() < 1e-6, "pressure 1.0 = unity gain");
    let soft = gain_from_pressure(0.0, db);
    let expected = 10f32.powf(-db / 20.0);
    assert!((soft - expected).abs() < 1e-4, "pressure 0 = -{db} dB: {soft} vs {expected}");
    assert!(gain_from_pressure(0.75, db) > gain_from_pressure(0.25, db), "louder hit -> more gain");
  }

  // ---- light/hard quantization (plumbing; nothing binds it yet) ----

  #[test]
  fn is_hard_is_a_threshold_on_the_raw_sum() {
    assert!(!is_hard(199, 200), "below the threshold is light");
    assert!(is_hard(200, 200), "at the threshold is hard (>=)");
    assert!(is_hard(201, 200));
  }

  /// A hard stomp measures ~430-460 of 508 (the only figure ever recorded), so the
  /// default 200 must call that hard, and a gentle press light.
  #[test]
  fn the_default_threshold_separates_a_recorded_hard_hit_from_a_gentle_one() {
    let t = SoftstepParams::default().pressure_threshold_sum;
    assert!(is_hard(440, t), "a recorded hard hit (~430-460) must read hard");
    assert!(!is_hard(60, t), "a gentle press must read light");
  }

  /// The classification has to ride the PEAK, not the onset sum. The device scans
  /// every ~10 ms and a real strike peaks on the 2nd refresh, so at onset the peak
  /// has not arrived: a press that ends up hard necessarily starts light and is
  /// corrected by a Revise, exactly as the loudness is.
  #[test]
  fn a_hard_stomp_arrives_as_light_then_revises_to_hard() {
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    // Onset: the sum crosses on_sum (20) but is nowhere near the threshold (200).
    d.on_cc(44, 30, t0);
    let mut ev = vec![];
    d.poll(t0, &mut ev);
    assert!(
      matches!(ev.as_slice(), [DrumEvent::Fire { label: 1, hard: false, .. }]),
      "onset fires light: {ev:?}",
    );

    // The real peak lands on the next refresh, inside the attack window.
    ev.clear();
    d.on_cc(44, 120, t0 + Duration::from_millis(10));
    d.on_cc(45, 120, t0 + Duration::from_millis(10));
    d.on_cc(46, 120, t0 + Duration::from_millis(10));
    d.poll(t0 + Duration::from_millis(10), &mut ev);
    assert!(
      ev.iter().any(|e| matches!(e, DrumEvent::Revise { label: 1, hard: true, .. })),
      "the peak crosses the threshold and revises to hard: {ev:?}",
    );
  }

  #[test]
  fn a_light_tap_never_reports_hard() {
    let mut d = decoder(0, 0);
    let t0 = Instant::now();
    d.on_cc(44, 30, t0);
    let mut ev = vec![];
    d.poll(t0, &mut ev);
    d.on_cc(44, 45, t0 + Duration::from_millis(10));
    d.poll(t0 + Duration::from_millis(10), &mut ev);
    assert!(
      !ev.iter().any(|e| matches!(
        e,
        DrumEvent::Fire { hard: true, .. } | DrumEvent::Revise { hard: true, .. }
      )),
      "a light tap stays light throughout: {ev:?}",
    );
  }
}
