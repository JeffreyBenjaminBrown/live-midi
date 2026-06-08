//! The looper's shared state and the pure key/LED logic that drives it.
//!
//! Phase 1 covers the edo grid: play through the `NoteSink`, the shift pad moving
//! the play register, and the LED reflection (a sounding pitch lights every
//! octave-equivalent cell solid). The loops grid and the loop transport land in
//! later phases. Everything here is a pure function of state + event so it can be
//! unit-tested without sockets, threads, or cpal.

use std::collections::{HashMap, HashSet};

use super::edo::{register_delta, shift_for_cell, step_for_cell};
use super::sink::{NoteSource, SawNoteSink};

/// Monome LED levels, in the four buckets this grid actually shows (0/4/8/15).
pub const LEVEL_OFF: i32 = 0;
pub const LEVEL_DIM: i32 = 4;
pub const LEVEL_FULL: i32 = 15;

/// Which grid an event belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grid {
  Edo,
  Loops,
}

pub struct LooperState {
  pub sink: SawNoteSink,

  // Edo-grid tuning + geometry (immutable after construction).
  x_step: i32,
  y_step: i32,
  edo: i32,
  grid_w: i32,
  grid_h: i32,
  edo_rect: [i32; 4],
  shift_rect: [i32; 4],

  // Play state.
  register: i32,
  /// Edo cells currently held -> the absolute pitch each one sounded. A note is
  /// pinned to the pitch it was struck at, so it keeps ringing if the grid is
  /// shifted afterward.
  down: HashMap<(i32, i32), i32>,
}

impl LooperState {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    sink: SawNoteSink,
    x_step: i32,
    y_step: i32,
    edo: i32,
    grid_w: i32,
    grid_h: i32,
    edo_rect: [i32; 4],
    shift_rect: [i32; 4],
  ) -> Self {
    LooperState {
      sink,
      x_step,
      y_step,
      edo,
      grid_w,
      grid_h,
      edo_rect,
      shift_rect,
      register: 0,
      down: HashMap::new(),
    }
  }

  /// Handle a key on the edo grid. Returns true if the edo LEDs may have changed
  /// (so the caller re-renders). The shift pad overlays the grid, so a press
  /// there is a shift, never a note.
  pub fn edo_key(&mut self, x: i32, y: i32, press: bool) -> bool {
    if press {
      if let Some(shift) = shift_for_cell(self.shift_rect, (x, y)) {
        self.register += register_delta(shift, self.x_step, self.y_step, self.edo);
        return true;
      }
      if in_rect(self.edo_rect, x, y) {
        let pitch = step_for_cell(self.x_step, self.y_step, self.register, x, y);
        self.down.insert((x, y), pitch);
        self.sink.note_on(pitch, NoteSource::Live);
        return true;
      }
      false
    } else if let Some(pitch) = self.down.remove(&(x, y)) {
      self.sink.note_off(pitch, NoteSource::Live);
      true
    } else {
      false // release of a shift-pad cell or an empty cell.
    }
  }

  /// The full edo-grid LED vector (row-major, len grid_w*grid_h). A cell is solid
  /// when its pitch class matches a sounding pitch (all octave-equivalents light);
  /// otherwise the six shift-pad cells show dim so they are findable.
  pub fn edo_levels(&self) -> Vec<i32> {
    let sounding: HashSet<i32> = self
      .down
      .values()
      .map(|pitch| pitch.rem_euclid(self.edo))
      .collect();
    let mut levels = vec![LEVEL_OFF; (self.grid_w * self.grid_h) as usize];
    for y in 0..self.grid_h {
      for x in 0..self.grid_w {
        let idx = (y * self.grid_w + x) as usize;
        let class = step_for_cell(self.x_step, self.y_step, self.register, x, y).rem_euclid(self.edo);
        if sounding.contains(&class) {
          levels[idx] = LEVEL_FULL;
        } else if shift_for_cell(self.shift_rect, (x, y)).is_some() {
          levels[idx] = LEVEL_DIM;
        }
      }
    }
    levels
  }

  /// The blank loops grid (Phase 1). Later phases paint slots + the loop display.
  pub fn loops_levels(&self) -> Vec<i32> {
    vec![LEVEL_OFF; (self.grid_w * self.grid_h) as usize]
  }

  /// How many live (edo-grid-held) notes are sounding right now.
  pub fn live_pitch_count(&self) -> usize {
    self.down.len()
  }

  pub fn grid_w(&self) -> i32 {
    self.grid_w
  }
  pub fn grid_h(&self) -> i32 {
    self.grid_h
  }
  #[cfg(test)]
  pub fn register(&self) -> i32 {
    self.register
  }
}

