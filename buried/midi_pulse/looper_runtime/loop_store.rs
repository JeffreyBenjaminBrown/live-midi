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

use crate::types::Timbre;

/// A note-on/off in a loop. On-events carry the timbre captured at record time
/// (6_plan C6); off-events' timbre is unused. (No `Eq`: `Timbre` holds f32s.)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopEvent {
  pub elapsed: Duration,
  pub pitch: i32,
  pub on: bool,
  pub timbre: Timbre,
}

impl LoopEvent {
  /// A note-on with the default timbre (handy in tests).
  pub fn on(elapsed: Duration, pitch: i32) -> Self {
    LoopEvent { elapsed, pitch, on: true, timbre: Timbre::default() }
  }
  /// A note-on carrying a captured timbre.
  pub fn on_with(elapsed: Duration, pitch: i32, timbre: Timbre) -> Self {
    LoopEvent { elapsed, pitch, on: true, timbre }
  }
  pub fn off(elapsed: Duration, pitch: i32) -> Self {
    LoopEvent { elapsed, pitch, on: false, timbre: Timbre::default() }
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
  /// The save-undo checkpoints (C7, 6_plan 2.8): the last committed timbres and the
  /// one before it. Both start at the as-recorded events ("save 0") on finalize.
  pub committed: Vec<LoopEvent>,
  pub prev_committed: Option<Vec<LoopEvent>>,
}

/// The snap window for record-time quantization: a recorded note-on this close to a
/// metronome boundary is treated as if it landed on the beat. This is deliberately
/// *fixed*, not derived from the tempo: it models how precisely a human can place a
/// note by ear (~100 ms is realistic, so half that is a sane "close enough"), which
/// is a neurological constant, not a musical one -- it should not shrink just because
/// the clock is faster. (We can only adjust the recording after the fact; the live
/// note already sounded when it was played -- snapping that would need time travel.)
const QUANTIZE_WINDOW: Duration = Duration::from_millis(50);

/// Turn raw recorded events + the chosen loop length into the finalized loop.
///
/// - *Grid quantize*: a note-on within `QUANTIZE_WINDOW` of a metronome boundary (a
///   multiple of `period`) snaps onto that boundary. The recording started on a
///   boundary (the transport snaps presses, so `duration` is a whole number of
///   cycles), hence the loop start and end are boundaries too; a note-on snapping
///   onto the closing boundary wraps to the next-pass start (`Duration::ZERO`) --
///   never `== duration`, which the playhead would never reach. Note-offs are left
///   as played ("quantized to have *started* at the boundary").
/// - *Strip ambient notes*: a note-off with no preceding note-on within the loop
///   is a release of a note held from before recording; drop it. (A note still
///   held at the *end* keeps its on and no off -- playback's wrap release cuts it
///   at the boundary.)
pub fn finalize_recording(
  mut raw: Vec<LoopEvent>,
  duration: Duration,
  period: Duration,
) -> Vec<LoopEvent> {
  let p = period.as_nanos();
  if p > 0 {
    let window = QUANTIZE_WINDOW.as_nanos();
    let dur = duration.as_nanos();
    for event in raw.iter_mut().filter(|e| e.on) {
      let e = event.elapsed.as_nanos();
      let snapped = ((e + p / 2) / p) * p; // nearest multiple of the period
      if snapped.abs_diff(e) <= window {
        let snapped = if snapped >= dur { 0 } else { snapped };
        event.elapsed = Duration::from_nanos(snapped as u64);
      }
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlayAction {
  On(i32, Timbre),
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

  /// The index of the next event to emit. C7c reads this around `step` to find which
  /// note-ons the playhead just crossed (for the play-along destructive rewrite).
  pub fn cursor(&self) -> usize {
    self.cursor
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
        PlayAction::On(event.pitch, event.timbre)
      } else {
        PlayAction::Off(event.pitch)
      });
      self.cursor += 1;
    }
  }
}

/// Recordings shorter than this are discarded (a stray tap, not a loop).
pub const MIN_LOOP: Duration = Duration::from_millis(10);

struct Recording {
  slot: usize,
  start: Duration,
  buffer: Vec<LoopEvent>,
}

/// The multi-slot transport. Exactly one slot is `active` (the control target);
/// at most one is `sounding` (no layering). Times are injected as `Duration`s
/// since a monotonic epoch, so the whole transport is pure and testable.
pub struct LoopStore {
  pub slots: Vec<LoopSlot>,
  pub active: usize,
  pub sounding: Option<usize>,
  clock_period: Duration,
  recording: Option<Recording>,
}

impl LoopStore {
  pub fn new(n_slots: usize, clock_period: Duration) -> Self {
    LoopStore {
      slots: vec![LoopSlot::default(); n_slots.max(1)],
      active: 0,
      sounding: None,
      clock_period,
      recording: None,
    }
  }

