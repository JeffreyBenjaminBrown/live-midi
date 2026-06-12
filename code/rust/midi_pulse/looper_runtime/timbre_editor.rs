//! The on-grid timbre editor: a stack of radio parameter rows plus a control row of
//! waveform / arm / slot cells. Pure layout + value mapping (no I/O, no stored
//! state); `state.rs` owns the live timbre, the slot store, and arm state, and
//! routes presses/LEDs through here. See 6_plan 2.3 / 2.6 / 2.7 / 2.9.
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
//! Live in C3b: the four waveform cells + rows 1..6. Added in C4: the arm cell and
//! the slot cells (recall on press; arm -> capture one -> auto-disarm). Still inert
//! placeholders: fold (C5b), sustain + save-undo (C7). The editor OCCLUDES its rect:
//! `paint` overwrites every covered cell and `state.rs` consumes every press inside
//! the rect (an inert cell eats the press rather than playing a note).

use midi_pulse::config::TimbreTarget;

use super::state::{LEVEL_DIM, LEVEL_FULL, LEVEL_MID, LEVEL_OFF};
use super::timbre_rows::RowRange;
use crate::types::{Timbre, Waveform};

/// One radio change a value row / waveform cell produces. Applied to a `Timbre` --
/// the live timbre in C3b/C4, a loop note's stored timbre in C7 -- so it stays
/// target-agnostic.
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

  /// Which parameter this is, value aside -- the key for play-along overrides (one
  /// active override per parameter; the latest value wins). [C7c]
  pub fn kind(self) -> ParamKind {
    match self {
      TimbreParam::Waveform(_) => ParamKind::Waveform,
      TimbreParam::Gain(_) => ParamKind::Gain,
      TimbreParam::AmDepth(_) => ParamKind::AmDepth,
      TimbreParam::AmFreq(_) => ParamKind::AmFreq,
      TimbreParam::AmShape(_) => ParamKind::AmShape,
      TimbreParam::FmDepthCents(_) => ParamKind::FmDepthCents,
      TimbreParam::FmFreq(_) => ParamKind::FmFreq,
    }
  }
}

/// A parameter identity (no value), keying the play-along override set. [C7c]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParamKind {
  Waveform,
  Gain,
  AmDepth,
  AmFreq,
  AmShape,
  FmDepthCents,
  FmFreq,
}

/// What a press resolves to inside the editor rect. `state.rs` applies it (a blank /
/// inert cell yields None but the press is still consumed -- occlusion).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EditorAction {
  /// A radio change to the edited timbre.
  Param(TimbreParam),
  /// Toggle the arm latch (next slot press captures instead of recalls). [C4]
  ToggleArm,
  /// A timbre-slot cell (0-based). Recall, or capture-then-disarm if armed. [C4]
  Slot(usize),
  /// The fold cell: toggle folded <-> unfolded. [C5b]
  ToggleFold,
  /// A folded-strip quick cell (0..4). Recall the i-th most-recent timbre, or -- until
  /// the first save -- set the i-th waveform. `state.rs` decides by the quick-roll. [C5b]
  QuickCell(usize),
  /// The save-undo cell: a loop-target checkpoint / one-level revert (6_plan 2.8).
  /// Inert in a live-target editor. [C7b]
  SaveUndo,
  /// The sustain-the-edits cell: toggles whether a released play-along override keeps
  /// applying (loop-target, no-selection only). Inert in a live editor. [C7c]
  ToggleSustain,
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

// Control-row local x layout (6_plan 2.7): fold | 4 waveforms | arm | sustain |
// save-undo | slots... .
const FOLD_X: i32 = 0;
const WAVEFORM_X0: i32 = 1;
const WAVEFORMS: [Waveform; 4] =
  [Waveform::Sine, Waveform::Triangle, Waveform::Square, Waveform::Saw];
const ARM_X: i32 = 5;
const SUSTAIN_X: i32 = 6;
const SAVE_UNDO_X: i32 = 7;
const SLOTS_X0: i32 = 8;

