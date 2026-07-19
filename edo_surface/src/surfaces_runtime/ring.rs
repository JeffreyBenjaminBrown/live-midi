//! The reasons a pitch keeps ringing on a grid, reified (cleaning phase 6, per
//! `1_vision` item 1 and its DONE response -- the fingers carve-out).
//!
//! *Edit implies sustain (branch-3 queue item 4).* A pitch keeps SOUNDING for one of
//! two reasons only, and edit mode is no longer one of them:
//! - *Finger*: a finger is on a cell that sounds it. DERIVED, never stored here -- it
//!   is the count of held-map entries at that pitch. Two colliding cells (e.g. with
//!   `x_step = 9`, `(x, y)` and `(x+1, y-9)`) finger ONE pitch, so a finger is a
//!   COUNT, not a boolean; flattening it to a boolean re-creates the class of bug that
//!   cell-keying the held map fixed. So fingers stay owned by the held map and are fed
//!   in as `finger_count(pitch)` on demand.
//! - *Sustain*: the accrete bank (a pedal, the accrete mode, or the per-note sustain
//!   toggle) is holding it. Stored here as [`Reason::Sustain`]. This is the only stored
//!   life-support reason.
//!
//! *Edit is a SELECTION, not a reason to ring.* [`Reason::Edit`] still names a stored
//! pitch set, but membership in it never keeps a note alive: it marks the notes the
//! multipliers / `=1` / drag / dances act on. Entering edit mode adds the pitch to BOTH
//! sets, so the invariant **edited ⊆ sustained** holds at all times -- an edited note is
//! always sustained (and so always audible, and always square-dancing). Two consequences
//! fall out of that invariant, and both are enforced here:
//! - *Leaving edit ends nothing.* Dropping the edit reason (the exit gesture, the
//!   editmode clear) is pure deselection: the note is still sustained, so it rings on.
//! - *Losing sustain deselects.* Every removal of the sustain reason ([`discard_sustain`]
//!   and [`remove_sustain`]) also drops edit membership -- nothing silent may stay
//!   selected (the historical "dancing ghost"), and the invariant is preserved from the
//!   other side.
//!
//! This SUPERSEDES the earlier symmetric-clears design, where Edit was a third
//! life-support reason and each clear removed only its own reason (so a doubly-held note
//! survived either clear alone and died only to both). That is gone: the sustain clear
//! now ends an edited-and-sustained note (and deselects it), and the editmode clear ends
//! no voice at all.
//!
//! [`RingStore`] owns the SUSTAIN and EDIT pitch sets (absorbing what used to be
//! `AccreteState.sustained` and `EditState.pitches`). Its one life-support operation is
//! [`RingStore::remove_sustain`]: take the sustain reason away from some pitches and
//! report exactly those left with no finger. The caller ends precisely those drones.
//!
//! [`GridRing`] bundles one grid's [`AccreteState`], [`EditState`], and [`RingStore`]
//! under ONE lock (the runtime holds a `Vec<GridRing>` behind a single mutex). The two
//! state machines keep their logic; every method that used to touch its own set now
//! reaches through the shared store, so the two can never disagree about what rings and
//! there is no two-lock ordering hazard between them.

use std::collections::HashSet;

use super::accrete::AccreteState;
use super::edit::EditState;

/// A stored pitch set. Only [`Reason::Sustain`] keeps a note alive; [`Reason::Edit`] is
/// a selection over the sustained (or fingered) voices, never life-support (see the
/// module docs). Finger is deliberately absent from both: it is derived from the held
/// map, never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
  Sustain,
  Edit,
}

/// One grid's stored ring reasons: the sustained and the edited pitch sets.
#[derive(Debug, Default)]
pub struct RingStore {
  sustained: HashSet<i32>,
  edited: HashSet<i32>,
}

impl RingStore {
  pub fn new() -> Self {
    RingStore::default()
  }

  fn set(&self, r: Reason) -> &HashSet<i32> {
    match r {
      Reason::Sustain => &self.sustained,
      Reason::Edit => &self.edited,
    }
  }

  fn set_mut(&mut self, r: Reason) -> &mut HashSet<i32> {
    match r {
      Reason::Sustain => &mut self.sustained,
      Reason::Edit => &mut self.edited,
    }
  }

  /// Does `pitch` ring for reason `r`?
  pub fn has(&self, r: Reason, pitch: i32) -> bool {
    self.set(r).contains(&pitch)
  }

  /// Add reason `r` to `pitch`. Returns whether it was NEWLY added.
  pub fn add(&mut self, r: Reason, pitch: i32) -> bool {
    self.set_mut(r).insert(pitch)
  }

  /// Drop reason `r` from `pitch` with no reason-less bookkeeping -- for callers that
  /// only want the set membership gone (an erase removing the sustain reason while the
  /// finger keeps the note sounding). Returns whether it was present.
  pub fn discard(&mut self, r: Reason, pitch: i32) -> bool {
    self.set_mut(r).remove(&pitch)
  }

