//! The sustain ("accrete") state machine for the surfaces runtime -- pure logic, no
//! I/O, unit-tested without a device. See TODO/misc.org "sustain (accrete)".
//!
//! One `AccreteState` is one *bank*, and each monome gets its own (misc.org "two
//! monome-specific accrete banks"): a grid's button trio drives only its own bank,
//! and only notes fingered on that grid can join its sustained set. The runtime
//! holds the banks in one `Vec` under one lock; the same pitch sustained from both
//! grids is one entry in each bank (and two voices), matching the surfaces
//! "independent voices" rule.
//!
//! Three buttons (per grid):
//! - *clear* silences and flushes this bank's sustained set (key-down; lit while held);
//! - *needs_holding* toggles how *accrete* behaves (key-down; lit while on);
//! - *accrete*: with needs_holding, notes fingered while it is HELD join the sustained
//!   set; without, key-down toggles an accrete *mode* during which every note played
//!   joins. Either way, entering the accreting condition also captures the notes
//!   already held at that moment (on this bank's grid), and a note in the set keeps
//!   ringing after its finger lifts, until *clear*.
//!
//! Nothing ever leaves the set except through `press_clear` (un-toggling accrete mode
//! stops *additions* only).
//!
//! The runtime asks `note_released_sustains` at each note-off: a note sustains if it
//! is already in the set OR the accreting condition holds right then. The latter makes
//! needs-holding mode read as the *continuous* condition Jeff described ("any notes
//! fingered ... as long as they are now held") rather than an edge-triggered one --
//! e.g. after a clear, notes still fingered under a still-held accrete re-join the set
//! when they are next examined.

use std::collections::HashSet;

pub struct AccreteState {
  /// Whether *accrete* must be held (vs toggling a mode). Starts false: accrete is a
  /// toggle until the needs-holding button says otherwise.
  needs_holding: bool,
  /// The toggle-mode "accrete mode" flag (only consulted when !needs_holding).
  mode_on: bool,
  /// How many accrete buttons are physically held right now (the on-grid button and
  /// its pedal mirror can overlap). Tracked unconditionally so a needs-holding flip
  /// mid-hold stays consistent.
  hold_count: usize,
  /// How many clear buttons are physically held (drives that button's LED only).
  clear_count: usize,
  /// The sustained set: absolute pitches (this bank's grid only).
  sustained: HashSet<i32>,
}

impl AccreteState {
  pub fn new() -> Self {
    AccreteState {
      needs_holding: false,
      mode_on: false,
      hold_count: 0,
      clear_count: 0,
      sustained: HashSet::new(),
    }
  }

  /// Is the accreting condition live right now?
  pub fn accreting(&self) -> bool {
    if self.needs_holding {
      self.hold_count > 0
    } else {
      self.mode_on
    }
  }

  /// Key-down on *clear*: flush the set. The runtime silences the sustained voices
  /// (they are its to ramp out); notes still fingered keep sounding as ordinary
  /// held notes.
  pub fn press_clear(&mut self) {
    self.clear_count += 1;
    self.sustained.clear();
  }

  pub fn release_clear(&mut self) {
    self.clear_count = self.clear_count.saturating_sub(1);
  }

  /// Key-down on *needs_holding*: flip the accrete button's behavior. Entering
  /// needs-holding cancels any toggled accrete mode (the set is untouched). Returns
  /// true if this press *started* the accreting condition (an already-held accrete
  /// button suddenly counts), in which case the caller must capture held notes.
  pub fn press_needs_holding(&mut self) -> bool {
    let before = self.accreting();
    self.needs_holding = !self.needs_holding;
    if self.needs_holding {
      self.mode_on = false;
    }
    self.accreting() && !before
  }

  /// Key-down on *accrete*. Returns true if this press started the accreting
  /// condition (caller captures the notes currently held on this bank's grid).
  pub fn press_accrete(&mut self) -> bool {
    let before = self.accreting();
    self.hold_count += 1;
    if !self.needs_holding {
      self.mode_on = !self.mode_on;
    }
    self.accreting() && !before
  }

  /// Key-up on *accrete*: only meaningful in needs-holding mode (does nothing to a
  /// toggled accrete mode), but always tracked.
  pub fn release_accrete(&mut self) {
    self.hold_count = self.hold_count.saturating_sub(1);
  }