// Folded strip (C5b, 6_plan 2.7): [ fold | 4 quick-timbres | amplitude across the rest ].
const QUICK_X0: i32 = 1;
const QUICK_CELLS: i32 = 4;
const FOLDED_AMP_X0: i32 = QUICK_X0 + QUICK_CELLS; // 5

/// The mutable timbre state `state.rs` owns, borrowed for painting: the displayed
/// timbre, the slot store, the current slot, the arm latch, and the quick-roll
/// (most-recent saved timbres; empty = the strip is in waveform mode).
pub struct EditorView<'a> {
  pub timbre: &'a Timbre,
  pub slots: &'a [Option<Timbre>],
  pub current: Option<usize>,
  pub armed: bool,
  pub quick: &'a [Timbre],
  pub sustain: bool,
}

/// A timbre editor placed at an absolute rect on one grid. `target` selects what it
/// edits once C7 lands; in C3b/C4 both targets edit the live timbre.
pub struct TimbreEditor {
  pub rect: [i32; 4],
  /// C3b/C4 edit the live timbre for both targets; C7 reads this to branch loop edits.
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

  /// How many timbre slots the control row holds: the cells from SLOTS_X0 to the
  /// right edge (6_plan: "slots = width - 8" on a 16-wide editor).
  pub fn slot_count(&self) -> usize {
    (self.width() as i32 - SLOTS_X0).max(0) as usize
  }

  /// The amplitude row's top-cell value -- "unity", the loudest the row can dial
  /// (6_plan 2.5: "top cell = unity = the pre-timbre level"). The live timbre boots
  /// here so it starts at a value the row can actually reach, rather than at the
  /// `Timbre::default` linear gain of 1.0, which sits above this ceiling.
  pub fn amplitude_unity(&self) -> f32 {
    let w = self.width();
    self.amplitude.value_at(w.saturating_sub(1), w)
  }

  /// The fixed-value RowRange for a value-row offset (rows 1..6); None for the
  /// control row, which `press` special-cases.
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

  /// The control-row action for a local x: the fold toggle, a waveform set, the arm
  /// toggle, a slot, or None for sustain / save-undo / past the last slot.
  fn control_action(&self, local_x: i32) -> Option<EditorAction> {
    if local_x == FOLD_X {
      return Some(EditorAction::ToggleFold);
    }
    let wf_index = local_x - WAVEFORM_X0;
    if (0..WAVEFORMS.len() as i32).contains(&wf_index) {
      return Some(EditorAction::Param(TimbreParam::Waveform(WAVEFORMS[wf_index as usize])));
    }
    if local_x == ARM_X {
      return Some(EditorAction::ToggleArm);
    }
    if local_x == SUSTAIN_X {
      return Some(EditorAction::ToggleSustain);
    }
    if local_x == SAVE_UNDO_X {
      return Some(EditorAction::SaveUndo);
    }
    let slot = local_x - SLOTS_X0;
    if (0..self.slot_count() as i32).contains(&slot) {
      return Some(EditorAction::Slot(slot as usize));
    }
    None
  }

  /// Cells in the folded strip: [ fold | 4 quick | amplitude across the rest ].
  fn folded_press(&self, local_x: i32) -> Option<EditorAction> {
    if local_x == FOLD_X {
      return Some(EditorAction::ToggleFold);
    }
    let quick = local_x - QUICK_X0;
    if (0..QUICK_CELLS).contains(&quick) {
      return Some(EditorAction::QuickCell(quick as usize));
    }
    let k = local_x - FOLDED_AMP_X0;
    let width = self.folded_amp_width();
    if k >= 0 && (k as usize) < width {
      let value = self.amplitude.value_at(k as usize, width);
      return Some(EditorAction::Param(TimbreParam::Gain(value)));
    }
    None
  }

  fn folded_amp_width(&self) -> usize {
    (self.width() as i32 - FOLDED_AMP_X0).max(1) as usize
  }

