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

use crate::edo_play::{shift_for_cell, step_for_cell, Shift};

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
/// any. The slots are the rig's `[[timbres]]` (or the plain sine / triangle /
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
/// `dance_cells` are the cells this grid's diamond (edit-mode) AND square (sustained)
/// dances want lit RIGHT NOW, already resolved from the edit/sustain sets and the
/// shared clock by the caller (only the caller knows the register) -- one merged set,
/// since both dances paint identically here. Exact CELLS, not pitch classes: the dance
/// is local -- it marks the note being edited or sustained, never its
/// octave-equivalents (2_discussion 4e; queue item 3 for the square dance).
///
/// `octave_flash` says which octave-shift corner is blinking this instant because an
/// edit-mode or sustained note is off-screen that way (4g).
///
/// `overlay_dim_on` is the slow control-flash clock (`dance::overlay_dim_on`,
/// 200 ms dim / 200 ms off): every OVERLAY cell that resolves to a resting DIM --
/// unselected selector slots, scroll arrows, resting buttons, resting chord slots
/// -- shows OFF during the off half, so controls at rest read differently from dim
/// PLAY cells (trails and dancers, which are never gated by this). The rule is
/// general on purpose (queues/branch-2.org): cells that are never dim -- the
/// octave corners, the arm cell, the tap cell's black/bright blink, bright
/// anything -- are untouched simply because nothing dim is there to gate.
#[allow(clippy::too_many_arguments)]
pub fn levels_for_grid(
  sounding_classes: &HashSet<i32>,
  trail_classes: &HashSet<i32>,
  dance_cells: &HashSet<(i32, i32)>,
  x_cells: &HashSet<(i32, i32)>,
  octave_flash: super::dance::OffScreen,
  overlay_dim_on: bool,
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
  // Gate an OVERLAY cell's resting dim by the slow flash clock; every other level
  // passes through. Applied only in the overlay branches below, never to play cells.
  let overlay = |level: i32| if level == DIM && !overlay_dim_on { OFF } else { level };
  let mut levels = vec![OFF; (grid_w * grid_h) as usize];
  for y in 0..grid_h {
    for x in 0..grid_w {
      let level = if in_rect(selector_rect, x, y) {
        overlay(match slot_for_selector_cell(selector_rect, (x, y)) {
          Some(slot) if slot == selected_slot => BRIGHT,
          _ => DIM,
        })
      } else if in_rect(volume_rect, x, y) {
        if x == volume_col {
          BRIGHT
        } else {
          OFF
        }
      } else if let Some(level) = button_level_at(buttons, x, y) {
        overlay(level)
      } else if in_rect(scroll_rect, x, y) {
        overlay(match shift_for_cell(scroll_rect, (x, y)) {
          Some(Shift::Up | Shift::Down | Shift::Left | Shift::Right) => DIM,
          // The octave corners rest dark, and flash while a note is off-screen the
          // way they would bring it back. Raising the register raises the visible
          // pitches, so a note ABOVE the window is reached by OctaveUp.
          Some(Shift::OctaveUp) if octave_flash.above => BRIGHT,
          Some(Shift::OctaveDown) if octave_flash.below => BRIGHT,
          _ => OFF,
        })
      } else if in_rect(edo_rect, x, y) {
        let class = step_for_cell(x_step, y_step, register, x, y).rem_euclid(edo);
        let base = if sounding_classes.contains(&class) {
          BRIGHT
        } else if trail_classes.contains(&class) {
          DIM
        } else {
          OFF
        };
        // The dancers paint DIM, like the trails (Jeff), and yield to a sounding
        // note, which is real information. A yielded corner is skipped, NOT
        // retimed -- the clock is absolute, so this cell simply shows the note
        // underneath for its slot. (Over a trailed cell the dance draws but dim
        // on dim is indistinguishable; accepted.)
        let under = match base {
          BRIGHT => super::dance::Occupancy::Bright,
          DIM => super::dance::Occupancy::Dim,
          _ => super::dance::Occupancy::Dark,
        };
        // The fine-transpose X's walking dots: FULLY LIT, and where lit they
        // overwrite everything on the play surface -- voices, dances, trails
        // (Jeff's revision of the original everything-clobbers-it X, which read
        // poorly). Where the X is dark it overwrites nothing at all.
        if x_cells.contains(&(x, y)) {
          BRIGHT
        } else if dance_cells.contains(&(x, y)) && super::dance::draws_over(under) {
          // A dancer TOGGLES a trailed cell (Jeff): dim over dark as ever, but
          // BLACK over a trail -- dim on dim was invisible, and the dark hole
          // walking through the trail is what makes the dance legible there.
          if base == DIM {
            OFF
          } else {
            DIM
          }
        } else {
          base
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

  // 58-8-1 tuning (the rig's), for register-aware class math.
  const XS: i32 = 8;
  const YS: i32 = 1;
  const EDO: i32 = 58;

  fn empty() -> HashSet<i32> {
    HashSet::new()
  }

  /// No diamond dance and no off-screen flash: the baseline these tests describe.
  fn no_dance() -> HashSet<(i32, i32)> {
    HashSet::new()
  }
  const NO_FLASH: super::super::dance::OffScreen =
    super::super::dance::OffScreen { below: false, above: false };
  /// The overlay-flash clock in its DIM half: the pre-flash behavior these tests
  /// describe. The off half has its own test.
  const DIM_ON: bool = true;

  /// No fine-transpose X: the baseline every non-X test paints against.
  fn no_x() -> HashSet<(i32, i32)> {
    HashSet::new()
  }

  fn paint(
    sounding: &HashSet<i32>,
    trail: &HashSet<i32>,
    register: i32,
    volume_col: i32,
  ) -> Vec<i32> {
    levels_for_grid(
      sounding, trail, &no_dance(), &no_x(), NO_FLASH, DIM_ON, FULL, SELECTOR, 1, VOLUME, volume_col, SCROLL,
      &[], register, XS, YS, EDO, 16, 16,
    )
  }

  fn at(levels: &[i32], x: i32, y: i32) -> i32 {
    levels[(y * 16 + x) as usize]
  }

  fn class_at(register: i32, x: i32, y: i32) -> i32 {
    step_for_cell(XS, YS, register, x, y).rem_euclid(EDO)
  }

  /// The slow control flash (queues/branch-2.org "non-grid buttons should flash
  /// slowish"): in the clock's OFF half, every OVERLAY cell that would rest dim
  /// goes dark -- unselected selector slots, scroll arrows, resting buttons --
  /// while dim PLAY cells (trails) hold steady, which is the whole point: controls
  /// at rest are distinguishable from grid notes. Cells that are never dim (the
  /// octave corners, a bright selection, the volume column) are untouched.
  #[test]
  fn the_off_half_of_the_control_flash_blanks_overlay_dims_but_never_play_dims() {
    let buttons: Vec<ButtonOverlay> = vec![
      ([0, 15, 0, 15], button_level(false)), // a resting button
      ([1, 15, 1, 15], button_level(true)),  // a lit one
    ];
    let trail: HashSet<i32> = [class_at(0, 6, 6)].into_iter().collect();
    let off_half = levels_for_grid(
      &empty(), &trail, &no_dance(), &no_x(), NO_FLASH, false, FULL, SELECTOR, 1, VOLUME, 10, SCROLL,
      &buttons, 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&off_half, 0, 0), OFF, "an unselected selector slot goes dark");
    assert_eq!(at(&off_half, 1, 0), BRIGHT, "the selected slot holds bright");
    assert_eq!(at(&off_half, 14, 14), OFF, "a scroll arrow goes dark");
    assert_eq!(at(&off_half, 0, 15), OFF, "a resting button goes dark");
    assert_eq!(at(&off_half, 1, 15), BRIGHT, "a lit button holds bright");
    assert_eq!(at(&off_half, 10, 0), BRIGHT, "the volume column holds bright");
    assert_eq!(at(&off_half, 6, 6), DIM, "a trailed PLAY cell holds its steady dim");

    // The dim half is exactly the old picture (pinned throughout the other tests
    // via DIM_ON); spot-check the same cells.
    let dim_half = levels_for_grid(
      &empty(), &trail, &no_dance(), &no_x(), NO_FLASH, true, FULL, SELECTOR, 1, VOLUME, 10, SCROLL,
      &buttons, 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&dim_half, 0, 0), DIM);
    assert_eq!(at(&dim_half, 14, 14), DIM);
    assert_eq!(at(&dim_half, 0, 15), DIM);
    assert_eq!(at(&dim_half, 6, 6), DIM);
  }

  /// The fine-transpose X's walking dots are FULLY LIT and, where lit, overwrite
  /// everything on the play surface -- trails, dances, whatever sounds there.
  /// Where the X is dark it overwrites nothing.
  #[test]
  fn the_x_dots_paint_bright_over_everything_where_lit_and_nothing_where_not() {
    let x: HashSet<(i32, i32)> = [(6, 6), (8, 8)].into_iter().collect();
    let trail: HashSet<i32> = [class_at(0, 6, 6), class_at(0, 5, 5)].into_iter().collect();
    let levels = levels_for_grid(
      &empty(), &trail, &dance_at(&[(8, 8)]), &x, NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1,
      NONE, &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&levels, 6, 6), BRIGHT, "a dot overwrites a trailed cell");
    assert_eq!(at(&levels, 8, 8), BRIGHT, "a dot overwrites a dance corner");
    assert_eq!(at(&levels, 5, 5), DIM, "where the X is dark, everything shows as ever");
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
      &empty(), &empty(), &no_dance(), &no_x(), NO_FLASH, DIM_ON, FULL, SELECTOR, 2, NONE, -1, NONE,
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
      &cls, &empty(), &no_dance(), &no_x(), NO_FLASH, DIM_ON, FULL, SELECTOR, 1, VOLUME, 10, SCROLL, &buttons,
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
      &cls, &empty(), &no_dance(), &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 1, NONE, -1, SCROLL, &buttons,
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

  // ---- the diamond dance + off-screen flash, as painted ----

  fn dance_at(cells: &[(i32, i32)]) -> HashSet<(i32, i32)> {
    cells.iter().copied().collect()
  }

  #[test]
  fn a_danced_cell_lights_dim_over_nothing() {
    // The dancers are dim, like the trails (Jeff, superseding the bright-dance 4e).
    let levels = levels_for_grid(
      &empty(), &empty(), &dance_at(&[(5, 4)]), &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1, NONE,
      &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&levels, 5, 4), DIM);
    assert_eq!(at(&levels, 5, 5), OFF, "only the corner, not the note's own cell");
  }

  /// Over a trailed cell the dance draws INVERTED -- the trail goes black for the
  /// slot (Jeff's "toggle the color of a trail"; dim on dim was invisible).
  #[test]
  fn a_danced_cell_toggles_a_trailed_cell_black() {
    let trail: HashSet<i32> = [class_at(0, 5, 4)].into_iter().collect();
    let danced = levels_for_grid(
      &empty(), &trail, &dance_at(&[(5, 4)]), &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1, NONE,
      &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&danced, 5, 4), OFF, "dance over trail = the trail toggles black");
  }

  /// ...but it YIELDS to a sounding note, which is real information. The cell simply
  /// shows the note; the dance is skipped for that slot, not retimed (its clock is
  /// absolute, so it stays in phase with every other dance).
  #[test]
  fn a_danced_cell_yields_to_a_sounding_note_and_is_simply_skipped() {
    let sounding: HashSet<i32> = [class_at(0, 5, 4)].into_iter().collect();
    let levels = levels_for_grid(
      &sounding, &empty(), &dance_at(&[(5, 4)]), &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1, NONE,
      &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&levels, 5, 4), BRIGHT, "still bright -- but as the NOTE, not the dance");
    // Indistinguishable by level alone, which is the accepted cost of the yield rule:
    // the dance loses that slot rather than destroying the note's own signal.
  }

  // ---- the square dance (sustained pitches), as painted ----
  //
  // `levels_for_grid` takes one merged `dance_cells` set -- it does not know or care
  // whether a cell came from the diamond's `corner_cell` or the square's
  // `diagonal_cell` (mod.rs resolves both into the same set before calling here). So
  // these mirror the diamond's three compositing tests exactly, just with a diagonal
  // offset, to pin that the square dance gets identical treatment through the shared
  // path rather than some parallel rule.

  #[test]
  fn a_square_danced_cell_lights_dim_over_nothing() {
    let levels = levels_for_grid(
      &empty(), &empty(), &dance_at(&[(6, 4)]), &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1, NONE,
      &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&levels, 6, 4), DIM, "the diagonal neighbour, e.g. NE of (5,5)");
    assert_eq!(at(&levels, 5, 5), OFF, "only the corner, not the note's own cell");
  }

  #[test]
  fn a_square_danced_cell_toggles_a_trail_and_yields_to_a_sounding_note() {
    let trail: HashSet<i32> = [class_at(0, 6, 4)].into_iter().collect();
    let danced = levels_for_grid(
      &empty(), &trail, &dance_at(&[(6, 4)]), &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1, NONE,
      &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&danced, 6, 4), OFF, "dance over trail toggles black, diamond or square alike");

    let sounding: HashSet<i32> = [class_at(0, 6, 4)].into_iter().collect();
    let yielded = levels_for_grid(
      &sounding, &empty(), &dance_at(&[(6, 4)]), &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1, NONE,
      &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&yielded, 6, 4), BRIGHT, "bright -- as the NOTE, the dance yields");
  }

  /// The visible payoff of the T/8 offset: a voice both edited (diamond) AND
  /// sustained (square) lights TWO distinct cells around it at once -- an edge
  /// neighbour and a diagonal one -- which is what reads as "roughly one thing
  /// circling it" once the clock turns (queue item 3).
  #[test]
  fn a_voice_both_edited_and_sustained_lights_an_edge_and_a_diagonal_cell_at_once() {
    let elapsed = std::time::Duration::from_millis(75);
    let note = (5, 5);
    let mut both = HashSet::new();
    both.insert(super::super::dance::corner_cell(note, elapsed));
    both.insert(super::super::dance::diagonal_cell(note, elapsed));
    assert_eq!(both.len(), 2, "the two dances are 45 degrees apart, never the same cell");

    let levels =
      levels_for_grid(&empty(), &empty(), &both, &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1, NONE, &[], 0, XS, YS, EDO, 16, 16);
    for (x, y) in both.iter().copied() {
      assert_eq!(at(&levels, x, y), DIM, "({x},{y}) should be lit (dim) by one of the two dances");
    }
    assert_eq!(at(&levels, 5, 5), OFF, "the note's own cell is untouched by either dance");
  }

  #[test]
  fn the_octave_corners_rest_dark_and_flash_when_a_note_is_off_screen_that_way() {
    let dark = levels_for_grid(
      &empty(), &empty(), &no_dance(), &no_x(), NO_FLASH, DIM_ON, FULL, NONE, 0, NONE, -1, SCROLL,
      &[], 0, XS, YS, EDO, 16, 16,
    );
    // Find the two octave corners of the scroll pad.
    let mut up = None;
    let mut down = None;
    for y in SCROLL[1]..=SCROLL[3] {
      for x in SCROLL[0]..=SCROLL[2] {
        match shift_for_cell(SCROLL, (x, y)) {
          Some(Shift::OctaveUp) => up = Some((x, y)),
          Some(Shift::OctaveDown) => down = Some((x, y)),
          _ => {}
        }
      }
    }
    let (ux, uy) = up.expect("an octave-up corner");
    let (dx, dy) = down.expect("an octave-down corner");
    assert_eq!(at(&dark, ux, uy), OFF, "the corners rest dark");
    assert_eq!(at(&dark, dx, dy), OFF);

    // A note ABOVE the window is reached by raising the register -> OctaveUp flashes.
    let above = levels_for_grid(
      &empty(), &empty(), &no_dance(), &no_x(),
      super::super::dance::OffScreen { below: false, above: true }, DIM_ON,
      FULL, NONE, 0, NONE, -1, SCROLL, &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&above, ux, uy), BRIGHT, "octave-up points at a note above the window");
    assert_eq!(at(&above, dx, dy), OFF, "octave-down stays dark");

    // Both directions at once -> both corners (1_vision).
    let both = levels_for_grid(
      &empty(), &empty(), &no_dance(), &no_x(),
      super::super::dance::OffScreen { below: true, above: true }, DIM_ON,
      FULL, NONE, 0, NONE, -1, SCROLL, &[], 0, XS, YS, EDO, 16, 16,
    );
    assert_eq!(at(&both, ux, uy), BRIGHT);
    assert_eq!(at(&both, dx, dy), BRIGHT);
  }

  /// The four arrows never flash -- Jeff's call (4g): "Flash only the octave shifters,
  /// not the finer ones." So a note scrolled off by less than an octave gets no hint.
  #[test]
  fn the_scroll_arrows_stay_dim_and_never_flash() {
    let levels = levels_for_grid(
      &empty(), &empty(), &no_dance(), &no_x(),
      super::super::dance::OffScreen { below: true, above: true }, DIM_ON,
      FULL, NONE, 0, NONE, -1, SCROLL, &[], 0, XS, YS, EDO, 16, 16,
    );
    for y in SCROLL[1]..=SCROLL[3] {
      for x in SCROLL[0]..=SCROLL[2] {
        if matches!(
          shift_for_cell(SCROLL, (x, y)),
          Some(Shift::Up | Shift::Down | Shift::Left | Shift::Right)
        ) {
          assert_eq!(at(&levels, x, y), DIM, "arrow ({x},{y}) is dim, flashing or not");
        }
      }
    }
  }
}
