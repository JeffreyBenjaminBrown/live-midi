//! Pitch math and the per-grid pitch index.

use std::collections::HashMap;

use crate::types::{MonomeKey, PitchClass};

pub fn build_pitch_class(x_step: i32, y_step: i32, edo: i32, w: i32, h: i32) -> PitchClass {
  let mut k2p = HashMap::new();
  let mut k2c = HashMap::new();
  let mut c2k: HashMap<i32, Vec<MonomeKey>> = HashMap::new();
  for x in 0..w {
    for y in 0..h {
      let abs = x_step * x + y_step * y;
      let cls = abs.rem_euclid(edo);
      k2p.insert((x, y), abs);
      k2c.insert((x, y), cls);
      c2k.entry(cls).or_default().push((x, y));
    }
  }
  PitchClass {
    key_to_pitch:       k2p,
    key_to_pitchclass:  k2c,
    pitchclass_to_keys: c2k,
  }
}

// fund * 2^(pitch / edo). pitch is absolute (with octave).
pub fn freq_for_pitch(pitch: i32, fund: f64, edo: i32) -> f32 {
  (fund * 2.0_f64.powf(pitch as f64 / edo as f64)) as f32
}

pub fn freq_for(x: i32, y: i32, fund: f64, edo: i32, x_step: i32, y_step: i32) -> f32 {
  freq_for_pitch(x_step * x + y_step * y, fund, edo)
}

// All cells whose LEDs reflect the same pitch class as `cell` —
// the pressed cell itself plus its enharmonic equivalents.
pub fn cells_for_pitch_of(pc: &PitchClass, cell: MonomeKey) -> Vec<MonomeKey> {
  pc.key_to_pitchclass.get(&cell)
    .and_then(|c| pc.pitchclass_to_keys.get(c))
    .cloned()
    .unwrap_or_else(|| vec![cell])
}

// All cells whose pitch class equals `pitch.rem_euclid(edo)`.
pub fn cells_for_pitch(pc: &PitchClass, pitch: i32, edo: i32) -> Vec<MonomeKey> {
  let cls = pitch.rem_euclid(edo);
  pc.pitchclass_to_keys.get(&cls).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pitch_class_46_9_1_groups_x1_yminus9_together() {
    // Default layout: edo=46, x_step=9, y_step=1.
    // (x, y) and (x+1, y-9) should share a pitch class.
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let c_4_10 = pc.key_to_pitchclass[&(4, 10)];
    let c_5_1 = pc.key_to_pitchclass[&(5, 1)];
    assert_eq!(c_4_10, c_5_1, "(4,10) and (5,1) should be enharmonic");
    let group = &pc.pitchclass_to_keys[&c_4_10];
    assert!(group.contains(&(4, 10)));
    assert!(group.contains(&(5, 1)));
  }

  #[test]
  fn cells_for_pitch_of_with_pc_returns_full_equivalence_group() {
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let g = cells_for_pitch_of(&pc, (4, 10));
    assert!(g.contains(&(4, 10)));
    assert!(g.contains(&(5, 1)));
  }

  #[test]
  fn cells_for_pitch_of_with_unknown_cell_returns_just_itself() {
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    // (-1,-1) isn't in the grid, so the lookup falls through to vec![cell].
    assert_eq!(cells_for_pitch_of(&pc, (-1, -1)), vec![(-1, -1)]);
  }
}