  /// Whether a press at (x,y) lands on the editor given the fold state. Folded, only
  /// the top strip row is occluded (rows below fall through to the grid and play);
  /// unfolded, the whole rect.
  pub fn occludes(&self, x: i32, y: i32, folded: bool) -> bool {
    self.contains(x, y) && (!folded || y == self.rect[1])
  }

  /// The waveform a folded quick cell selects before the first save (waveform mode).
  pub fn quick_waveform(i: usize) -> Option<Waveform> {
    WAVEFORMS.get(i).copied()
  }

  /// The action a press at absolute (x, y) makes given the fold state, or None for a
  /// blank / inert cell that is still consumed (occlusion). Caller checks `occludes`.
  pub fn press(&self, x: i32, y: i32, folded: bool) -> Option<EditorAction> {
    if !self.occludes(x, y, folded) {
      return None;
    }
    let local_x = x - self.rect[0];
    let local_y = y - self.rect[1];
    if folded {
      return self.folded_press(local_x);
    }
    if local_y == ROW_CONTROL {
      return self.control_action(local_x);
    }
    let row = self.value_row(local_y)?;
    let value = row.value_at(local_x as usize, self.width());
    Self::make_param(local_y, value).map(EditorAction::Param)
  }

  /// The control-row local x lit for the current waveform.
  fn waveform_cell(t: &Timbre) -> i32 {
    let idx = WAVEFORMS.iter().position(|w| *w == t.waveform).unwrap_or(0) as i32;
    WAVEFORM_X0 + idx
  }

  /// Overwrite the editor's whole rect into `levels` (occlusion): blank everything
  /// covered, then light the radio cell of each row plus the control-row state
  /// (fold = Mid, current waveform = Full, arm = Full when armed, slots per 6_plan
  /// 2.9: empty Off / occupied Dim / current Full, the current flashing 50/50 when
  /// the live timbre has diverged from it). Called last in `edo_levels`.
  pub fn paint(&self, view: &EditorView, folded: bool, levels: &mut [i32], grid_w: i32, flash_on: bool) {
    if folded {
      self.paint_folded(view, levels, grid_w);
    } else {
      self.paint_unfolded(view, levels, grid_w, flash_on);
    }
  }

  /// A `put` closure that writes one cell of this editor's rect (clamped to the rect
  /// and grid). Returned so both paint paths share the occlusion bookkeeping.
  fn cell_writer(&self, grid_w: i32) -> impl Fn(&mut [i32], i32, i32, i32) {
    let [x0, y0, x1, y1] = self.rect;
    move |levels: &mut [i32], lx: i32, ly: i32, level: i32| {
      let gx = x0 + lx;
      let gy = y0 + ly;
      if gx >= x0 && gx <= x1 && gy >= y0 && gy <= y1 && gx >= 0 && gx < grid_w && gy >= 0 {
        let idx = (gy * grid_w + gx) as usize;
        if idx < levels.len() {
          levels[idx] = level;
        }
      }
    }
  }

  /// The folded strip: one row, [ fold (Mid) | 4 quick cells | amplitude radio ].
  /// Quick cells show the current waveform (waveform mode, quick empty) or the
  /// occupied most-recent timbres (Dim) once any timbre is saved.
  fn paint_folded(&self, view: &EditorView, levels: &mut [i32], grid_w: i32) {
    let put = self.cell_writer(grid_w);
    let width = self.width();
    for lx in 0..width as i32 {
      put(levels, lx, ROW_CONTROL, LEVEL_OFF); // blank just the strip row
    }
    put(levels, FOLD_X, ROW_CONTROL, LEVEL_MID);
    for i in 0..QUICK_CELLS {
      let level = if view.quick.is_empty() {
        // Waveform mode: radio on the current waveform.
        if WAVEFORMS.get(i as usize) == Some(&view.timbre.waveform) { LEVEL_FULL } else { LEVEL_OFF }
      } else if (i as usize) < view.quick.len() {
        LEVEL_DIM // occupied most-recent timbre
      } else {
        LEVEL_OFF
      };
      put(levels, QUICK_X0 + i, ROW_CONTROL, level);
    }
    let amp_width = self.folded_amp_width();
    let lit = self.amplitude.cell_for(view.timbre.gain, amp_width) as i32;
    put(levels, FOLDED_AMP_X0 + lit, ROW_CONTROL, LEVEL_FULL);
  }

