//! Pure KMSS-press decoding: Program Change -> pedal, plus per-pedal debounce.
//!
//! Kept free of MIDI/audio I/O so it is unit-testable without hardware. The KMSS
//! sends only Program Changes today (`PC = preset*10 + pedal`), so the pedal label
//! is `program % 10` -- independent of which on-device preset is selected.

use std::time::{Duration, Instant};

/// The printed pedal label (0..9) carried by a Program Change `program` value.
/// `preset = program / 10` is intentionally discarded so the kit works on any
/// on-device preset (see hey-jeff.org Q2).
pub fn pedal_from_program(program: u8) -> u8 {
  program % 10
}

/// Extract every Program Change *program* value carried by a raw MIDI buffer,
/// appending each to `out`. Robust to a buffer holding more than one message and
/// to Program-Change running status (`C0 05 07` = programs 5 then 7).
///
/// In practice the KMSS sends one 2-byte PC per ALSA event, so this almost always
/// yields a single value -- but parsing the whole buffer means two near-coincident
/// presses are never dropped even if a backend ever coalesces them into one call.
pub fn collect_program_changes(message: &[u8], out: &mut Vec<u8>) {
  let mut i = 0;
  let mut in_program_change = false; // last status byte was a Program Change (0xCx)
  while i < message.len() {
    let b = message[i];
    if b & 0x80 != 0 {
      // A status byte. Classify it and skip its data bytes.
      let high = b & 0xF0;
      in_program_change = high == 0xC0;
      match high {
        // Program change / channel pressure: one data byte.
        0xC0 | 0xD0 => {
          if i + 1 >= message.len() {
            break;
          }
          if high == 0xC0 {
            out.push(message[i + 1] & 0x7F);
          }
          i += 2;
        }
        // Note off/on, poly pressure, CC, pitch bend: two data bytes.
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => i += 3,
        // System common / realtime (0xF*): don't try to be clever, just step.
        _ => {
          in_program_change = false;
          i += 1;
        }
      }
    } else if in_program_change {
      // Running status: another bare program number under the previous 0xCx.
      out.push(b & 0x7F);
      i += 1;
    } else {
      // A stray data byte with no governing status; skip it.
      i += 1;
    }
  }
}

/// Per-pedal contact-bounce debounce. A hard stomp can emit the *same* Program
/// Change twice within a few ms; we suppress a repeat of the same pedal within the
/// window. The window is **per pedal**, so two different pedals struck together
/// (e.g. two feet within 1 ms) both fire -- faithful to reality (hey-jeff.org Q4).
pub struct Debouncer {
  window: Duration,
  last: [Option<Instant>; 10],
}

impl Debouncer {
  pub fn new(window_ms: u64) -> Self {
    Debouncer { window: Duration::from_millis(window_ms), last: [None; 10] }
  }

  /// Whether to fire this pedal now. Records the time when it fires. A zero window
  /// fires every press (no debounce).
  pub fn accept(&mut self, pedal: u8, now: Instant) -> bool {
    let i = (pedal % 10) as usize;
    let fire = match self.last[i] {
      Some(prev) if now.duration_since(prev) < self.window => false,
      _ => true,
    };
    if fire {
      self.last[i] = Some(now);
    }
    fire
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pedal_is_program_mod_ten_preset_independent() {
    // preset 0..12, pedal label 0..9: PC = preset*10 + pedal.
    assert_eq!(pedal_from_program(0), 0);
    assert_eq!(pedal_from_program(5), 5);
    assert_eq!(pedal_from_program(31), 1, "preset 3, pedal 1");
    assert_eq!(pedal_from_program(38), 8, "preset 3, pedal 8");
    assert_eq!(pedal_from_program(120), 0, "preset 12, pedal 0");
  }

  fn pcs(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    collect_program_changes(message, &mut out);
    out
  }

  #[test]
  fn collects_program_change_messages() {
    assert_eq!(pcs(&[0xC0, 5]), vec![5], "one PC");
    assert_eq!(pcs(&[0xC9, 38]), vec![38], "any channel");
    assert_eq!(pcs(&[0x90, 60, 100]), Vec::<u8>::new(), "note-on is not a PC");
    assert_eq!(pcs(&[0xC0]), Vec::<u8>::new(), "truncated PC yields nothing");
  }

  #[test]
  fn collects_multiple_pcs_from_one_buffer() {
    // Two coalesced PCs (the near-simultaneous-presses case) -- both survive.
    assert_eq!(pcs(&[0xC0, 5, 0xC0, 7]), vec![5, 7], "two concatenated PCs");
    assert_eq!(pcs(&[0xC0, 5, 7]), vec![5, 7], "PC running status");
    assert_eq!(pcs(&[0xB0, 7, 127, 0xC0, 3]), vec![3], "a CC before a PC is skipped");
  }

  #[test]
  fn debounce_suppresses_same_pedal_repeat_within_window() {
    let mut d = Debouncer::new(50);
    let t0 = Instant::now();
    assert!(d.accept(1, t0), "first hit fires");
    assert!(!d.accept(1, t0 + Duration::from_millis(10)), "bounce within 50ms suppressed");
    assert!(!d.accept(1, t0 + Duration::from_millis(49)), "still within window");
    assert!(d.accept(1, t0 + Duration::from_millis(60)), "after window fires again");
  }

  #[test]
  fn debounce_is_per_pedal_two_feet_both_fire() {
    let mut d = Debouncer::new(50);
    let t0 = Instant::now();
    assert!(d.accept(1, t0), "left foot");
    // A different pedal struck ~1ms later must still fire -- two feet at once.
    assert!(d.accept(4, t0 + Duration::from_millis(1)), "right foot, independent");
  }

  #[test]
  fn zero_window_never_debounces() {
    let mut d = Debouncer::new(0);
    let t0 = Instant::now();
    assert!(d.accept(2, t0));
    assert!(d.accept(2, t0), "same instant, zero window: still fires");
  }
}
