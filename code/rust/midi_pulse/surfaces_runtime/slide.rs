//! The slide feature's candidate bookkeeping -- pure logic, no I/O (see
//! TODO/misc.org "slide"). Each grid thread owns one `SlideCandidates`.
//!
//! While slide is on, a new note may *slide from* a note that was held recently but
//! released before it: the nearest such pitch is re-triggered and glides into the
//! new one (the glide itself lives in the engine -- `VoiceState::glide_per_sample`).
//! Once a pitch has been used as a slide source it leaves the candidate set for
//! good ("removed ... for any further notes"). In mono mode the set is a singleton:
//! each release replaces everything (misc.org: "the set of candidates is a
//! singleton").
//!
//! Only *audible* releases are recorded -- a note that transferred to an accrete
//! drone is still ringing, and sliding "from" it while it drones would double it.

use std::time::{Duration, Instant};

pub struct SlideCandidates {
  /// Recently released notes, oldest first: (absolute pitch, release time). A pitch
  /// appears at most once (its newest release).
  entries: Vec<(i32, Instant)>,
}

impl SlideCandidates {
  pub fn new() -> Self {
    SlideCandidates { entries: Vec::new() }
  }

  /// Record an audible release. `mono` collapses the set to this one entry.
  pub fn note_released(&mut self, pitch: i32, now: Instant, mono: bool) {
    if mono {
      self.entries.clear();
    } else {
      self.entries.retain(|(p, _)| *p != pitch);
    }
    self.entries.push((pitch, now));
  }

  /// A new note struck at `pitch` with slide on: return the nearest candidate
  /// released within `window` (ties break toward the more recent release), removing
  /// it from the set. The same pitch is never a candidate (a zero-distance slide is
  /// no slide). Expired entries are dropped as a side effect.
  pub fn pick(&mut self, pitch: i32, now: Instant, window: Duration) -> Option<i32> {
    self.entries.retain(|(_, t)| now.duration_since(*t) <= window);
    let (index, _) = self
      .entries
      .iter()
      .enumerate()
      .filter(|(_, (p, _))| *p != pitch)
      .min_by_key(|(_, (p, t))| ((*p - pitch).abs(), std::cmp::Reverse(*t)))?;
    Some(self.entries.remove(index).0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const WINDOW: Duration = Duration::from_millis(1000);

  #[test]
  fn picks_the_nearest_recent_release_and_removes_it() {
    let t0 = Instant::now();
    let mut c = SlideCandidates::new();
    c.note_released(10, t0, false);
    c.note_released(30, t0, false);
    assert_eq!(c.pick(24, t0, WINDOW), Some(30), "30 is nearer to 24 than 10");
    assert_eq!(c.pick(24, t0, WINDOW), Some(10), "30 was consumed; 10 remains");
    assert_eq!(c.pick(24, t0, WINDOW), None, "the set is dry");
  }

  #[test]
  fn expired_and_same_pitch_candidates_are_ignored() {
    let t0 = Instant::now();
    let mut c = SlideCandidates::new();
    c.note_released(10, t0, false);
    assert_eq!(c.pick(10, t0, WINDOW), None, "a zero-distance slide is no slide");
    assert_eq!(
      c.pick(12, t0 + Duration::from_millis(1500), WINDOW),
      None,
      "released longer than the window ago",
    );
  }

  #[test]
  fn a_repeated_pitch_keeps_one_entry_with_the_newest_time() {
    let t0 = Instant::now();
    let mut c = SlideCandidates::new();
    c.note_released(10, t0, false);
    // Re-released much later: the pitch must survive a pick past the FIRST window.
    let t1 = t0 + Duration::from_millis(900);
    c.note_released(10, t1, false);
    assert_eq!(c.pick(14, t1 + Duration::from_millis(500), WINDOW), Some(10));
  }

  #[test]
  fn ties_break_toward_the_newer_release() {
    let t0 = Instant::now();
    let mut c = SlideCandidates::new();
    c.note_released(20, t0, false);
    c.note_released(28, t0 + Duration::from_millis(10), false);
    // 24 is equidistant from 20 and 28; the newer release (28) wins.
    assert_eq!(c.pick(24, t0 + Duration::from_millis(20), WINDOW), Some(28));
  }

  #[test]
  fn mono_keeps_only_the_last_release() {
    let t0 = Instant::now();
    let mut c = SlideCandidates::new();
    c.note_released(10, t0, true);
    c.note_released(30, t0, true);
    assert_eq!(c.pick(11, t0, WINDOW), Some(30), "only the newest survives mono");
    assert_eq!(c.pick(11, t0, WINDOW), None);
  }
}
