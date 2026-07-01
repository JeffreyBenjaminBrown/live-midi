//! Pure per-grid presentation logic for the surfaces runtime: the 4-cell waveform
//! selector mapping and the full-grid LED level vector. No I/O, so it is unit-tested
//! without OSC or a device.
//!
//! Octave-equivalence between cells is *register-independent*: a scroll shifts every
//! cell's pitch by the same constant, so which cells share a pitch class never
//! changes. So the note LEDs depend only on which cells are held, not on the play
//! register -- scrolling moves the pitches under your fingers without moving the lit
//! octave-equivalents. (The play register still sets the actual sounding pitch at
//! note-on; that lives in the runtime, not here.)

use std::collections::HashSet;

use crate::types::{MonomeKey, PitchClass, Waveform};

/// OSC LED levels (0..15). The monome buckets to four visible steps.
const OFF: i32 = 0;
const DIM: i32 = 4;
const BRIGHT: i32 = 15;

/// The four waveform cells of a selector strip, left to right: sine, triangle,
/// square, saw (`rect = [x0, y0, x0+3, y0]`).
const SELECTOR_WAVEFORMS: [Waveform; 4] =
  [Waveform::Sine, Waveform::Triangle, Waveform::Square, Waveform::Saw];

/// True if `cell` lies in the inclusive rect.
fn in_rect(rect: [i32; 4], cell: MonomeKey) -> bool {
  let [x0, y0, x1, y1] = rect;
  let (x, y) = cell;
  x >= x0 && x <= x1 && y >= y0 && y <= y1
}

/// Which waveform a press on a selector cell selects, if any.
pub fn waveform_for_selector_cell(rect: [i32; 4], cell: MonomeKey) -> Option<Waveform> {
  if !in_rect(rect, cell) {
    return None;
  }
  SELECTOR_WAVEFORMS.get((cell.0 - rect[0]) as usize).copied()
}

/// An optional overlay rect: `[-1, -1, -1, -1]` means "not present" (the sentinel the
/// runtime uses for an absent scroll pad / selector), which `in_rect` never matches.
pub type OverlayRect = [i32; 4];