  fn paint_unfolded(&self, view: &EditorView, levels: &mut [i32], grid_w: i32, flash_on: bool) {
    let [x0, y0, x1, y1] = self.rect;
    let width = self.width();
    let put = self.cell_writer(grid_w);
    // 1. Blank the whole rect (occlude whatever the grid painted underneath).
    for ly in 0..=(y1 - y0) {
      for lx in 0..=(x1 - x0) {
        put(levels, lx, ly, LEVEL_OFF);
      }
    }
    // 2. Control row.
    put(levels, FOLD_X, ROW_CONTROL, LEVEL_MID); // findable, distinct from values
    put(levels, Self::waveform_cell(view.timbre), ROW_CONTROL, LEVEL_FULL);
    put(levels, ARM_X, ROW_CONTROL, if view.armed { LEVEL_FULL } else { LEVEL_DIM });
    // sustain-the-edits toggle: Bright on, Off off (6_plan 2.9).
    put(levels, SUSTAIN_X, ROW_CONTROL, if view.sustain { LEVEL_FULL } else { LEVEL_OFF });
    let dirty = view
      .current
      .and_then(|c| view.slots.get(c))
      .is_some_and(|s| s.as_ref() != Some(view.timbre));
    for i in 0..self.slot_count() {
      let level = if view.current == Some(i) {
        if dirty && !flash_on { LEVEL_OFF } else { LEVEL_FULL }
      } else if view.slots.get(i).is_some_and(Option::is_some) {
        LEVEL_DIM
      } else {
        LEVEL_OFF
      };
      put(levels, SLOTS_X0 + i as i32, ROW_CONTROL, level);
    }
    // 3. Value rows: the radio cell matching the current value.
    let lit = |row: RowRange, value: f32| row.cell_for(value, width) as i32;
    put(levels, lit(self.amplitude, view.timbre.gain), ROW_AMPLITUDE, LEVEL_FULL);
    put(levels, lit(self.am_depth, view.timbre.am.depth), ROW_AM_DEPTH, LEVEL_FULL);
    put(levels, lit(self.am_freq, view.timbre.am.freq), ROW_AM_FREQ, LEVEL_FULL);
    put(levels, lit(self.am_shape, view.timbre.am.shape), ROW_AM_SHAPE, LEVEL_FULL);
    put(levels, lit(self.fm_depth, view.timbre.fm.depth_cents), ROW_FM_DEPTH, LEVEL_FULL);
    put(levels, lit(self.fm_freq, view.timbre.fm.freq), ROW_FM_FREQ, LEVEL_FULL);
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

  fn param(a: Option<EditorAction>) -> TimbreParam {
    match a {
      Some(EditorAction::Param(p)) => p,
      other => panic!("expected a param, got {other:?}"),
    }
  }

  // Unfolded press (C3b/C4 cells live in the full editor).
  fn up(ed: &TimbreEditor, x: i32, y: i32) -> Option<EditorAction> {
    ed.press(x, y, false)
  }

  #[test]
  fn waveform_cells_select_each_waveform() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(param(up(&ed, 1, 0)), TimbreParam::Waveform(Waveform::Sine));
    assert_eq!(param(up(&ed, 2, 0)), TimbreParam::Waveform(Waveform::Triangle));
    assert_eq!(param(up(&ed, 3, 0)), TimbreParam::Waveform(Waveform::Square));
    assert_eq!(param(up(&ed, 4, 0)), TimbreParam::Waveform(Waveform::Saw));
  }