  /// A note-on happened on this bank's grid. If accreting, it joins the sustained
  /// set immediately.
  pub fn note_played(&mut self, pitch: i32) {
    if self.accreting() {
      self.sustained.insert(pitch);
    }
  }

  /// Bulk-capture the notes currently held on this bank's grid -- called when the
  /// accreting condition turns on.
  pub fn capture_held<I: IntoIterator<Item = i32>>(&mut self, held: I) {
    self.sustained.extend(held);
  }

  /// A note-off is happening on this bank's grid: should this note keep ringing (be
  /// transferred to a sustain voice) instead of releasing? True if it is already in
  /// the set, or the accreting condition holds at release time (joining the set on
  /// the spot).
  pub fn note_released_sustains(&mut self, pitch: i32) -> bool {
    if self.accreting() {
      self.sustained.insert(pitch);
    }
    self.sustained.contains(&pitch)
  }

  /// The sustained pitch *classes* (for the shared bright-LED reflection; the
  /// runtime paints the union of every bank's classes -- they are all sounding).
  pub fn sustained_classes(&self, edo: i32) -> HashSet<i32> {
    self.sustained.iter().map(|p| p.rem_euclid(edo)).collect()
  }

  // --- LED states ---

  pub fn clear_lit(&self) -> bool {
    self.clear_count > 0
  }

  pub fn needs_holding_lit(&self) -> bool {
    self.needs_holding
  }

  pub fn accrete_lit(&self) -> bool {
    self.accreting()
  }

  #[cfg(test)]
  fn sustained_len(&self) -> usize {
    self.sustained.len()
  }
}

impl Default for AccreteState {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn starts_as_an_inert_toggle() {
    let s = AccreteState::new();
    assert!(!s.needs_holding_lit(), "needs-holding starts off (accrete is a toggle)");
    assert!(!s.accrete_lit(), "not accreting at startup");
    assert!(!s.clear_lit());
    assert_eq!(s.sustained_classes(58).len(), 0);
  }

  #[test]
  fn toggle_mode_accretes_notes_played_while_on() {
    let mut s = AccreteState::new();
    assert!(s.press_accrete(), "toggle-mode entry is an activation (capture held notes)");
    assert!(s.accrete_lit(), "key-down toggled accrete mode on");
    s.release_accrete();
    assert!(s.accrete_lit(), "key-up does nothing in toggle mode");
    s.note_played(30);
    s.note_played(42);
    assert!(s.note_released_sustains(30), "a note played during the mode sustains");
    assert!(s.note_released_sustains(42));
    // Un-toggle: new notes stop joining, but nothing is deleted.
    s.press_accrete();
    s.release_accrete();
    assert!(!s.accrete_lit());
    s.note_played(50);
    assert!(!s.note_released_sustains(50), "a note played after the mode does not sustain");
    assert!(s.note_released_sustains(30), "the earlier note still sustains");
  }

  #[test]
  fn press_accrete_reports_activation_edges_only() {
    // Activation (the "capture currently-held notes" trigger) fires exactly when the
    // accreting condition flips off -> on: entering toggle mode, or the FIRST accrete
    // press in needs-holding mode -- not on repeats or de-activations.
    let mut s = AccreteState::new();
    assert!(s.press_accrete(), "toggle mode entered: activation");
    s.release_accrete();
    assert!(!s.press_accrete(), "toggle mode exited: no activation");
    s.release_accrete();

    s.press_needs_holding();
    assert!(s.press_accrete(), "first hold: activation");
    assert!(!s.press_accrete(), "second simultaneous hold (pedal mirror): already on");
    s.release_accrete();
    assert!(s.accrete_lit(), "still held once");
    s.release_accrete();
    assert!(!s.accrete_lit());
    assert!(s.press_accrete(), "a fresh hold re-activates");
  }

  #[test]
  fn needs_holding_mode_accretes_only_while_held() {
    let mut s = AccreteState::new();
    assert!(!s.press_needs_holding(), "flipping the mode with accrete unheld activates nothing");
    assert!(s.needs_holding_lit());
    assert!(s.press_accrete(), "accrete down starts the condition -> capture held notes");
    assert!(s.accrete_lit());
    s.note_played(30);
    s.release_accrete();
    assert!(!s.accrete_lit(), "key-up ends the condition in needs-holding mode");
    assert!(s.note_released_sustains(30), "captured while held -> still sustains");
    s.note_played(44);
    assert!(!s.note_released_sustains(44), "played after the hold -> does not sustain");
  }

