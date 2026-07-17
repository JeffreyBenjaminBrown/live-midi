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
//! press drags the nearest edited note instead. You leave edit mode to play again.
//!
//! *Asymmetry, deliberate.* A pitch drag moves the NEAREST edited note (one of them);
//! a factored-pulse change (`polyrhythm`) applies to ALL of them at once. So "in edit
//! mode" denotes a different selection depending on which parameter you touch.
//!
//! Edit mode is a property of the PITCH, not of a voice or a cell, so it survives
//! retriggering (`2_discussion` 4h) and it follows the note when the note moves.
//! Nothing here is octave-duplicated or mirrored across grids -- unlike every other
//! LED rule in this codebase, this one is local.

use std::collections::HashSet;

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

/// One grid's set of pitches currently in edit mode.
#[derive(Debug, Default)]
pub struct EditState {
  pitches: HashSet<i32>,
}

impl EditState {
  pub fn new() -> Self {
    EditState { pitches: HashSet::new() }
  }

  pub fn is_editing(&self, pitch: i32) -> bool {
    self.pitches.contains(&pitch)
  }

  pub fn any(&self) -> bool {
    !self.pitches.is_empty()
  }

  pub fn pitches(&self) -> impl Iterator<Item = i32> + '_ {
    self.pitches.iter().copied()
  }

  /// The edited pitch nearest to `target`. Ties go to the LOWER note (`2_discussion`
  /// 2d), matching the looper's lowest-first rule.
  pub fn nearest(&self, target: i32) -> Option<i32> {
    self.pitches.iter().copied().min_by_key(|p| ((p - target).abs(), *p))
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
  /// whether a pitch is currently audible on this grid, by any reason at all.
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
  ) -> Press {
    if let Some(above) = edit_target {
      if self.is_editing(above) {
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
    // Striking an edited note's OWN pitch retriggers it, exactly as striking a
    // sustained note's pitch does: the ordinary note-on path cuts the old voice and
    // the new one replaces it. Without this the press would be a drag onto the note's
    // current pitch -- a no-op -- so re-articulating an edited drone was impossible.
    // Edit mode is a property of the pitch, so it survives the retrigger.
    if self.is_editing(pressed_pitch) {
      return Press::Play;
    }
    match self.nearest(pressed_pitch) {
      Some(from) => Press::Drag { from, to: pressed_pitch },
      None => Press::Play,
    }
  }

  /// Start editing `pitch`. Being edited is itself a reason the note rings, so it
  /// keeps sounding once the finger lifts (`1_vision`) without this state having to
  /// reach into anyone else's bookkeeping.
  pub fn enter(&mut self, pitch: i32) {
    self.pitches.insert(pitch);
  }

  /// Stop editing `pitch`. Whether it then falls silent is the caller's to work out:
  /// a finger or a sustain is its own reason to keep ringing.
  pub fn exit(&mut self, pitch: i32) {
    self.pitches.remove(&pitch);
  }

  /// Follow a note that moved: edit mode belongs to the pitch, so dragging a note
  /// from one pitch to another carries its edit mode along. Without this the note
  /// would arrive un-edited and its dance would stop after one drag.
  pub fn moved(&mut self, from: i32, to: i32) {
    if self.pitches.remove(&from) {
      self.pitches.insert(to);
    }
  }

  /// Drop every pitch (the vision's "a button somewhere to clear edit mode from all
  /// notes", left unimplemented for now -- `1_vision` §undecided).
  /// Stop editing everything. The caller silences whatever that leaves with no
  /// reason to ring -- `clear` already releases this grid's voices anyway.
  pub fn clear(&mut self) {
    self.pitches.clear();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Nothing sounding, nothing edited: the grid plays.
  #[test]
  fn a_press_with_nothing_edited_is_an_ordinary_note() {
    let e = EditState::new();
    assert_eq!(e.classify(Some(20), None, 21, |_| false), Press::Play);
  }

  /// The trigger only fires under a SOUNDING note; otherwise that cell is just a note.
  #[test]
  fn the_cell_under_a_silent_note_still_plays() {
    let e = EditState::new();
    assert_eq!(e.classify(Some(20), None, 21, |p| p == 99), Press::Play);
  }

  #[test]
  fn pressing_under_a_sounding_note_enters_edit_and_pressing_again_exits() {
    let mut e = EditState::new();
    assert_eq!(e.classify(Some(20), None, 21, |p| p == 20), Press::EnterEdit { pitch: 20 });
    e.enter(20);
    assert_eq!(e.classify(Some(20), None, 21, |p| p == 20), Press::ExitEdit { pitch: 20 });
  }

  /// The whole point of 2b: the exit gesture and the drag gesture are the same press,
  /// and the trigger must win. Otherwise pressing under an edited note would drag it
  /// up by one step and there would be no way out of edit mode.
  #[test]
  fn the_exit_trigger_beats_the_drag_when_both_could_fire() {
    let mut e = EditState::new();
    e.enter(20);
    // Cell 21's neighbour above is the edited, sounding note 20. Both rules apply.
    assert_eq!(
      e.classify(Some(20), None, 21, |p| p == 20),
      Press::ExitEdit { pitch: 20 },
      "exit wins; the accepted cost is that you cannot drag a note onto its own exit cell",
    );
  }

  /// While anything is edited the grid is a pitch-picker: presses drag, they do not
  /// sound. This is the biggest playability change in the feature.
  #[test]
  fn while_editing_an_ordinary_press_drags_instead_of_playing() {
    let mut e = EditState::new();
    e.enter(20);
    assert_eq!(e.classify(Some(50), None, 40, |p| p == 20), Press::Drag { from: 20, to: 40 });
  }

  #[test]
  fn a_drag_moves_the_nearest_edited_note() {
    let mut e = EditState::new();
    e.enter(10);
    e.enter(30);
    assert_eq!(e.classify(None, None, 28, |_| false), Press::Drag { from: 30, to: 28 });
    assert_eq!(e.classify(None, None, 12, |_| false), Press::Drag { from: 10, to: 12 });
  }

  /// Ties go to the LOWER note (2d), matching the looper's lowest-first rule.
  #[test]
  fn a_tie_drags_the_lower_note() {
    let mut e = EditState::new();
    e.enter(10);
    e.enter(20);
    assert_eq!(e.classify(None, None, 15, |_| false), Press::Drag { from: 10, to: 15 });
  }

  /// Edit mode belongs to the PITCH, so it travels with the note. Otherwise a dragged
  /// note would arrive un-edited and could only be moved once.
  #[test]
  fn edit_mode_follows_a_note_that_moves() {
    let mut e = EditState::new();
    e.enter(20);
    e.moved(20, 35);
    assert!(!e.is_editing(20));
    assert!(e.is_editing(35), "the note is still being edited at its new pitch");
    // ...and it can be dragged again from there.
    assert_eq!(e.classify(None, None, 40, |_| false), Press::Drag { from: 35, to: 40 });
  }

  #[test]
  fn moving_a_note_that_is_not_edited_does_nothing() {
    let mut e = EditState::new();
    e.enter(20);
    e.moved(99, 100);
    assert!(e.is_editing(20));
    assert!(!e.is_editing(100));
  }

  /// Retriggering a sustaining note replaces its voice; edit mode is a property of
  /// the pitch, so it survives that (4h). Nothing here keys on a voice at all --
  /// this test pins the intent.
  #[test]
  fn edit_mode_survives_a_retrigger() {
    let mut e = EditState::new();
    e.enter(20);
    // The voice is replaced; the pitch is unchanged.
    assert!(e.is_editing(20));
    assert_eq!(e.classify(Some(20), None, 21, |p| p == 20), Press::ExitEdit { pitch: 20 });
  }

  /// A note on the bottom row has no cell below it, so it can never be edited. The
  /// caller passes `pitch_above: None` there; it must not become a drag target rule.
  #[test]
  fn a_press_with_no_cell_above_falls_through_to_play_or_drag() {
    let mut e = EditState::new();
    assert_eq!(e.classify(None, None, 21, |_| true), Press::Play);
    e.enter(5);
    assert_eq!(e.classify(None, None, 21, |_| true), Press::Drag { from: 5, to: 21 });
  }

  #[test]
  fn multiple_notes_can_be_edited_at_once() {
    let mut e = EditState::new();
    e.enter(10);
    e.enter(20);
    e.enter(30);
    let mut got: Vec<i32> = e.pitches().collect();
    got.sort();
    assert_eq!(got, [10, 20, 30]);
    assert!(e.any());
    e.exit(20);
    assert!(!e.is_editing(20));
    assert!(e.is_editing(10) && e.is_editing(30));
    e.clear();
    assert!(!e.any());
  }

  // ---- the sustain trigger (press ABOVE a note), mirror of the edit one ----

  /// Jeff's idea: the cell below a note toggles editing it, so the cell above toggles
  /// sustaining it. Both act on a NEIGHBOUR, both are toggles, neither sounds.
  #[test]
  fn pressing_above_a_sounding_note_toggles_its_sustain() {
    let e = EditState::new();
    assert_eq!(
      e.classify(None, Some(20), 19, |p| p == 20),
      Press::ToggleSustain { pitch: 20 },
    );
  }

  /// The trigger only fires on a note that is actually audible; otherwise that cell
  /// is just a cell.
  #[test]
  fn pressing_above_a_silent_note_is_an_ordinary_press() {
    let e = EditState::new();
    assert_eq!(e.classify(None, Some(20), 19, |_| false), Press::Play);
  }

  /// A note on the bottom row has nothing below it, so nothing to sustain from there
  /// -- the same asymmetry the edit trigger has at the top row.
  #[test]
  fn a_press_with_no_cell_below_cannot_sustain() {
    let e = EditState::new();
    assert_eq!(e.classify(None, None, 19, |_| true), Press::Play);
  }

  /// The ambiguity that has to be decided rather than accidental: a cell with sounding
  /// notes BOTH above and below it -- two notes one cell apart, straddling the press.
  /// Edit wins.
  #[test]
  fn edit_beats_sustain_when_a_cell_is_sandwiched_between_two_sounding_notes() {
    let e = EditState::new();
    assert_eq!(
      e.classify(Some(21), Some(19), 20, |_| true),
      Press::EnterEdit { pitch: 21 },
      "the note above (edit) wins over the note below (sustain)",
    );
  }

  /// ...and exiting still outranks both, so the escape hatch is never shadowed.
  #[test]
  fn exit_beats_sustain_too() {
    let mut e = EditState::new();
    e.enter(21);
    assert_eq!(
      e.classify(Some(21), Some(19), 20, |_| true),
      Press::ExitEdit { pitch: 21 },
    );
  }

  /// Sustain is a plain toggle with no state of its own here: the caller holds the
  /// sustained set and decides which way the toggle goes. classify only says "the
  /// user means this pitch".
  #[test]
  fn the_sustain_trigger_names_the_pitch_and_leaves_the_direction_to_the_caller() {
    let e = EditState::new();
    // Same answer whether or not it is already sustained -- `sounding` is true either
    // way, and the caller knows which.
    assert_eq!(
      e.classify(None, Some(20), 19, |p| p == 20),
      Press::ToggleSustain { pitch: 20 },
    );
  }

  /// While something is edited the grid is a pitch-picker -- but the sustain trigger
  /// must still work, or you could not sustain a second note while editing a first.
  #[test]
  fn the_sustain_trigger_still_works_while_another_note_is_edited() {
    let mut e = EditState::new();
    e.enter(50);
    assert_eq!(
      e.classify(None, Some(20), 19, |p| p == 20 || p == 50),
      Press::ToggleSustain { pitch: 20 },
      "sustain beats the drag fallback",
    );
  }

  // ---- no inescapable state ----

  /// Jeff's repro: "press two buttons, sustain them both, put one into edit mode,
  /// then clear both. Now I have a dancing ghost that won't go away and blocks all
  /// sound."
  ///
  /// `clear` silences the drones but knows nothing about edit mode, so the pitch was
  /// left edited with no voice. It kept dancing; `any()` stayed true so every press
  /// dragged instead of playing; and the exit gesture required the note to be
  /// SOUNDING -- which the clear had just made false. The one way out was the one
  /// thing the bug disabled.
  #[test]
  fn a_silenced_note_can_still_be_dismissed_from_edit_mode() {
    let mut e = EditState::new();
    e.enter(20); // it was already accreted, as in Jeff's repro
    // ...clear happens: the note is silenced, nothing sounds any more.
    let silent = |_: i32| false;
    assert_eq!(
      e.classify(Some(20), None, 21, silent),
      Press::ExitEdit { pitch: 20 },
      "a silent edited note must still be dismissable, or the grid is stuck forever",
    );
    e.exit(20);
    assert!(!e.any(), "and dismissing it frees the grid");
  }

  /// The grid must be playable again the moment nothing is edited.
  #[test]
  fn dismissing_the_last_ghost_restores_ordinary_play() {
    let mut e = EditState::new();
    e.enter(20);
    let silent = |_: i32| false;
    assert!(matches!(e.classify(None, None, 40, silent), Press::Drag { .. }), "stuck while edited");
    e.exit(20);
    assert_eq!(e.classify(None, None, 40, silent), Press::Play, "playable again");
  }

  /// `clear` dismisses every ghost at once -- it is the panic button, so it must
  /// leave the grid playable rather than half-stuck.
  #[test]
  fn clearing_edit_mode_leaves_no_ghost_behind() {
    let mut e = EditState::new();
    e.enter(10);
    e.enter(20);
    e.clear();
    assert!(!e.any());
    assert_eq!(e.classify(None, None, 40, |_| false), Press::Play);
  }

  /// The general guarantee, not just Jeff's path: whatever silenced it and however it
  /// got there, an edited pitch is always dismissable by the cell below it.
  #[test]
  fn every_edited_pitch_is_dismissable_regardless_of_what_is_sounding() {
    for sounds in [true, false] {
      let mut e = EditState::new();
      e.enter(20);
      assert_eq!(
        e.classify(Some(20), None, 21, |p| sounds && p == 20),
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
    assert_eq!(e.classify(Some(20), None, 21, |_| false), Press::Play);
  }

  // ---- retriggering an edited note ----

  /// Jeff: "if it's in edit mode and I hit the pitch again, it should retrigger,
  /// cutting off the old one ... just like sustained notes already do."
  ///
  /// It used to be a drag onto the note's own pitch -- a no-op -- so an edited drone
  /// could not be re-articulated at all.
  #[test]
  fn striking_an_edited_notes_own_pitch_retriggers_it() {
    let mut e = EditState::new();
    e.enter(20);
    assert_eq!(
      e.classify(None, None, 20, |p| p == 20),
      Press::Play,
      "the ordinary note-on path cuts the old voice and replaces it",
    );
  }

  /// Edit mode is a property of the PITCH, so it survives the retrigger -- the note
  /// keeps dancing and stays draggable.
  #[test]
  fn a_retriggered_note_is_still_edited() {
    let mut e = EditState::new();
    e.enter(20);
    e.classify(None, None, 20, |p| p == 20);
    assert!(e.is_editing(20), "retriggering must not end the edit");
  }

  /// Retrigger is exact: a NEIGHBOURING pitch still drags, or you could never move a
  /// note by one step.
  #[test]
  fn striking_next_to_an_edited_note_still_drags_it() {
    let mut e = EditState::new();
    e.enter(20);
    assert_eq!(e.classify(None, None, 21, |_| false), Press::Drag { from: 20, to: 21 });
    assert_eq!(e.classify(None, None, 19, |_| false), Press::Drag { from: 20, to: 19 });
  }

  /// With several edited notes, striking one of them retriggers THAT one rather than
  /// dragging the nearest -- the pressed pitch is unambiguous.
  #[test]
  fn striking_one_of_several_edited_notes_retriggers_rather_than_drags() {
    let mut e = EditState::new();
    e.enter(10);
    e.enter(20);
    assert_eq!(e.classify(None, None, 20, |_| true), Press::Play);
  }

  /// The triggers still outrank a retrigger: the cell below an edited note is its
  /// exit, even if that cell's own pitch happens to be edited too.
  #[test]
  fn the_exit_trigger_still_beats_a_retrigger() {
    let mut e = EditState::new();
    e.enter(20); // the note above the pressed cell
    e.enter(21); // and the pressed cell's own pitch
    assert_eq!(
      e.classify(Some(20), None, 21, |_| true),
      Press::ExitEdit { pitch: 20 },
      "dismissing must never be shadowed",
    );
  }
}