fn in_rect(rect: [i32; 4], x: i32, y: i32) -> bool {
  let [x0, y0, x1, y1] = rect;
  x >= x0 && x <= x1 && y >= y0 && y <= y1
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::types::VoiceMap;
  use std::collections::HashMap as Map;
  use std::sync::{Arc, Mutex};

  // 58-8-1 on a full 16x16 grid, with the shift pad in the lower-right.
  fn state() -> LooperState {
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(Map::new()));
    let sink = SawNoteSink::new(voices, 80.0, 58, 48000.0, 0.003, 0.05);
    LooperState::new(sink, 8, 1, 58, 16, 16, [0, 0, 15, 15], [13, 14, 15, 15])
  }

  fn level_at(levels: &[i32], x: i32, y: i32) -> i32 {
    levels[(y * 16 + x) as usize]
  }

  #[test]
  fn pressing_a_cell_lights_it_and_its_octave_equivalents_solid() {
    let mut s = state();
    // (0,0) sounds step 0; its octave-equivalent on a 58-edo 8-1 grid is at the
    // cell whose step is a multiple of 58. (5,2): 8*5 + 2 = 42 -> no. The clean
    // octave equivalent of step 0 is wherever 8x + y == 58: (7,2)=58.
    s.edo_key(0, 0, true);
    let levels = s.edo_levels();
    assert_eq!(level_at(&levels, 0, 0), LEVEL_FULL);
    assert_eq!(level_at(&levels, 7, 2), LEVEL_FULL, "step 58 is the octave above step 0");
    // A cell of a different pitch class stays dark.
    assert_eq!(level_at(&levels, 1, 0), LEVEL_OFF);
  }

  #[test]
  fn releasing_unlights() {
    let mut s = state();
    s.edo_key(0, 0, true);
    s.edo_key(0, 0, false);
    let levels = s.edo_levels();
    assert_eq!(level_at(&levels, 0, 0), LEVEL_OFF);
    assert_eq!(level_at(&levels, 7, 2), LEVEL_OFF);
  }

  #[test]
  fn the_shift_pad_shows_dim_and_does_not_play() {
    let s0 = state();
    let levels = s0.edo_levels();
    assert_eq!(level_at(&levels, 14, 15), LEVEL_DIM, "an idle pad cell is findable");

    let mut s = state();
    // Pressing the down-shift pad cell must not create a held note.
    assert!(s.edo_key(14, 15, true));
    assert!(s.down.is_empty(), "a pad press is a shift, not a note");
  }

  #[test]
  fn octave_up_shift_moves_where_a_held_pitch_lights() {
    let mut s = state();
    s.edo_key(0, 0, true); // pitch 0 sounding; lit at (0,0) and (7,2).
    // Octave-up: top-right pad cell (15,14) -> register += 58.
    s.edo_key(15, 14, true);
    assert_eq!(s.register(), 58);
    let levels = s.edo_levels();
    // Pitch 0 (class 0) now lights cells whose (step + 58) class == 0, i.e. step
    // class == 0 still -- but the cell that *was* step 0 now reads step 58 (still
    // class 0), so (0,0) stays lit; the reflection tracks pitch class, not cells.
    assert_eq!(level_at(&levels, 0, 0), LEVEL_FULL);
  }

  #[test]
  fn two_cells_of_the_same_pitch_class_both_light() {
    let mut s = state();
    s.edo_key(7, 2, true); // step 58, class 0.
    let levels = s.edo_levels();
    assert_eq!(level_at(&levels, 7, 2), LEVEL_FULL);
    assert_eq!(level_at(&levels, 0, 0), LEVEL_FULL, "class 0 also lights step 0");
  }
}
