//! The on-grid timbre editor: a stack of radio parameter rows that read and write a
//! `Timbre`. Pure layout + value mapping (no I/O); `state.rs` owns placing it on a
//! grid and routing presses/LEDs through here. See 6_plan 2.6 / 2.7.
//!
//! Rows (top to bottom, 6_plan 2.7); each value row is a radio strip whose width is
//! the editor rect's width, so the number of steps scales with the rect:
//!   row 0  controls: [ fold | Sine Triangle Square Saw | arm | sustain | save-undo | slots.. ]
//!   row 1  amplitude (gain)      -- log_range
//!   row 2  AM amplitude (depth)  -- linear
//!   row 3  AM frequency          -- log_factor
//!   row 4  AM shape (morph pos)  -- linear
//!   row 5  FM amplitude (cents)  -- log_factor
//!   row 6  FM frequency          -- log_factor
//!
//! In C3b only the four waveform cells and rows 1..6 are live; the fold cell (C5b),
//! arm + slots (C4), and sustain + save-undo (C7) are present-but-inert placeholders
//! so later commits drop in without shifting the layout. The editor OCCLUDES its
//! rect: `paint` overwrites every covered cell and `state.rs` consumes every press
//! inside the rect (so an inert cell eats the press rather than playing a note).

use midi_pulse::config::TimbreTarget;

use super::state::{LEVEL_FULL, LEVEL_MID, LEVEL_OFF};
use super::timbre_rows::RowRange;
use crate::types::{Timbre, Waveform};

/// One radio change the editor produces. Applied to a `Timbre` -- the live timbre in
/// C3b, a loop note's stored timbre in C7 -- so it stays target-agnostic.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimbreParam {
  Waveform(Waveform),
  Gain(f32),
  AmDepth(f32),
  AmFreq(f32),
  AmShape(f32),
  FmDepthCents(f32),
  FmFreq(f32),
}

impl TimbreParam {
  /// Stamp this single-parameter change onto a timbre (the rest is untouched).
  pub fn apply(self, t: &mut Timbre) {
    match self {
      TimbreParam::Waveform(w) => t.waveform = w,
      TimbreParam::Gain(g) => t.gain = g,
      TimbreParam::AmDepth(d) => t.am.depth = d,
      TimbreParam::AmFreq(f) => t.am.freq = f,
      TimbreParam::AmShape(s) => t.am.shape = s,
      TimbreParam::FmDepthCents(c) => t.fm.depth_cents = c,
      TimbreParam::FmFreq(f) => t.fm.freq = f,
    }
  }
}

// Local row offsets within the editor rect (y - rect.top).
const ROW_CONTROL: i32 = 0;
const ROW_AMPLITUDE: i32 = 1;
const ROW_AM_DEPTH: i32 = 2;
const ROW_AM_FREQ: i32 = 3;
const ROW_AM_SHAPE: i32 = 4;
const ROW_FM_DEPTH: i32 = 5;
const ROW_FM_FREQ: i32 = 6;

/// The unfolded editor is this many rows tall (control row + amplitude + 5 FX rows).
#[allow(dead_code)] // C5b uses this for the fold/unfold height swap.
pub const EDITOR_ROWS: i32 = 7;

/// The four waveform cells occupy local x 1..=4 of the control row (after the fold
/// cell at local x 0), in `Waveform` enum order.
const WAVEFORM_X0: i32 = 1;
const WAVEFORMS: [Waveform; 4] =
  [Waveform::Sine, Waveform::Triangle, Waveform::Square, Waveform::Saw];

/// A timbre editor placed at an absolute rect on one grid. `target` selects what it
/// edits once C6/C7 land; in C3b both targets edit the live timbre.
pub struct TimbreEditor {
  pub rect: [i32; 4],
  /// C3b edits the live timbre for both targets; C7 reads this to branch loop edits.
  #[allow(dead_code)]
  pub target: TimbreTarget,
  amplitude: RowRange,
  am_depth: RowRange,
  am_freq: RowRange,
  am_shape: RowRange,
  fm_depth: RowRange,
  fm_freq: RowRange,
}

