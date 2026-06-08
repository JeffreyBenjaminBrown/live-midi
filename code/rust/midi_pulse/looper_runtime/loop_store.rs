//! The loop store: a fresh multi-slot model (not an evolved RecordRuntime), plus
//! the two pure pieces it needs -- finalizing a recording and stepping playback.
//!
//! A `LoopEvent` is a note-on/off at an elapsed offset from loop start, pitch =
//! absolute EDO step. Finalizing applies boundary quantization and strips the
//! "ambient note held from before recording" case. Playback is uniform real time;
//! the *boundary cut* of notes still held at the loop end falls out of releasing
//! everything on wrap (so no explicit end-of-loop note-offs are stored).

use std::collections::HashSet;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopEvent {
  pub elapsed: Duration,
  pub pitch: i32,
  pub on: bool,
}

impl LoopEvent {
  pub fn on(elapsed: Duration, pitch: i32) -> Self {
    LoopEvent { elapsed, pitch, on: true }
  }
  pub fn off(elapsed: Duration, pitch: i32) -> Self {
    LoopEvent { elapsed, pitch, on: false }
  }
}

#[derive(Clone, Default)]
pub struct LoopSlot {
  /// None = empty/dark. Some = the loop's length.
  pub loop_duration: Option<Duration>,
  /// Sorted by elapsed.
  pub events: Vec<LoopEvent>,
  /// Snapshots of `events` before each remap, for one-press undo (Phase 4).
  pub history: Vec<Vec<LoopEvent>>,
}

/// Turn raw recorded events + the chosen loop length into the finalized loop.
///
/// - *Boundary quantize*: an event within `quantize` of either boundary snaps to
///   the next-pass start (`Duration::ZERO`) -- never to `== duration`, which the
///   playhead would never reach.
/// - *Strip ambient notes*: a note-off with no preceding note-on within the loop
///   is a release of a note held from before recording; drop it. (A note still
///   held at the *end* keeps its on and no off -- playback's wrap release cuts it
///   at the boundary.)
pub fn finalize_recording(
  mut raw: Vec<LoopEvent>,
  duration: Duration,
  quantize: Duration,
) -> Vec<LoopEvent> {
  for event in &mut raw {
    let near_end = event.elapsed + quantize >= duration;
    let near_start = event.elapsed <= quantize;
    if near_end || near_start {
      event.elapsed = Duration::ZERO;
    }
  }
  // Offs before ons at the same instant, so a release+repress at a boundary nets
  // to "sounding".
  raw.sort_by(|a, b| a.elapsed.cmp(&b.elapsed).then(a.on.cmp(&b.on)));
  let mut held: HashSet<i32> = HashSet::new();
  let mut out = Vec::with_capacity(raw.len());
  for event in raw {
    if event.on {
      held.insert(event.pitch);
      out.push(event);
    } else if held.remove(&event.pitch) {
      out.push(event);
    }
    // else: a dangling off (ambient note) -> dropped.
  }
  out
}

/// One action playback asks of the note sink, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayAction {
  On(i32),
  Off(i32),
  /// The loop wrapped: release everything this slot is sounding (this is what
  /// cuts notes held at the loop boundary), then the following `On`s start the
  /// next pass.
  ReleaseAll,
}

/// A looping playhead over one slot's events. `step` is fed the total elapsed
/// since playback started and returns the actions due since the last step.
/// Assumes a single `step` advances less than one whole loop (true at the ~1 ms
/// playback tick); a larger jump still wraps at most once.
pub struct Playback {
  duration_nanos: u128,
  last_lap: u128,
  cursor: usize,
  started: bool,
}

impl Playback {
  pub fn new(duration: Duration) -> Self {
    Playback {
      duration_nanos: duration.as_nanos().max(1),
      last_lap: 0,
      cursor: 0,
      started: false,
    }
  }

  pub fn step(&mut self, events: &[LoopEvent], total: Duration) -> Vec<PlayAction> {
    let dur = self.duration_nanos;
    let total = total.as_nanos();
    let pos = total % dur;
    let lap = total / dur;
    let mut out = vec![];

    if !self.started {
      self.started = true;
      self.last_lap = lap;
      self.cursor = 0;
      self.emit_up_to(events, pos, &mut out);
      return out;
    }

    if lap == self.last_lap {
      self.emit_up_to(events, pos, &mut out);
    } else {
      // Crossed at least one boundary. Finish the previous lap, release, restart.
      self.emit_up_to(events, dur, &mut out);
      out.push(PlayAction::ReleaseAll);
      self.cursor = 0;
      self.last_lap = lap;
      self.emit_up_to(events, pos, &mut out);
    }
    out
  }

