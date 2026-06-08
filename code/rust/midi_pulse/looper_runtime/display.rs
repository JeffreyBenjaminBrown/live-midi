//! The 2D loop display model: turn a slot's events into a grid of pitch-rows x
//! time-columns, merging nearest when either axis overflows its cap.
//!
//! Both axes use the same rule (2a/3f): start with one bucket per distinct value,
//! then repeatedly merge the nearest *adjacent* pair until the count fits the cap,
//! breaking ties toward the earlier pair. For pitch this keeps the axis monotonic
//! (a shared row holds only adjacent pitches); for time it merges the closest
//! note-groups (no wrap across the loop boundary).

use std::time::Duration;

use super::loop_store::LoopEvent;

/// Merge the sorted-ascending `positions` into at most `cap` contiguous buckets by
/// repeatedly joining the nearest adjacent pair. Returns each bucket as an
/// inclusive index range `[start, end]` into `positions`.
pub fn merge_to_cap(positions: &[i64], cap: usize) -> Vec<(usize, usize)> {
  let n = positions.len();
  if n == 0 {
    return vec![];
  }
  let cap = cap.max(1);
  let mut buckets: Vec<(usize, usize)> = (0..n).map(|i| (i, i)).collect();
  while buckets.len() > cap {
    let mut best = 0;
    let mut best_gap = i64::MAX;
    for k in 0..buckets.len() - 1 {
      let gap = positions[buckets[k + 1].0] - positions[buckets[k].1];
      if gap < best_gap {
        best_gap = gap;
        best = k; // strictly-less keeps the earliest pair on ties.
      }
    }
    buckets[best] = (buckets[best].0, buckets[best + 1].1);
    buckets.remove(best + 1);
  }
  buckets
}

pub struct LoopDisplay {
  /// Row index (0 = lowest) -> the pitch(es) on that row, ascending.
  pub pitch_rows: Vec<Vec<i32>>,
  /// Column index (0 = earliest) -> the distinct pitches sounding in it.
  pub column_pitches: Vec<Vec<i32>>,
}

impl LoopDisplay {
  pub fn row_of(&self, pitch: i32) -> Option<usize> {
    self.pitch_rows.iter().position(|row| row.contains(&pitch))
  }

  /// The lit (row, col) cells: every pitch in every column at its row.
  pub fn lit_cells(&self) -> Vec<(usize, usize)> {
    let mut cells = vec![];
    for (col, pitches) in self.column_pitches.iter().enumerate() {
      for &pitch in pitches {
        if let Some(row) = self.row_of(pitch) {
          cells.push((row, col));
        }
      }
    }
    cells
  }
}

