//! Pure value<->cell mapping for the timbre editor's parameter rows.
//!
//! Every parameter is a radio row: a `width`-cell strip where exactly one lit cell
//! marks the current value. Cell index `k` (0..width) maps to a value, and a value
//! snaps back to its nearest cell. The three range kinds are the settled design
//! (6_plan 2.6 / 4_critique 2c):
//!   - Linear    [min, max]              -- e.g. AM amplitude (depth) 0..1
//!   - LogFactor [least, multiplier]     -- top GROWS with width (AM/FM freq,
//!                                          FM depth cents): "if 8 cells, 32 Hz"
//!   - LogRange  [least, greatest]       -- range FIXED, step scales with width
//!                                          (the amplitude row, which is full-width
//!                                          unfolded and narrower folded)
//!
//! This module is intentionally free of rig/runtime coupling so it can be
//! tested in isolation and reused by whatever editor wiring lands next.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowRange {
  Linear { min: f32, max: f32 },
  LogFactor { least: f32, multiplier: f32 },
  LogRange { least: f32, greatest: f32 },
}

impl RowRange {
  /// The value at cell `k` (0-based) of a `width`-cell row. `k` is clamped to the
  /// row; a zero width is treated as one cell.
  pub fn value_at(&self, k: usize, width: usize) -> f32 {
    let n = width.max(1);
    let k = k.min(n - 1);
    match *self {
      RowRange::Linear { min, max } => {
        if n == 1 {
          min
        } else {
          min + (max - min) * (k as f32) / ((n - 1) as f32)
        }
      }
      RowRange::LogFactor { least, multiplier } => least * multiplier.powi(k as i32),
      RowRange::LogRange { least, greatest } => {
        if n == 1 {
          least
        } else {
          let factor = (greatest / least).powf(1.0 / ((n - 1) as f32));
          least * factor.powi(k as i32)
        }
      }
    }
  }

  /// The cell whose value is closest to `value` (the radio snap). Values are
  /// monotonic in `k`, so nearest-in-value round-trips with `value_at`.
  pub fn cell_for(&self, value: f32, width: usize) -> usize {
    let n = width.max(1);
    (0..n)
      .min_by(|&a, &b| {
        let da = (self.value_at(a, n) - value).abs();
        let db = (self.value_at(b, n) - value).abs();
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
      })
      .unwrap_or(0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn approx(a: f32, b: f32) {
    assert!((a - b).abs() < 1e-3, "{a} != {b}");
  }

  #[test]
  fn linear_endpoints_and_midpoint() {
    let r = RowRange::Linear { min: 0.0, max: 1.0 };
    approx(r.value_at(0, 8), 0.0);
    approx(r.value_at(7, 8), 1.0);
    approx(r.value_at(7, 8) - r.value_at(0, 8), 1.0);
    approx(r.value_at(3, 8), 3.0 / 7.0);
  }

  #[test]
  fn log_factor_grows_with_width() {
    // "if 8 buttons, the highest value is 32 Hz" -- 0.25 * 2^7.
    let r = RowRange::LogFactor { least: 0.25, multiplier: 2.0 };
    approx(r.value_at(0, 8), 0.25);
    approx(r.value_at(7, 8), 32.0);
    // A wider row reaches higher (the intended "audio rate is fun").
    approx(r.value_at(15, 16), 0.25 * 2f32.powi(15));
  }

  #[test]
  fn log_range_keeps_its_endpoints_at_any_width() {
    let r = RowRange::LogRange { least: 0.25, greatest: 32.0 };
    for &w in &[4usize, 8, 11, 16] {
      approx(r.value_at(0, w), 0.25);
      approx(r.value_at(w - 1, w), 32.0);
    }
    // Folded (11 cells) and unfolded (16) share the range, differ in step count.
    assert!(r.value_at(1, 16) < r.value_at(1, 11), "more cells -> finer low step");
  }

  #[test]
  fn cell_for_round_trips_value_at() {
    let rows = [
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
      RowRange::LogRange { least: 0.0009, greatest: 0.15 },
    ];
    for r in rows {
      for w in [1usize, 4, 8, 16] {
        for k in 0..w {
          assert_eq!(r.cell_for(r.value_at(k, w), w), k, "{r:?} w={w} k={k}");
        }
      }
    }
  }

  #[test]
  fn cell_for_snaps_between_values() {
    let r = RowRange::Linear { min: 0.0, max: 1.0 };
    // 0.6 of the way (cell 4..5 region in a 0..7 row) snaps to the nearer cell.
    assert_eq!(r.cell_for(0.0, 8), 0);
    assert_eq!(r.cell_for(1.0, 8), 7);
    assert_eq!(r.cell_for(-5.0, 8), 0, "below range clamps to first");
    assert_eq!(r.cell_for(99.0, 8), 7, "above range clamps to last");
  }

  #[test]
  fn width_one_is_a_single_value() {
    approx(RowRange::Linear { min: 3.0, max: 9.0 }.value_at(0, 1), 3.0);
    approx(RowRange::LogRange { least: 0.25, greatest: 32.0 }.value_at(0, 1), 0.25);
    assert_eq!(RowRange::LogFactor { least: 1.0, multiplier: 2.0 }.cell_for(100.0, 1), 0);
  }
}
