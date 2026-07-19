//! The reasons a pitch keeps ringing on a grid, reified (cleaning phase 6, per
//! `1_vision` item 1 and its DONE response -- the fingers carve-out).
//!
//! A pitch on a grid can sound for up to THREE reasons:
//! - *Finger*: a finger is on a cell that sounds it. DERIVED, never stored here -- it
//!   is the count of held-map entries at that pitch. Two colliding cells (e.g. with
//!   `x_step = 9`, `(x, y)` and `(x+1, y-9)`) finger ONE pitch, so a finger is a
//!   COUNT, not a boolean; flattening it to a boolean re-creates the class of bug that
//!   cell-keying the held map fixed. So fingers stay owned by the held map and are fed
//!   in as `finger_count(pitch)` on demand.
//! - *Sustain*: the accrete bank (a pedal, the accrete mode, or the per-note sustain
//!   toggle) is holding it. Stored here as [`Reason::Sustain`].
//! - *Edit*: the pitch is in per-voice edit mode, itself a reason to keep sounding
//!   (`1_vision`: "even if the note in edit mode was being fingered rather than
//!   sustained, it will continue to sound until exiting edit mode"). Stored here as
//!   [`Reason::Edit`].
//!
//! [`RingStore`] owns the SUSTAIN and EDIT pitch sets (absorbing what used to be
//! `AccreteState.sustained` and `EditState.pitches`). Its one real operation is
//! [`RingStore::remove_reason`]: take a reason away from some pitches and report
//! exactly those left with NO reason at all -- not the other set, not a finger. The
//! caller ends precisely those drones. That single operation replaces the five
//! hand-rolled "does anything else still hold this note up?" checks (exit-edit,
//! sustain-toggle-off, the two clears, and -- in query form -- the release path) that
//! each earned their own bug historically.
//!
//! [`GridRing`] bundles one grid's [`AccreteState`], [`EditState`], and [`RingStore`]
//! under ONE lock (the runtime holds a `Vec<GridRing>` behind a single mutex). The two
//! state machines keep their logic; every method that used to touch its own set now
//! reaches through the shared store, so the two can never disagree about what rings and
//! there is no two-lock ordering hazard between them.

use std::collections::HashSet;

use super::accrete::AccreteState;
use super::edit::EditState;

/// A stored reason a pitch rings. (Finger is deliberately absent: it is derived from
/// the held map, never stored -- see the module docs.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
  Sustain,
  Edit,
}

impl Reason {
  /// The other stored reason -- the one `remove_reason` must still respect.
  fn other(self) -> Reason {
    match self {
      Reason::Sustain => Reason::Edit,
      Reason::Edit => Reason::Sustain,
    }
  }
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

  /// THE operation. Remove reason `r` from each of `pitches`, and return exactly those
  /// pitches left with NO remaining reason: not the other stored reason, and not a
  /// finger (`finger_count(pitch) == 0`). The caller ends precisely those drones (via
  /// `synth::end_drones_at`); everything spared keeps ringing for the reason it still
  /// has. A finger's own voice is never among the returned pitches, so it can only ever
  /// be ended by its own release -- the rule the exit gesture lives by.
  pub fn remove_reason(
    &mut self,
    r: Reason,
    pitches: impl IntoIterator<Item = i32>,
    finger_count: impl Fn(i32) -> usize,
  ) -> Vec<i32> {
    let other = r.other();
    let mut reasonless = Vec::new();
    for pitch in pitches {
      self.set_mut(r).remove(&pitch);
      if !self.set(other).contains(&pitch) && finger_count(pitch) == 0 {
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
  fn remove_reason_returns_only_the_reason_less_pitches() {
    // Pitch 10 is sustained only; 20 is sustained AND edited; 30 is sustained but
    // fingered. Removing Sustain from all three ends ONLY 10: 20 still has Edit, and
    // 30 still has a finger.
    let mut s = RingStore::new();
    for p in [10, 20, 30] {
      s.add(Sustain, p);
    }
    s.add(Edit, 20);
    let ended = s.remove_reason(Sustain, [10, 20, 30], |p| usize::from(p == 30));
    assert_eq!(ended, [10], "only the pitch with no other reason and no finger ends");
    assert!(!s.has(Sustain, 10) && !s.has(Sustain, 20) && !s.has(Sustain, 30), "Sustain gone from all");
    assert!(s.has(Edit, 20), "the other reason is untouched");
  }

  #[test]
  fn remove_reason_respects_the_finger_count() {
    // A colliding-cell pitch fingered by TWO cells: removing its only stored reason
    // must not report it (the fingers still hold it), and a boolean would be enough to
    // see that -- but the COUNT is what the release path later decrements correctly.
    let mut s = RingStore::new();
    s.add(Sustain, 42);
    let ended = s.remove_reason(Sustain, [42], |p| if p == 42 { 2 } else { 0 });
    assert!(ended.is_empty(), "two fingers hold the pitch: nothing ends");
    assert!(!s.has(Sustain, 42), "but the sustain reason is still removed");
  }

  #[test]
  fn the_doubly_held_matrix() {
    // Every combination of the three reasons on one pitch, removing one stored reason
    // at a time, asserting whether the pitch is reported reason-less (its drone dies).
    // Finger is derived, so "removing the finger" is modelled by dropping the count to
    // zero on the next query -- there is no finger reason to remove here; we remove a
    // STORED reason and let the finger count decide.
    for finger in [0usize, 1] {
      for sustain in [false, true] {
        for edit in [false, true] {
          // Remove Sustain.
          if sustain {
            let mut s = RingStore::new();
            s.add(Sustain, 7);
            if edit {
              s.add(Edit, 7);
            }
            let ended = s.remove_reason(Sustain, [7], |_| finger);
            let should_die = !edit && finger == 0;
            assert_eq!(
              ended == [7],
              should_die,
              "remove Sustain with finger={finger} sustain=true edit={edit}: dies={should_die}",
            );
          }
          // Remove Edit.
          if edit {
            let mut s = RingStore::new();
            s.add(Edit, 7);
            if sustain {
              s.add(Sustain, 7);
            }
            let ended = s.remove_reason(Edit, [7], |_| finger);
            let should_die = !sustain && finger == 0;
            assert_eq!(
              ended == [7],
              should_die,
              "remove Edit with finger={finger} sustain={sustain} edit=true: dies={should_die}",
            );
          }
        }
      }
    }
  }

  #[test]
  fn colliding_cells_two_fingers_hold_one_pitch() {
    // Two colliding cells finger pitch 8. Even after both stored reasons are stripped,
    // the pitch is never reported reason-less while EITHER finger is down -- the count
    // carries it. Only when the count reaches zero does removing the last reason end it.
    let mut s = RingStore::new();
    s.add(Sustain, 8);
    // Both fingers down: sustain-clear spares it.
    assert!(s.remove_reason(Sustain, [8], |_| 2).is_empty(), "two fingers: spared");
    // Re-sustain, one finger lifts (count 1): still spared.
    s.add(Sustain, 8);
    assert!(s.remove_reason(Sustain, [8], |_| 1).is_empty(), "one finger left: spared");
    // Re-sustain, both fingers gone: now the sustain reason was the last thing, so it ends.
    s.add(Sustain, 8);
    assert_eq!(s.remove_reason(Sustain, [8], no_fingers), [8], "no fingers: ends");
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