impl TimbreEditor {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    rect: [i32; 4],
    target: TimbreTarget,
    amplitude: RowRange,
    am_depth: RowRange,
    am_freq: RowRange,
    am_shape: RowRange,
    fm_depth: RowRange,
    fm_freq: RowRange,
  ) -> Self {
    TimbreEditor { rect, target, amplitude, am_depth, am_freq, am_shape, fm_depth, fm_freq }
  }

  pub fn contains(&self, x: i32, y: i32) -> bool {
    let [x0, y0, x1, y1] = self.rect;
    x >= x0 && x <= x1 && y >= y0 && y <= y1
  }

  fn width(&self) -> usize {
    (self.rect[2] - self.rect[0] + 1).max(1) as usize
  }

  /// The fixed-value RowRange for a value-row offset (rows 1..6); None for the
  /// control row, which the caller special-cases.
  fn value_row(&self, local_y: i32) -> Option<RowRange> {
    match local_y {
      ROW_AMPLITUDE => Some(self.amplitude),
      ROW_AM_DEPTH => Some(self.am_depth),
      ROW_AM_FREQ => Some(self.am_freq),
      ROW_AM_SHAPE => Some(self.am_shape),
      ROW_FM_DEPTH => Some(self.fm_depth),
      ROW_FM_FREQ => Some(self.fm_freq),
      _ => None,
    }
  }

  /// Build the `TimbreParam` a value row produces for a chosen value.
  fn make_param(local_y: i32, value: f32) -> Option<TimbreParam> {
    Some(match local_y {
      ROW_AMPLITUDE => TimbreParam::Gain(value),
      ROW_AM_DEPTH => TimbreParam::AmDepth(value),
      ROW_AM_FREQ => TimbreParam::AmFreq(value),
      ROW_AM_SHAPE => TimbreParam::AmShape(value),
      ROW_FM_DEPTH => TimbreParam::FmDepthCents(value),
      ROW_FM_FREQ => TimbreParam::FmFreq(value),
      _ => return None,
    })
  }

  /// The radio change a press at absolute (x, y) makes, or None for a blank / inert
  /// cell inside the rect (the caller still consumes the press -- occlusion).
  pub fn press(&self, x: i32, y: i32) -> Option<TimbreParam> {
    if !self.contains(x, y) {
      return None;
    }
    let local_x = x - self.rect[0];
    let local_y = y - self.rect[1];
    if local_y == ROW_CONTROL {
      let wf_index = local_x - WAVEFORM_X0;
      if (0..WAVEFORMS.len() as i32).contains(&wf_index) {
        return Some(TimbreParam::Waveform(WAVEFORMS[wf_index as usize]));
      }
      return None; // fold / arm / sustain / save-undo / slots: inert in C3b
    }
    let row = self.value_row(local_y)?;
    let value = row.value_at(local_x as usize, self.width());
    Self::make_param(local_y, value)
  }

  /// The control-row local x lit for the current waveform.
  fn waveform_cell(t: &Timbre) -> i32 {
    let idx = WAVEFORMS.iter().position(|w| *w == t.waveform).unwrap_or(0) as i32;
    WAVEFORM_X0 + idx
  }

  /// Overwrite the editor's whole rect into `levels` (occlusion): blank everything
  /// covered, then light each row's radio cell (Full) plus the fold cell (Mid,
  /// findable). Called last in `edo_levels`, after the play/remap painting.
  pub fn paint(&self, t: &Timbre, levels: &mut [i32], grid_w: i32) {
    let [x0, y0, x1, y1] = self.rect;
    let width = self.width();
    let put = |levels: &mut [i32], lx: i32, ly: i32, level: i32| {
      let gx = x0 + lx;
      let gy = y0 + ly;
      if gx >= x0 && gx <= x1 && gy >= y0 && gy <= y1 && gx >= 0 && gx < grid_w && gy >= 0 {
        let idx = (gy * grid_w + gx) as usize;
        if idx < levels.len() {
          levels[idx] = level;
        }
      }
    };
    // 1. Blank the whole rect (occlude whatever the grid painted underneath).
    for ly in 0..=(y1 - y0) {
      for lx in 0..=(x1 - x0) {
        put(levels, lx, ly, LEVEL_OFF);
      }
    }
    // 2. Control row: fold cell findable (Mid), current waveform lit (Full).
    put(levels, 0, ROW_CONTROL, LEVEL_MID);
    put(levels, Self::waveform_cell(t), ROW_CONTROL, LEVEL_FULL);
    // 3. Value rows: the radio cell matching the current value.
    let lit = |row: RowRange, value: f32| row.cell_for(value, width) as i32;
    put(levels, lit(self.amplitude, t.gain), ROW_AMPLITUDE, LEVEL_FULL);
    put(levels, lit(self.am_depth, t.am.depth), ROW_AM_DEPTH, LEVEL_FULL);
    put(levels, lit(self.am_freq, t.am.freq), ROW_AM_FREQ, LEVEL_FULL);
    put(levels, lit(self.am_shape, t.am.shape), ROW_AM_SHAPE, LEVEL_FULL);
    put(levels, lit(self.fm_depth, t.fm.depth_cents), ROW_FM_DEPTH, LEVEL_FULL);
    put(levels, lit(self.fm_freq, t.fm.freq), ROW_FM_FREQ, LEVEL_FULL);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn editor(rect: [i32; 4]) -> TimbreEditor {
    // 6_plan 5 default ranges.
    TimbreEditor::new(
      rect,
      TimbreTarget::Loop,
      RowRange::LogRange { least: 0.0009, greatest: 0.15 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 5.0, multiplier: 2.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
    )
  }

  #[test]
  fn waveform_cells_select_each_waveform() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(ed.press(1, 0), Some(TimbreParam::Waveform(Waveform::Sine)));
    assert_eq!(ed.press(2, 0), Some(TimbreParam::Waveform(Waveform::Triangle)));
    assert_eq!(ed.press(3, 0), Some(TimbreParam::Waveform(Waveform::Square)));
    assert_eq!(ed.press(4, 0), Some(TimbreParam::Waveform(Waveform::Saw)));
  }

  #[test]
  fn control_row_fold_and_later_commit_cells_are_inert() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(ed.press(0, 0), None, "fold cell inert in C3b");
    assert_eq!(ed.press(5, 0), None, "arm cell inert until C4");
    assert_eq!(ed.press(7, 0), None, "save-undo inert until C7");
  }

  fn gain_of(p: Option<TimbreParam>) -> f32 {
    match p {
      Some(TimbreParam::Gain(g)) => g,
      other => panic!("expected a gain, got {other:?}"),
    }
  }

  #[test]
  fn amplitude_row_endpoints_map_to_range() {
    let ed = editor([0, 0, 15, 6]);
    // 16-wide amplitude row: cell 0 = least, cell 15 = greatest (log endpoints are
    // only approximate at the top, hence the tolerance).
    assert!((gain_of(ed.press(0, 1)) - 0.0009).abs() < 1e-6, "floor = least");
    assert!((gain_of(ed.press(15, 1)) - 0.15).abs() < 1e-4, "ceil ~= greatest");
  }

  #[test]
  fn fx_rows_map_to_their_params() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(ed.press(0, 2), Some(TimbreParam::AmDepth(0.0)), "AM depth floor");
    assert_eq!(ed.press(15, 2), Some(TimbreParam::AmDepth(1.0)), "AM depth ceil");
    assert!(matches!(ed.press(0, 3), Some(TimbreParam::AmFreq(_))));
    assert!(matches!(ed.press(7, 4), Some(TimbreParam::AmShape(_))));
    assert!(matches!(ed.press(0, 5), Some(TimbreParam::FmDepthCents(_))));
    assert!(matches!(ed.press(0, 6), Some(TimbreParam::FmFreq(_))));
  }

  #[test]
  fn press_outside_rect_is_none() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(ed.press(0, 7), None, "below the editor");
    assert_eq!(ed.press(0, 15), None);
  }

  #[test]
  fn apply_then_paint_round_trips_the_lit_cell() {
    let ed = editor([0, 0, 15, 6]);
    let mut t = Timbre::default();
    // Set a saw + a mid amplitude, then confirm paint lights those cells.
    ed.press(4, 0).unwrap().apply(&mut t); // Saw
    if let Some(p) = ed.press(8, 1) {
      p.apply(&mut t); // some amplitude cell 8
    }
    let mut levels = vec![LEVEL_OFF; 16 * 16];
    ed.paint(&t, &mut levels, 16);
    // Saw is the 4th waveform -> control-row cell WAVEFORM_X0 + 3 = 4.
    assert_eq!(levels[4], LEVEL_FULL, "saw cell lit");
    assert_eq!(levels[0], LEVEL_MID, "fold cell findable");
    // Amplitude row (y=1) lights exactly the cell we set.
    let row1: Vec<i32> = (0..16).map(|x| levels[16 + x as usize]).collect();
    assert_eq!(row1.iter().filter(|&&l| l == LEVEL_FULL).count(), 1, "one lit amp cell");
    assert_eq!(levels[16 + 8], LEVEL_FULL, "amp cell 8 lit");
  }

  #[test]
  fn paint_occludes_underlying_cells() {
    let ed = editor([0, 0, 15, 6]);
    let t = Timbre::default();
    // Pretend the grid lit everything; the editor must blank its rect first.
    let mut levels = vec![LEVEL_FULL; 16 * 16];
    ed.paint(&t, &mut levels, 16);
    // A blank control-row cell (x=10, a slot placeholder) is now off.
    assert_eq!(levels[10], LEVEL_OFF, "occluded blank cell is dark");
    // A cell below the editor (y=7) is untouched (still full).
    assert_eq!(levels[16 * 7], LEVEL_FULL, "outside the rect untouched");
  }
}
