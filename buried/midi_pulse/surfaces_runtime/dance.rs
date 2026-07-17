//! The "diamond dance": how an edit-mode note is marked on its grid, and how an
//! off-screen note is pointed at (`TODO/many/1_vision.org` "the diamond dance" and
//! `2_discussion.org` 4e-4g).
//!
//! A single lit cell rotates around the edited note -- above, left, below, right --
//! each corner holding for `CORNER_MS`, so one full turn is `4 * CORNER_MS`. Exactly
//! one of the four is lit at any instant.
//!
//! *Everything shares one clock.* The phase is a pure function of absolute elapsed
//! time, not of when a note was edited, so every dance on the instrument turns in
//! step. That is Jeff's stated reason for the skip rule below: a corner that cannot
//! be drawn still *consumes its slot* rather than being passed over, because
//! compressing the turn would drift that dance out of phase with the others.
//!
//! *Unlike every other LED rule here, this one is local*: it marks the exact cells
//! holding that exact pitch on that one grid -- never octave-equivalents, never the
//! other grid. So it cannot reuse the cross-grid reflection path.

use std::time::Duration;

/// How long each corner holds. 150 ms, up from the vision's 100: Jeff relaxed it
/// ("100 ms is unnecessarily fast"), which also eases the monobright grid, whose
/// "dim" is faked by fast flashing and gets flickery under load.
pub const CORNER_MS: u64 = 150;

/// Off-screen indicator blink, 150 ms on / 150 ms off (`1_vision`).
pub const FLASH_MS: u64 = 150;

/// The four corners in turn order: above, left, below, right (`1_vision`). Grid
/// coordinates, so `above` is `y - 1`.
pub const CORNERS: [(i32, i32); 4] = [(0, -1), (-1, 0), (0, 1), (1, 0)];

/// Which corner is lit at `elapsed`. Pure, absolute, and shared by every dance --
/// that is what keeps them in phase.
pub fn corner_at(elapsed: Duration) -> usize {
  let ms = elapsed.as_millis() as u64;
  ((ms / CORNER_MS) % CORNERS.len() as u64) as usize
}

/// The cell this dance wants to light at `elapsed`, given the danced cell.
pub fn corner_cell(cell: (i32, i32), elapsed: Duration) -> (i32, i32) {
  let (dx, dy) = CORNERS[corner_at(elapsed)];
  (cell.0 + dx, cell.1 + dy)
}

/// Is the off-screen indicator lit at `elapsed`? A 50% square wave at `FLASH_MS`.
pub fn flash_on(elapsed: Duration) -> bool {
  (elapsed.as_millis() as u64 / FLASH_MS) % 2 == 0
}

/// What the dance may do to a cell that is already lit for another reason.
///
/// Jeff's rule (4e): the dance *clobbers* a dim trail -- "the trails are dim; a
/// diamond dance can temporarily make a trail bright" -- but yields to a cell that is
/// already bright, because that cell is a sounding note and overwriting it would
/// destroy real information. A yielded corner is *skipped, not retimed*.
///
/// Without the clobber the indicator could vanish exactly when the grid is busiest:
/// with up to `trails_max` (9) classes lit dim across both grids, a note whose four
/// neighbours all happen to be trailed would dance invisibly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occupancy {
  /// Nothing there: the dance draws.
  Dark,
  /// A trail. The dance wins and shows bright.
  Dim,
  /// A sounding note. The dance yields; this corner is simply not drawn.
  Bright,
}

/// Should the dance paint this corner? See [`Occupancy`].
pub fn draws_over(under: Occupancy) -> bool {
  match under {
    Occupancy::Dark | Occupancy::Dim => true,
    Occupancy::Bright => false,
  }
}

/// Which octave-shift control to flash: the note is off-screen `below` the window
/// (needs shifting one way) and/or `above` it. Both can be true at once -- there may
/// be off-screen notes in both directions, and then both corners flash (`1_vision`).
///
/// Only the two OCTAVE corners flash, never the four arrows -- Jeff's call (4g):
/// "Flash only the octave shifters, not the finer ones. I'll understand." A note
/// scrolled off by less than an octave therefore gets no indicator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OffScreen {
  pub below: bool,
  pub above: bool,
}