  /// Emit events from the cursor whose elapsed <= `pos_nanos`, advancing it.
  fn emit_up_to(&mut self, events: &[LoopEvent], pos_nanos: u128, out: &mut Vec<PlayAction>) {
    while self.cursor < events.len() && events[self.cursor].elapsed.as_nanos() <= pos_nanos {
      let event = events[self.cursor];
      out.push(if event.on {
        PlayAction::On(event.pitch)
      } else {
        PlayAction::Off(event.pitch)
      });
      self.cursor += 1;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
  }

  #[test]
  fn finalize_keeps_a_clean_pair() {
    let raw = vec![LoopEvent::on(ms(100), 30), LoopEvent::off(ms(400), 30)];
    let out = finalize_recording(raw, ms(1000), ms(70));
    assert_eq!(out.len(), 2);
    assert!(out[0].on && !out[1].on);
  }

  #[test]
  fn finalize_quantizes_a_late_onset_to_the_next_pass_start() {
    // A note-on 50 ms before the 1000 ms boundary (within the 70 ms window)
    // snaps to elapsed 0, not to == duration (which would never play).
    let raw = vec![LoopEvent::on(ms(950), 42), LoopEvent::off(ms(990), 42)];
    let out = finalize_recording(raw, ms(1000), ms(70));
    assert!(out.iter().all(|e| e.elapsed == Duration::ZERO));
    assert!(out.iter().all(|e| e.elapsed < ms(1000)));
  }

  #[test]
  fn finalize_strips_an_ambient_held_note_release() {
    // An off with no preceding on (a note held from before recording, released
    // mid-loop) is dropped; the real pair survives.
    let raw = vec![
      LoopEvent::off(ms(200), 7), // ambient release -> strip
      LoopEvent::on(ms(300), 9),
      LoopEvent::off(ms(500), 9),
    ];
    let out = finalize_recording(raw, ms(1000), ms(70));
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|e| e.pitch == 9));
  }

  #[test]
  fn finalize_leaves_a_held_at_end_note_as_an_unpaired_on() {
    // on with no off before the boundary: kept as on-only; playback wrap cuts it.
    let raw = vec![LoopEvent::on(ms(800), 12)];
    let out = finalize_recording(raw, ms(1000), ms(70));
    assert_eq!(out.len(), 1);
    assert!(out[0].on);
  }

  #[test]
  fn playback_emits_events_as_the_playhead_passes_them() {
    let events = vec![LoopEvent::on(ms(100), 30), LoopEvent::off(ms(400), 30)];
    let mut pb = Playback::new(ms(1000));
    assert_eq!(pb.step(&events, ms(50)), vec![]); // before the on
    assert_eq!(pb.step(&events, ms(150)), vec![PlayAction::On(30)]);
    assert_eq!(pb.step(&events, ms(300)), vec![]); // between on and off
    assert_eq!(pb.step(&events, ms(450)), vec![PlayAction::Off(30)]);
  }

  #[test]
  fn playback_wrap_releases_all_then_restarts_the_pass() {
    let events = vec![LoopEvent::on(ms(100), 30), LoopEvent::off(ms(400), 30)];
    let mut pb = Playback::new(ms(1000));
    let _ = pb.step(&events, ms(150)); // On(30)
    let _ = pb.step(&events, ms(450)); // Off(30)
    // Jump past the boundary into the next pass (1000 + 150 = 1150).
    let out = pb.step(&events, ms(1150));
    assert_eq!(out, vec![PlayAction::ReleaseAll, PlayAction::On(30)]);
  }

  #[test]
  fn playback_wrap_cuts_a_held_at_end_note_via_release_all() {
    // A note on at 800 ms with no off: it sounds until the wrap, where ReleaseAll
    // cuts it -- the boundary cut, with no stored end-of-loop note-off.
    let events = vec![LoopEvent::on(ms(800), 12)];
    let mut pb = Playback::new(ms(1000));
    assert_eq!(pb.step(&events, ms(850)), vec![PlayAction::On(12)]);
    let out = pb.step(&events, ms(1050));
    assert_eq!(out[0], PlayAction::ReleaseAll);
  }
}
