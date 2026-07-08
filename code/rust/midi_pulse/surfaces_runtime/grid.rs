//! Pure per-grid presentation logic for the surfaces runtime: the waveform-selector
//! and volume-strip cell mappings, and the full-grid LED level vector. No I/O, so it is
//! unit-tested without OSC or a device.
//!
//! *Register-aware.* A held (or trailed) pitch's lit octave-equivalents are computed
//! from the *current* play register (via `step_for_cell`), so shifting the window
//! *moves* the lit cells under a held note -- matching the looper's `paint_classes`
//! (`0p5_also.org` item 5). Earlier surfaces builds painted a register-*independent*
//! map; that is deliberately reversed here.
//!
//! Three logical LED states per cell -- `OFF`, `DIM`, `BRIGHT`. On a varibright grid
//! these render as levels 0 / 4 / 15 directly; on a monobright grid the runtime fakes
//! `DIM` by flashing (see `mod.rs`), since a monobright grid thresholds any level <= 7
//! to off.

use std::collections::HashSet;

use midi_pulse::edo_play::{shift_for_cell, step_for_cell, Shift};

use crate::types::MonomeKey;

/// OSC LED levels (0..15). The three visible states the painter emits.
pub const OFF: i32 = 0;
pub const DIM: i32 = 4;
pub const BRIGHT: i32 = 15;

/// The number of cells in (and timbre slots behind) a selector strip.
pub const SELECTOR_CELLS: usize = 4;

/// An optional overlay rect: `[-1, -1, -1, -1]` means "not present" (the sentinel the
/// runtime uses for an absent overlay), which `in_rect` never matches.
pub type OverlayRect = [i32; 4];

/// True if `(x, y)` lies in the inclusive rect.
fn in_rect(rect: [i32; 4], x: i32, y: i32) -> bool {
  let [x0, y0, x1, y1] = rect;
  x >= x0 && x <= x1 && y >= y0 && y <= y1
}

/// Which timbre slot (0..4, left to right) a press on a selector cell selects, if
/// any. The slots are the config's `[[timbres]]` (or the plain sine / triangle /
/// square / saw when absent).
pub fn slot_for_selector_cell(rect: [i32; 4], cell: MonomeKey) -> Option<usize> {
  if !in_rect(rect, cell.0, cell.1) {
    return None;
  }
  let slot = (cell.0 - rect[0]) as usize;
  (slot < SELECTOR_CELLS).then_some(slot)
}

/// The number of cells in a volume strip (its inclusive width). 0 if absent.
pub fn volume_cells(rect: OverlayRect) -> i32 {
  if rect == [-1, -1, -1, -1] {
    0
  } else {
    rect[2] - rect[0] + 1
  }
}

/// A single-cell control button overlaid on the play grid: its rect (`NO_RECT`
/// sentinel when absent) and the level it paints. It occludes the play cells
/// beneath it. Most buttons are two-state via `button_level` (BRIGHT when lit, DIM
/// at rest -- the resting glow keeps an idle button findable on the play grid, like
/// the scroll arrows); the tap-tempo cell also uses `OFF`, blinking black <->
/// fully lit once a tempo exists (misc.org "tap blink between black and fully lit").
pub type ButtonOverlay = (OverlayRect, i32);

/// The steady on/off buttons' level: `BRIGHT` when lit, `DIM` at rest.
pub fn button_level(lit: bool) -> i32 {
  if lit {
    BRIGHT
  } else {
    DIM
  }
}

/// The level for `(x, y)` if it is one of the buttons, else None.
fn button_level_at(buttons: &[ButtonOverlay], x: i32, y: i32) -> Option<i32> {
  buttons.iter().find(|(rect, _)| in_rect(*rect, x, y)).map(|(_, level)| *level)
}

