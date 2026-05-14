//! Sparse LED-reasons map for the EDO grid.
//!
//! Each cell can have multiple reasons to be lit (a finger held a
//! pitch-equivalent key, an emitting chord contains its pitch, etc.).
//! The cell is lit iff its reason set is non-empty; LED commands fire
//! only on the empty↔non-empty transition.

use crate::types::{Brightness, MonomeKey, PitchLedReason, PitchLedReasons};

// Map our 4-state brightness enum to the OSC level integer the
// device expects. Verified by 12-brightness-test.sh: this device
// renders levels 0..15 in 4-wide buckets aligned to multiples of 4
// (see README "Coding the monome"), so any value in each bucket
// looks the same. We pick the bucket-aligned value.
pub fn low_res_brightness(b: Brightness) -> i32 {
  match b {
    Brightness::Off    => 0,
    Brightness::Dim    => 4,
    Brightness::Mid    => 8,
    Brightness::Bright => 12,
  }
}

// Mutate `reasons`; return Some(true) iff `cell` newly lit (was empty
// or absent, now has this reason). Returns None when the cell already
// had reasons, or when the reason was already present (no transition).
pub fn add_reason(reasons: &mut PitchLedReasons, cell: MonomeKey, r: PitchLedReason) -> Option<bool> {
  let entry = reasons.entry(cell).or_default();
  let was_empty = entry.is_empty();
  let inserted = entry.insert(r);
  if was_empty && inserted { Some(true) } else { None }
}

// Mutate `reasons`; return Some(false) iff `cell` newly dark (had
// this reason, now has none). Returns None when no transition (cell
// wasn't lit, reason wasn't there, or other reasons remain).
pub fn remove_reason(reasons: &mut PitchLedReasons, cell: MonomeKey, r: PitchLedReason) -> Option<bool> {
  let entry = match reasons.get_mut(&cell) {
    Some(s) => s,
    None => return None,
  };
  if !entry.remove(&r) { return None; }
  if entry.is_empty() {
    reasons.remove(&cell);
    Some(false)
  } else {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;
  use crate::pitch::{build_pitch_class, cells_for_pitch_of};

  #[test]
  fn add_then_remove_same_reason_returns_lit_then_dark_transition() {
    let mut reasons: PitchLedReasons = HashMap::new();
    let r = PitchLedReason::PitchEquivalent { source_xy: (3, 3) };
    assert_eq!(add_reason(&mut reasons, (4, 4), r), Some(true), "newly lit");
    assert_eq!(add_reason(&mut reasons, (4, 4), r), None, "already lit, same reason");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r), Some(false), "newly dark");
    assert!(!reasons.contains_key(&(4, 4)));
  }

  #[test]
  fn two_reasons_must_both_be_removed_before_dark_transition() {
    let mut reasons: PitchLedReasons = HashMap::new();
    let r1 = PitchLedReason::PitchEquivalent { source_xy: (3, 3) };
    let r2 = PitchLedReason::PitchEquivalent { source_xy: (5, 5) };
    add_reason(&mut reasons, (4, 4), r1);
    assert_eq!(add_reason(&mut reasons, (4, 4), r2), None,
               "second reason on already-lit cell: no transition");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r1), None,
               "removing one of two: still lit");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r2), Some(false),
               "removing the last: now dark");
  }

  #[test]
  fn pitch_class_group_press_release_via_led_reasons() {
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let mut reasons: PitchLedReasons = HashMap::new();

    // Press (4,10): every cell in the pitch-class group transitions to lit.
    let r_410 = PitchLedReason::PitchEquivalent { source_xy: (4, 10) };
    let mut newly_lit_410 = vec![];
    for c in cells_for_pitch_of(&pc, (4, 10)) {
      if let Some(true) = add_reason(&mut reasons, c, r_410) {
        newly_lit_410.push(c);
      }
    }
    assert!(newly_lit_410.contains(&(4, 10)));
    assert!(newly_lit_410.contains(&(5, 1)));

    // Press (5,1) — same group; no further LED transitions.
    let r_51 = PitchLedReason::PitchEquivalent { source_xy: (5, 1) };
    for c in cells_for_pitch_of(&pc, (5, 1)) {
      assert_eq!(add_reason(&mut reasons, c, r_51), None,
                 "cell {c:?} should already be lit");
    }

    // Release (5,1) while (4,10) is still pressed — no dark transitions.
    for c in cells_for_pitch_of(&pc, (5, 1)) {
      assert_eq!(remove_reason(&mut reasons, c, r_51), None,
                 "cell {c:?} still lit by (4,10)'s reason");
    }

    // Release (4,10) — every group cell transitions to dark.
    let mut newly_dark = vec![];
    for c in cells_for_pitch_of(&pc, (4, 10)) {
      if let Some(false) = remove_reason(&mut reasons, c, r_410) {
        newly_dark.push(c);
      }
    }
    assert!(newly_dark.contains(&(4, 10)));
    assert!(newly_dark.contains(&(5, 1)));
  }
}