  pub fn is_recording(&self) -> bool {
    self.recording.is_some()
  }
  pub fn recording_slot(&self) -> Option<usize> {
    self.recording.as_ref().map(|r| r.slot)
  }
  pub fn is_occupied(&self, slot: usize) -> bool {
    self.slots.get(slot).is_some_and(|s| s.loop_duration.is_some())
  }

  pub fn set_active(&mut self, slot: usize) {
    if slot < self.slots.len() {
      self.active = slot;
    }
  }

  /// Begin recording into the active slot (overwrites it; no overdub). If the
  /// active slot was the one sounding, it stops -- you are replacing it.
  pub fn start_recording(&mut self, now: Duration) {
    if self.sounding == Some(self.active) {
      self.sounding = None;
    }
    self.recording = Some(Recording { slot: self.active, start: now, buffer: vec![] });
  }

  /// Capture a live note (default timbre) while recording (a no-op otherwise).
  pub fn record_note(&mut self, now: Duration, pitch: i32, on: bool) {
    self.record_note_with(now, pitch, on, Timbre::default());
  }

  /// Capture a live note carrying `timbre` (the live timbre at this instant);
  /// off-events ignore it. This is the record-time per-note capture (6_plan C6).
  pub fn record_note_with(&mut self, now: Duration, pitch: i32, on: bool, timbre: Timbre) {
    if let Some(rec) = &mut self.recording {
      rec.buffer.push(LoopEvent { elapsed: now.saturating_sub(rec.start), pitch, on, timbre });
    }
  }

  /// Stop the active slot's current activity: finalize a recording (without
  /// playing it), or -- if the active slot is the one sounding -- silence it.
  pub fn stop(&mut self, now: Duration) {
    if self.recording.is_some() {
      self.finalize(now);
    } else if self.sounding == Some(self.active) {
      self.sounding = None;
    }
  }

  /// Stop recording (if any) and make the active slot the sole sounding loop. With
  /// nothing recording, just loops the active slot if it has content.
  pub fn play(&mut self, now: Duration) {
    let recorded = self.finalize(now);
    let target = recorded.unwrap_or(self.active);
    if self.slots[target].loop_duration.is_some() {
      self.sounding = Some(target);
    }
  }

  /// Finalize an in-progress recording into its slot. Returns the slot if a
  /// non-trivial loop was stored, else None (too short -> discarded, slot kept).
  fn finalize(&mut self, now: Duration) -> Option<usize> {
    let rec = self.recording.take()?;
    let duration = now.saturating_sub(rec.start);
    if duration < MIN_LOOP {
      return None;
    }
    self.slots[rec.slot].loop_duration = Some(duration);
    let finalized = finalize_recording(rec.buffer, duration, self.clock_period);
    // "save 0": the checkpoints start at the as-recorded timbres.
    self.slots[rec.slot].committed = finalized.clone();
    self.slots[rec.slot].prev_committed = None;
    self.slots[rec.slot].events = finalized;
    Some(rec.slot)
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
    let out = finalize_recording(raw, ms(1000), ms(200));
    assert_eq!(out.len(), 2);
    assert!(out[0].on && !out[1].on);
  }

