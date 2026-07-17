use super::remap::apply_snapshot;
use super::state::{RemapSnapshot, RemappableEdoState};

/// The two arm buttons of the scale-saving feature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScaleControl {
  /// "The next slot pressed is written with the current scale."
  Store,
  /// "The next slot pressed is emptied."
  Empty,
}

/// Runtime state for the scale-saving feature: the saved scales and which arm
/// button (if any) is currently armed. The number of slots is fixed by the
/// rig's `scale_slots` window.
#[derive(Clone, Default)]
pub(crate) struct ScaleSlotsRuntime {
  /// One entry per slot cell, in the rig's row-major order; `None` is empty.
  pub(crate) slots: Vec<Option<RemapSnapshot>>,
  /// The armed arm button, if any.
  pub(crate) armed: Option<ScaleControl>,
  /// The slot most recently recalled, rendered solid so the user remembers
  /// where they last jumped -- even if the live map has since been edited away
  /// from it. Set only by recall; cleared if that slot is emptied. Storing
  /// never changes it.
  pub(crate) active: Option<usize>,
}

impl ScaleSlotsRuntime {
  pub(crate) fn new(slot_count: usize) -> Self {
    ScaleSlotsRuntime {
      slots: vec![None; slot_count],
      armed: None,
      active: None,
    }
  }
}

/// Toggle an arm button. Arming one disarms the other, and pressing the armed
/// button again disarms it.
pub(crate) fn toggle_arm(state: &mut RemappableEdoState, control: ScaleControl) -> bool {
  state.scale.armed = if state.scale.armed == Some(control) {
    None
  } else {
    Some(control)
  };
  true
}

/// Handle a press on the slot at `index`. With "store" armed, the current scale
/// is written there; with "empty" armed, the slot is cleared; otherwise a
/// filled slot's scale is imposed (and an empty slot does nothing).
pub(crate) fn press_slot(state: &mut RemappableEdoState, index: usize) -> bool {
  if index >= state.scale.slots.len() {
    return false;
  }
  match state.scale.armed {
    Some(ScaleControl::Store) => {
      // Storing writes the slot but never highlights it: a freshly filled
      // (previously empty) slot becomes a dim written slot, and storing over an
      // already-filled slot leaves its LED state untouched.
      let snapshot = state.snapshot();
      state.scale.slots[index] = Some(snapshot);
      state.scale.armed = None;
      true
    }
    Some(ScaleControl::Empty) => {
      state.scale.slots[index] = None;
      if state.scale.active == Some(index) {
        state.scale.active = None;
      }
      state.scale.armed = None;
      true
    }
    None => recall_slot(state, index),
  }
}

/// Recall a saved scale: highlight the slot solid (it is now where the user
/// "is") and, when the live map actually changes, impose it as a single
/// undoable move -- the whole 12-value map replaced at once, with the prior
/// state pushed onto the undo history so the undo button reverts it entirely.
fn recall_slot(state: &mut RemappableEdoState, index: usize) -> bool {
  let Some(snapshot) = state.scale.slots[index] else {
    return false; // empty slot: nothing to recall
  };
  state.scale.active = Some(index);
  if snapshot.map != state.map {
    let before = state.snapshot();
    apply_snapshot(state, snapshot);
    state.history.push(before);
  }
  true
}

/// Whether slot `index` is the one most recently recalled. Rendering lights it
/// solid; the highlight persists even after the live map is edited away from it.
pub(crate) fn slot_is_active(state: &RemappableEdoState, index: usize) -> bool {
  state.scale.active == Some(index)
}

#[cfg(test)]
mod tests {
  use super::*;
  use super::super::rig::{RemapRig, RemapIdiom};
  use super::super::remap::undo_remap;

