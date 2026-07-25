//! Which output a keyboard's events go to, and the 'l' identification gesture.
//!
//! The runtime opens two input/output pairs. Output 0 is "58-edo 1" -- the LEFT
//! keyboard's Reaper input, and the sustain pedal's target; output 1 is "58-edo 2".
//! Nothing physical says which keyboard is on the left, so by default input i
//! routes to output i, and the 'l' command flips one shared SWAPPED bit: the
//! player types 'l' Enter, plays a note on the left keyboard, and whichever input
//! port that note arrived on becomes the one routed to output 0.
//!
//! Each input owns one [`DestRouter`]: note-ons take the CURRENT route and are
//! remembered, and note-offs follow the note-on that opened them -- so a swap while
//! notes are held can never strand a note-on on one output with its note-off sent
//! to the other (a stuck note in the receiving synth).

use std::collections::HashMap;

use edo_surface::midi;

pub(crate) const NUM_KEYBOARDS: usize = 2;

/// The output for input `source` (0 or 1): itself, or the other one when swapped.
pub(crate) fn dest_of_source(source: usize, swapped: bool) -> usize {
  if swapped {
    NUM_KEYBOARDS - 1 - source
  } else {
    source
  }
}

/// The identifying note came from input `source`; the left keyboard must drive
/// output 0, so the pair is swapped exactly when that note arrived on input 1.
pub(crate) fn swapped_after_identify(source: usize) -> bool {
  source != 0
}

/// Per-input memory of where each held note's note-on went.
pub(crate) struct DestRouter {
  dest_by_note: HashMap<u8, usize>,
}

impl DestRouter {
  pub(crate) fn new() -> Self {
    DestRouter {
      dest_by_note: HashMap::new(),
    }
  }

  /// The output this message should go to, given the input's current route.
  /// Note-ons take (and remember) the current route; note-offs follow their
  /// note-on, falling back to the current route when unpaired; everything else
  /// (CCs, pitch bend, ...) takes the current route.
  pub(crate) fn dest_for(&mut self, message: &[u8], current: usize) -> usize {
    if message.len() < 3 || !midi::is_note_event(message) {
      return current;
    }
    let note = message[1];
    if midi::is_note_on(message) {
      self.dest_by_note.insert(note, current);
      current
    } else {
      self.dest_by_note.remove(&note).unwrap_or(current)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const ON: u8 = 0x90;
  const OFF: u8 = 0x80;

  #[test]
  fn unswapped_routes_each_input_to_its_own_output() {
    assert_eq!(dest_of_source(0, false), 0);
    assert_eq!(dest_of_source(1, false), 1);
  }

  #[test]
  fn swapped_crosses_the_pair() {
    assert_eq!(dest_of_source(0, true), 1);
    assert_eq!(dest_of_source(1, true), 0);
  }

  #[test]
  fn identifying_from_input_0_leaves_the_pair_unswapped() {
    assert!(!swapped_after_identify(0));
    assert!(swapped_after_identify(1));
  }

  #[test]
  fn a_note_off_follows_its_note_on_across_a_swap() {
    let mut router = DestRouter::new();
    assert_eq!(router.dest_for(&[ON, 60, 100], 0), 0);
    // The swap lands while the note is held; its off must still go to output 0.
    assert_eq!(router.dest_for(&[OFF, 60, 0], 1), 0);
    // The note released, so the next press takes the new route.
    assert_eq!(router.dest_for(&[ON, 60, 100], 1), 1);
    assert_eq!(router.dest_for(&[OFF, 60, 0], 1), 1);
  }

  #[test]
  fn a_note_on_with_zero_velocity_is_an_off_and_follows_its_note_on() {
    let mut router = DestRouter::new();
    assert_eq!(router.dest_for(&[ON, 60, 100], 1), 1);
    assert_eq!(router.dest_for(&[ON, 60, 0], 0), 1);
  }

  #[test]
  fn an_unpaired_note_off_takes_the_current_route() {
    let mut router = DestRouter::new();
    assert_eq!(router.dest_for(&[OFF, 60, 0], 1), 1);
  }

  #[test]
  fn non_note_messages_take_the_current_route_and_are_not_remembered() {
    let mut router = DestRouter::new();
    assert_eq!(router.dest_for(&[0xB0, 64, 127], 1), 1);
    assert_eq!(router.dest_for(&[OFF, 64, 0], 0), 0);
  }
}