  /// The pitches ringing for reason `r`.
  pub fn iter(&self, r: Reason) -> impl Iterator<Item = i32> + '_ {
    self.set(r).iter().copied()
  }

  /// Is anything ringing for reason `r`?
  pub fn any(&self, r: Reason) -> bool {
    !self.set(r).is_empty()
  }

  /// The pitch *classes* ringing for reason `r` (octave-folded), for the bright-LED
  /// reflection.
  pub fn classes(&self, r: Reason, edo: i32) -> HashSet<i32> {
    self.set(r).iter().map(|p| p.rem_euclid(edo)).collect()
  }

  /// Re-file a pitch that MOVED (a per-voice pitch edit dragged it), for BOTH reasons
  /// at once -- the sets are keyed by pitch, so a dragged note filed under the pitch it
  /// left would be flushed by the wrong name and re-droned at the old pitch. A no-op
  /// for whichever set did not hold `from`.
  pub fn note_moved(&mut self, from: i32, to: i32) {
    for r in [Reason::Sustain, Reason::Edit] {
      if self.set_mut(r).remove(&from) {
        self.set_mut(r).insert(to);
      }
    }
  }

  /// Drop the sustain reason from `pitch`, cascading to the edit SELECTION. The model's
  /// invariant is edited ⊆ sustained, and nothing silent may stay selected (the
  /// historical "dancing ghost"), so a pitch that stops being sustained also stops being
  /// edited. Returns whether it had been sustained. Used where a finger keeps the note
  /// audible (erase), so no drone-ending is implied here.
  pub fn discard_sustain(&mut self, pitch: i32) -> bool {
    self.edited.remove(&pitch);
    self.sustained.remove(&pitch)
  }

  /// THE life-support operation. Take the sustain reason away from each of `pitches`
  /// (cascading edit membership, per [`discard_sustain`]) and return exactly those left
  /// with NO finger (`finger_count(pitch) == 0`) -- the caller ends precisely those
  /// drones (via `synth::end_drones_at`). A finger's own voice is never returned, so it
  /// can only ever be ended by its own release.
  ///
  /// Sustain is the only life-support reason, so this is the only operation that ends
  /// voices. Removing the edit reason ends nothing (it is pure deselection), so there is
  /// deliberately no `remove_edit` counterpart -- callers drop edit membership with
  /// `discard(Reason::Edit, ..)` and end nothing.
  pub fn remove_sustain(
    &mut self,
    pitches: impl IntoIterator<Item = i32>,
    finger_count: impl Fn(i32) -> usize,
  ) -> Vec<i32> {
    let mut reasonless = Vec::new();
    for pitch in pitches {
      self.discard_sustain(pitch);
      if finger_count(pitch) == 0 {
        reasonless.push(pitch);
      }
    }
    reasonless
  }
}

/// One grid's ring: its accrete state machine, its edit state machine, and the shared
/// store both point at -- all three under one lock in the runtime's `Vec<GridRing>`.
#[derive(Default)]
pub struct GridRing {
  pub accrete: AccreteState,
  pub edit: EditState,
  pub store: RingStore,
}

