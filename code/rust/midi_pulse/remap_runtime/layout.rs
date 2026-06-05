use super::config::RemapConfig;
use super::record::RecordControl;
use super::{MAP_W, PREIMAGE_ROW_Y};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowId {
  PreimageRow,
  Undo,
  Edo,
  RecordControl(RecordControl),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GridRect {
  pub(crate) x0: i32,
  pub(crate) y0: i32,
  pub(crate) x1: i32,
  pub(crate) y1: i32,
}

pub(crate) fn grid_step(config: &RemapConfig, x: i32, y: i32) -> i16 {
  ((config.x_step as i32 * x + config.y_step as i32 * y).rem_euclid(config.edo as i32)) as i16
}

pub(crate) fn map_rect(config: &RemapConfig) -> GridRect {
  let w = MAP_W.min(config.grid_w).max(0);
  let y0 = PREIMAGE_ROW_Y + 1;
  GridRect {
    x0: 0,
    y0,
    x1: w,
    y1: config.grid_h.max(y0),
  }
}

pub(crate) fn edo_local_cell(config: &RemapConfig, x: i32, y: i32) -> Option<(i32, i32)> {
  let rect = map_rect(config);
  if x >= rect.x0 && x < rect.x1 && y >= rect.y0 && y < rect.y1 {
    Some((x - rect.x0, y - rect.y0))
  } else {
    None
  }
}

pub(crate) fn undo_cell(config: &RemapConfig) -> Option<(i32, i32)> {
  if config.grid_w <= 0 || config.grid_h <= 0 {
    None
  } else {
    Some((config.grid_w - 1, config.grid_h - 1))
  }
}

// Cells come from the TOML config (see `[[monome_windows]] kind =
// "record_control"`) and are clamped to the discovered device size at use
// time.
pub(crate) fn record_control_cells(config: &RemapConfig) -> Vec<((i32, i32), RecordControl)> {
  config
    .record_controls
    .iter()
    .filter(|(_, (x, y))| *x >= 0 && *x < config.grid_w && *y >= 0 && *y < config.grid_h)
    .map(|(control, cell)| (*cell, *control))
    .collect()
}
