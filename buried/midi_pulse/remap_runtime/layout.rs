use super::rig::RemapRig;
use super::record::RecordControl;
use super::scale::ScaleControl;
use super::{MAP_W, PREIMAGE_ROW_Y};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowId {
  PreimageRow,
  Undo,
  Edo,
  RecordControl(RecordControl),
  ScaleSlots,
  ScaleControl(ScaleControl),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GridRect {
  pub(crate) x0: i32,
  pub(crate) y0: i32,
  pub(crate) x1: i32,
  pub(crate) y1: i32,
}

pub(crate) fn grid_step(rig: &RemapRig, x: i32, y: i32) -> i16 {
  ((rig.x_step as i32 * x + rig.y_step as i32 * y).rem_euclid(rig.edo as i32)) as i16
}

pub(crate) fn map_rect(rig: &RemapRig) -> GridRect {
  let w = MAP_W.min(rig.grid_w).max(0);
  let y0 = PREIMAGE_ROW_Y + 1;
  GridRect {
    x0: 0,
    y0,
    x1: w,
    y1: rig.grid_h.max(y0),
  }
}

pub(crate) fn edo_local_cell(rig: &RemapRig, x: i32, y: i32) -> Option<(i32, i32)> {
  let rect = map_rect(rig);
  if x >= rect.x0 && x < rect.x1 && y >= rect.y0 && y < rect.y1 {
    Some((x - rect.x0, y - rect.y0))
  } else {
    None
  }
}

pub(crate) fn undo_cell(rig: &RemapRig) -> Option<(i32, i32)> {
  if rig.grid_w <= 0 || rig.grid_h <= 0 {
    None
  } else {
    Some((rig.grid_w - 1, rig.grid_h - 1))
  }
}

// Cells come from the TOML rig (see `[[monome_windows]] kind =
// "record_control"`) and are clamped to the discovered device size at use
// time.
pub(crate) fn record_control_cells(rig: &RemapRig) -> Vec<((i32, i32), RecordControl)> {
  rig
    .record_controls
    .iter()
    .filter(|(_, (x, y))| *x >= 0 && *x < rig.grid_w && *y >= 0 && *y < rig.grid_h)
    .map(|(control, cell)| (*cell, *control))
    .collect()
}

// The scale-slot cells in rig (row-major) order. Returned in full so a
// slot's index is stable regardless of device size; callers guard the device
// bounds when rendering and dispatching.
pub(crate) fn scale_slot_cells(rig: &RemapRig) -> Vec<(i32, i32)> {
  let Some([x0, y0, x1, y1]) = rig.scale_slots else {
    return vec![];
  };
  let mut cells = Vec::new();
  for y in y0..=y1 {
    for x in x0..=x1 {
      cells.push((x, y));
    }
  }
  cells
}

// The store/empty arm-button cells from the TOML, clamped to the device size.
pub(crate) fn scale_control_cells(rig: &RemapRig) -> Vec<((i32, i32), ScaleControl)> {
  rig
    .scale_controls
    .iter()
    .filter(|(_, (x, y))| *x >= 0 && *x < rig.grid_w && *y >= 0 && *y < rig.grid_h)
    .map(|(control, cell)| (*cell, *control))
    .collect()
}

// The bounding rect of the scale-slot grid, if any, as ((x0, y0), (x1, y1)).
pub(crate) fn scale_slots_rect(rig: &RemapRig) -> Option<((i32, i32), (i32, i32))> {
  rig.scale_slots.map(|[x0, y0, x1, y1]| ((x0, y0), (x1, y1)))
}
