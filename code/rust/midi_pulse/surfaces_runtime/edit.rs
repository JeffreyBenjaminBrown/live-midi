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
//! a pulse change (`polyrhythm`) applies to ALL of them at once. So "in edit mode"
//! denotes a different selection depending on which parameter you touch.
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
  /// Drag the edited note `from` to `to`, gliding. The pressed cell does not sound.
  Drag { from: i32, to: i32 },
  /// Something is being edited but nothing can move (only possible if the edit set is
  /// empty, which `Drag` already covers) -- or the press is a trigger for a note that
  /// is not sounding. Silent no-op.
  Ignore,
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

  /// Decide what a press means. `pitch_above` is the pitch of the cell directly above
  /// the pressed one (`None` if that cell is off-grid), `pressed_pitch` the pressed
  /// cell's own pitch, and `sounding` says whether a pitch is currently audible on
  /// this grid (fingered or sustained -- both count; an edited note keeps sounding).
  ///
  /// Order matters. The trigger is checked FIRST: pressing the cell under a sounding
  /// note always means "edit that note", never "drag to here". Otherwise the exit
  /// gesture and the drag gesture would both fire on the same press, since the cell
  /// under an edited note is also a perfectly good drag target (`2_discussion` 2b).
  /// The cost: you can never drag an edited note down onto the cell that exits it.
  pub fn classify(
    &self,
    pitch_above: Option<i32>,
    pressed_pitch: i32,
    sounding: impl Fn(i32) -> bool,
  ) -> Press {
    if let Some(above) = pitch_above {
      if sounding(above) {
        return if self.is_editing(above) {
          Press::ExitEdit { pitch: above }
        } else {
          Press::EnterEdit { pitch: above }
        };
      }
    }
    match self.nearest(pressed_pitch) {
      Some(from) => Press::Drag { from, to: pressed_pitch },
      None => Press::Play,
    }
  }

  pub fn enter(&mut self, pitch: i32) {
    self.pitches.insert(pitch);
  }

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
    assert_eq!(e.classify(Some(20), 21, |_| false), Press::Play);
  }

  /// The trigger only fires under a SOUNDING note; otherwise that cell is just a note.
  #[test]
  fn the_cell_under_a_silent_note_still_plays() {
    let e = EditState::new();
    assert_eq!(e.classify(Some(20), 21, |p| p == 99), Press::Play);
  }

  #[test]
  fn pressing_under_a_sounding_note_enters_edit_and_pressing_again_exits() {
    let mut e = EditState::new();
    assert_eq!(e.classify(Some(20), 21, |p| p == 20), Press::EnterEdit { pitch: 20 });
    e.enter(20);
    assert_eq!(e.classify(Some(20), 21, |p| p == 20), Press::ExitEdit { pitch: 20 });
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
      e.classify(Some(20), 21, |p| p == 20),
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
    assert_eq!(e.classify(Some(50), 40, |p| p == 20), Press::Drag { from: 20, to: 40 });
  }

  #[test]
  fn a_drag_moves_the_nearest_edited_note() {
    let mut e = EditState::new();
    e.enter(10);
    e.enter(30);
    assert_eq!(e.classify(None, 28, |_| false), Press::Drag { from: 30, to: 28 });
    assert_eq!(e.classify(None, 12, |_| false), Press::Drag { from: 10, to: 12 });
  }

  /// Ties go to the LOWER note (2d), matching the looper's lowest-first rule.
  #[test]
  fn a_tie_drags_the_lower_note() {
    let mut e = EditState::new();
    e.enter(10);
    e.enter(20);
    assert_eq!(e.classify(None, 15, |_| false), Press::Drag { from: 10, to: 15 });
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
    assert_eq!(e.classify(None, 40, |_| false), Press::Drag { from: 35, to: 40 });
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
    assert_eq!(e.classify(Some(20), 21, |p| p == 20), Press::ExitEdit { pitch: 20 });
  }

  /// A note on the bottom row has no cell below it, so it can never be edited. The
  /// caller passes `pitch_above: None` there; it must not become a drag target rule.
  #[test]
  fn a_press_with_no_cell_above_falls_through_to_play_or_drag() {
    let mut e = EditState::new();
    assert_eq!(e.classify(None, 21, |_| true), Press::Play);
    e.enter(5);
    assert_eq!(e.classify(None, 21, |_| true), Press::Drag { from: 5, to: 21 });
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
}
