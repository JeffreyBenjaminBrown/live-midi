use midi_pulse::monome_window;

use crate::config::EdoConfig;
use crate::{MAP_W, PREIMAGE_ROW_Y};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowId {
  Undo,
  Edo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GridRect {
  pub(crate) x0: i32,
  pub(crate) y0: i32,
  pub(crate) x1: i32,
  pub(crate) y1: i32,
}

pub(crate) fn grid_step(config: &EdoConfig, x: i32, y: i32) -> i16 {
  ((config.x_step as i32 * x + config.y_step as i32 * y).rem_euclid(config.edo as i32)) as i16
}

pub(crate) fn map_rect(config: &EdoConfig) -> GridRect {
  let w = MAP_W.min(config.grid_w).max(0);
  let y0 = PREIMAGE_ROW_Y + 1;
  GridRect {
    x0: 0,
    y0,
    x1: w,
    y1: config.grid_h.max(y0),
  }
}

pub(crate) fn edo_local_cell(config: &EdoConfig, x: i32, y: i32) -> Option<(i32, i32)> {
  if window_for_cell(config, x, y) != Some(WindowId::Edo) {
    return None;
  }
  let rect = map_rect(config);
  if x >= rect.x0 && x < rect.x1 && y >= rect.y0 && y < rect.y1 {
    Some((x - rect.x0, y - rect.y0))
  } else {
    None
  }
}

pub(crate) fn window_for_cell(config: &EdoConfig, x: i32, y: i32) -> Option<WindowId> {
  monome_window::window_for_cell(&monome_windows(config), (x, y))
}

pub(crate) fn monome_windows(config: &EdoConfig) -> Vec<monome_window::Window<WindowId>> {
  let mut windows = vec![];
  if let Some(cell) = undo_cell(config) {
    windows.push(monome_window::Window {
      id: WindowId::Undo,
      rect: (cell, cell),
    });
  }
  let rect = map_rect(config);
  if rect.x0 < rect.x1 && rect.y0 < rect.y1 {
    windows.push(monome_window::Window {
      id: WindowId::Edo,
      rect: ((rect.x0, rect.y0), (rect.x1 - 1, rect.y1 - 1)),
    });
  }
  windows
}

pub(crate) fn undo_cell(config: &EdoConfig) -> Option<(i32, i32)> {
  if config.grid_w <= 0 || config.grid_h <= 0 {
    None
  } else {
    Some((config.grid_w - 1, config.grid_h - 1))
  }
}