  #[test]
  fn a_note_released_while_accrete_is_held_sustains_even_if_missed_at_note_on() {
    // The continuous reading: in needs-holding mode, "fingered while accrete is down"
    // is judged at release time too (e.g. a note that was held before a clear).
    let mut s = AccreteState::new();
    s.press_needs_holding();
    s.press_accrete();
    assert!(s.note_released_sustains(25), "released under a held accrete -> joins the set");
  }

  #[test]
  fn clear_flushes_but_does_not_stop_accretion() {
    let mut s = AccreteState::new();
    s.press_accrete(); // toggle mode on
    s.note_played(30);
    assert_eq!(s.sustained_len(), 1);
    s.press_clear();
    assert!(s.clear_lit(), "clear lights while pressed");
    assert_eq!(s.sustained_len(), 0, "the set is flushed");
    s.release_clear();
    assert!(!s.clear_lit(), "clear goes dark on key-up");
    assert!(s.accrete_lit(), "clear does not exit accrete mode");
    s.note_played(31);
    assert!(s.note_released_sustains(31), "accretion continues after a clear");
  }

  #[test]
  fn entering_needs_holding_cancels_a_toggled_mode() {
    let mut s = AccreteState::new();
    s.press_accrete(); // tap: toggle mode on...
    s.release_accrete(); // ...and lift the finger
    assert!(s.accrete_lit());
    assert!(!s.press_needs_holding(), "no activation: the condition just turned OFF");
    assert!(!s.accrete_lit(), "needs-holding cancels the toggled mode");
    assert!(s.needs_holding_lit());
  }

  #[test]
  fn flipping_needs_holding_while_accrete_is_held_activates_the_hold() {
    let mut s = AccreteState::new();
    // Hold accrete in toggle mode (mode toggles on), then enter needs-holding: the
    // physically-held button now satisfies the hold condition -- an activation edge
    // only if the condition was off; here it was ON (mode), so no new capture.
    s.press_accrete();
    assert!(!s.press_needs_holding(), "condition stayed on (mode -> hold), no re-capture");
    assert!(s.accrete_lit(), "still accreting via the held button");
    s.release_accrete();
    assert!(!s.accrete_lit());
    // With accrete held but the condition off (needs_holding on, nothing held), toggle
    // needs-holding back off: mode_on was cleared, so nothing activates.
    assert!(!s.press_needs_holding(), "leaving needs-holding with no mode on: inert");
    assert!(!s.accrete_lit());
  }

  #[test]
  fn capture_held_takes_a_batch() {
    let mut s = AccreteState::new();
    s.press_accrete();
    s.capture_held([10, 20]);
    assert!(s.note_released_sustains(10));
    assert!(s.note_released_sustains(20));
    assert_eq!(s.sustained_classes(58), [10, 20].into_iter().collect());
  }

  #[test]
  fn banks_are_independent() {
    // Two banks (one per monome): accreting on one adds nothing to -- and clears
    // nothing from -- the other.
    let mut banks = [AccreteState::new(), AccreteState::new()];
    banks[0].press_accrete(); // bank 0 into toggle mode
    banks[0].note_played(30);
    banks[1].note_played(30); // bank 1 is not accreting
    assert!(banks[0].note_released_sustains(30), "bank 0 sustains its own note");
    assert!(!banks[1].note_released_sustains(30), "bank 1 does not follow suit");
    banks[1].press_clear();
    assert!(banks[0].note_released_sustains(30), "bank 1's clear leaves bank 0 ringing");
    assert!(!banks[1].accrete_lit(), "bank 1 never entered accrete mode");
  }

  #[test]
  fn sustained_classes_fold_octaves() {
    let mut s = AccreteState::new();
    s.press_accrete();
    s.capture_held([60, 2, -56]);
    // 60 % 58 = 2, -56 mod 58 = 2: all one class.
    assert_eq!(s.sustained_classes(58), [2].into_iter().collect());
  }
}
