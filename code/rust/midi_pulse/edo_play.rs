//! Edo-grid play math: cell -> absolute EDO step through a movable *play
//! register*, and the 3x2 shift pad that moves the register.
//!
//! A cell `(x, y)` sounds `x_step*x + y_step*y + register`. The shift pad nudges
//! the register: arrows by one cell (vertical = y_step, horizontal = x_step),
//! the two top corners by a whole octave (edo).
//!
//! Direction: the vision anchors the octave corners -- top-left is "down a full
//! octave; notes sound lower" (register - edo), top-right the opposite. The
//! arrows follow consistently: down/right raise pitch, up/left lower it ("up
//! means look lower"). Left/right pitch direction is the one bit the vision left
//! unstated; chosen here so the right column is always the higher one.
//!
//! Lifted to the lib (from the looper) so any grid runtime -- the looper AND the
//! surfaces runtime's two independent play grids -- shares one scroll-math source.
//! The looper re-exports these from `looper_runtime/edo.rs`, so its call sites are
//! unchanged.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
  Up,
  Down,
  Left,
  Right,
  OctaveDown,
  OctaveUp,
}

/// The absolute step a cell sounds, given the current play register.
pub fn step_for_cell(x_step: i32, y_step: i32, register: i32, x: i32, y: i32) -> i32 {
  x_step * x + y_step * y + register
}

/// Which shift a press on the 3x2 pad means, if any. Pad layout:
/// ```text
///   (x0,  y0  )=octave-down  (x0+1,y0  )=up    (x0+2,y0  )=octave-up
///   (x0,  y0+1)=left         (x0+1,y0+1)=down  (x0+2,y0+1)=right
/// ```
pub fn shift_for_cell(pad_rect: [i32; 4], cell: (i32, i32)) -> Option<Shift> {
  let [x0, y0, x1, y1] = pad_rect;
  let (x, y) = cell;
  if x < x0 || x > x1 || y < y0 || y > y1 {
    return None;
  }
  match (x - x0, y - y0) {
    (0, 0) => Some(Shift::OctaveDown),
    (1, 0) => Some(Shift::Up),
    (2, 0) => Some(Shift::OctaveUp),
    (0, 1) => Some(Shift::Left),
    (1, 1) => Some(Shift::Down),
    (2, 1) => Some(Shift::Right),
    _ => None,
  }
}

/// How a shift moves the play register.
pub fn register_delta(shift: Shift, x_step: i32, y_step: i32, edo: i32) -> i32 {
  match shift {
    Shift::Up => -y_step,
    Shift::Down => y_step,
    Shift::Left => -x_step,
    Shift::Right => x_step,
    Shift::OctaveDown => -edo,
    Shift::OctaveUp => edo,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const PAD: [i32; 4] = [13, 14, 15, 15]; // the 58-8-1 config's shift pad.

  #[test]
  fn step_applies_tuning_and_register() {
    // 58-8-1: cell (2,3) with register 0 -> 8*2 + 1*3 = 19.
    assert_eq!(step_for_cell(8, 1, 0, 2, 3), 19);
    // A register of +58 (one octave up) raises every cell by an octave.
    assert_eq!(step_for_cell(8, 1, 58, 2, 3), 19 + 58);
  }

  #[test]
  fn pad_cells_map_to_the_six_shifts() {
    assert_eq!(shift_for_cell(PAD, (13, 14)), Some(Shift::OctaveDown));
    assert_eq!(shift_for_cell(PAD, (14, 14)), Some(Shift::Up));
    assert_eq!(shift_for_cell(PAD, (15, 14)), Some(Shift::OctaveUp));
    assert_eq!(shift_for_cell(PAD, (13, 15)), Some(Shift::Left));
    assert_eq!(shift_for_cell(PAD, (14, 15)), Some(Shift::Down));
    assert_eq!(shift_for_cell(PAD, (15, 15)), Some(Shift::Right));
  }

  #[test]
  fn cells_outside_the_pad_are_not_shifts() {
    assert_eq!(shift_for_cell(PAD, (12, 14)), None);
    assert_eq!(shift_for_cell(PAD, (0, 0)), None);
  }

  #[test]
  fn octave_corners_match_the_vision() {
    // Top-left = down a full octave (notes sound lower); top-right the opposite.
    assert_eq!(register_delta(Shift::OctaveDown, 8, 1, 58), -58);
    assert_eq!(register_delta(Shift::OctaveUp, 8, 1, 58), 58);
  }

  #[test]
  fn arrows_move_by_one_cell_with_up_and_left_lowering() {
    assert_eq!(register_delta(Shift::Up, 8, 1, 58), -1); // up = look lower
    assert_eq!(register_delta(Shift::Down, 8, 1, 58), 1);
    assert_eq!(register_delta(Shift::Left, 8, 1, 58), -8); // one column = x_step
    assert_eq!(register_delta(Shift::Right, 8, 1, 58), 8);
  }
}
