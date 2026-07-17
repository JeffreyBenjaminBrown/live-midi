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
//! Four buttons (per grid):
//! - *clear* silences and flushes this bank's sustained set (key-down; lit while held);
//! - *needs_holding* toggles how *accrete* AND *erase* behave (key-down; lit while on);
//! - *accrete*: with needs_holding, notes fingered while it is HELD join the sustained
//!   set; without, key-down toggles an accrete *mode* during which every note played
//!   joins. Either way, entering the accreting condition also captures the notes
//!   already held at that moment (on this bank's grid), and a note in the set keeps
//!   ringing after its finger lifts, until *clear*.
//! - *erase* (misc.org "erase button"): the same hold-or-toggle shape as *accrete*,
//!   under the same *needs_holding*, but it REMOVES pressed pitches from the sustained
//!   set -- each keeps sounding until its own finger lifts (the runtime's ordinary
//!   release, which no longer sustains it). Entering the erasing condition also erases
//!   the notes already held on this grid. When both conditions are live, erase wins.
//!
//! Nothing else ever leaves the set except through `press_clear` (un-toggling accrete
//! mode stops *additions* only). Retriggering a sustaining pitch does not touch the set
//! either: the runtime cuts the old drone VOICE (`SurfaceSink::cut_sustained`) and the
//! new note takes its place -- still a member, so its release re-drones it (unless
//! erase is live, in which case the press removed it from the set).
//!
//! The runtime asks `note_released_sustains` at each note-off: a note sustains if it
//! is already in the set OR the accreting condition holds right then. The latter makes
//! needs-holding mode read as the *continuous* condition Jeff described ("any notes
//! fingered ... as long as they are now held") rather than an edge-triggered one --
//! e.g. after a clear, notes still fingered under a still-held accrete re-join the set
//! when they are next examined.

use std::collections::HashSet;

pub struct AccreteState {
  /// Whether *accrete* and *erase* must be held (vs toggling a mode). Starts false:
  /// both are toggles until the needs-holding button says otherwise.
  needs_holding: bool,
  /// The toggle-mode "accrete mode" flag (only consulted when !needs_holding).
  mode_on: bool,
  /// How many accrete buttons are physically held right now (the on-grid button and
  /// its pedal mirror can overlap). Tracked unconditionally so a needs-holding flip
  /// mid-hold stays consistent.
  hold_count: usize,
  /// The toggle-mode "erase mode" flag (only consulted when !needs_holding).
  erase_mode_on: bool,
  /// How many erase buttons are physically held right now (same bookkeeping as
  /// `hold_count`).
  erase_hold_count: usize,
  /// How many clear buttons are physically held (drives that button's LED only).
  clear_count: usize,
  /// The sustained set: absolute pitches (this bank's grid only).
  sustained: HashSet<i32>,
}

/// Which conditions a `press_needs_holding` flip turned ON (off -> on edges only).
/// The caller must then apply the corresponding bulk action to the notes currently
/// held on this bank's grid: capture them (accrete) or erase them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Activated {
  pub accrete: bool,
  pub erase: bool,
}

impl AccreteState {
  /// A bank whose `needs_holding` switch IS bound somewhere, so the player can flip
  /// it. Starts in toggle mode, as it always has.
  pub fn new() -> Self {
    AccreteState {
      needs_holding: false,
      mode_on: false,
      hold_count: 0,
      erase_mode_on: false,
      erase_hold_count: 0,
      clear_count: 0,
      sustained: HashSet::new(),
    }
  }

  /// A bank that is MOMENTARY forever: accrete is live only while its button is
  /// physically held.
  ///
  /// For rigs that bind no `needs_holding` control anywhere. Without that switch the
  /// mode can never be changed, so leaving it at the toggle default is a trap: one tap
  /// of accrete and every later note sustains, with nothing on the surface saying why
  /// and no way back except `clear`. Jeff hit exactly that ("even after removing my
  /// foot, new notes sustain ... for this rig it should only ever be in momentary
  /// mode"). Bind a `needs_holding` control and you get the switchable bank instead.
  pub fn new_momentary() -> Self {
    AccreteState { needs_holding: true, ..AccreteState::new() }
  }

  /// Is the accreting condition live right now?
  pub fn accreting(&self) -> bool {
    if self.needs_holding {
      self.hold_count > 0
    } else {
      self.mode_on
    }
  }

