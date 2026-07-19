//! Per-voice edit mode: the pure state of "which notes am I editing, and what does a
//! press mean right now" (see `TODO/many/1_vision.org` "per-voice pitch edit" and
//! `2_discussion.org` 2a-2d for the decisions).
//!
//! *The gesture.* Pressing a cell whose neighbour DIRECTLY ABOVE holds a sounding
//! note toggles that note's edit mode. Geometric, not pitch-defined: Jeff's call, so
//! the gesture does not move when the tuning changes ("my thumb can only reach from
//! below"). With `y_step = 1` the cell below is one microtone up in pitch, but nothing
//! here relies on that.
//!
//! *The cost, accepted.* That cell can then never sound, and while ANY note on a grid
//! is in edit mode the grid stops playing new notes and becomes a pitch-picker: a
//! press on a FREE pitch (claimed by no voice in the editing set or the sustain set)
//! drags the nearest edited note instead; a press on any sustained or edited pitch
//! retriggers it in place, exactly as it would outside edit mode (branch-3 queue item
//! 6). You leave edit mode to play again.
//!
//! *Asymmetry, deliberate.* A pitch drag moves the NEAREST edited note (one of them);
//! a factored-pulse change (`polyrhythm`) applies to ALL of them at once. So "in edit
//! mode" denotes a different selection depending on which parameter you touch.
//!
//! *Edit is a selection, and it implies sustain (branch-3 queue item 4).* Being in edit
//! mode is no longer a reason a note rings; it is the selection the multipliers / `=1` /
//! drag / dances act on. To keep an edited note audible, `enter` puts it in the sustain
//! set too, so the invariant edited ⊆ sustained holds: an edited note is always
//! sustained. Leaving edit mode (`exit`, `clear`) is therefore pure deselection and ends
//! nothing; losing sustain (`RingStore::remove_sustain`) is what ends a voice, and it
//! cascades the edit removal so nothing silent can stay selected. This supersedes the
//! earlier model where edit was a third life-support reason and the two clears were
//! symmetric.
//!
//! Edit mode is a property of the PITCH, not of a voice or a cell, so it survives
//! retriggering (`2_discussion` 4h) and it follows the note when the note moves.
//! Nothing here is octave-duplicated or mirrored across grids -- unlike every other
//! LED rule in this codebase, this one is local.

use std::collections::HashSet;

use super::ring::{Reason, RingStore};

/// What a press on a play grid means, given that grid's edit state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
  /// Ordinary note-on: nothing is being edited and this cell is not a trigger.
  Play,
  /// The cell above holds sounding note `pitch`, which was NOT being edited: start
  /// editing it. The pressed cell does not sound.
  EnterEdit { pitch: i32 },
  /// The cell above holds sounding note `pitch`, which WAS being edited: stop. The
  /// pressed cell does not sound.
  ExitEdit { pitch: i32 },
  /// Toggle whether `pitch` keeps sounding with no finger on it. The pressed cell
  /// does not sound.
  ToggleSustain { pitch: i32 },
  /// Drag the edited note `from` to `to`, gliding. The pressed cell does not sound.
  Drag { from: i32, to: i32 },
}

/// One grid's edit-mode state machine. The set of edited pitches now lives in the
/// shared [`RingStore`] (cleaning phase 6, alongside the sustained set), so this struct
/// carries no state of its own -- every query and mutation reaches through a `store`
/// argument. Keeping the two sets in one store means edit and sustain can never
/// disagree about what rings.
#[derive(Debug, Default)]
pub struct EditState;

impl EditState {
  pub fn new() -> Self {
    EditState
  }

  pub fn is_editing(&self, pitch: i32, store: &RingStore) -> bool {
    store.has(Reason::Edit, pitch)
  }

  pub fn any(&self, store: &RingStore) -> bool {
    store.any(Reason::Edit)
  }