/// Build the display for a slot's events: cluster note-ons within `cluster` into
/// time-groups (greedy, chained on consecutive onsets), one column per group
/// (merged to `col_cap`); one row per distinct pitch (merged to `row_cap`).
pub fn build_display(
  events: &[LoopEvent],
  cluster: Duration,
  row_cap: usize,
  col_cap: usize,
) -> LoopDisplay {
  let mut ons: Vec<(Duration, i32)> =
    events.iter().filter(|e| e.on).map(|e| (e.elapsed, e.pitch)).collect();
  ons.sort_by_key(|(t, _)| *t);

  // Cluster onsets into time-groups (a new group starts when an onset is more
  // than `cluster` after the previous onset).
  let mut groups: Vec<(Duration, Vec<i32>)> = vec![];
  let mut prev_onset: Option<Duration> = None;
  for &(t, p) in &ons {
    let starts_group = match prev_onset {
      Some(prev) => t > prev + cluster,
      None => true,
    };
    if starts_group {
      groups.push((t, vec![p]));
    } else {
      groups.last_mut().unwrap().1.push(p);
    }
    prev_onset = Some(t);
  }

  // Distinct pitches -> rows.
  let mut pitches: Vec<i32> = ons.iter().map(|(_, p)| *p).collect();
  pitches.sort_unstable();
  pitches.dedup();
  let pitch_pos: Vec<i64> = pitches.iter().map(|&p| p as i64).collect();
  let pitch_rows = merge_to_cap(&pitch_pos, row_cap)
    .into_iter()
    .map(|(a, b)| pitches[a..=b].to_vec())
    .collect();

  // Time-groups -> columns (each column's pitch set is the union of its groups').
  let group_pos: Vec<i64> = groups.iter().map(|(t, _)| t.as_nanos() as i64).collect();
  let column_pitches = merge_to_cap(&group_pos, col_cap)
    .into_iter()
    .map(|(a, b)| {
      let mut set: Vec<i32> = vec![];
      for group in &groups[a..=b] {
        for &p in &group.1 {
          if !set.contains(&p) {
            set.push(p);
          }
        }
      }
      set.sort_unstable();
      set
    })
    .collect();

  LoopDisplay { pitch_rows, column_pitches }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
  }

  #[test]
  fn merge_to_cap_joins_the_two_nearest_pairs() {
    // 0,10 are close; 100,110 are close; the two clusters are far apart.
    let buckets = merge_to_cap(&[0, 10, 100, 110], 2);
    assert_eq!(buckets, vec![(0, 1), (2, 3)]);
  }

  #[test]
  fn merge_to_cap_is_identity_under_cap() {
    assert_eq!(merge_to_cap(&[0, 10, 20], 5), vec![(0, 0), (1, 1), (2, 2)]);
  }

  #[test]
  fn merge_to_cap_breaks_ties_toward_the_earlier_pair() {
    // Equal gaps of 10; with cap 2 the first pair merges.
    let buckets = merge_to_cap(&[0, 10, 20], 2);
    assert_eq!(buckets, vec![(0, 1), (2, 2)]);
  }

  #[test]
  fn distinct_pitches_and_groups_become_rows_and_columns() {
    // Two chords: {30,40} near t=0, {35} near t=500.
    let events = vec![
      LoopEvent::on(ms(0), 30),
      LoopEvent::on(ms(20), 40),
      LoopEvent::on(ms(500), 35),
    ];
    let d = build_display(&events, ms(100), 8, 8);
    assert_eq!(d.pitch_rows, vec![vec![30], vec![35], vec![40]]); // ascending, own rows
    assert_eq!(d.column_pitches, vec![vec![30, 40], vec![35]]); // two time-groups
    // (30,t0) is row 0 col 0; (40,t0) row 2 col 0; (35,t500) row 1 col 1.
    let mut lit = d.lit_cells();
    lit.sort_unstable();
    assert_eq!(lit, vec![(0, 0), (1, 1), (2, 0)]);
  }

  #[test]
  fn pitch_overflow_merges_nearest_pitches_onto_shared_rows() {
    // Five distinct pitches, only 3 rows: nearest pairs share a row, axis stays
    // monotonic (no low+high pairing).
    let events = vec![
      LoopEvent::on(ms(0), 0),
      LoopEvent::on(ms(0), 1),
      LoopEvent::on(ms(0), 10),
      LoopEvent::on(ms(0), 20),
      LoopEvent::on(ms(0), 21),
    ];
    let d = build_display(&events, ms(100), 3, 8);
    assert_eq!(d.pitch_rows, vec![vec![0, 1], vec![10], vec![20, 21]]);
  }

  #[test]
  fn time_overflow_merges_nearest_groups() {
    // Four separate onsets, only 2 columns: the two nearest-in-time pairs merge.
    let events = vec![
      LoopEvent::on(ms(0), 1),
      LoopEvent::on(ms(50), 2),
      LoopEvent::on(ms(900), 3),
      LoopEvent::on(ms(950), 4),
    ];
    let d = build_display(&events, ms(10), 8, 2);
    assert_eq!(d.column_pitches.len(), 2);
    assert_eq!(d.column_pitches[0], vec![1, 2]);
    assert_eq!(d.column_pitches[1], vec![3, 4]);
  }
}