  /// Is the erasing condition live right now? Same shape as `accreting`, driven by
  /// the erase button under the same needs-holding switch. Both conditions can be
  /// live at once; where they conflict, erase wins.
  pub fn erasing(&self) -> bool {
    if self.needs_holding {
      self.erase_hold_count > 0
    } else {
      self.erase_mode_on
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

  /// Key-down on *needs_holding*: flip how the accrete AND erase buttons behave.
  /// Entering needs-holding cancels any toggled accrete/erase mode (the set is
  /// untouched). Returns which conditions this press *started* (an already-held
  /// button suddenly counts), for which the caller must bulk-apply held notes.
  pub fn press_needs_holding(&mut self) -> Activated {
    let acc_before = self.accreting();
    let erase_before = self.erasing();
    self.needs_holding = !self.needs_holding;
    if self.needs_holding {
      self.mode_on = false;
      self.erase_mode_on = false;
    }
    Activated {
      accrete: self.accreting() && !acc_before,
      erase: self.erasing() && !erase_before,
    }
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

  /// Key-down on *erase*: the accrete button's shape (hold or toggle, per the same
  /// needs_holding). Returns true if this press started the erasing condition
  /// (caller erases the notes currently held on this bank's grid).
  pub fn press_erase(&mut self) -> bool {
    let before = self.erasing();
    self.erase_hold_count += 1;
    if !self.needs_holding {
      self.erase_mode_on = !self.erase_mode_on;
    }
    self.erasing() && !before
  }

  /// Key-up on *erase*: only meaningful in needs-holding mode, but always tracked.
  pub fn release_erase(&mut self) {
    self.erase_hold_count = self.erase_hold_count.saturating_sub(1);
  }

  /// A note-on happened on this bank's grid. If erasing, the pitch leaves the
  /// sustained set (the note itself keeps sounding until its finger lifts; erase
  /// overrides accrete); else if accreting, it joins immediately.
  pub fn note_played(&mut self, pitch: i32) {
    if self.erasing() {
      self.sustained.remove(&pitch);
    } else if self.accreting() {
      self.sustained.insert(pitch);
    }
  }

  /// Bulk-capture the notes currently held on this bank's grid -- called when the
  /// accreting condition turns on. Inert while erase is live (erase wins).
  pub fn capture_held<I: IntoIterator<Item = i32>>(&mut self, held: I) {
    if self.erasing() {
      return;
    }
    self.sustained.extend(held);
  }

  /// Bulk-erase the notes currently held on this bank's grid -- called when the
  /// erasing condition turns on (they keep sounding under their fingers).
  pub fn erase_held<I: IntoIterator<Item = i32>>(&mut self, held: I) {
    for pitch in held {
      self.sustained.remove(&pitch);
    }
  }

  /// A note-off is happening on this bank's grid: should this note keep ringing (be
  /// transferred to a sustain voice) instead of releasing? Under a live erasing
  /// condition, never -- the pitch leaves the set on the spot (the continuous
  /// reading, mirroring accrete's). Otherwise true if it is already in the set, or
  /// the accreting condition holds at release time (joining the set on the spot).
  pub fn note_released_sustains(&mut self, pitch: i32) -> bool {
    if self.erasing() {
      self.sustained.remove(&pitch);
      return false;
    }
    if self.accreting() {
      self.sustained.insert(pitch);
    }
    self.sustained.contains(&pitch)
  }

  /// The sustained pitches themselves (not classes). Edit mode asks per-pitch
  /// whether a note is sounding on THIS grid, and a drone counts: an edited note
  /// keeps ringing with no finger on it and must stay editable.
  pub fn sustained_pitches(&self) -> impl Iterator<Item = i32> + '_ {
    self.sustained.iter().copied()
  }

  /// Sustain `pitch` outright, whatever the accrete condition. Returns whether it was
  /// NEWLY sustained -- i.e. whether the caller is now the reason it rings.
  ///
  /// For per-voice edit mode, which is a *second* reason a note keeps sounding
  /// (`1_vision`: "even if the note in edit mode was being fingered rather than
  /// sustained, it will continue to sound until exiting edit mode"). Routing it through
  /// this bank rather than inventing a parallel set means every downstream reader --
  /// the release path, the bright-LED reflection, `clear` -- treats an edited note
  /// exactly like any other sounding note, with no special cases.
  pub fn sustain_pitch(&mut self, pitch: i32) -> bool {
    self.sustained.insert(pitch)
  }

  /// Stop sustaining `pitch`. The mirror of [`sustain_pitch`], for a caller undoing
  /// its own reason (edit mode ending); the voice itself is the caller's to release.
  pub fn drop_pitch(&mut self, pitch: i32) {
    self.sustained.remove(&pitch);
  }

  /// Re-file a sustained pitch that MOVED (a per-voice pitch edit dragged it). The
  /// set is keyed by pitch, so without this a dragged drone would still be filed
  /// under the pitch it left: clear would still flush it, but by the wrong name, and
  /// `note_released_sustains` would re-drone the old pitch. A no-op if `from` was not
  /// sustained (a fingered note was dragged, which the finger map tracks instead).
  pub fn note_moved(&mut self, from: i32, to: i32) {
    if self.sustained.remove(&from) {
      self.sustained.insert(to);
    }
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

  /// Lit while the erasing condition is live. Independent of `accrete_lit`: with
  /// both conditions on, both buttons light (the override is behavioral).
  pub fn erase_lit(&self) -> bool {
    self.erasing()
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
    assert!(!s.press_needs_holding().accrete, "flipping the mode with accrete unheld activates nothing");
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
    assert!(!s.press_needs_holding().accrete, "no activation: the condition just turned OFF");
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
    assert!(!s.press_needs_holding().accrete, "condition stayed on (mode -> hold), no re-capture");
    assert!(s.accrete_lit(), "still accreting via the held button");
    s.release_accrete();
    assert!(!s.accrete_lit());
    // With accrete held but the condition off (needs_holding on, nothing held), toggle
    // needs-holding back off: mode_on was cleared, so nothing activates.
    assert!(!s.press_needs_holding().accrete, "leaving needs-holding with no mode on: inert");
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

  #[test]
  fn erase_toggle_mode_removes_pressed_pitches_until_untoggled() {
    let mut s = AccreteState::new();
    s.press_accrete(); // accrete mode on
    s.note_played(30);
    s.note_played(42);
    s.press_accrete(); // accrete mode off (both pitches stay)
    assert!(s.press_erase(), "erase-mode entry is an activation (erase held notes)");
    s.release_erase();
    assert!(s.erase_lit(), "key-down toggled erase mode on; key-up does nothing");
    s.note_played(30);
    assert!(!s.note_released_sustains(30), "a pitch pressed under erase leaves the set");
    assert_eq!(s.sustained_classes(58), [42].into_iter().collect(), "unpressed pitches stay");
    // Un-toggle: erasure stops.
    s.press_erase();
    s.release_erase();
    assert!(!s.erase_lit());
    s.note_played(42);
    assert!(s.note_released_sustains(42), "after erase mode ends, presses stop erasing");
  }

  #[test]
  fn erase_activation_edges_mirror_accretes() {
    let mut s = AccreteState::new();
    assert!(s.press_erase(), "toggle-mode entry is an activation (erase held notes)");
    s.release_erase();
    assert!(!s.press_erase(), "toggle-mode exit: no activation");
    s.release_erase();
    s.press_needs_holding();
    assert!(s.press_erase(), "first hold under needs-holding: activation");
    assert!(!s.press_erase(), "second simultaneous hold: already on");
    s.release_erase();
    assert!(s.erase_lit(), "still held once");
    s.release_erase();
    assert!(!s.erase_lit(), "key-up ends the condition in needs-holding mode");
  }

  #[test]
  fn needs_holding_governs_erase_like_accrete() {
    let mut s = AccreteState::new();
    s.press_accrete(); // tap: accrete mode on, fill the set
    s.release_accrete();
    s.note_played(30);
    s.press_accrete(); // tap: and off again
    s.release_accrete();
    s.press_needs_holding();
    s.press_erase();
    assert!(s.erase_lit(), "erase live while held");
    s.note_played(30);
    assert!(!s.note_released_sustains(30), "pressed under a held erase -> removed");
    s.release_erase();
    assert!(!s.erase_lit());
    s.note_played(30); // does nothing: neither condition live
    assert!(!s.note_released_sustains(30), "30 was erased and never re-added");
  }

  #[test]
  fn entering_needs_holding_cancels_a_toggled_erase_mode() {
    let mut s = AccreteState::new();
    s.press_erase(); // toggle erase mode on...
    s.release_erase(); // ...and lift
    assert!(s.erase_lit());
    let a = s.press_needs_holding();
    assert!(!a.erase, "no activation: the condition just turned OFF");
    assert!(!s.erase_lit(), "needs-holding cancels the toggled erase mode");
  }

  #[test]
  fn flipping_needs_holding_with_erase_held_activates_erase() {
    let mut s = AccreteState::new();
    s.press_accrete();
    s.note_played(30);
    s.press_accrete(); // set holds 30; accrete off
    s.press_needs_holding(); // needs-holding on
    s.press_needs_holding(); // and back off (toggle semantics restored)
    s.press_erase(); // toggle mode: erase on (and held)
    let a = s.press_needs_holding(); // held button now satisfies the hold condition
    assert!(!a.erase, "condition stayed on (mode -> hold): not an edge");
    assert!(s.erase_lit(), "still erasing via the held button");
    s.release_erase();
    assert!(!s.erase_lit());
  }

  #[test]
  fn erase_overrides_accrete_when_both_are_live() {
    let mut s = AccreteState::new();
    s.press_accrete(); // accrete mode on
    s.note_played(30);
    assert!(s.press_erase(), "erase mode on too (activation)");
    s.note_played(30);
    assert!(!s.note_released_sustains(30), "pressed under both -> erased, not re-added");
    s.note_played(44);
    assert!(!s.note_released_sustains(44), "a fresh pitch under both is NOT captured");
    assert!(s.accrete_lit() && s.erase_lit(), "both LEDs show their own condition");
    // capture_held is inert while erase is live (erase wins for bulk actions too).
    s.capture_held([50]);
    assert!(!s.note_released_sustains(50), "bulk capture under live erase adds nothing");
    // Un-toggle erase: accrete mode is still on and resumes adding.
    s.press_erase();
    s.note_played(44);
    assert!(s.note_released_sustains(44), "accrete resumes once erase ends");
  }

  #[test]
  fn erase_held_bulk_removes_only_the_given_pitches() {
    let mut s = AccreteState::new();
    s.press_accrete();
    s.capture_held([10, 20, 30]);
    s.press_accrete(); // accrete off; set = {10, 20, 30}
    s.press_erase(); // erase on: the caller bulk-erases held notes
    s.erase_held([10, 30]);
    assert_eq!(s.sustained_classes(58), [20].into_iter().collect(), "only the held pitches left");
    s.press_erase(); // erase off again
    s.release_erase();
    assert!(s.note_released_sustains(20), "the survivor still sustains");
  }

  // ---- momentary banks (rigs that bind no needs_holding control) ----

  /// The bug Jeff hit on the hardware: with the toggle default, ONE tap of accrete
  /// latched the mode on, so every later note sustained even with his foot off it.
  #[test]
  fn a_momentary_bank_stops_accreting_the_moment_the_button_is_released() {
    let mut a = AccreteState::new_momentary();
    a.press_accrete();
    assert!(a.accreting(), "held: accreting");
    a.release_accrete();
    assert!(!a.accreting(), "released: NOT accreting -- a tap must not latch");

    // ...and a note played afterwards must not sustain.
    a.note_played(40);
    assert!(!a.note_released_sustains(40), "a note played after the lift is not sustained");
  }

  /// The contrast, and why the default is not simply wrong: a bank WITH the switch
  /// bound still toggles, which is what the drums rig has always done.
  #[test]
  fn a_switchable_bank_still_toggles_on_a_tap() {
    let mut a = AccreteState::new();
    a.press_accrete();
    a.release_accrete();
    assert!(a.accreting(), "toggle mode: the tap latches, by design");
  }

  #[test]
  fn a_momentary_bank_sustains_notes_played_while_held() {
    let mut a = AccreteState::new_momentary();
    a.press_accrete();
    a.note_played(40);
    assert!(a.note_released_sustains(40), "held: the note joins the sustained set");
    a.release_accrete();
    // It stays sustained -- lifting the foot stops ADDING, it does not flush.
    assert!(a.sustained_pitches().any(|p| p == 40), "lifting does not clear what was caught");
  }

  /// Two feet / a pedal and a button can overlap on one bank; the hold is counted,
  /// not a boolean, so releasing one does not cancel the other.
  #[test]
  fn a_momentary_banks_holds_are_counted() {
    let mut a = AccreteState::new_momentary();
    a.press_accrete();
    a.press_accrete();
    a.release_accrete();
    assert!(a.accreting(), "still held by the second presser");
    a.release_accrete();
    assert!(!a.accreting());
  }

  #[test]
  fn clear_still_flushes_a_momentary_bank() {
    let mut a = AccreteState::new_momentary();
    a.press_accrete();
    a.note_played(40);
    a.release_accrete();
    a.press_clear();
    assert!(a.sustained_pitches().next().is_none(), "clear empties the set");
  }
}
