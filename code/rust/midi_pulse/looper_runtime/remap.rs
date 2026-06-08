//! Loop-remap resolution: turn a row/column/cell selection into the pitches to
//! remap, and the two destructive edits -- fine (set each selected pitch to a new
//! absolute pitch) and coarse (replace the selection with a transposed copy at
//! each fingered interval).
//!
//! All pure transforms over a slot's events; the runtime supplies the selection,
//! the lowest-first fine destinations, and the coarse intervals (relative to the
//! center). See 6_plan.org Phase 4.

use std::collections::{HashMap, HashSet};

use super::display::LoopDisplay;
use super::loop_store::LoopEvent;

/// A remap selection. Rows / columns / cells are mutually exclusive (the runtime
/// enforces that); this resolves any of them against the display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
  Rows(Vec<usize>),
  Columns(Vec<usize>),
  /// (row, col) cells in the main display area.
  Cells(Vec<(usize, usize)>),
}

/// The distinct pitches a selection covers, ascending. Rows take their row's
/// pitch(es) across all time; columns take the chord sounding in that time-slice;
/// cells take the pitch(es) under them (a row pitch that also sounds in that
/// column).
pub fn selected_pitches(display: &LoopDisplay, selection: &Selection) -> Vec<i32> {
  let mut set: Vec<i32> = vec![];
  let mut push = |p: i32| {
    if !set.contains(&p) {
      set.push(p);
    }
  };
  match selection {
    Selection::Rows(rows) => {
      for &r in rows {
        if let Some(row) = display.pitch_rows.get(r) {
          row.iter().for_each(|&p| push(p));
        }
      }
    }
    Selection::Columns(cols) => {
      for &c in cols {
        if let Some(col) = display.column_pitches.get(c) {
          col.iter().for_each(|&p| push(p));
        }
      }
    }
    Selection::Cells(cells) => {
      for &(r, c) in cells {
        if let (Some(row), Some(col)) = (display.pitch_rows.get(r), display.column_pitches.get(c)) {
          row.iter().filter(|p| col.contains(p)).for_each(|&p| push(p));
        }
      }
    }
  }
  set.sort_unstable();
  set
}

/// Fine remap: set each selected pitch to a new absolute pitch. The runtime builds
/// `mapping` by pairing the selection (lowest-first) with the edo-grid presses.
/// Destructive; non-mapped pitches are untouched.
pub fn apply_fine(events: &[LoopEvent], mapping: &[(i32, i32)]) -> Vec<LoopEvent> {
  let map: HashMap<i32, i32> = mapping.iter().copied().collect();
  events
    .iter()
    .map(|e| LoopEvent { elapsed: e.elapsed, pitch: *map.get(&e.pitch).unwrap_or(&e.pitch), on: e.on })
    .collect()
}

/// Coarse remap: replace the selected notes with a transposed copy at each fingered
/// interval (relative to the center). N intervals over M selected notes yields N*M
/// notes. Destructive and additive; non-selected notes are untouched. With no
/// intervals (idle), the loop is unchanged.
pub fn apply_coarse(events: &[LoopEvent], selected: &[i32], intervals: &[i32]) -> Vec<LoopEvent> {
  if intervals.is_empty() {
    return events.to_vec();
  }
  let sel: HashSet<i32> = selected.iter().copied().collect();
  let mut out: Vec<LoopEvent> = events.iter().filter(|e| !sel.contains(&e.pitch)).copied().collect();
  for &interval in intervals {
    for e in events.iter().filter(|e| sel.contains(&e.pitch)) {
      out.push(LoopEvent { elapsed: e.elapsed, pitch: e.pitch + interval, on: e.on });
    }
  }
  out.sort_by(|a, b| a.elapsed.cmp(&b.elapsed).then(a.on.cmp(&b.on)));
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
  }

  fn display() -> LoopDisplay {
    LoopDisplay {
      pitch_rows: vec![vec![30], vec![35], vec![40]],
      column_pitches: vec![vec![30, 40], vec![35]],
    }
  }

  #[test]
  fn a_row_selects_its_pitch_across_time() {
    assert_eq!(selected_pitches(&display(), &Selection::Rows(vec![2])), vec![40]);
  }

  #[test]
  fn a_column_selects_the_chord_at_that_moment() {
    assert_eq!(selected_pitches(&display(), &Selection::Columns(vec![0])), vec![30, 40]);
  }

  #[test]
  fn a_cell_selects_the_pitch_under_it() {
    // row 2 = pitch 40, which sounds in column 0 -> {40}; in column 1 it doesn't.
    assert_eq!(selected_pitches(&display(), &Selection::Cells(vec![(2, 0)])), vec![40]);
    assert_eq!(selected_pitches(&display(), &Selection::Cells(vec![(2, 1)])), Vec::<i32>::new());
  }

  #[test]
  fn fine_remap_substitutes_pitches() {
    let events = vec![LoopEvent::on(ms(0), 30), LoopEvent::off(ms(100), 30), LoopEvent::on(ms(0), 40)];
    let out = apply_fine(&events, &[(30, 33)]);
    assert_eq!(out[0].pitch, 33);
    assert_eq!(out[1].pitch, 33);
    assert_eq!(out[2].pitch, 40, "unmapped pitch untouched");
  }

  #[test]
  fn coarse_remap_duplicates_the_selection_at_each_interval() {
    // 2 selected notes, 3 intervals -> 6 note-ons (the "2 x 3 fingers = 6" case).
    let events = vec![
      LoopEvent::on(ms(0), 10),
      LoopEvent::off(ms(100), 10),
      LoopEvent::on(ms(0), 20),
      LoopEvent::off(ms(100), 20),
    ];
    let out = apply_coarse(&events, &[10, 20], &[0, 5, 7]);
    let ons: Vec<i32> = out.iter().filter(|e| e.on).map(|e| e.pitch).collect();
    let mut got = ons.clone();
    got.sort_unstable();
    assert_eq!(got, vec![10, 15, 17, 20, 25, 27]);
    assert_eq!(ons.len(), 6);
  }

  #[test]
  fn coarse_remap_leaves_non_selected_notes_alone() {
    let events = vec![LoopEvent::on(ms(0), 10), LoopEvent::on(ms(0), 99)];
    let out = apply_coarse(&events, &[10], &[5]);
    let mut ons: Vec<i32> = out.iter().filter(|e| e.on).map(|e| e.pitch).collect();
    ons.sort_unstable();
    assert_eq!(ons, vec![15, 99], "10 -> 15, 99 untouched");
  }

  #[test]
  fn coarse_remap_with_no_intervals_is_a_noop() {
    let events = vec![LoopEvent::on(ms(0), 10)];
    assert_eq!(apply_coarse(&events, &[10], &[]), events);
  }
}