  pub fn pitches<'a>(&self, store: &'a RingStore) -> impl Iterator<Item = i32> + 'a {
    store.iter(Reason::Edit)
  }

  /// The edited pitch nearest to `target`, over the full edit SELECTION (the piano
  /// layer's store set unioned with the edit-flagged chord voices' pitches -- the
  /// caller builds the union). Ties go to the LOWER note (`2_discussion` 2d),
  /// matching the looper's lowest-first rule.
  pub fn nearest(&self, target: i32, edited: &HashSet<i32>) -> Option<i32> {
    edited.iter().copied().min_by_key(|p| ((p - target).abs(), *p))
  }

  /// Decide what a press means.
  ///
  /// The two triggers are mirror images, and both act on a NEIGHBOUR of the pressed
  /// cell rather than on the cell itself:
  /// - `edit_target` is the note directly ABOVE the pressed cell (press below a note
  ///   to toggle editing it);
  /// - `sustain_target` is the note directly BELOW it (press above a note to toggle
  ///   sustaining it).
  ///
  /// Either is `None` when that neighbour is off the play grid. `sounding` says
  /// whether a pitch is currently audible on this grid, by any reason at all --
  /// including a CHORD-layer voice (chord-storage-v2: the handles work on chord
  /// voices too). `sustained` says whether a pitch is in the sustain set
  /// specifically -- branch-3 queue item 6: a press must retrigger, never drag, when
  /// it lands on a pitch already claimed by any voice in the editing set or the
  /// sustain set. `edited` is the full edit SELECTION -- the piano layer's edited
  /// pitches (⊆ sustained by the branch-3 invariant) unioned with the edit-flagged
  /// chord voices' pitches (which are NOT sustained: Jeff's round-2 correction) --
  /// so it drives the exit check, the drag's nearest-note choice, and the half of
  /// the retrigger guard the sustain set no longer covers.
  ///
  /// Order matters, twice over:
  ///
  /// *Exit is checked first,* before anything looks at whether a note is audible.
  /// Anything that silences an edited note out from under us would otherwise strand
  /// it: still dancing, still forcing every press to drag, and un-dismissable because
  /// the gesture that dismisses it needed the sound that just went away. Keeping this
  /// first makes "no inescapable state" true by construction.
  ///
  /// *Edit beats sustain* when a cell has sounding notes both above and below it --
  /// i.e. two notes one cell apart, straddling the press. Rare (Jeff: "there's rarely
  /// musical reason to play both"), but it must be decided rather than accidental.
  /// Edit wins because it is the gesture with an exit condition: a wrong sustain is
  /// undone by pressing again, a wrong edit-mode entry too, but only one of them can
  /// leave you unable to tell which note you are acting on.
  pub fn classify(
    &self,
    edit_target: Option<i32>,
    sustain_target: Option<i32>,
    pressed_pitch: i32,
    sounding: impl Fn(i32) -> bool,
    sustained: impl Fn(i32) -> bool,
    edited: &HashSet<i32>,
  ) -> Press {
    if let Some(above) = edit_target {
      if edited.contains(&above) {
        return Press::ExitEdit { pitch: above };
      }
      if sounding(above) {
        return Press::EnterEdit { pitch: above };
      }
    }
    if let Some(below) = sustain_target {
      if sounding(below) {
        return Press::ToggleSustain { pitch: below };
      }
    }
    // Striking a pitch that is ALREADY claimed -- edited, or merely sustained by some
    // other voice -- retriggers it rather than dragging: the ordinary note-on path
    // (keys.rs: `cut_sustained` + `note_on_with_phase`) cuts the old voice and the new
    // one replaces it, continuing its phase. Without this a press on a sustained-but-
    // unedited pitch would drag the nearest edited note ONTO it, silently collapsing
    // two voices into one (branch-3 queue item 6 -- the bug this guard fixes). Edit
    // mode is a property of the pitch, so it survives the retrigger either way.
    //
    // The sustain check alone covered an edited pressed pitch while edited ⊆
    // sustained was the whole story; an edit-flagged CHORD voice's pitch is in
    // `edited` without being sustained, hence the explicit union here.
    if sustained(pressed_pitch) || edited.contains(&pressed_pitch) {
      return Press::Play;
    }
    match self.nearest(pressed_pitch, edited) {
      Some(from) => Press::Drag { from, to: pressed_pitch },
      None => Press::Play,
    }
  }

  /// Start editing `pitch`. Editing IMPLIES sustaining (branch-3 queue item 4: the
  /// invariant edited ⊆ sustained), so this adds the pitch to BOTH sets. A note that was
  /// only fingered thereby becomes sustained -- its finger lift no longer ends it, it
  /// drones, exactly as before, but now via the sustain reason rather than via edit
  /// being its own life-support. This supersedes the old model where edit was a third
  /// reason to ring.
  pub fn enter(&self, pitch: i32, store: &mut RingStore) {
    store.add(Reason::Sustain, pitch);
    store.add(Reason::Edit, pitch);
  }

  /// Stop editing `pitch` -- pure DESELECTION. It keeps ringing: it is still sustained
  /// (edited ⊆ sustained), so leaving edit mode ends no voice. (Contrast removing
  /// sustain, `RingStore::remove_sustain`, which cascades the edit removal and can end
  /// the voice.)
  pub fn exit(&self, pitch: i32, store: &mut RingStore) {
    store.discard(Reason::Edit, pitch);
  }

  /// Follow a note that moved: edit mode belongs to the pitch, so dragging a note
  /// from one pitch to another carries its edit mode along. Without this the note
  /// would arrive un-edited and its dance would stop after one drag. (Re-files only
  /// the EDIT reason; the store's `note_moved` re-files both sets when a drag lands.)
  pub fn moved(&self, from: i32, to: i32, store: &mut RingStore) {
    if store.discard(Reason::Edit, from) {
      store.add(Reason::Edit, to);
    }
  }

  /// Drop every pitch from the edit SELECTION -- the vision's "a button somewhere to
  /// clear edit mode from all notes", now the editmode clear (`editmode_clear`: a
  /// softstep pedal and/or an on-grid button). Pure deselection: every cleared note is
  /// still sustained (edited ⊆ sustained), so this ends NO voice -- the editmode clear
  /// silences nothing. (This supersedes the old model, where an edit-only drone ended
  /// here.)
  pub fn clear(&self, store: &mut RingStore) {
    for pitch in store.iter(Reason::Edit).collect::<Vec<_>>() {
      store.discard(Reason::Edit, pitch);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::surfaces_runtime::ring::RingStore;

  /// Every test's `sustained` predicate reads the same store the `sounding` closure
  /// and `store` argument already reference -- the real caller (keys.rs) snapshots
  /// `Reason::Sustain` into a `HashSet` the same way. A few tests instead need a
  /// pitch that is sustained WITHOUT being edited, so they call `store.add(Reason::
  /// Sustain, ..)` directly rather than going through `enter` (which always adds
  /// both reasons).
  fn is_sustained(store: &RingStore) -> impl Fn(i32) -> bool + '_ {
    |p| store.has(Reason::Sustain, p)
  }

  /// The edit SELECTION as the real caller (keys.rs) builds it: the store's edited
  /// pitches -- unioned, in chord tests, with edit-flagged chord voices' pitches
  /// (none here; the chord-layer union has its own tests).
  fn edited_of(store: &RingStore) -> HashSet<i32> {
    store.iter(Reason::Edit).collect()
  }

  // ---- the chord layer in the selection (chord-storage-v2) ----
  //
  // These pass the union set directly, as keys.rs builds it: a chord voice's pitch
  // can be SOUNDING without any store membership, and EDITED (flagged) without
  // being sustained.

  /// The handles work on chord voices: a press under a pitch only a chord voice
  /// sounds enters edit mode for it.
  #[test]
  fn pressing_under_a_chord_only_pitch_enters_edit() {
    let e = EditState::new();
    let chord_sounds = |p: i32| p == 20;
    assert_eq!(
      e.classify(Some(20), None, 21, chord_sounds, |_| false, &HashSet::new()),
      Press::EnterEdit { pitch: 20 },
    );
  }

  /// ...and once flagged (in the union, though never sustained), the same press
  /// exits -- the exit check runs on the UNION, or a chord-edited note would be
  /// un-dismissable.
  #[test]
  fn the_exit_check_sees_a_chord_edited_pitch() {
    let e = EditState::new();
    let edited: HashSet<i32> = [20].into_iter().collect();
    assert_eq!(
      e.classify(Some(20), None, 21, |p| p == 20, |_| false, &edited),
      Press::ExitEdit { pitch: 20 },
    );
  }

  /// A chord-only edited pitch drives the pitch-picker: with the store empty, a
  /// press on a free pitch still drags the nearest (chord) edited note.
  #[test]
  fn a_chord_only_selection_still_drags() {
    let e = EditState::new();
    let edited: HashSet<i32> = [20].into_iter().collect();
    assert_eq!(
      e.classify(None, None, 28, |p| p == 20, |_| false, &edited),
      Press::Drag { from: 20, to: 28 },
    );
  }

  /// The retrigger guard's union half: a press ON a chord-edited pitch (which is
  /// NOT sustained -- the invariant is the piano layer's) plays/retriggers rather
  /// than dragging another note onto it.
  #[test]
  fn pressing_a_chord_edited_pitch_plays_instead_of_dragging() {
    let e = EditState::new();
    let edited: HashSet<i32> = [20, 40].into_iter().collect();
    assert_eq!(
      e.classify(None, None, 20, |p| p == 20 || p == 40, |_| false, &edited),
      Press::Play,
    );
  }

  /// Nothing sounding, nothing edited: the grid plays. The edited set lives in the
  /// shared store now (cleaning phase 6), so each test builds one and threads it
  /// through -- the assertions are unchanged.
  #[test]
  fn a_press_with_nothing_edited_is_an_ordinary_note() {
    let e = EditState::new();
    let store = RingStore::new();
    assert_eq!(e.classify(Some(20), None, 21, |_| false, is_sustained(&store), &edited_of(&store)), Press::Play);
  }

  /// The trigger only fires under a SOUNDING note; otherwise that cell is just a note.
  #[test]
  fn the_cell_under_a_silent_note_still_plays() {
    let e = EditState::new();
    let store = RingStore::new();
    assert_eq!(e.classify(Some(20), None, 21, |p| p == 99, is_sustained(&store), &edited_of(&store)), Press::Play);
  }

  #[test]
  fn pressing_under_a_sounding_note_enters_edit_and_pressing_again_exits() {
    let e = EditState::new();
    let mut store = RingStore::new();
    assert_eq!(
      e.classify(Some(20), None, 21, |p| p == 20, is_sustained(&store), &edited_of(&store)),
      Press::EnterEdit { pitch: 20 },
    );
    e.enter(20, &mut store);
    assert_eq!(
      e.classify(Some(20), None, 21, |p| p == 20, is_sustained(&store), &edited_of(&store)),
      Press::ExitEdit { pitch: 20 },
    );
  }

  /// The whole point of 2b: the exit gesture and the drag gesture are the same press,
  /// and the trigger must win. Otherwise pressing under an edited note would drag it
  /// up by one step and there would be no way out of edit mode.
  #[test]
  fn the_exit_trigger_beats_the_drag_when_both_could_fire() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    // Cell 21's neighbour above is the edited, sounding note 20. Both rules apply.
    assert_eq!(
      e.classify(Some(20), None, 21, |p| p == 20, is_sustained(&store), &edited_of(&store)),
      Press::ExitEdit { pitch: 20 },
      "exit wins; the accepted cost is that you cannot drag a note onto its own exit cell",
    );
  }

  /// While anything is edited the grid is a pitch-picker: presses drag, they do not
  /// sound. This is the biggest playability change in the feature.
  #[test]
  fn while_editing_an_ordinary_press_drags_instead_of_playing() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    assert_eq!(
      e.classify(Some(50), None, 40, |p| p == 20, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 20, to: 40 },
    );
  }

  #[test]
  fn a_drag_moves_the_nearest_edited_note() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(10, &mut store);
    e.enter(30, &mut store);
    assert_eq!(
      e.classify(None, None, 28, |_| false, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 30, to: 28 },
    );
    assert_eq!(
      e.classify(None, None, 12, |_| false, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 10, to: 12 },
    );
  }

  /// Ties go to the LOWER note (2d), matching the looper's lowest-first rule.
  #[test]
  fn a_tie_drags_the_lower_note() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(10, &mut store);
    e.enter(20, &mut store);
    assert_eq!(
      e.classify(None, None, 15, |_| false, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 10, to: 15 },
    );
  }

  /// Edit mode belongs to the PITCH, so it travels with the note. Otherwise a dragged
  /// note would arrive un-edited and could only be moved once.
  #[test]
  fn edit_mode_follows_a_note_that_moves() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    e.moved(20, 35, &mut store);
    assert!(!e.is_editing(20, &store));
    assert!(e.is_editing(35, &store), "the note is still being edited at its new pitch");
    // ...and it can be dragged again from there.
    assert_eq!(
      e.classify(None, None, 40, |_| false, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 35, to: 40 },
    );
  }

  #[test]
  fn moving_a_note_that_is_not_edited_does_nothing() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    e.moved(99, 100, &mut store);
    assert!(e.is_editing(20, &store));
    assert!(!e.is_editing(100, &store));
  }

  /// Retriggering a sustaining note replaces its voice; edit mode is a property of
  /// the pitch, so it survives that (4h). Nothing here keys on a voice at all --
  /// this test pins the intent.
  #[test]
  fn edit_mode_survives_a_retrigger() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    // The voice is replaced; the pitch is unchanged.
    assert!(e.is_editing(20, &store));
    assert_eq!(
      e.classify(Some(20), None, 21, |p| p == 20, is_sustained(&store), &edited_of(&store)),
      Press::ExitEdit { pitch: 20 },
    );
  }

  /// A note on the bottom row has no cell below it, so it can never be edited. The
  /// caller passes `pitch_above: None` there; it must not become a drag target rule.
  #[test]
  fn a_press_with_no_cell_above_falls_through_to_play_or_drag() {
    let e = EditState::new();
    let mut store = RingStore::new();
    assert_eq!(e.classify(None, None, 21, |_| true, is_sustained(&store), &edited_of(&store)), Press::Play);
    e.enter(5, &mut store);
    assert_eq!(
      e.classify(None, None, 21, |_| true, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 5, to: 21 },
    );
  }

  #[test]
  fn multiple_notes_can_be_edited_at_once() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(10, &mut store);
    e.enter(20, &mut store);
    e.enter(30, &mut store);
    let mut got: Vec<i32> = e.pitches(&store).collect();
    got.sort();
    assert_eq!(got, [10, 20, 30]);
    assert!(e.any(&store));
    e.exit(20, &mut store);
    assert!(!e.is_editing(20, &store));
    assert!(e.is_editing(10, &store) && e.is_editing(30, &store));
    e.clear(&mut store);
    assert!(!e.any(&store));
  }

  // ---- edit implies sustain (branch-3 queue item 4) ----

  /// Entering edit mode on a note that was only FINGERED makes it sustained, so its
  /// finger lift no longer ends it -- it drones, as before, but now via the sustain
  /// reason. The invariant is edited ⊆ sustained.
  #[test]
  fn entering_edit_sustains_a_fingered_only_note() {
    let e = EditState::new();
    let mut store = RingStore::new();
    // Nothing sustained yet -- the note is fingered only (not in the store at all).
    assert!(!store.has(Reason::Sustain, 20));
    e.enter(20, &mut store);
    assert!(e.is_editing(20, &store), "it is being edited");
    assert!(store.has(Reason::Sustain, 20), "and now sustained: it will drone after the finger lifts");
  }

  /// Exiting edit is pure deselection: the note stays sustained, so it goes on droning.
  /// Leaving edit mode ends nothing.
  #[test]
  fn exiting_edit_leaves_the_note_sustained_and_droning() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    e.exit(20, &mut store);
    assert!(!e.is_editing(20, &store), "no longer selected");
    assert!(store.has(Reason::Sustain, 20), "still sustained -- exiting edit ended nothing");
  }

  /// The editmode clear is the same: every pitch leaves the selection, but each stays
  /// sustained, so the clear silences nothing.
  #[test]
  fn clearing_edit_mode_leaves_every_note_sustained() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(10, &mut store);
    e.enter(20, &mut store);
    e.clear(&mut store);
    assert!(!e.any(&store), "nothing selected");
    assert!(store.has(Reason::Sustain, 10) && store.has(Reason::Sustain, 20), "both still sustained");
  }

  // ---- the sustain trigger (press ABOVE a note), mirror of the edit one ----

  /// Jeff's idea: the cell below a note toggles editing it, so the cell above toggles
  /// sustaining it. Both act on a NEIGHBOUR, both are toggles, neither sounds.
  #[test]
  fn pressing_above_a_sounding_note_toggles_its_sustain() {
    let e = EditState::new();
    let store = RingStore::new();
    assert_eq!(
      e.classify(None, Some(20), 19, |p| p == 20, is_sustained(&store), &edited_of(&store)),
      Press::ToggleSustain { pitch: 20 },
    );
  }

  /// The trigger only fires on a note that is actually audible; otherwise that cell
  /// is just a cell.
  #[test]
  fn pressing_above_a_silent_note_is_an_ordinary_press() {
    let e = EditState::new();
    let store = RingStore::new();
    assert_eq!(e.classify(None, Some(20), 19, |_| false, is_sustained(&store), &edited_of(&store)), Press::Play);
  }

  /// A note on the bottom row has nothing below it, so nothing to sustain from there
  /// -- the same asymmetry the edit trigger has at the top row.
  #[test]
  fn a_press_with_no_cell_below_cannot_sustain() {
    let e = EditState::new();
    let store = RingStore::new();
    assert_eq!(e.classify(None, None, 19, |_| true, is_sustained(&store), &edited_of(&store)), Press::Play);
  }

  /// The ambiguity that has to be decided rather than accidental: a cell with sounding
  /// notes BOTH above and below it -- two notes one cell apart, straddling the press.
  /// Edit wins.
  #[test]
  fn edit_beats_sustain_when_a_cell_is_sandwiched_between_two_sounding_notes() {
    let e = EditState::new();
    let store = RingStore::new();
    assert_eq!(
      e.classify(Some(21), Some(19), 20, |_| true, is_sustained(&store), &edited_of(&store)),
      Press::EnterEdit { pitch: 21 },
      "the note above (edit) wins over the note below (sustain)",
    );
  }

  /// ...and exiting still outranks both, so the escape hatch is never shadowed.
  #[test]
  fn exit_beats_sustain_too() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(21, &mut store);
    assert_eq!(
      e.classify(Some(21), Some(19), 20, |_| true, is_sustained(&store), &edited_of(&store)),
      Press::ExitEdit { pitch: 21 },
    );
  }

  /// Sustain is a plain toggle with no state of its own here: the caller holds the
  /// sustained set and decides which way the toggle goes. classify only says "the
  /// user means this pitch".
  #[test]
  fn the_sustain_trigger_names_the_pitch_and_leaves_the_direction_to_the_caller() {
    let e = EditState::new();
    let store = RingStore::new();
    // Same answer whether or not it is already sustained -- `sounding` is true either
    // way, and the caller knows which.
    assert_eq!(
      e.classify(None, Some(20), 19, |p| p == 20, is_sustained(&store), &edited_of(&store)),
      Press::ToggleSustain { pitch: 20 },
    );
  }

  /// While something is edited the grid is a pitch-picker -- but the sustain trigger
  /// must still work, or you could not sustain a second note while editing a first.
  #[test]
  fn the_sustain_trigger_still_works_while_another_note_is_edited() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(50, &mut store);
    assert_eq!(
      e.classify(None, Some(20), 19, |p| p == 20 || p == 50, is_sustained(&store), &edited_of(&store)),
      Press::ToggleSustain { pitch: 20 },
      "sustain beats the drag fallback",
    );
  }

  // ---- no inescapable state ----

  /// Jeff's original repro (historical): "press two buttons, sustain them both, put one
  /// into edit mode, then clear both. Now I have a dancing ghost that won't go away and
  /// blocks all sound." Back then `clear` silenced the drones but knew nothing about
  /// edit mode, so the pitch was left edited with no voice -- still dancing, `any()` true
  /// so every press dragged, and the exit gesture required the note to be SOUNDING, which
  /// the clear had just made false.
  ///
  /// The branch-3 model closes that off at the source (edited ⊆ sustained; losing sustain
  /// cascades the edit removal, so nothing silent stays selected). But `classify` still
  /// checks exit FIRST, before it looks at audibility, so the escape hatch is guaranteed
  /// by construction even if some future path re-created a silent-edited pitch.
  #[test]
  fn a_silenced_note_can_still_be_dismissed_from_edit_mode() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store); // it was already accreted, as in Jeff's repro
    // ...clear happens: the note is silenced, nothing sounds any more.
    let silent = |_: i32| false;
    assert_eq!(
      e.classify(Some(20), None, 21, silent, is_sustained(&store), &edited_of(&store)),
      Press::ExitEdit { pitch: 20 },
      "a silent edited note must still be dismissable, or the grid is stuck forever",
    );
    e.exit(20, &mut store);
    assert!(!e.any(&store), "and dismissing it frees the grid");
  }

  /// The grid must be playable again the moment nothing is edited.
  #[test]
  fn dismissing_the_last_ghost_restores_ordinary_play() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    let silent = |_: i32| false;
    assert!(
      matches!(e.classify(None, None, 40, silent, is_sustained(&store), &edited_of(&store)), Press::Drag { .. }),
      "stuck while edited",
    );
    e.exit(20, &mut store);
    assert_eq!(
      e.classify(None, None, 40, silent, is_sustained(&store), &edited_of(&store)),
      Press::Play,
      "playable again",
    );
  }

  /// `clear` dismisses every ghost at once -- it is the panic button, so it must
  /// leave the grid playable rather than half-stuck.
  #[test]
  fn clearing_edit_mode_leaves_no_ghost_behind() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(10, &mut store);
    e.enter(20, &mut store);
    e.clear(&mut store);
    assert!(!e.any(&store));
    assert_eq!(e.classify(None, None, 40, |_| false, is_sustained(&store), &edited_of(&store)), Press::Play);
  }

  /// The general guarantee, not just Jeff's path: whatever silenced it and however it
  /// got there, an edited pitch is always dismissable by the cell below it.
  #[test]
  fn every_edited_pitch_is_dismissable_regardless_of_what_is_sounding() {
    for sounds in [true, false] {
      let e = EditState::new();
      let mut store = RingStore::new();
      e.enter(20, &mut store);
      assert_eq!(
        e.classify(Some(20), None, 21, |p| sounds && p == 20, is_sustained(&store), &edited_of(&store)),
        Press::ExitEdit { pitch: 20 },
        "sounding={sounds}: must be dismissable",
      );
    }
  }

  /// ...but a note that is merely SILENT and not edited is still just a note: the
  /// trigger must not fire on nothing.
  #[test]
  fn the_cell_under_a_silent_unedited_note_is_still_an_ordinary_press() {
    let e = EditState::new();
    let store = RingStore::new();
    assert_eq!(e.classify(Some(20), None, 21, |_| false, is_sustained(&store), &edited_of(&store)), Press::Play);
  }

  // ---- retriggering an edited note ----

  /// Jeff: "if it's in edit mode and I hit the pitch again, it should retrigger,
  /// cutting off the old one ... just like sustained notes already do."
  ///
  /// It used to be a drag onto the note's own pitch -- a no-op -- so an edited drone
  /// could not be re-articulated at all.
  #[test]
  fn striking_an_edited_notes_own_pitch_retriggers_it() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    assert_eq!(
      e.classify(None, None, 20, |p| p == 20, is_sustained(&store), &edited_of(&store)),
      Press::Play,
      "the ordinary note-on path cuts the old voice and replaces it",
    );
  }

  /// Edit mode is a property of the PITCH, so it survives the retrigger -- the note
  /// keeps dancing and stays draggable.
  #[test]
  fn a_retriggered_note_is_still_edited() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    e.classify(None, None, 20, |p| p == 20, is_sustained(&store), &edited_of(&store));
    assert!(e.is_editing(20, &store), "retriggering must not end the edit");
  }

  /// Retrigger is exact: a NEIGHBOURING pitch still drags, or you could never move a
  /// note by one step.
  #[test]
  fn striking_next_to_an_edited_note_still_drags_it() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    assert_eq!(
      e.classify(None, None, 21, |_| false, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 20, to: 21 },
    );
    assert_eq!(
      e.classify(None, None, 19, |_| false, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 20, to: 19 },
    );
  }

  /// With several edited notes, striking one of them retriggers THAT one rather than
  /// dragging the nearest -- the pressed pitch is unambiguous.
  #[test]
  fn striking_one_of_several_edited_notes_retriggers_rather_than_drags() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(10, &mut store);
    e.enter(20, &mut store);
    assert_eq!(e.classify(None, None, 20, |_| true, is_sustained(&store), &edited_of(&store)), Press::Play);
  }

  /// The triggers still outrank a retrigger: the cell below an edited note is its
  /// exit, even if that cell's own pitch happens to be edited too.
  #[test]
  fn the_exit_trigger_still_beats_a_retrigger() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store); // the note above the pressed cell
    e.enter(21, &mut store); // and the pressed cell's own pitch
    assert_eq!(
      e.classify(Some(20), None, 21, |_| true, is_sustained(&store), &edited_of(&store)),
      Press::ExitEdit { pitch: 20 },
      "dismissing must never be shadowed",
    );
  }

  // ---- branch-3 queue item 6: drag needs a genuinely FREE pitch ----

  /// Jeff's bug report: with two (or more) notes edited, pressing the pitch of one
  /// of them retriggers it -- pinned above. But pressing a pitch that belongs to some
  /// OTHER voice that is merely SUSTAINED (not edited) used to fall through to the
  /// drag fallback, dragging the nearest EDITED note onto it and silently collapsing
  /// two voices into one. It must retrigger that sustained voice instead, exactly like
  /// an edited pitch does -- `sustained` covers it without even needing `is_editing`,
  /// because `store.add(Reason::Sustain, ..)` here (not `enter`) makes 50 sustained
  /// but deliberately NOT edited.
  #[test]
  fn striking_a_sustained_but_unedited_pitch_retriggers_rather_than_drags() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store); // edited (and, per the invariant, therefore sustained)
    store.add(Reason::Sustain, 50); // sustained by some OTHER means -- NOT edited
    assert!(!e.is_editing(50, &store), "sanity: 50 is sustained but not edited");
    assert_eq!(
      e.classify(None, None, 50, |p| p == 20 || p == 50, is_sustained(&store), &edited_of(&store)),
      Press::Play,
      "a sustained pitch retriggers instead of dragging the edited note onto it",
    );
  }

  /// The counterpart: a pitch that is free of BOTH sets -- claimed by no voice in the
  /// editing set or the sustain set -- still drags the nearest edited note. The guard
  /// above must not swallow the ordinary drag path.
  #[test]
  fn striking_a_genuinely_free_pitch_still_drags_the_nearest_edited_note() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store);
    assert!(!store.has(Reason::Sustain, 45), "sanity: 45 is free of both sets");
    assert_eq!(
      e.classify(None, None, 45, |_| false, is_sustained(&store), &edited_of(&store)),
      Press::Drag { from: 20, to: 45 },
    );
  }

  /// The trigger arms still outrank the broadened retrigger check too: the cell below
  /// an edited note is its exit, even when the pressed cell's own pitch happens to be
  /// sustained (not edited) rather than free.
  #[test]
  fn the_exit_trigger_still_beats_a_retrigger_on_a_sustained_unedited_pitch() {
    let e = EditState::new();
    let mut store = RingStore::new();
    e.enter(20, &mut store); // the note above the pressed cell
    store.add(Reason::Sustain, 21); // the pressed cell's own pitch: sustained, not edited
    assert_eq!(
      e.classify(Some(20), None, 21, |_| true, is_sustained(&store), &edited_of(&store)),
      Press::ExitEdit { pitch: 20 },
      "dismissing must never be shadowed, even by a sustained-unedited pressed pitch",
    );
  }
}