/// Which directions hold notes outside the visible pitch window `[lo, hi]`.
/// `pitches` is every pitch this grid cares to point at -- edit-mode AND sustained
/// notes, deliberately one signal for both (`1_vision`: "in both cases"), so you
/// cannot tell from the LED which kind you are chasing.
pub fn off_screen(pitches: impl IntoIterator<Item = i32>, lo: i32, hi: i32) -> OffScreen {
  let mut r = OffScreen::default();
  for p in pitches {
    if p < lo {
      r.below = true;
    } else if p > hi {
      r.above = true;
    }
  }
  r
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
  }

  #[test]
  fn the_dance_turns_above_left_below_right_and_repeats() {
    assert_eq!(corner_at(ms(0)), 0);
    assert_eq!(corner_at(ms(149)), 0);
    assert_eq!(corner_at(ms(150)), 1);
    assert_eq!(corner_at(ms(300)), 2);
    assert_eq!(corner_at(ms(450)), 3);
    assert_eq!(corner_at(ms(600)), 0, "one full turn is 4 x 150 = 600 ms");
  }

  #[test]
  fn exactly_one_corner_is_lit_at_any_instant() {
    // The corners are distinct offsets, and corner_at yields exactly one index.
    let mut seen = std::collections::HashSet::new();
    for c in CORNERS {
      assert!(seen.insert(c), "the four corners must be distinct cells");
    }
    for t in (0..600).step_by(37) {
      assert!(corner_at(ms(t)) < 4);
    }
  }

  #[test]
  fn the_corner_cell_is_the_neighbour_in_grid_coordinates() {
    assert_eq!(corner_cell((5, 5), ms(0)), (5, 4), "above = y-1");
    assert_eq!(corner_cell((5, 5), ms(150)), (4, 5), "left");
    assert_eq!(corner_cell((5, 5), ms(300)), (5, 6), "below");
    assert_eq!(corner_cell((5, 5), ms(450)), (6, 5), "right");
  }

  /// The whole reason the clock is absolute: two dances started at different times
  /// must still turn together, so the picture reads as one rhythm.
  #[test]
  fn every_dance_shares_one_phase_regardless_of_when_its_note_was_edited() {
    let t = ms(1_234);
    // Two different notes, same instant -> same corner index.
    assert_eq!(corner_at(t), corner_at(t));
    assert_eq!(corner_cell((2, 2), t).0 - 2, corner_cell((9, 9), t).0 - 9);
    assert_eq!(corner_cell((2, 2), t).1 - 2, corner_cell((9, 9), t).1 - 9);
  }

  /// Jeff's skip rule: a corner that cannot be drawn is passed over, but the CLOCK
  /// does not stop for it. If a skip advanced the turn early, that dance would drift
  /// out of phase with every other one.
  #[test]
  fn a_skipped_corner_still_consumes_its_slot() {
    // Corner 1 (left) is unavailable; at 150..299 ms the dance simply shows nothing,
    // and at 300 ms it is on corner 2 exactly as if nothing had been skipped.
    assert_eq!(corner_at(ms(150)), 1);
    assert_eq!(corner_at(ms(299)), 1, "the slot is still spent");
    assert_eq!(corner_at(ms(300)), 2, "and the next corner starts on time");
  }

  #[test]
  fn the_dance_clobbers_a_dim_trail_but_yields_to_a_sounding_note() {
    assert!(draws_over(Occupancy::Dark));
    assert!(draws_over(Occupancy::Dim), "a trail is overwritten (4e)");
    assert!(!draws_over(Occupancy::Bright), "a sounding note is real information");
  }

  #[test]
  fn the_off_screen_flash_is_a_50_percent_square_wave() {
    assert!(flash_on(ms(0)));
    assert!(flash_on(ms(149)));
    assert!(!flash_on(ms(150)));
    assert!(!flash_on(ms(299)));
    assert!(flash_on(ms(300)));
  }

  #[test]
  fn off_screen_reports_each_direction_that_holds_a_note() {
    assert_eq!(off_screen([5, 10], 0, 15), OffScreen { below: false, above: false });
    assert_eq!(off_screen([-3], 0, 15), OffScreen { below: true, above: false });
    assert_eq!(off_screen([99], 0, 15), OffScreen { below: false, above: true });
  }

  /// Both corners flash when there are notes in both directions (`1_vision`).
  #[test]
  fn notes_off_screen_in_both_directions_flash_both_corners() {
    assert_eq!(off_screen([-3, 99, 7], 0, 15), OffScreen { below: true, above: true });
  }

  #[test]
  fn a_note_at_the_window_edge_is_not_off_screen() {
    assert_eq!(off_screen([0, 15], 0, 15), OffScreen::default(), "inclusive bounds");
  }
}