  // A 16x16 state whose scale_slots window covers a 4x4 grid (16 slots).
  fn state_with_slots() -> RemappableEdoState {
    let rig = RemapRig::new(80.0, 12, 1, 0, RemapIdiom::Snap, 16, 16)
      .with_scale_slots(Some([12, 0, 15, 3]))
      .with_scale_controls(vec![
        (ScaleControl::Store, (15, 4)),
        (ScaleControl::Empty, (14, 4)),
      ]);
    RemappableEdoState::new(rig)
  }

  #[test]
  fn slot_count_follows_the_rect() {
    let state = state_with_slots();
    assert_eq!(state.scale.slots.len(), 16);
  }

  #[test]
  fn arming_is_mutually_exclusive_and_toggles_off() {
    let mut state = state_with_slots();
    toggle_arm(&mut state, ScaleControl::Store);
    assert_eq!(state.scale.armed, Some(ScaleControl::Store));
    // Arming the sibling interrupts the first.
    toggle_arm(&mut state, ScaleControl::Empty);
    assert_eq!(state.scale.armed, Some(ScaleControl::Empty));
    // Pressing the armed button again disarms it.
    toggle_arm(&mut state, ScaleControl::Empty);
    assert_eq!(state.scale.armed, None);
  }

  #[test]
  fn store_then_recall_imposes_saved_scale_as_one_undo_move() {
    let mut state = state_with_slots();
    let saved_map = state.map;

    // Storing writes the slot and disarms, but does NOT highlight it.
    toggle_arm(&mut state, ScaleControl::Store);
    assert!(press_slot(&mut state, 0));
    assert_eq!(state.scale.armed, None);
    assert!(state.scale.slots[0].is_some());
    assert!(!slot_is_active(&state, 0));

    // Diverge the live map.
    state.map[0] = (saved_map[0] + 3).rem_euclid(12);

    // Recalling slot 0 restores the whole map as a single undo move and
    // highlights the slot.
    assert!(press_slot(&mut state, 0));
    assert_eq!(state.map, saved_map);
    assert_eq!(state.history.len(), 1);
    assert!(slot_is_active(&state, 0));

    // Undo reverts the imposition; the highlight persists so the user still
    // sees where they last recalled, even though the map has wandered.
    assert!(undo_remap(&mut state));
    assert_eq!(state.map[0], (saved_map[0] + 3).rem_euclid(12));
    assert!(slot_is_active(&state, 0));
  }

  #[test]
  fn recall_moves_the_highlight_and_emptying_the_active_slot_clears_it() {
    let mut state = state_with_slots();

    // Fill two slots with the (identical) current scale; storing never lights.
    toggle_arm(&mut state, ScaleControl::Store);
    press_slot(&mut state, 1);
    toggle_arm(&mut state, ScaleControl::Store);
    press_slot(&mut state, 2);
    assert!(!slot_is_active(&state, 1));
    assert!(!slot_is_active(&state, 2));

    // Recall highlights even when the map is unchanged, and moves on re-recall.
    assert!(press_slot(&mut state, 1));
    assert!(slot_is_active(&state, 1));
    assert!(press_slot(&mut state, 2));
    assert!(slot_is_active(&state, 2));
    assert!(!slot_is_active(&state, 1));

    // Emptying the active slot clears the highlight.
    toggle_arm(&mut state, ScaleControl::Empty);
    assert!(press_slot(&mut state, 2));
    assert!(!slot_is_active(&state, 2));
    assert!(state.scale.active.is_none());
  }

  #[test]
  fn empty_clears_a_slot_and_disarms() {
    let mut state = state_with_slots();
    toggle_arm(&mut state, ScaleControl::Store);
    press_slot(&mut state, 2);
    assert!(state.scale.slots[2].is_some());

    toggle_arm(&mut state, ScaleControl::Empty);
    assert!(press_slot(&mut state, 2));
    assert!(state.scale.slots[2].is_none());
    assert_eq!(state.scale.armed, None);
  }

  #[test]
  fn pressing_an_empty_slot_unarmed_does_nothing() {
    let mut state = state_with_slots();
    assert!(!press_slot(&mut state, 5));
    assert!(state.history.is_empty());
  }
}