  #[test]
  fn control_row_cells_resolve() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(up(&ed, 0, 0), Some(EditorAction::ToggleFold), "fold toggle");
    assert_eq!(up(&ed, 5, 0), Some(EditorAction::ToggleArm));
    assert_eq!(up(&ed, 8, 0), Some(EditorAction::Slot(0)), "first slot");
    assert_eq!(up(&ed, 15, 0), Some(EditorAction::Slot(7)), "last of 8 slots");
    assert_eq!(up(&ed, 6, 0), Some(EditorAction::ToggleSustain), "sustain toggle");
    assert_eq!(up(&ed, 7, 0), Some(EditorAction::SaveUndo), "save-undo cell");
    assert_eq!(ed.slot_count(), 8, "16-wide editor -> 8 slots");
  }

  fn gain_of(a: Option<EditorAction>) -> f32 {
    match param(a) {
      TimbreParam::Gain(g) => g,
      other => panic!("expected a gain, got {other:?}"),
    }
  }

  #[test]
  fn amplitude_row_endpoints_map_to_range() {
    let ed = editor([0, 0, 15, 6]);
    assert!((gain_of(up(&ed, 0, 1)) - 0.0009).abs() < 1e-6, "floor = least");
    assert!((gain_of(up(&ed, 15, 1)) - 0.15).abs() < 1e-4, "ceil ~= greatest");
  }

  #[test]
  fn fx_rows_map_to_their_params() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(param(up(&ed, 0, 2)), TimbreParam::AmDepth(0.0), "AM depth floor");
    assert_eq!(param(up(&ed, 15, 2)), TimbreParam::AmDepth(1.0), "AM depth ceil");
    assert!(matches!(param(up(&ed, 0, 3)), TimbreParam::AmFreq(_)));
    assert!(matches!(param(up(&ed, 7, 4)), TimbreParam::AmShape(_)));
    assert!(matches!(param(up(&ed, 0, 5)), TimbreParam::FmDepthCents(_)));
    assert!(matches!(param(up(&ed, 0, 6)), TimbreParam::FmFreq(_)));
  }

  #[test]
  fn press_outside_rect_is_none() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(up(&ed, 0, 7), None, "below the editor");
    assert_eq!(up(&ed, 0, 15), None);
  }

  // ---- C5b: folding ----

  #[test]
  fn folded_occludes_only_the_strip_row() {
    let ed = editor([0, 0, 15, 6]);
    assert!(ed.occludes(0, 0, true), "strip row occluded folded");
    assert!(!ed.occludes(0, 3, true), "rows below fall through when folded");
    assert!(ed.occludes(0, 3, false), "all rows occluded unfolded");
  }

  #[test]
  fn folded_strip_cells_resolve() {
    let ed = editor([0, 0, 15, 6]);
    assert_eq!(ed.press(0, 0, true), Some(EditorAction::ToggleFold), "fold toggle");
    assert_eq!(ed.press(1, 0, true), Some(EditorAction::QuickCell(0)), "first quick cell");
    assert_eq!(ed.press(4, 0, true), Some(EditorAction::QuickCell(3)), "fourth quick cell");
    // Amplitude across x 5..15 (11 cells); cell 0 = least.
    assert!((gain_of(ed.press(5, 0, true)) - 0.0009).abs() < 1e-6, "folded amp floor");
    assert!((gain_of(ed.press(15, 0, true)) - 0.15).abs() < 1e-4, "folded amp ceil keeps range");
  }

  fn view<'a>(
    t: &'a Timbre,
    slots: &'a [Option<Timbre>],
    current: Option<usize>,
    armed: bool,
    quick: &'a [Timbre],
  ) -> EditorView<'a> {
    EditorView { timbre: t, slots, current, armed, quick, sustain: false }
  }

  #[test]
  fn paint_lights_waveform_fold_and_slots() {
    let ed = editor([0, 0, 15, 6]);
    let t = Timbre { waveform: Waveform::Saw, ..Timbre::default() };
    let mut slots = vec![None; 8];
    slots[0] = Some(t);
    slots[1] = Some(Timbre::default());
    let mut levels = vec![LEVEL_OFF; 16 * 16];
    ed.paint(&view(&t, &slots, Some(0), false, &[]), false, &mut levels, 16, true);
    assert_eq!(levels[FOLD_X as usize], LEVEL_MID, "fold findable");
    assert_eq!(levels[4], LEVEL_FULL, "saw waveform lit");
    assert_eq!(levels[ARM_X as usize], LEVEL_DIM, "arm idle dim");
    assert_eq!(levels[SLOTS_X0 as usize], LEVEL_FULL, "current slot bright");
    assert_eq!(levels[SLOTS_X0 as usize + 1], LEVEL_DIM, "occupied slot dim");
    assert_eq!(levels[SLOTS_X0 as usize + 2], LEVEL_OFF, "empty slot off");
  }

  #[test]
  fn dirty_current_slot_flashes() {
    let ed = editor([0, 0, 15, 6]);
    let live = Timbre { waveform: Waveform::Square, ..Timbre::default() };
    let mut slots = vec![None; 8];
    slots[3] = Some(Timbre::default());
    let at = |flash| {
      let mut l = vec![LEVEL_OFF; 16 * 16];
      ed.paint(&view(&live, &slots, Some(3), false, &[]), false, &mut l, 16, flash);
      l[SLOTS_X0 as usize + 3]
    };
    assert_eq!(at(true), LEVEL_FULL, "dirty current bright on flash-on");
    assert_eq!(at(false), LEVEL_OFF, "dirty current dark on flash-off (50/50)");
  }

  #[test]
  fn paint_unfolded_occludes_underlying_cells() {
    let ed = editor([0, 0, 15, 6]);
    let t = Timbre::default();
    let mut levels = vec![LEVEL_FULL; 16 * 16];
    ed.paint(&view(&t, &[None; 8], None, false, &[]), false, &mut levels, 16, true);
    assert_eq!(levels[6], LEVEL_OFF, "occluded inert cell dark");
    assert_eq!(levels[16 * 7], LEVEL_FULL, "outside the rect untouched");
  }

  #[test]
  fn paint_folded_blanks_only_the_strip_and_shows_waveform_mode() {
    let ed = editor([0, 0, 15, 6]);
    let t = Timbre { waveform: Waveform::Square, ..Timbre::default() };
    let mut levels = vec![LEVEL_FULL; 16 * 16];
    ed.paint(&view(&t, &[None; 8], None, false, &[]), true, &mut levels, 16, true);
    // Strip row: fold Mid; quick cells in waveform mode -> Square (cell 2) lit.
    assert_eq!(levels[FOLD_X as usize], LEVEL_MID, "fold cell");
    assert_eq!(levels[(QUICK_X0 + 2) as usize], LEVEL_FULL, "square quick cell lit");
    assert_eq!(levels[(QUICK_X0) as usize], LEVEL_OFF, "sine quick cell off");
    // Rows below the strip are NOT painted (grid shows through).
    assert_eq!(levels[16 * 3], LEVEL_FULL, "row 3 untouched when folded");
  }

  #[test]
  fn paint_folded_timbre_mode_dims_occupied_quick_cells() {
    let ed = editor([0, 0, 15, 6]);
    let t = Timbre::default();
    let quick = vec![Timbre { waveform: Waveform::Saw, ..Timbre::default() }];
    let mut levels = vec![LEVEL_OFF; 16 * 16];
    ed.paint(&view(&t, &[None; 8], None, false, &quick), true, &mut levels, 16, true);
    assert_eq!(levels[QUICK_X0 as usize], LEVEL_DIM, "occupied quick cell dim");
    assert_eq!(levels[(QUICK_X0 + 1) as usize], LEVEL_OFF, "empty quick cell off");
  }
}
