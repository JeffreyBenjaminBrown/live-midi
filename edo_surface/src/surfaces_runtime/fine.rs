//! Fine transpose (queues/branch-2.org): a per-grid mode in which the play grid
//! becomes a TRANSPOSE CONTROLLER for the edit selection. A dancing X marks a
//! center pitch; pressing any play cell sets the selection's transpose to the
//! interval from the pressed pitch to that center; the octave corners move the
//! center itself. The transpose is a SCALAR, so the keys behave mono-style: the
//! most recent press wins, releasing a finger snaps to the most recently still-
//! held key, and releasing the LAST key leaves the transpose where it is. Exiting
//! the mode keeps a nonzero transpose in effect (the voices were moved live, so
//! there is nothing left to apply) and forgets the center (a re-entry starts at
//! the board center again -- "such range changes do not persist").
//!
//! Pure state: the caller (keys.rs) applies each transpose CHANGE to the voices
//! (as a delta through `shift_edited_voices`) and paints the X (mod.rs).

/// One grid's fine-transpose state.
#[derive(Debug, Default)]
pub struct FineTranspose {
  pub on: bool,
  /// The X's center pitch (absolute EDO step). Seeded from the board center at
  /// entry; the octave corners move it by ±edo, possibly off-screen.
  pub center: i32,
  /// The transpose currently applied to the selection, in EDO steps.
  pub applied: i32,
  /// The transpose keys currently held, in press order (most recent last):
  /// (cell, the interval that key set).
  stack: Vec<((i32, i32), i32)>,
}

impl FineTranspose {
  pub fn new() -> Self {
    FineTranspose::default()
  }

  /// Enter the mode: the X seeds at `center`, the transpose starts at 0 (nothing
  /// has been pressed), no keys held.
  pub fn enter(&mut self, center: i32) {
    self.on = true;
    self.center = center;
    self.applied = 0;
    self.stack.clear();
  }

  /// Exit: a nonzero transpose REMAINS IN EFFECT (the voices already moved); only
  /// the mode's own state is forgotten.
  pub fn exit(&mut self) {
    self.on = false;
    self.stack.clear();
  }

  /// A transpose key went down at `cell` sounding `pitch`: the transpose becomes
  /// the interval from `pitch` to the center. Returns the DELTA the caller must
  /// apply to the selection (0 when the key lands on the current transpose).
  pub fn press(&mut self, cell: (i32, i32), pitch: i32) -> i32 {
    let interval = pitch - self.center;
    self.stack.retain(|(c, _)| *c != cell);
    self.stack.push((cell, interval));
    let delta = interval - self.applied;
    self.applied = interval;
    delta
  }

  /// A transpose key came up at `cell`. If other transpose keys are still held,
  /// the transpose SNAPS to the most recently pressed of them; releasing the last
  /// key changes nothing ("the transpose does not revert to 0"). Returns the delta
  /// to apply, and whether the cell was a held transpose key at all (a finger that
  /// predates the mode is not ours and must release normally).
  pub fn release(&mut self, cell: (i32, i32)) -> Option<i32> {
    let before = self.stack.len();
    self.stack.retain(|(c, _)| *c != cell);
    if self.stack.len() == before {
      return None;
    }
    let target = self.stack.last().map(|(_, i)| *i).unwrap_or(self.applied);
    let delta = target - self.applied;
    self.applied = target;
    Some(delta)
  }

  /// An octave corner moved the X: the center shifts by `delta` (±edo). Held keys'
  /// stored intervals are deliberately NOT recomputed -- a snap-back after this
  /// lands on the interval each key SET, and the next fresh press reads the new
  /// center.
  pub fn move_center(&mut self, delta: i32) {
    self.center += delta;
  }
}

/// The X's walk path: one slash end to end, then the other, 10 steps around the
/// loop (25 ms per step -- a full lap every 250 ms). The order of the slashes and
/// their directions are arbitrary ("the order doesn't matter"); what matters is
/// that the trail runs each slash whole. `(0, 0)` -- the center -- appears in
/// both slashes, so the walk passes through it twice per lap.
const X_PATH: [(i32, i32); 10] = [
  (-2, -2), (-1, -1), (0, 0), (1, 1), (2, 2), // one slash...
  (-2, 2), (-1, 1), (0, 0), (1, -1), (2, -2), // ...then the other
];

