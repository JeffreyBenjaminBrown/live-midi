use super::rig::RemapIdiom;
use super::layout::{edo_local_cell, grid_step};
use super::state::{LooseState, RemapSnapshot, RemappableEdoState};

pub(crate) fn apply_grid_press(state: &mut RemappableEdoState, x: i32, y: i32) -> bool {
  let Some((local_x, local_y)) = edo_local_cell(&state.rig, x, y) else {
    return false;
  };
  let step = grid_step(&state.rig, local_x, local_y);
  let before = state.snapshot();
  let changed = match state.rig.remap_idiom {
    RemapIdiom::Loose => apply_loose_grid_press(state, step),
    RemapIdiom::Snap => apply_snap_grid_press(state, step),
  };
  if changed && state.snapshot() != before {
    state.history.push(before);
  }
  changed
}

fn apply_loose_grid_press(state: &mut RemappableEdoState, step: i16) -> bool {
  if let Some(preimage) = preimage_for_step(state, step) {
    state.loose[preimage] = LooseState::Loose;
    eprintln!("loose {} -> {step}", pitch_name(preimage));
    return true;
  }
  let Some(preimage) = loose_neighbor_for_dark_step(step, state) else {
    return false;
  };
  move_preimage_to_step(state, preimage, step)
}

fn apply_snap_grid_press(state: &mut RemappableEdoState, step: i16) -> bool {
  if preimage_for_step(state, step).is_some() {
    return false;
  }
  let (lower, higher) = nearest_light_neighbors(step, &state.map, state.rig.edo);
  let preimage = if lower.distance < higher.distance {
    lower.preimage
  } else {
    higher.preimage
  };
  move_preimage_to_step(state, preimage, step)
}

fn move_preimage_to_step(state: &mut RemappableEdoState, preimage: usize, step: i16) -> bool {
  let current = state.map[preimage];
  let Some(delta) = move_delta(current, step, &state.map, state.rig.edo) else {
    return false;
  };
  state.map[preimage] = step;
  state.deltas[preimage] += delta;
  state.loose[preimage] = LooseState::Fixed;
  eprintln!("moved {}: {current} -> {step}", pitch_name(preimage));
  true
}

pub(crate) fn undo_remap(state: &mut RemappableEdoState) -> bool {
  let Some(snapshot) = state.history.pop() else {
    return false;
  };
  state.map = snapshot.map;
  state.deltas = snapshot.deltas;
  state.loose = snapshot.loose;
  eprintln!("undo remap");
  true
}

pub(crate) fn apply_snapshot(state: &mut RemappableEdoState, snapshot: RemapSnapshot) {
  state.map = snapshot.map;
  state.deltas = snapshot.deltas;
  state.loose = snapshot.loose;
}

pub(crate) fn preimage_for_step(state: &RemappableEdoState, step: i16) -> Option<usize> {
  state.map.iter().position(|s| *s == step)
}

fn loose_neighbor_for_dark_step(step: i16, state: &RemappableEdoState) -> Option<usize> {
  let (lower, higher) = nearest_light_neighbors(step, &state.map, state.rig.edo);
  match (
    state.loose[lower.preimage] == LooseState::Loose,
    state.loose[higher.preimage] == LooseState::Loose,
  ) {
    (false, false) => None,
    (true, false) => Some(lower.preimage),
    (false, true) => Some(higher.preimage),
    (true, true) => {
      if lower.distance < higher.distance {
        Some(lower.preimage)
      } else {
        Some(higher.preimage)
      }
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Neighbor {
  preimage: usize,
  distance: i16,
}

fn nearest_light_neighbors(step: i16, lit_steps: &[i16; 12], edo: i16) -> (Neighbor, Neighbor) {
  let mut lower = Neighbor {
    preimage: 0,
    distance: edo,
  };
  let mut higher = Neighbor {
    preimage: 0,
    distance: edo,
  };
  for (preimage, lit) in lit_steps.iter().enumerate() {
    let lower_distance = (step - *lit).rem_euclid(edo);
    if lower_distance > 0 && lower_distance < lower.distance {
      lower = Neighbor {
        preimage,
        distance: lower_distance,
      };
    }
    let higher_distance = (*lit - step).rem_euclid(edo);
    if higher_distance > 0 && higher_distance < higher.distance {
      higher = Neighbor {
        preimage,
        distance: higher_distance,
      };
    }
  }
  (lower, higher)
}

pub(crate) fn move_delta(from: i16, to: i16, lit_steps: &[i16; 12], edo: i16) -> Option<i16> {
  if from == to {
    return Some(0);
  }
  let cw = (to - from).rem_euclid(edo);
  let ccw = (from - to).rem_euclid(edo);
  if cw < ccw {
    let blocked = lit_steps.iter().any(|step| {
      let d = (*step - from).rem_euclid(edo);
      d > 0 && d < cw
    });
    if blocked { None } else { Some(cw) }
  } else {
    let blocked = lit_steps.iter().any(|step| {
      let d = (from - *step).rem_euclid(edo);
      d > 0 && d < ccw
    });
    if blocked { None } else { Some(-ccw) }
  }
}

fn pitch_name(pc: usize) -> &'static str {
  [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
  ][pc]
}