/// Compute the full LED level vector (row-major, `grid_w * grid_h`) for one grid:
/// - selector cells: BRIGHT for the currently-selected waveform, DIM otherwise;
/// - scroll-pad cells: OFF (they occlude the play grid and carry no pitch);
/// - a play cell inside `edo_rect`: BRIGHT if octave-equivalent to a held note, else OFF;
/// - anything else (outside every window): OFF.
///
/// Overlays win over the play grid (they are drawn on top). `selector_waveform` is the
/// waveform the strip currently has selected (i.e. the *controlled* grid's waveform).
pub fn levels_for_grid(
  pc: &PitchClass,
  held: &HashSet<MonomeKey>,
  edo_rect: [i32; 4],
  selector_rect: OverlayRect,
  selector_waveform: Waveform,
  scroll_rect: OverlayRect,
  grid_w: i32,
  grid_h: i32,
) -> Vec<i32> {
  // The pitch classes currently sounding (held cells' classes). Register-independent.
  let held_classes: HashSet<i32> = held
    .iter()
    .filter_map(|cell| pc.key_to_pitchclass.get(cell).copied())
    .collect();

  let mut levels = vec![OFF; (grid_w * grid_h) as usize];
  for y in 0..grid_h {
    for x in 0..grid_w {
      let cell = (x, y);
      let level = if in_rect(selector_rect, cell) {
        match waveform_for_selector_cell(selector_rect, cell) {
          Some(w) if w == selector_waveform => BRIGHT,
          _ => DIM,
        }
      } else if in_rect(scroll_rect, cell) {
        OFF
      } else if in_rect(edo_rect, cell) {
        match pc.key_to_pitchclass.get(&cell) {
          Some(cls) if held_classes.contains(cls) => BRIGHT,
          _ => OFF,
        }
      } else {
        OFF
      };
      levels[(y * grid_w + x) as usize] = level;
    }
  }
  levels
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::pitch::build_pitch_class;

  const NONE: OverlayRect = [-1, -1, -1, -1];
  const SELECTOR: OverlayRect = [0, 0, 3, 0];
  const FULL: [i32; 4] = [0, 0, 15, 15];

  #[test]
  fn selector_cells_map_left_to_right() {
    assert_eq!(waveform_for_selector_cell(SELECTOR, (0, 0)), Some(Waveform::Sine));
    assert_eq!(waveform_for_selector_cell(SELECTOR, (1, 0)), Some(Waveform::Triangle));
    assert_eq!(waveform_for_selector_cell(SELECTOR, (2, 0)), Some(Waveform::Square));
    assert_eq!(waveform_for_selector_cell(SELECTOR, (3, 0)), Some(Waveform::Saw));
    assert_eq!(waveform_for_selector_cell(SELECTOR, (4, 0)), None, "past the strip");
    assert_eq!(waveform_for_selector_cell(SELECTOR, (0, 1)), None, "wrong row");
  }

  #[test]
  fn selector_lights_the_selected_waveform_bright_others_dim() {
    let pc = build_pitch_class(8, 1, 58, 16, 16);
    let held = HashSet::new();
    let levels = levels_for_grid(&pc, &held, FULL, SELECTOR, Waveform::Square, NONE, 16, 16);
    let at = |x: i32, y: i32| levels[(y * 16 + x) as usize];
    assert_eq!(at(2, 0), BRIGHT, "square is selected");
    assert_eq!(at(0, 0), DIM, "sine unselected");
    assert_eq!(at(1, 0), DIM, "triangle unselected");
    assert_eq!(at(3, 0), DIM, "saw unselected");
  }

  #[test]
  fn a_held_note_lights_its_octave_equivalents() {
    // 58-8-1: (0,0) has pitch class 0; so does (0,58%..) -- within a 16x16 grid the
    // class-0 cells are (0,0) and any (x,y) with 8x+y ≡ 0 (mod 58), e.g. (0,0) alone
    // low, plus wrap cells. Use a cell with a known equivalent instead: (0,0) and the
    // cell reached by +58 steps. Simpler: press (2,3); its own cell must light.
    let pc = build_pitch_class(8, 1, 58, 16, 16);
    let mut held = HashSet::new();
    held.insert((2, 3));
    let levels = levels_for_grid(&pc, &held, FULL, NONE, Waveform::Triangle, NONE, 16, 16);
    let at = |x: i32, y: i32| levels[(y * 16 + x) as usize];
    assert_eq!(at(2, 3), BRIGHT, "the held cell lights");
    // Every lit cell shares (2,3)'s pitch class.
    let cls = pc.key_to_pitchclass[&(2, 3)];
    for y in 0..16 {
      for x in 0..16 {
        if at(x, y) == BRIGHT {
          assert_eq!(pc.key_to_pitchclass[&(x, y)], cls, "({x},{y}) lit but wrong class");
        }
      }
    }
  }

  #[test]
  fn an_octave_equivalent_cell_lights_with_the_held_one() {
    // Find a genuine equivalent pair and confirm holding one lights the other.
    let pc = build_pitch_class(8, 1, 58, 16, 16);
    let cls = pc.key_to_pitchclass[&(0, 0)];
    let group = &pc.pitchclass_to_keys[&cls];
    assert!(group.len() >= 2, "class 0 should have >=2 cells in a 16x16 58-8-1 grid: {group:?}");
    let other = *group.iter().find(|c| **c != (0, 0)).unwrap();
    let mut held = HashSet::new();
    held.insert((0, 0));
    let levels = levels_for_grid(&pc, &held, FULL, NONE, Waveform::Triangle, NONE, 16, 16);
    assert_eq!(levels[(other.1 * 16 + other.0) as usize], BRIGHT, "octave-equivalent {other:?} lights");
  }

  #[test]
  fn scroll_pad_cells_stay_dark_even_over_a_lit_class() {
    let pc = build_pitch_class(8, 1, 58, 16, 16);
    // Hold a cell whose class also appears inside the scroll-pad rect, then confirm the
    // pad cell is forced OFF.
    let scroll: OverlayRect = [13, 14, 15, 15];
    // pick any pad cell and hold a cell in its class
    let pad_cell = (13, 14);
    let cls = pc.key_to_pitchclass[&pad_cell];
    let held_cell = *pc.pitchclass_to_keys[&cls].iter().find(|c| !in_rect(scroll, **c)).unwrap();
    let mut held = HashSet::new();
    held.insert(held_cell);
    let levels = levels_for_grid(&pc, &held, FULL, NONE, Waveform::Triangle, scroll, 16, 16);
    assert_eq!(levels[(14 * 16 + 13) as usize], OFF, "scroll-pad cell forced dark");
    assert_eq!(levels[(held_cell.1 * 16 + held_cell.0) as usize], BRIGHT, "the held cell still lights");
  }
}