/// Compute the full LED level vector (row-major, `grid_w * grid_h`) for one grid.
///
/// Overlays occlude the play grid (they are drawn on top) and are checked first:
/// - *selector* cells: `BRIGHT` for the selected timbre slot, else `DIM`;
/// - *volume* cells: `BRIGHT` at the active column (`volume_col`), else `OFF`;
/// - *scroll-pad* cells: `DIM` for the four arrows, `OFF` for the two octave corners
///   (the pad fully occludes the edo grid beneath it -- no note ever shows there);
/// - control `buttons` (the accrete trio, the toggles, the polyrhythm pad): the level
///   each carries -- `BRIGHT` when lit (pressed / on / accreting), `DIM` at rest, and
///   the tap cell's blink-off `OFF`;
/// - a play cell inside `edo_rect`: its class under the *current register* is `BRIGHT`
///   if octave-equivalent to a sounding note (either grid, fingered or sustained),
///   else `DIM` if in the trail, else `OFF`;
/// - anything else: `OFF`.
///
/// `sounding_classes` and `trail_classes` are pitch classes (`0..edo`). A class that is
/// both sounding and trailed paints `BRIGHT` (sounding wins -- full clobbers dim).
#[allow(clippy::too_many_arguments)]
pub fn levels_for_grid(
  sounding_classes: &HashSet<i32>,
  trail_classes: &HashSet<i32>,
  edo_rect: [i32; 4],
  selector_rect: OverlayRect,
  selected_slot: usize,
  volume_rect: OverlayRect,
  volume_col: i32,
  scroll_rect: OverlayRect,
  buttons: &[ButtonOverlay],
  register: i32,
  x_step: i32,
  y_step: i32,
  edo: i32,
  grid_w: i32,
  grid_h: i32,
) -> Vec<i32> {
  let mut levels = vec![OFF; (grid_w * grid_h) as usize];
  for y in 0..grid_h {
    for x in 0..grid_w {
      let level = if in_rect(selector_rect, x, y) {
        match slot_for_selector_cell(selector_rect, (x, y)) {
          Some(slot) if slot == selected_slot => BRIGHT,
          _ => DIM,
        }
      } else if in_rect(volume_rect, x, y) {
        if x == volume_col {
          BRIGHT
        } else {
          OFF
        }
      } else if let Some(level) = button_level_at(buttons, x, y) {
        level
      } else if in_rect(scroll_rect, x, y) {
        match shift_for_cell(scroll_rect, (x, y)) {
          Some(Shift::Up | Shift::Down | Shift::Left | Shift::Right) => DIM,
          _ => OFF, // octave corners stay dark
        }
      } else if in_rect(edo_rect, x, y) {
        let class = step_for_cell(x_step, y_step, register, x, y).rem_euclid(edo);
        if sounding_classes.contains(&class) {
          BRIGHT
        } else if trail_classes.contains(&class) {
          DIM
        } else {
          OFF
        }
      } else {
        OFF
      };
      levels[(y * grid_w + x) as usize] = level;
    }
  }
  levels
}