  #[test]
  fn finalize_snaps_near_boundary_onsets_onto_the_grid() {
    // Fixed 50 ms window, 200 ms cycle. A note-on 5 ms before the 1000 ms close (a
    // boundary) snaps onto it, which wraps to the next-pass start (0), never
    // == duration. An interior onset 40 ms past the 600 ms boundary snaps back to it
    // (it would NOT under the old tempo-derived window). The note-off is left alone.
    let raw = vec![
      LoopEvent::on(ms(995), 42),
      LoopEvent::off(ms(998), 42),
      LoopEvent::on(ms(640), 50),
    ];
    let out = finalize_recording(raw, ms(1000), ms(200));
    let on42 = out.iter().find(|e| e.pitch == 42 && e.on).unwrap();
    assert_eq!(on42.elapsed, Duration::ZERO, "closing-boundary onset wraps to 0");
    let on50 = out.iter().find(|e| e.pitch == 50 && e.on).unwrap();
    assert_eq!(on50.elapsed, ms(600), "interior onset snaps onto the boundary");
    let off42 = out.iter().find(|e| e.pitch == 42 && !e.on).unwrap();
    assert_eq!(off42.elapsed, ms(998), "the off is left as played");
    assert!(out.iter().all(|e| e.elapsed < ms(1000)));
  }