impl GridRing {
  /// A grid ring around an already-configured accrete bank (switchable or momentary).
  pub fn new(accrete: AccreteState) -> Self {
    GridRing { accrete, edit: EditState::new(), store: RingStore::new() }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use Reason::{Edit, Sustain};

  /// No fingers anywhere.
  fn no_fingers(_: i32) -> usize {
    0
  }

  #[test]
  fn remove_sustain_returns_the_fingerless_pitches_and_cascades_edit() {
    // NEW model (was `remove_reason_returns_only_the_reason_less_pitches`): edit is no
    // longer a life-support reason, so removing Sustain ends every pitch without a
    // finger -- an edit membership does NOT spare it. Pitch 10 is sustained only; 20 is
    // sustained AND edited; 30 is sustained but fingered. Removing Sustain ends 10 and
    // 20 (no fingers), spares 30 (its finger). And 20's edit membership is dropped: the
    // invariant edited ⊆ sustained, and nothing silent may stay selected.
    let mut s = RingStore::new();
    for p in [10, 20, 30] {
      s.add(Sustain, p);
    }
    s.add(Edit, 20);
    let mut ended = s.remove_sustain([10, 20, 30], |p| usize::from(p == 30));
    ended.sort();
    assert_eq!(ended, [10, 20], "every fingerless pitch ends -- edit does not keep 20 alive");
    assert!(!s.has(Sustain, 10) && !s.has(Sustain, 20) && !s.has(Sustain, 30), "Sustain gone from all");
    assert!(!s.has(Edit, 20), "losing sustain deselected 20 -- no silent selection");
  }

  #[test]
  fn remove_sustain_respects_the_finger_count() {
    // A colliding-cell pitch fingered by TWO cells: removing the sustain reason must not
    // report it (the fingers still hold it), and a boolean would be enough to see that --
    // but the COUNT is what the release path later decrements correctly.
    let mut s = RingStore::new();
    s.add(Sustain, 42);
    let ended = s.remove_sustain([42], |p| if p == 42 { 2 } else { 0 });
    assert!(ended.is_empty(), "two fingers hold the pitch: nothing ends");
    assert!(!s.has(Sustain, 42), "but the sustain reason is still removed");
  }

  #[test]
  fn discard_sustain_deselects_the_ghost_invariant() {
    // The other half of edited ⊆ sustained: whenever sustain leaves, edit leaves too, so
    // no silent pitch can remain selected (the historical "dancing ghost"). `discard_sustain`
    // is the erase-style removal (a finger keeps the note audible), so it ends no drone
    // but must still deselect.
    let mut s = RingStore::new();
    s.add(Sustain, 5);
    s.add(Edit, 5);
    assert!(s.discard_sustain(5), "it had been sustained");
    assert!(!s.has(Sustain, 5) && !s.has(Edit, 5), "sustain AND edit both gone");
  }

  #[test]
  fn leaving_edit_ends_nothing() {
    // Edit is a selection, not life-support: dropping it (the exit gesture / editmode
    // clear) is `discard(Edit, ..)` and leaves the note sustained, so it rings on. There
    // is deliberately no `remove_edit` that could end a voice.
    let mut s = RingStore::new();
    s.add(Sustain, 9);
    s.add(Edit, 9);
    assert!(s.discard(Edit, 9), "the edit membership is dropped");
    assert!(s.has(Sustain, 9), "but the note is still sustained -- it keeps ringing");
    assert!(!s.has(Edit, 9));
  }

  #[test]
  fn the_doubly_held_matrix() {
    // Every combination of finger / sustain / edit on one pitch, removing the sustain
    // reason, asserting whether the pitch is reported life-less (its drone dies). Edit is
    // NOT life-support now, so it never keeps the pitch alive; only a finger does. And
    // removing sustain always deselects (cascade). Finger is derived, modelled as a count
    // the query returns.
    for finger in [0usize, 1] {
      for edit in [false, true] {
        // Only a SUSTAINED pitch can be edited (edited ⊆ sustained), so we always
        // sustain, optionally edit, then remove Sustain.
        let mut s = RingStore::new();
        s.add(Sustain, 7);
        if edit {
          s.add(Edit, 7);
        }
        let ended = s.remove_sustain([7], |_| finger);
        let should_die = finger == 0; // edit is irrelevant to life; only the finger spares
        assert_eq!(
          ended == [7],
          should_die,
          "remove Sustain with finger={finger} edit={edit}: dies={should_die}",
        );
        assert!(!s.has(Edit, 7), "and edit membership always cascades away with sustain");
      }
    }
  }

  #[test]
  fn colliding_cells_two_fingers_hold_one_pitch() {
    // Two colliding cells finger pitch 8. Even after the sustain reason is stripped, the
    // pitch is never reported life-less while EITHER finger is down -- the count carries
    // it. Only when the count reaches zero does removing sustain end it.
    let mut s = RingStore::new();
    s.add(Sustain, 8);
    // Both fingers down: sustain-clear spares it.
    assert!(s.remove_sustain([8], |_| 2).is_empty(), "two fingers: spared");
    // Re-sustain, one finger lifts (count 1): still spared.
    s.add(Sustain, 8);
    assert!(s.remove_sustain([8], |_| 1).is_empty(), "one finger left: spared");
    // Re-sustain, both fingers gone: now sustain was the last thing, so it ends.
    s.add(Sustain, 8);
    assert_eq!(s.remove_sustain([8], no_fingers), [8], "no fingers: ends");
  }

  #[test]
  fn note_moved_refiles_both_reason_sets() {
    let mut s = RingStore::new();
    s.add(Sustain, 20);
    s.add(Edit, 20);
    s.note_moved(20, 35);
    assert!(!s.has(Sustain, 20) && !s.has(Edit, 20), "the old pitch is vacated in both sets");
    assert!(s.has(Sustain, 35) && s.has(Edit, 35), "and re-filed under the new one");
    // A note not in a set is left alone there.
    s.note_moved(99, 100);
    assert!(!s.has(Sustain, 100) && !s.has(Edit, 100));
  }

  #[test]
  fn classes_fold_octaves_per_reason() {
    let mut s = RingStore::new();
    s.add(Sustain, 60);
    s.add(Sustain, 2);
    s.add(Edit, 3);
    assert_eq!(s.classes(Sustain, 58), [2].into_iter().collect(), "60 and 2 are one class mod 58");
    assert_eq!(s.classes(Edit, 58), [3].into_iter().collect());
  }
}