/// The per-column volume gain (linear, `1.0` = unity at the top). The strip has `cells`
/// positions `0..cells`; the top (`cells-1`) is unity (0 dB) and each step down is
/// `db_range / (cells-1)` dB quieter, so the bottom is `-db_range` dB.
pub fn volume_gain_for_pos(pos: i32, cells: i32, db_range: f32) -> f32 {
  if cells <= 1 {
    return 1.0;
  }
  let top = (cells - 1) as f32;
  let db = -db_range * (top - pos as f32) / top;
  10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
  use super::*;

  const NONE: OverlayRect = [-1, -1, -1, -1];
  const SELECTOR: OverlayRect = [0, 0, 3, 0];
  const VOLUME: OverlayRect = [4, 0, 15, 0];
  const SCROLL: OverlayRect = [13, 14, 15, 15];
  const FULL: [i32; 4] = [0, 0, 15, 15];

  // 58-8-1 tuning (the config's), for register-aware class math.
  const XS: i32 = 8;
  const YS: i32 = 1;
  const EDO: i32 = 58;

  fn empty() -> HashSet<i32> {
    HashSet::new()
  }

  fn paint(
    sounding: &HashSet<i32>,
    trail: &HashSet<i32>,
    register: i32,
    volume_col: i32,
  ) -> Vec<i32> {
    levels_for_grid(
      sounding, trail, FULL, SELECTOR, 1, VOLUME, volume_col, SCROLL,
      &[], register, XS, YS, EDO, 16, 16,
    )
  }

  fn at(levels: &[i32], x: i32, y: i32) -> i32 {
    levels[(y * 16 + x) as usize]
  }

  fn class_at(register: i32, x: i32, y: i32) -> i32 {
    step_for_cell(XS, YS, register, x, y).rem_euclid(EDO)
  }

  #[test]
  fn selector_cells_map_left_to_right() {
    assert_eq!(slot_for_selector_cell(SELECTOR, (0, 0)), Some(0));
    assert_eq!(slot_for_selector_cell(SELECTOR, (3, 0)), Some(3));
    assert_eq!(slot_for_selector_cell(SELECTOR, (4, 0)), None, "past the strip");
  }

  #[test]
  fn selector_lights_selected_bright_others_dim() {
    let levels = levels_for_grid(
      &empty(), &empty(), FULL, SELECTOR, 2, NONE, -1, NONE,
      &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&levels, 2, 0), BRIGHT, "slot 2 (square) selected");
    assert_eq!(at(&levels, 0, 0), DIM, "slot 0 unselected dim");
  }

  #[test]
  fn control_buttons_paint_dim_at_rest_bright_when_lit_and_occlude_notes() {
    // The accrete trio at (0,15)..(2,15) -- needs_holding lit -- plus a lit
    // distortion toggle at (0,1).
    let buttons: Vec<ButtonOverlay> = vec![
      ([0, 15, 0, 15], button_level(false)), // clear, at rest
      ([1, 15, 1, 15], button_level(true)),  // needs_holding, on
      ([2, 15, 2, 15], button_level(false)), // accrete, at rest
      ([0, 1, 0, 1], button_level(true)),    // distortion, on
    ];
    // Make the class under the clear button "sound" -- the button must occlude it.
    let cls: HashSet<i32> = [class_at(0, 0, 15)].into_iter().collect();
    let levels = levels_for_grid(
      &cls, &empty(), FULL, SELECTOR, 1, VOLUME, 10, SCROLL, &buttons,
      0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&levels, 0, 15), DIM, "clear rests dim even though its class sounds");
    assert_eq!(at(&levels, 1, 15), BRIGHT, "needs_holding lit");
    assert_eq!(at(&levels, 2, 15), DIM, "accrete rests dim");
    assert_eq!(at(&levels, 0, 1), BRIGHT, "distortion toggle lit");
  }

  #[test]
  fn an_off_button_paints_dark_and_still_occludes() {
    // The tap-tempo cell between flashes: it carries OFF, and must go truly dark
    // even when a sounding class lands under it (black <-> fully lit blink).
    let buttons: Vec<ButtonOverlay> = vec![([15, 0, 15, 0], OFF)];
    let cls: HashSet<i32> = [class_at(0, 15, 0)].into_iter().collect();
    let levels = levels_for_grid(
      &cls, &empty(), FULL, NONE, 1, NONE, -1, SCROLL, &buttons,
      0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&levels, 15, 0), OFF, "blink-off tap cell is black, not dim");
  }

  #[test]
  fn volume_lights_only_the_active_column() {
    let levels = paint(&empty(), &empty(), 0, 10);
    assert_eq!(at(&levels, 10, 0), BRIGHT, "active volume column lit");
    assert_eq!(at(&levels, 4, 0), OFF, "other volume cells unlit");
    assert_eq!(at(&levels, 15, 0), OFF, "headroom cells unlit");
  }

  #[test]
  fn scroll_arrows_dim_octave_corners_dark() {
    let levels = paint(&empty(), &empty(), 0, 10);
    // Pad layout: (13,14)=8va-down, (14,14)=up, (15,14)=8va-up; row 15 = left/down/right.
    assert_eq!(at(&levels, 14, 14), DIM, "up arrow dim");
    assert_eq!(at(&levels, 14, 15), DIM, "down arrow dim");
    assert_eq!(at(&levels, 13, 15), DIM, "left arrow dim");
    assert_eq!(at(&levels, 15, 15), DIM, "right arrow dim");
    assert_eq!(at(&levels, 13, 14), OFF, "octave-down corner dark");
    assert_eq!(at(&levels, 15, 14), OFF, "octave-up corner dark");
  }

  #[test]
  fn a_sounding_class_lights_its_octave_equivalents_bright() {
    let cls = class_at(0, 5, 5);
    let sounding: HashSet<i32> = [cls].into_iter().collect();
    let levels = paint(&sounding, &empty(), 0, -1);
    assert_eq!(at(&levels, 5, 5), BRIGHT, "the sounding cell lights");
    // Every bright *play* cell shares the class.
    for y in 1..14 {
      for x in 0..13 {
        if at(&levels, x, y) == BRIGHT {
          assert_eq!(class_at(0, x, y), cls, "({x},{y}) bright but wrong class");
        }
      }
    }
  }

  #[test]
  fn shifting_the_register_moves_the_lit_cells() {
    // Hold the pitch struck at (5,5) with register 0; its class is fixed.
    let cls = class_at(0, 5, 5);
    let sounding: HashSet<i32> = [cls].into_iter().collect();
    // At register 0, (5,5) is lit.
    assert_eq!(at(&paint(&sounding, &empty(), 0, -1), 5, 5), BRIGHT);
    // Shift the register by +YS (one row): the cell that now *carries* that class moves,
    // and (5,5) -- whose class is now cls+YS -- is no longer lit.
    let shifted = paint(&sounding, &empty(), YS, -1);
    assert_ne!(class_at(YS, 5, 5), cls, "the register shifted (5,5)'s class");
    assert_eq!(at(&shifted, 5, 5), OFF, "the finger cell goes dark as the pitch moves off it");
    // Some cell now has the held class and is lit.
    let mut found = false;
    for y in 1..14 {
      for x in 0..13 {
        if at(&shifted, x, y) == BRIGHT {
          assert_eq!(class_at(YS, x, y), cls);
          found = true;
        }
      }
    }
    assert!(found, "the held pitch still lights somewhere after the shift");
  }

  #[test]
  fn trail_is_dim_and_sounding_clobbers_it() {
    let held = class_at(0, 5, 5);
    let trailed = class_at(0, 6, 6);
    let sounding: HashSet<i32> = [held].into_iter().collect();
    // The trail contains BOTH the held class and a released one.
    let trail: HashSet<i32> = [held, trailed].into_iter().collect();
    let levels = paint(&sounding, &trail, 0, -1);
    assert_eq!(at(&levels, 5, 5), BRIGHT, "sounding class bright (clobbers its trail entry)");
    assert_eq!(at(&levels, 6, 6), DIM, "released-but-trailed class is dim");
  }

  #[test]
  fn overlays_occlude_the_play_grid() {
    // A sounding class that also lands on scroll / selector / volume cells must NOT
    // light there -- the overlay wins.
    let scroll_cls = class_at(0, 14, 15); // a down-arrow cell
    let sounding: HashSet<i32> = [scroll_cls].into_iter().collect();
    let levels = paint(&sounding, &empty(), 0, 10);
    assert_eq!(at(&levels, 14, 15), DIM, "arrow stays dim even though its class sounds");
  }

  #[test]
  fn volume_gain_curve() {
    // 12 cells, 30 dB: top unity, bottom -30 dB, default col-10 (pos 6 within [4..15]).
    assert!((volume_gain_for_pos(11, 12, 30.0) - 1.0).abs() < 1e-6, "top = unity");
    assert!((volume_gain_for_pos(0, 12, 30.0) - 10f32.powf(-30.0 / 20.0)).abs() < 1e-6, "bottom -30 dB");
    let def = volume_gain_for_pos(6, 12, 30.0);
    assert!((def - 0.2083).abs() < 1e-3, "default ~-13.6 dB -> ~0.208, got {def}");
  }
}
