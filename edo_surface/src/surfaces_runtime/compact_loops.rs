//! Freehand, per-grid compact loops for the sustain/chord surfaces instrument.
//!
//! Time is represented as monotonic nanoseconds supplied by the owning grid
//! thread. The module owns no clock and touches no audio: every operation returns
//! ordered spawn/release actions for `SurfaceSink`.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

pub const SLOTS: usize = 11;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SourceKey {
  Finger(i32, i32),
  Sustain(i32),
  Chord(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockCell {
  Start,
  Stop,
  SelectTargetLoop,
  PhaseMode,
  Slot(usize),
}

/// The top-left cell belongs to CLEAR SUSTAIN. Everything else in the 8x2 block
/// is compact-loop state.
pub fn block_cell(rect: [i32; 4], cell: (i32, i32)) -> Option<BlockCell> {
  let [x0, y0, x1, y1] = rect;
  let (x, y) = cell;
  if x < x0 || x > x1 || y < y0 || y > y1 {
    return None;
  }
  let dx = (x - x0) as usize;
  match (y - y0, dx) {
    (0, 1) => Some(BlockCell::SelectTargetLoop),
    (0, 2..=7) => Some(BlockCell::Slot(dx - 2)),
    (1, 0) => Some(BlockCell::Stop),
    (1, 1) => Some(BlockCell::Start),
    (1, 2..=6) => Some(BlockCell::Slot(6 + dx - 2)),
    (1, 7) => Some(BlockCell::PhaseMode),
    _ => None,
  }
}

pub fn slot_cell(rect: [i32; 4], slot: usize) -> (i32, i32) {
  if slot < 6 {
    (rect[0] + 2 + slot as i32, rect[1])
  } else {
    (rect[0] + 2 + (slot - 6) as i32, rect[1] + 1)
  }
}

pub fn start_cell(rect: [i32; 4]) -> (i32, i32) {
  (rect[0] + 1, rect[1] + 1)
}

pub fn stop_cell(rect: [i32; 4]) -> (i32, i32) {
  (rect[0], rect[1] + 1)
}

pub fn select_cell(rect: [i32; 4]) -> (i32, i32) {
  (rect[0] + 1, rect[1])
}

pub fn phase_cell(rect: [i32; 4]) -> (i32, i32) {
  (rect[2], rect[3])
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LoopInterval {
  pub pitch: i32,
  pub start_ns: u64,
  /// `None` is a full-cycle drone; finite values are strictly less than duration.
  pub duration_ns: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LoopSlot {
  pub duration_ns: u64,
  pub intervals: Vec<LoopInterval>,
}

impl LoopSlot {
  pub fn normalized(mut self) -> Option<Self> {
    if self.duration_ns == 0 || self.intervals.is_empty() {
      return None;
    }
    self.intervals.retain_mut(|interval| {
      interval.start_ns %= self.duration_ns;
      match interval.duration_ns {
        Some(0) => false,
        Some(duration) if duration >= self.duration_ns => {
          interval.duration_ns = None;
          true
        }
        _ => true,
      }
    });
    (!self.intervals.is_empty()).then_some(self)
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseMode {
  KeepRunning,
  Restart,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Occurrence {
  interval: usize,
  lap: i64,
}

#[derive(Clone, Debug)]
struct Playback {
  gate: bool,
  epoch_ns: Option<u64>,
  last_tick_ns: u64,
  live: HashMap<Occurrence, u64>,
}

impl Default for Playback {
  fn default() -> Self {
    Self { gate: false, epoch_ns: None, last_tick_ns: 0, live: HashMap::new() }
  }
}

#[derive(Clone, Copy, Debug)]
struct LiveSource {
  pitch: i32,
  onset_ns: u64,
}

#[derive(Clone, Copy, Debug)]
struct CapturedLife {
  pitch: i32,
  onset_ns: u64,
  end_ns: Option<u64>,
  start_phase: Option<u64>,
}

#[derive(Clone, Debug)]
struct Recording {
  slot: usize,
  started_ns: u64,
  first_take: bool,
  phase_epoch_ns: u64,
  captured: Vec<CapturedLife>,
  open: HashMap<SourceKey, usize>,
  pending_play: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceAction {
  Spawn { seq: u64, pitch: i32 },
  Release { seq: u64 },
}

#[derive(Clone, Debug, Default)]
pub struct StopResult {
  pub actions: Vec<VoiceAction>,
  pub changed: bool,
}

#[derive(Debug)]
pub struct CompactLoops {
  pub slots: [Option<LoopSlot>; SLOTS],
  pub target_loop_slot: usize,
  pub selecting_target_loop: bool,
  pub phase_mode: PhaseMode,
  pub clear_tap_ns: u64,
  playback: [Playback; SLOTS],
  sources: HashMap<SourceKey, LiveSource>,
  recording: Option<Recording>,
  next_seq: u64,
}

impl CompactLoops {
  pub fn new(clear_tap_ms: u64) -> Self {
    Self {
      slots: std::array::from_fn(|_| None),
      target_loop_slot: 0,
      selecting_target_loop: false,
      phase_mode: PhaseMode::KeepRunning,
      clear_tap_ns: clear_tap_ms.saturating_mul(1_000_000),
      playback: std::array::from_fn(|_| Playback::default()),
      sources: HashMap::new(),
      recording: None,
      next_seq: 1 << 62,
    }
  }

  pub fn recording(&self) -> bool {
    self.recording.is_some()
  }

  pub fn slot_sounding(&self, slot: usize) -> bool {
    self.playback.get(slot).is_some_and(|p| p.gate)
  }

  pub fn live_pitches(&self) -> Vec<i32> {
    let mut pitches = Vec::new();
    for slot in 0..SLOTS {
      let Some(stored) = self.slots[slot].as_ref() else { continue };
      pitches.extend(
        self.playback[slot]
          .live
          .keys()
          .map(|occurrence| stored.intervals[occurrence.interval].pitch),
      );
    }
    pitches
  }

  pub fn source_on(&mut self, key: SourceKey, pitch: i32, now_ns: u64) {
    if self.sources.contains_key(&key) {
      self.source_off(key, now_ns);
    }
    let source = LiveSource { pitch, onset_ns: now_ns };
    self.sources.insert(key, source);
    let Some(recording) = self.recording.as_mut() else { return };
    let phase = (!recording.first_take).then(|| {
      signed_phase(now_ns, recording.phase_epoch_ns, self.slots[recording.slot].as_ref().unwrap().duration_ns)
    });
    let index = recording.captured.len();
    recording.captured.push(CapturedLife {
      pitch,
      onset_ns: now_ns,
      end_ns: None,
      start_phase: phase,
    });
    recording.open.insert(key, index);
  }

  pub fn source_off(&mut self, key: SourceKey, now_ns: u64) {
    self.sources.remove(&key);
    if let Some(recording) = self.recording.as_mut() {
      if let Some(index) = recording.open.remove(&key) {
        recording.captured[index].end_ns = Some(now_ns);
      }
    }
  }

  /// Re-key ownership without creating a musical edge (finger -> sustain and back).
  pub fn source_rekey(&mut self, from: SourceKey, to: SourceKey) {
    if from == to {
      return;
    }
    if let Some(source) = self.sources.remove(&from) {
      self.sources.insert(to, source);
    }
    if let Some(recording) = self.recording.as_mut() {
      if let Some(index) = recording.open.remove(&from) {
        recording.open.insert(to, index);
      }
    }
  }

  /// A live nominal-pitch move is represented as one interval ending and another
  /// beginning, while the audio voice itself glides continuously.
  pub fn source_move(&mut self, key: SourceKey, pitch: i32, now_ns: u64) {
    self.source_off(key, now_ns);
    self.source_on(key, pitch, now_ns);
  }

  pub fn select_target_loop_press(&mut self) {
    if self.recording.is_none() {
      self.selecting_target_loop = !self.selecting_target_loop;
    }
  }

  /// Returns true when the slot press was consumed as target-loop selection.
  pub fn choose_target_loop(&mut self, slot: usize) -> bool {
    if !self.selecting_target_loop || self.recording.is_some() || slot >= SLOTS {
      return false;
    }
    self.target_loop_slot = slot;
    self.selecting_target_loop = false;
    true
  }

  pub fn start_recording(&mut self, now_ns: u64) {
    if self.recording.is_some() {
      return;
    }
    let slot = self.target_loop_slot;
    let first_take = self.slots[slot].is_none();
    let phase_epoch_ns = if first_take {
      now_ns
    } else {
      self.playback[slot].epoch_ns.unwrap_or(now_ns)
    };
    let duration = self.slots[slot].as_ref().map(|s| s.duration_ns);
    let mut captured = Vec::with_capacity(self.sources.len());
    let mut open = HashMap::with_capacity(self.sources.len());
    for (key, source) in &self.sources {
      let start_phase = duration.map(|d| signed_phase(source.onset_ns, phase_epoch_ns, d));
      open.insert(*key, captured.len());
      captured.push(CapturedLife {
        pitch: source.pitch,
        onset_ns: source.onset_ns,
        end_ns: None,
        start_phase,
      });
    }
    self.recording = Some(Recording {
      slot,
      started_ns: now_ns,
      first_take,
      phase_epoch_ns,
      captured,
      open,
      pending_play: false,
    });
  }

  pub fn stop_recording(&mut self, now_ns: u64) -> StopResult {
    let Some(mut recording) = self.recording.take() else {
      return StopResult::default();
    };
    if now_ns.saturating_sub(recording.started_ns) < self.clear_tap_ns {
      let actions = self.clear_slot(recording.slot);
      return StopResult { actions, changed: true };
    }
    for index in recording.open.into_values() {
      recording.captured[index].end_ns = Some(now_ns);
    }
    let duration = if recording.first_take {
      now_ns.saturating_sub(recording.started_ns)
    } else {
      self.slots[recording.slot].as_ref().unwrap().duration_ns
    };
    if duration == 0 {
      return StopResult::default();
    }
    let mut added = Vec::new();
    for life in recording.captured {
      let end = life.end_ns.unwrap_or(now_ns).max(life.onset_ns);
      let lived = end.saturating_sub(life.onset_ns);
      if lived == 0 {
        continue;
      }
      let start_ns = life
        .start_phase
        .unwrap_or_else(|| signed_phase(life.onset_ns, recording.started_ns, duration));
      added.push(LoopInterval {
        pitch: life.pitch,
        start_ns,
        duration_ns: (lived < duration).then_some(lived),
      });
    }
    // An ordinary-length capture with no completed source lifetimes changes
    // nothing.  In particular, an empty overdub is not a persistence event.
    if added.is_empty() {
      return StopResult::default();
    }
    if recording.first_take {
      self.slots[recording.slot] = Some(LoopSlot { duration_ns: duration, intervals: added });
    } else {
      self.slots[recording.slot].as_mut().unwrap().intervals.extend(added);
    }
    let mut actions = Vec::new();
    if recording.first_take && recording.pending_play && self.slots[recording.slot].is_some() {
      actions.extend(self.turn_on(recording.slot, now_ns));
    }
    StopResult { actions, changed: true }
  }

  pub fn slot_press(&mut self, slot: usize, now_ns: u64) -> Vec<VoiceAction> {
    if slot >= SLOTS || self.choose_target_loop(slot) {
      return Vec::new();
    }
    if let Some(recording) = self.recording.as_mut() {
      if recording.first_take && recording.slot == slot && self.slots[slot].is_none() {
        recording.pending_play = !recording.pending_play;
        return Vec::new();
      }
    }
    if self.slots[slot].is_none() {
      return Vec::new();
    }
    if self.playback[slot].gate {
      self.turn_off(slot)
    } else {
      let reset_epoch = self.phase_mode == PhaseMode::Restart
        || self.playback[slot].epoch_ns.is_none();
      let actions = self.turn_on(slot, now_ns);
      if reset_epoch {
        if let Some(recording) = self.recording.as_mut().filter(|r| r.slot == slot) {
          recording.phase_epoch_ns = now_ns;
        }
      }
      actions
    }
  }

  fn turn_on(&mut self, slot: usize, now_ns: u64) -> Vec<VoiceAction> {
    if self.slots[slot].is_none() {
      return Vec::new();
    }
    if self.phase_mode == PhaseMode::Restart || self.playback[slot].epoch_ns.is_none() {
      self.playback[slot].epoch_ns = Some(now_ns);
    }
    self.playback[slot].gate = true;
    self.playback[slot].last_tick_ns = now_ns;
    self.reconstruct(slot, now_ns)
  }

  fn turn_off(&mut self, slot: usize) -> Vec<VoiceAction> {
    let playback = &mut self.playback[slot];
    playback.gate = false;
    if self.phase_mode == PhaseMode::Restart {
      playback.epoch_ns = None;
    }
    playback.live.drain().map(|(_, seq)| VoiceAction::Release { seq }).collect()
  }

  fn clear_slot(&mut self, slot: usize) -> Vec<VoiceAction> {
    let actions = self.turn_off(slot);
    self.playback[slot] = Playback::default();
    self.slots[slot] = None;
    actions
  }

  pub fn toggle_phase_mode(&mut self, now_ns: u64) -> Vec<VoiceAction> {
    self.phase_mode = match self.phase_mode {
      PhaseMode::KeepRunning => PhaseMode::Restart,
      PhaseMode::Restart => PhaseMode::KeepRunning,
    };
    if self.phase_mode == PhaseMode::KeepRunning {
      return Vec::new();
    }
    if let Some(recording) = self.recording.as_mut() {
      recording.phase_epoch_ns = now_ns;
    }
    let mut actions = Vec::new();
    for slot in 0..SLOTS {
      let sounding = self.playback[slot].gate;
      actions.extend(self.turn_off(slot));
      if sounding {
        actions.extend(self.turn_on(slot, now_ns));
      }
    }
    actions
  }

  pub fn shift_target_loop(&mut self, delta: i32) -> (Vec<(u64, i32)>, bool) {
    if self.recording.is_some() {
      return (Vec::new(), false);
    }
    let Some(slot) = self.slots[self.target_loop_slot].as_mut() else {
      return (Vec::new(), false);
    };
    for interval in &mut slot.intervals {
      interval.pitch += delta;
    }
    let pitches: HashMap<usize, i32> = slot
      .intervals
      .iter()
      .enumerate()
      .map(|(index, interval)| (index, interval.pitch))
      .collect();
    let moved = self.playback[self.target_loop_slot]
      .live
      .iter()
      .filter_map(|(occurrence, seq)| pitches.get(&occurrence.interval).map(|p| (*seq, *p)))
      .collect();
    (moved, true)
  }

  pub fn tick(&mut self, now_ns: u64) -> Vec<VoiceAction> {
    let mut actions = Vec::new();
    for slot in 0..SLOTS {
      if !self.playback[slot].gate || self.slots[slot].is_none() {
        continue;
      }
      let last = self.playback[slot].last_tick_ns;
      if now_ns <= last {
        continue;
      }
      actions.extend(self.events_between(slot, last, now_ns));
      self.playback[slot].last_tick_ns = now_ns;
    }
    actions
  }

  fn reconstruct(&mut self, slot: usize, now_ns: u64) -> Vec<VoiceAction> {
    let desired = self.desired_occurrences(slot, now_ns);
    let stale: Vec<Occurrence> = self.playback[slot]
      .live
      .keys()
      .copied()
      .filter(|occurrence| !desired.contains(occurrence))
      .collect();
    let mut actions = Vec::new();
    for occurrence in stale {
      if let Some(seq) = self.playback[slot].live.remove(&occurrence) {
        actions.push(VoiceAction::Release { seq });
      }
    }
    let mut fresh: Vec<Occurrence> = desired
      .into_iter()
      .filter(|occurrence| !self.playback[slot].live.contains_key(occurrence))
      .collect();
    pseudo_shuffle(&mut fresh, now_ns ^ slot as u64);
    for occurrence in fresh {
      let pitch = self.slots[slot].as_ref().unwrap().intervals[occurrence.interval].pitch;
      let seq = self.alloc_seq();
      self.playback[slot].live.insert(occurrence, seq);
      actions.push(VoiceAction::Spawn { seq, pitch });
    }
    actions
  }

  fn desired_occurrences(&self, slot: usize, now_ns: u64) -> HashSet<Occurrence> {
    let stored = self.slots[slot].as_ref().unwrap().clone();
    let epoch = self.playback[slot].epoch_ns.unwrap();
    let elapsed = now_ns.saturating_sub(epoch);
    let lap = (elapsed / stored.duration_ns) as i64;
    let phase = elapsed % stored.duration_ns;
    let mut desired = HashSet::new();
    for (index, interval) in stored.intervals.iter().enumerate() {
      match interval.duration_ns {
        None => {
          desired.insert(Occurrence { interval: index, lap: 0 });
        }
        Some(duration) => {
          let since = phase as i128 - interval.start_ns as i128;
          let (age, onset_lap) = if since >= 0 {
            (since as u64, lap)
          } else {
            ((since + stored.duration_ns as i128) as u64, lap - 1)
          };
          if age < duration {
            desired.insert(Occurrence { interval: index, lap: onset_lap });
          }
        }
      }
    }
    desired
  }

  fn events_between(&mut self, slot: usize, from_ns: u64, to_ns: u64) -> Vec<VoiceAction> {
    #[derive(Clone, Copy)]
    struct Event {
      at: i128,
      on: bool,
      occurrence: Occurrence,
    }
    let stored = self.slots[slot].as_ref().unwrap().clone();
    let epoch = self.playback[slot].epoch_ns.unwrap() as i128;
    let duration = stored.duration_ns as i128;
    let from = from_ns as i128;
    let to = to_ns as i128;
    let first_lap = ((from - epoch).div_euclid(duration) - 1) as i64;
    let last_lap = ((to - epoch).div_euclid(duration) + 1) as i64;
    let mut events = Vec::new();
    for (index, interval) in stored.intervals.iter().enumerate() {
      let Some(lifetime) = interval.duration_ns else { continue };
      for lap in first_lap..=last_lap {
        let onset = epoch + lap as i128 * duration + interval.start_ns as i128;
        let release = onset + lifetime as i128;
        let occurrence = Occurrence { interval: index, lap };
        if onset > from && onset <= to {
          events.push(Event { at: onset, on: true, occurrence });
        }
        if release > from && release <= to {
          events.push(Event { at: release, on: false, occurrence });
        }
      }
    }
    // Half-open intervals: release before onset at an identical boundary.
    events.sort_by_key(|event| (event.at, event.on));
    let mut actions = Vec::new();
    let mut cursor = 0;
    while cursor < events.len() {
      let at = events[cursor].at;
      let end = events[cursor..]
        .iter()
        .position(|event| event.at != at)
        .map(|offset| cursor + offset)
        .unwrap_or(events.len());
      let mut ons: Vec<Event> = events[cursor..end].iter().copied().filter(|e| e.on).collect();
      pseudo_shuffle(&mut ons, at as u64 ^ slot as u64);
      for event in events[cursor..end].iter().filter(|event| !event.on) {
        if let Some(seq) = self.playback[slot].live.remove(&event.occurrence) {
          actions.push(VoiceAction::Release { seq });
        }
      }
      for event in ons {
        if self.playback[slot].live.contains_key(&event.occurrence) {
          continue;
        }
        let pitch = stored.intervals[event.occurrence.interval].pitch;
        let seq = self.alloc_seq();
        self.playback[slot].live.insert(event.occurrence, seq);
        actions.push(VoiceAction::Spawn { seq, pitch });
      }
      cursor = end;
    }
    actions
  }

  fn alloc_seq(&mut self) -> u64 {
    let seq = self.next_seq;
    self.next_seq = self.next_seq.wrapping_add(1);
    seq
  }
}

fn signed_phase(time_ns: u64, epoch_ns: u64, duration_ns: u64) -> u64 {
  (time_ns as i128 - epoch_ns as i128).rem_euclid(duration_ns as i128) as u64
}

fn pseudo_shuffle<T>(items: &mut [T], mut state: u64) {
  if state == 0 {
    state = 0x9e37_79b9_7f4a_7c15;
  }
  for i in (1..items.len()).rev() {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    items.swap(i, state as usize % (i + 1));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const RECT: [i32; 4] = [8, 14, 15, 15];

  #[test]
  fn layout_has_eleven_slots_and_four_controls() {
    for slot in 0..SLOTS {
      assert_eq!(block_cell(RECT, slot_cell(RECT, slot)), Some(BlockCell::Slot(slot)));
    }
    assert_eq!(block_cell(RECT, (8, 14)), None);
    assert_eq!(select_cell(RECT), (9, 14));
    assert_eq!(start_cell(RECT), (9, 15));
    assert_eq!(block_cell(RECT, start_cell(RECT)), Some(BlockCell::Start));
    assert_eq!(block_cell(RECT, stop_cell(RECT)), Some(BlockCell::Stop));
    assert_eq!(block_cell(RECT, select_cell(RECT)), Some(BlockCell::SelectTargetLoop));
    assert_eq!(block_cell(RECT, phase_cell(RECT)), Some(BlockCell::PhaseMode));
  }

  #[test]
  fn first_take_uses_actual_onset_and_wraps() {
    let mut loops = CompactLoops::new(300);
    loops.source_on(SourceKey::Finger(1, 1), 9, 5_000_000_000);
    loops.start_recording(10_000_000_000);
    loops.source_off(SourceKey::Finger(1, 1), 13_000_000_000);
    let result = loops.stop_recording(20_000_000_000);
    assert!(result.changed);
    let interval = &loops.slots[0].as_ref().unwrap().intervals[0];
    assert_eq!(interval.start_ns, 5_000_000_000);
    assert_eq!(interval.duration_ns, Some(8_000_000_000));
  }

  #[test]
  fn a_voice_living_at_least_one_cycle_is_a_drone() {
    let mut loops = CompactLoops::new(300);
    loops.source_on(SourceKey::Chord(1), 12, 4_000_000_000);
    loops.start_recording(10_000_000_000);
    loops.source_off(SourceKey::Chord(1), 15_000_000_000);
    loops.stop_recording(20_000_000_000);
    assert_eq!(loops.slots[0].as_ref().unwrap().intervals[0].duration_ns, None);
  }

  #[test]
  fn sub_threshold_stop_clears_target_loop() {
    let mut loops = CompactLoops::new(300);
    loops.slots[0] = Some(LoopSlot {
      duration_ns: 1_000,
      intervals: vec![LoopInterval { pitch: 1, start_ns: 0, duration_ns: None }],
    });
    loops.start_recording(1_000_000_000);
    let result = loops.stop_recording(1_299_999_999);
    assert!(result.changed);
    assert!(loops.slots[0].is_none());
  }

  #[test]
  fn silent_first_take_leaves_slot_empty() {
    let mut loops = CompactLoops::new(300);
    loops.start_recording(0);
    let result = loops.stop_recording(500_000_000);
    assert!(!result.changed);
    assert!(loops.slots[0].is_none());
  }

  #[test]
  fn eventless_overdub_is_a_noop() {
    let mut loops = CompactLoops::new(300);
    loops.slots[0] = Some(LoopSlot {
      duration_ns: 1_000_000_000,
      intervals: vec![LoopInterval { pitch: 3, start_ns: 0, duration_ns: None }],
    });
    let before = loops.slots[0].clone();
    loops.start_recording(2_000_000_000);
    let result = loops.stop_recording(2_500_000_000);
    assert!(!result.changed);
    assert_eq!(loops.slots[0], before);
  }

  #[test]
  fn selecting_a_target_loop_consumes_the_slot_press_without_toggling_playback() {
    let mut loops = CompactLoops::new(300);
    loops.slots[1] = Some(LoopSlot {
      duration_ns: 100,
      intervals: vec![LoopInterval { pitch: 4, start_ns: 0, duration_ns: None }],
    });
    loops.select_target_loop_press();
    assert!(loops.slot_press(1, 20).is_empty());
    assert_eq!(loops.target_loop_slot, 1);
    assert!(!loops.selecting_target_loop);
    assert!(!loops.slot_sounding(1));
  }

  #[test]
  fn first_take_slot_touch_queues_playback_at_the_closed_boundary() {
    let mut loops = CompactLoops::new(300);
    loops.start_recording(0);
    loops.source_on(SourceKey::Finger(2, 2), 6, 100_000_000);
    loops.source_off(SourceKey::Finger(2, 2), 400_000_000);
    assert!(loops.slot_press(0, 450_000_000).is_empty());
    let result = loops.stop_recording(500_000_000);
    assert!(result.changed);
    assert!(loops.slot_sounding(0));
    assert!(result.actions.is_empty(), "the interval does not contain phase zero");
  }

  #[test]
  fn several_slots_play_and_gate_independently() {
    let mut loops = CompactLoops::new(300);
    for (slot, pitch) in [(0, 3), (1, 3)] {
      loops.slots[slot] = Some(LoopSlot {
        duration_ns: 100,
        intervals: vec![LoopInterval { pitch, start_ns: 0, duration_ns: None }],
      });
    }
    assert_eq!(loops.slot_press(0, 0).len(), 1);
    assert_eq!(loops.slot_press(1, 0).len(), 1);
    assert!(loops.slot_sounding(0) && loops.slot_sounding(1));
    assert_eq!(loops.slot_press(0, 10).len(), 1);
    assert!(!loops.slot_sounding(0));
    assert!(loops.slot_sounding(1));
  }

  #[test]
  fn keep_running_and_restart_choose_different_retrigger_phases() {
    let mut loops = CompactLoops::new(300);
    loops.slots[0] = Some(LoopSlot {
      duration_ns: 100,
      intervals: vec![LoopInterval { pitch: 1, start_ns: 40, duration_ns: Some(30) }],
    });
    assert!(loops.slot_press(0, 0).is_empty());
    assert!(loops.tick(50).iter().any(|a| matches!(a, VoiceAction::Spawn { .. })));
    assert!(loops.slot_press(0, 55).iter().any(|a| matches!(a, VoiceAction::Release { .. })));
    assert!(loops.slot_press(0, 65).iter().any(|a| matches!(a, VoiceAction::Spawn { .. })));
    loops.slot_press(0, 70);
    loops.toggle_phase_mode(70);
    assert!(loops.slot_press(0, 80).is_empty(), "restart begins at empty phase zero");
  }
}