  #[test]
  fn finalize_leaves_an_off_grid_onset_alone() {
    // 100 ms is 100 ms from either neighbouring boundary (0 / 200), outside the
    // 50 ms window: unmoved.
    let raw = vec![LoopEvent::on(ms(100), 30), LoopEvent::off(ms(400), 30)];
    let out = finalize_recording(raw, ms(1000), ms(200));
    assert_eq!(out[0].elapsed, ms(100));
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
    let out = finalize_recording(raw, ms(1000), ms(200));
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|e| e.pitch == 9));
  }

  #[test]
  fn finalize_leaves_a_held_at_end_note_as_an_unpaired_on() {
    // on with no off before the boundary: kept as on-only; playback wrap cuts it.
    let raw = vec![LoopEvent::on(ms(800), 12)];
    let out = finalize_recording(raw, ms(1000), ms(200));
    assert_eq!(out.len(), 1);
    assert!(out[0].on);
  }

  #[test]
  fn playback_emits_events_as_the_playhead_passes_them() {
    let events = vec![LoopEvent::on(ms(100), 30), LoopEvent::off(ms(400), 30)];
    let mut pb = Playback::new(ms(1000));
    assert_eq!(pb.step(&events, ms(50)), vec![]); // before the on
    assert_eq!(pb.step(&events, ms(150)), vec![PlayAction::On(30, Timbre::default())]);
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
    assert_eq!(out, vec![PlayAction::ReleaseAll, PlayAction::On(30, Timbre::default())]);
  }

  #[test]
  fn playback_wrap_cuts_a_held_at_end_note_via_release_all() {
    // A note on at 800 ms with no off: it sounds until the wrap, where ReleaseAll
    // cuts it -- the boundary cut, with no stored end-of-loop note-off.
    let events = vec![LoopEvent::on(ms(800), 12)];
    let mut pb = Playback::new(ms(1000));
    assert_eq!(pb.step(&events, ms(850)), vec![PlayAction::On(12, Timbre::default())]);
    let out = pb.step(&events, ms(1050));
    assert_eq!(out[0], PlayAction::ReleaseAll);
  }

  // ---- transport (LoopStore) ----

  fn store() -> LoopStore {
    LoopStore::new(16, ms(200)) // 300 bpm cycle
  }

  #[test]
  fn play_after_recording_stores_and_sounds_the_active_slot() {
    let mut s = store();
    s.set_active(3);
    s.start_recording(ms(0));
    s.record_note(ms(100), 30, true);
    s.record_note(ms(400), 30, false);
    s.play(ms(1000));
    assert!(!s.is_recording());
    assert_eq!(s.sounding, Some(3));
    assert!(s.is_occupied(3));
    assert_eq!(s.slots[3].loop_duration, Some(ms(1000)));
    assert_eq!(s.slots[3].events.len(), 2);
  }

  #[test]
  fn stop_while_recording_stores_without_sounding() {
    let mut s = store();
    s.start_recording(ms(0));
    s.record_note(ms(50), 10, true);
    s.record_note(ms(200), 10, false);
    s.stop(ms(500));
    assert!(!s.is_recording());
    assert!(s.is_occupied(0));
    assert_eq!(s.sounding, None, "stop finalizes but does not play");
  }

  #[test]
  fn play_on_an_empty_slot_is_a_noop() {
    let mut s = store();
    s.set_active(5);
    s.play(ms(100)); // nothing recorded, slot empty
    assert_eq!(s.sounding, None);
  }

  #[test]
  fn stop_silences_the_active_sounding_slot() {
    let mut s = store();
    s.start_recording(ms(0));
    s.record_note(ms(10), 1, true);
    s.play(ms(500)); // slot 0 sounding
    assert_eq!(s.sounding, Some(0));
    s.stop(ms(600)); // active==sounding==0 -> silence
    assert_eq!(s.sounding, None);
  }

  #[test]
  fn only_one_slot_sounds_at_a_time() {
    let mut s = store();
    // Record slot 0 and play it.
    s.start_recording(ms(0));
    s.record_note(ms(10), 1, true);
    s.play(ms(500));
    assert_eq!(s.sounding, Some(0));
    // Record slot 1 and play it -> slot 0 stops, slot 1 sounds.
    s.set_active(1);
    s.start_recording(ms(600));
    s.record_note(ms(610), 2, true);
    s.play(ms(1100));
    assert_eq!(s.sounding, Some(1));
  }

  #[test]
  fn recording_over_the_sounding_active_slot_stops_it() {
    let mut s = store();
    s.start_recording(ms(0));
    s.record_note(ms(10), 1, true);
    s.play(ms(500)); // slot 0 sounding
    s.start_recording(ms(600)); // re-record active slot 0
    assert_eq!(s.sounding, None, "re-recording the sounding slot stops it");
  }

  #[test]
  fn a_too_short_recording_is_discarded() {
    let mut s = store();
    s.start_recording(ms(100));
    s.play(ms(105)); // 5 ms < MIN_LOOP
    assert!(!s.is_occupied(0));
    assert_eq!(s.sounding, None);
  }

  #[test]
  fn record_captures_the_timbre_and_playback_carries_it() {
    use crate::types::Waveform;
    let saw = Timbre { waveform: Waveform::Saw, ..Timbre::default() };
    let mut s = store();
    s.start_recording(ms(0));
    s.record_note_with(ms(100), 30, true, saw);
    s.record_note(ms(400), 30, false); // off keeps the default timbre
    s.play(ms(1000));
    let on = s.slots[0].events.iter().find(|e| e.on).unwrap();
    assert_eq!(on.timbre.waveform, Waveform::Saw, "on-event keeps the captured timbre");
    let mut pb = Playback::new(ms(1000));
    assert_eq!(pb.step(&s.slots[0].events, ms(150)), vec![PlayAction::On(30, saw)]);
  }

  #[test]
  fn set_active_does_not_change_what_is_sounding() {
    let mut s = store();
    s.start_recording(ms(0));
    s.record_note(ms(10), 1, true);
    s.play(ms(500)); // slot 0 sounding
    s.set_active(7);
    assert_eq!(s.sounding, Some(0), "active is just the control target");
    assert_eq!(s.active, 7);
  }
}