/// The X's two FULLY-LIT dots at `elapsed`, around one on-screen image `(cx, cy)`
/// of the center pitch: a two-dot trail -- the current step and the one before it
/// -- walking the path above. Across the seam, one dot sits at the end of the old
/// slash while the other starts the new one, exactly as specified.
pub fn x_walk_cells(center: (i32, i32), elapsed: std::time::Duration) -> [(i32, i32); 2] {
  let step = ((elapsed.as_millis() / 25) % 10) as usize;
  let prev = (step + 9) % 10;
  let (cx, cy) = center;
  [
    (cx + X_PATH[step].0, cy + X_PATH[step].1),
    (cx + X_PATH[prev].0, cy + X_PATH[prev].1),
  ]
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  #[test]
  fn presses_set_the_interval_and_report_deltas() {
    let mut f = FineTranspose::new();
    f.enter(100);
    assert_eq!(f.press((3, 3), 103), 3, "first press: +3 from zero");
    assert_eq!(f.applied, 3);
    assert_eq!(f.press((4, 4), 105), 2, "second press: to +5, delta +2");
    assert_eq!(f.applied, 5);
  }

  #[test]
  fn releases_snap_to_the_most_recent_held_key_and_the_last_release_keeps_it() {
    let mut f = FineTranspose::new();
    f.enter(100);
    f.press((3, 3), 103); // +3
    f.press((4, 4), 105); // +5
    assert_eq!(f.release((4, 4)), Some(-2), "snap back to the still-held +3");
    assert_eq!(f.applied, 3);
    assert_eq!(f.release((3, 3)), Some(0), "the LAST release keeps the transpose");
    assert_eq!(f.applied, 3, "does not revert to 0");
  }

  #[test]
  fn releasing_a_key_that_was_never_ours_is_none() {
    // A finger that predates the mode: not in the stack, so the caller releases it
    // through the ordinary path.
    let mut f = FineTranspose::new();
    f.enter(100);
    assert_eq!(f.release((9, 9)), None);
  }

  #[test]
  fn moving_the_center_changes_fresh_presses_but_not_the_snap_stack() {
    let mut f = FineTranspose::new();
    f.enter(100);
    f.press((3, 3), 103); // +3
    f.move_center(-46); // the X an octave lower: intervals grow
    assert_eq!(f.press((4, 4), 105), 48, "105 - 54 = +51, from +3: delta +48");
    assert_eq!(f.release((4, 4)), Some(-48), "snap-back lands on the interval the key SET");
    assert_eq!(f.applied, 3);
  }

  #[test]
  fn exit_keeps_the_applied_transpose_and_reentry_reseeds() {
    let mut f = FineTranspose::new();
    f.enter(100);
    f.press((3, 3), 107);
    f.exit();
    assert_eq!(f.applied, 7, "a nonzero transpose remains in effect");
    assert!(!f.on);
    f.enter(200);
    assert_eq!(f.applied, 0, "a fresh entry starts untransposed");
    assert_eq!(f.center, 200, "and re-seeds the center -- range changes do not persist");
  }

  #[test]
  fn the_x_walks_two_dots_along_one_slash_then_the_other_at_25ms() {
    let at = |ms: u64| x_walk_cells((8, 8), Duration::from_millis(ms));
    // Two consecutive steps along the first slash...
    assert_eq!(at(25), [(7, 7), (6, 6)]);
    assert_eq!(at(50), [(8, 8), (7, 7)]);
    assert_eq!(at(100), [(10, 10), (9, 9)], "...to the end of the first slash");
    // The seam: one dot starts the new slash while the other ends the old one.
    assert_eq!(at(125), [(6, 10), (10, 10)]);
    assert_eq!(at(225), [(10, 6), (9, 7)], "the end of the second slash");
    // And around: step 0's trailing dot is the second slash's last cell.
    assert_eq!(at(250), [(6, 6), (10, 6)]);
  }
}
