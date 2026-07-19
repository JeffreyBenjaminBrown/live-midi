use edo_surface::midi;
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use super::remap::apply_snapshot as apply_remap_snapshot;
use super::state::{RemapSnapshot, RemappableEdoState};
use super::STOP_REQUESTED;

const MIN_LOOP_DURATION: Duration = Duration::from_millis(10);
const OVERDUB_CUT_BACK: Duration = Duration::from_millis(10);
const CLEAR_GESTURE_WINDOW: Duration = Duration::from_millis(100);
const CONTROL_FLASH: Duration = Duration::from_millis(100);
const PLAYBACK_SLEEP: Duration = Duration::from_millis(1);
const RECORD_TRACE_ENV: &str = "MIDI_PULSE_RECORD_TRACE";

static RECORD_TRACE_STARTED: OnceLock<Instant> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum OutputSource {
  Live { original_note: u8 },
  Playback { original_note: u8 },
}

#[derive(Clone)]
pub(crate) struct SharedOutputGate {
  inner: Arc<Mutex<OutputGate>>,
}

impl SharedOutputGate {
  pub(crate) fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
    SharedOutputGate {
      inner: Arc::new(Mutex::new(OutputGate::new(tx))),
    }
  }

  pub(crate) fn send_raw(&self, message: Vec<u8>) -> bool {
    self.inner.lock().unwrap().send_raw(message)
  }

  pub(crate) fn send_note_message(&self, source: OutputSource, message: &[u8]) -> bool {
    self.inner.lock().unwrap().send_note_message(source, message)
  }

  pub(crate) fn release_source(&self, source: OutputSource, output: OutputNote) -> bool {
    self.inner.lock().unwrap().release_source(source, output, 0)
  }
}

pub(crate) struct OutputGate {
  tx: mpsc::Sender<Vec<u8>>,
  active: HashMap<OutputNote, HashSet<OutputSource>>,
}

impl OutputGate {
  fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
    OutputGate {
      tx,
      active: HashMap::new(),
    }
  }

  fn send_raw(&mut self, message: Vec<u8>) -> bool {
    self.tx.send(message).is_ok()
  }

  fn send_note_message(&mut self, source: OutputSource, message: &[u8]) -> bool {
    if message.len() < 3 || !midi::is_note_event(message) {
      return self.send_raw(message.to_vec());
    }
    let output = OutputNote {
      channel: message[0] & 0x0f,
      note: message[1],
    };
    if midi::is_note_on(message) {
      self.add_source(source, output, message[2])
    } else if midi::is_note_off(message) {
      self.release_source(source, output, message[2])
    } else {
      false
    }
  }

  fn add_source(&mut self, source: OutputSource, output: OutputNote, velocity: u8) -> bool {
    let sources = self.active.entry(output).or_default();
    let was_empty = sources.is_empty();
    sources.insert(source);
    if was_empty {
      return self
        .tx
        .send(vec![0x90 | output.channel, output.note, velocity])
        .is_ok();
    }
    false
  }

  fn release_source(&mut self, source: OutputSource, output: OutputNote, velocity: u8) -> bool {
    let Some(sources) = self.active.get_mut(&output) else {
      return false;
    };
    sources.remove(&source);
    if sources.is_empty() {
      self.active.remove(&output);
      return self
        .tx
        .send(vec![0x80 | output.channel, output.note, velocity])
        .is_ok();
    }
    false
  }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OutputNote {
  pub(crate) channel: u8,
  pub(crate) note: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecordControl {
  Start,
  Stop,
  Loop,
  Arm,
  EraseOns,
  EndAll,
  Rscm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecordedEventKind {
  NoteOn,
  NoteOff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecordedEvent {
  pub(crate) elapsed: Duration,
  pub(crate) original_note: u8,
  pub(crate) output_channel: u8,
  pub(crate) output_note: u8,
  pub(crate) kind: RecordedEventKind,
  pub(crate) velocity: u8,
}

impl RecordedEvent {
  fn output(self) -> OutputNote {
    OutputNote {
      channel: self.output_channel,
      note: self.output_note,
    }
  }
}

pub(crate) fn trace_enabled() -> bool {
  std::env::var_os(RECORD_TRACE_ENV).is_some()
}

pub(crate) fn trace_midi_event(
  label: &str,
  message: &[u8],
  runtime: &RecordRuntime,
  now: Instant,
) {
  if !trace_enabled() {
    return;
  }
  trace_line(label, &format!("midi={}", format_midi_message(message)), runtime, now);
}

pub(crate) fn trace_runtime(label: &str, runtime: &RecordRuntime, now: Instant) {
  if !trace_enabled() {
    return;
  }
  trace_line(label, "", runtime, now);
}

fn trace_line(label: &str, detail: &str, runtime: &RecordRuntime, now: Instant) {
  let trace_started = RECORD_TRACE_STARTED.get_or_init(Instant::now);
  let duration = runtime.slot.loop_duration;
  let phase = runtime.loop_phase(now);
  eprintln!(
    "record-trace {:.3}ms {label}{} playback={} recording={} armed={} erase={} rscm={} phase={} loop={} next={} events={} playback_down={}",
    trace_started.elapsed().as_secs_f64() * 1000.0,
    if detail.is_empty() {
      String::new()
    } else {
      format!(" {detail}")
    },
    runtime.playback,
    runtime.recording,
    runtime.armed,
    runtime.erase_ons_held,
    runtime.rscm,
    format_duration(phase),
    format_duration(duration),
    runtime.next_playback_event,
    runtime.slot.events.len(),
    runtime.playback_down.len(),
  );
}

fn format_midi_message(message: &[u8]) -> String {
  if message.len() >= 3 && midi::is_note_event(message) {
    let kind = if midi::is_note_on(message) {
      "on"
    } else if midi::is_note_off(message) {
      "off"
    } else {
      "note"
    };
    return format!(
      "{kind} ch={} note={} vel={} raw={:02x?}",
      (message[0] & 0x0f) + 1,
      message[1],
      message[2],
      message
    );
  }
  format!("raw={message:02x?}")
}

fn format_duration(duration: Option<Duration>) -> String {
  duration
    .map(|duration| format!("{:.3}ms", duration.as_secs_f64() * 1000.0))
    .unwrap_or_else(|| "none".to_string())
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RecordingSlot {
  pub(crate) loop_duration: Option<Duration>,
  pub(crate) events: Vec<RecordedEvent>,
  pub(crate) stored_map: Option<RemapSnapshot>,
}

impl RecordingSlot {
  pub(crate) fn is_empty(&self) -> bool {
    self.loop_duration.is_none()
  }
}

#[derive(Clone, Debug)]
pub(crate) struct RecordRuntime {
  pub(crate) slot: RecordingSlot,
  pub(crate) armed: bool,
  pub(crate) recording: bool,
  pub(crate) playback: bool,
  pub(crate) erase_ons_held: bool,
  pub(crate) rscm: bool,
  pub(crate) recording_start: Option<Instant>,
  pub(crate) playback_start: Option<Instant>,
  pub(crate) last_start_press: Option<Instant>,
  pub(crate) recorded_down: HashMap<u8, OutputNote>,
  pub(crate) playback_down: HashMap<u8, OutputNote>,
  pub(crate) next_playback_event: usize,
  pub(crate) last_erase_elapsed: Option<Duration>,
  pub(crate) stop_flash_until: Option<Instant>,
}

impl RecordRuntime {
  pub(crate) fn new() -> Self {
    RecordRuntime {
      slot: RecordingSlot::default(),
      armed: false,
      recording: false,
      playback: false,
      erase_ons_held: false,
      rscm: false,
      recording_start: None,
      playback_start: None,
      last_start_press: None,
      recorded_down: HashMap::new(),
      playback_down: HashMap::new(),
      next_playback_event: 0,
      last_erase_elapsed: None,
      stop_flash_until: None,
    }
  }

  pub(crate) fn key_down(
    &mut self,
    control: RecordControl,
    now: Instant,
    snapshot: RemapSnapshot,
  ) -> RecordAction {
    match control {
      RecordControl::Arm => {
        self.armed = !self.armed;
        RecordAction::default()
      }
      RecordControl::Start => self.start(now),
      RecordControl::Stop => self.stop(now, snapshot),
      RecordControl::Loop => self.loop_button(now, snapshot),
      RecordControl::EraseOns => {
        self.erase_ons_held = true;
        self.last_erase_elapsed = self.current_record_elapsed(now);
        RecordAction::default()
      }
      RecordControl::EndAll => {
        self.end_all(now);
        RecordAction::default()
      }
      RecordControl::Rscm => {
        self.rscm = !self.rscm;
        RecordAction {
          apply_snapshot: if self.rscm && self.playback {
            self.slot.stored_map
          } else {
            None
          },
          ..RecordAction::default()
        }
      }
    }
  }

  pub(crate) fn loop_phase(&self, now: Instant) -> Option<Duration> {
    if self.playback {
      let duration = self.slot.loop_duration?;
      let start = self.playback_start?;
      return Some(wrap_duration(now.duration_since(start), duration));
    }
    if self.recording {
      let start = self.recording_start?;
      return Some(now.duration_since(start));
    }
    Some(Duration::ZERO).filter(|_| self.slot.loop_duration.is_some())
  }

  pub(crate) fn key_up(&mut self, control: RecordControl) -> bool {
    if control == RecordControl::EraseOns {
      self.erase_ons_held = false;
      self.last_erase_elapsed = None;
      return true;
    }
    false
  }

  fn start(&mut self, now: Instant) -> RecordAction {
    self.last_start_press = Some(now);
    if self.slot.is_empty() {
      if self.armed {
        self.recording = true;
        self.playback = false;
        self.recording_start = Some(now);
        self.recorded_down.clear();
      }
      return RecordAction::default();
    }
    self.playback = true;
    self.playback_start = Some(now);
    self.next_playback_event = 0;
    self.playback_down.clear();
    if self.armed {
      self.recording = true;
      self.recording_start = Some(now);
      self.recorded_down = recorded_down_at(&self.slot, Duration::ZERO);
    }
    RecordAction {
      apply_snapshot: if self.rscm { self.slot.stored_map } else { None },
      ..RecordAction::default()
    }
  }

  fn stop(&mut self, now: Instant, snapshot: RemapSnapshot) -> RecordAction {
    let releases = self.playback_releases();
    if self.recording {
      self.finalize_recording(now, snapshot);
      self.armed = false;
    }
    self.playback = false;
    self.playback_start = None;
    self.next_playback_event = 0;
    self.playback_down.clear();
    RecordAction {
      release_playback: releases,
      ..RecordAction::default()
    }
  }

  fn loop_button(&mut self, now: Instant, snapshot: RemapSnapshot) -> RecordAction {
    if self
      .last_start_press
      .is_some_and(|start| now.duration_since(start) <= CLEAR_GESTURE_WINDOW)
    {
      let releases = self.playback_releases();
      self.clear();
      return RecordAction {
        release_playback: releases,
        ..RecordAction::default()
      };
    }
    if !self.recording {
      return RecordAction::default();
    }
    self.finalize_recording(now, snapshot);
    self.armed = false;
    if self.slot.is_empty() {
      self.playback = false;
      self.playback_start = None;
    } else {
      self.playback = true;
      self.playback_start = Some(now);
    }
    self.next_playback_event = 0;
    self.playback_down.clear();
    RecordAction::default()
  }

  fn clear(&mut self) {
    let rscm = self.rscm;
    *self = RecordRuntime::new();
    self.rscm = rscm;
  }

  fn finalize_recording(&mut self, now: Instant, snapshot: RemapSnapshot) {
    let Some(start) = self.recording_start else {
      return;
    };
    let elapsed = now.duration_since(start);
    if elapsed < MIN_LOOP_DURATION {
      self.slot = RecordingSlot::default();
    } else {
      self.slot.loop_duration = Some(elapsed);
      self.slot.stored_map = Some(snapshot);
      self.normalize_events();
    }
    self.recording = false;
    self.recording_start = None;
    self.recorded_down.clear();
    self.erase_ons_held = false;
    self.last_erase_elapsed = None;
  }

  pub(crate) fn record_live_event(&mut self, mut event: RecordedEvent) {
    if !self.recording {
      return;
    }
    let Some(elapsed) = self.current_record_elapsed(Instant::now()) else {
      return;
    };
    if event.elapsed.is_zero() {
      event.elapsed = elapsed;
    }
    if self.erase_ons_held {
      self.erase_swept_to(elapsed);
    }
    match event.kind {
      RecordedEventKind::NoteOn => {
        if self
          .slot
          .loop_duration
          .is_some_and(|duration| note_is_down_at(&self.slot, event.original_note, event.elapsed, duration))
        {
          let cut_elapsed = wrap_sub(event.elapsed, OVERDUB_CUT_BACK, self.slot.loop_duration.unwrap());
          let mut cut = event;
          cut.elapsed = cut_elapsed;
          cut.kind = RecordedEventKind::NoteOff;
          cut.velocity = 0;
          self.insert_event(cut);
        }
        self.recorded_down.insert(event.original_note, event.output());
        self.insert_event(event);
      }
      RecordedEventKind::NoteOff => {
        self.recorded_down.remove(&event.original_note);
        self.insert_event(event);
      }
    }
  }

  fn current_record_elapsed(&self, now: Instant) -> Option<Duration> {
    if self.playback {
      let duration = self.slot.loop_duration?;
      let start = self.playback_start?;
      return Some(wrap_duration(now.duration_since(start), duration));
    }
    self.recording_start.map(|start| now.duration_since(start))
  }

  pub(crate) fn erase_swept_to(&mut self, to: Duration) {
    if !self.recording || !self.erase_ons_held {
      return;
    }
    let from = self.last_erase_elapsed.unwrap_or(to);
    self.erase_note_ons_in_sweep(from, to);
    self.last_erase_elapsed = Some(to);
  }

  fn erase_note_ons_in_sweep(&mut self, from: Duration, to: Duration) {
    let duration = self.slot.loop_duration.unwrap_or(to.max(from));
    let note_ons: Vec<usize> = self
      .slot
      .events
      .iter()
      .enumerate()
      .filter(|(_, event)| {
        event.kind == RecordedEventKind::NoteOn
          && duration_contains(from, to, event.elapsed, duration)
      })
      .map(|(index, _)| index)
      .collect();
    let mut remove = HashSet::new();
    for index in note_ons {
      remove.insert(index);
      if let Some(off) = matching_note_off_index(&self.slot.events, index) {
        remove.insert(off);
      }
    }
    self.slot.events = self
      .slot
      .events
      .iter()
      .enumerate()
      .filter(|(index, _)| !remove.contains(index))
      .map(|(_, event)| *event)
      .collect();
  }

  fn end_all(&mut self, now: Instant) {
    if !self.recording {
      return;
    }
    let Some(elapsed) = self.current_record_elapsed(now) else {
      return;
    };
    let downs: Vec<(u8, OutputNote)> = self.recorded_down.iter().map(|(k, v)| (*k, *v)).collect();
    for (original_note, output) in downs {
      if let Some(on_index) = latest_note_on_index(&self.slot.events, original_note, elapsed) {
        if let Some(off_index) = matching_note_off_index(&self.slot.events, on_index) {
          self.slot.events.remove(off_index);
        }
      }
      self.insert_event(RecordedEvent {
        elapsed,
        original_note,
        output_channel: output.channel,
        output_note: output.note,
        kind: RecordedEventKind::NoteOff,
        velocity: 0,
      });
      self.recorded_down.remove(&original_note);
    }
  }

  fn insert_event(&mut self, event: RecordedEvent) {
    self.slot.events.push(event);
    self.normalize_events();
  }

  fn normalize_events(&mut self) {
    if let Some(duration) = self.slot.loop_duration {
      self.slot.events.retain(|event| event.elapsed < duration);
    }
    self.slot.events.sort_by_key(|event| event.elapsed);
  }

  fn playback_releases(&self) -> Vec<(u8, OutputNote)> {
    self
      .playback_down
      .iter()
      .map(|(original_note, output)| (*original_note, *output))
      .collect()
  }
}

#[derive(Default)]
pub(crate) struct RecordAction {
  pub(crate) release_playback: Vec<(u8, OutputNote)>,
  pub(crate) apply_snapshot: Option<RemapSnapshot>,
}

pub(crate) fn recorded_event_from_live_message(
  input: &[u8],
  original_note: u8,
  transformed: &[Vec<u8>],
  recorder: &Arc<Mutex<RecordRuntime>>,
) -> Option<RecordedEvent> {
  if input.len() < 3 || !midi::is_note_event(input) || !recorder.lock().unwrap().recording {
    return None;
  }
  let kind = if midi::is_note_on(input) {
    RecordedEventKind::NoteOn
  } else if midi::is_note_off(input) {
    RecordedEventKind::NoteOff
  } else {
    return None;
  };
  let note_msg = transformed.iter().rev().find(|message| {
    message.len() >= 3
      && midi::is_note_event(message)
      && ((kind == RecordedEventKind::NoteOn && midi::is_note_on(message))
        || (kind == RecordedEventKind::NoteOff && midi::is_note_off(message)))
  })?;
  Some(RecordedEvent {
    elapsed: Duration::ZERO,
    original_note,
    output_channel: note_msg[0] & 0x0f,
    output_note: note_msg[1],
    kind,
    velocity: input[2],
  })
}

pub(crate) fn run_playback_thread(
  recorder: Arc<Mutex<RecordRuntime>>,
  state: Arc<Mutex<RemappableEdoState>>,
  output_gate: SharedOutputGate,
) {
  while !STOP_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
    let now = Instant::now();
    let (events, releases, snapshot_to_apply, wrapped, trace_snapshot) = {
      let mut recorder = recorder.lock().unwrap();
      let tick = playback_tick(&mut recorder, now);
      let trace_snapshot = if trace_enabled() {
        Some(recorder.clone())
      } else {
        None
      };
      (tick.0, tick.1, tick.2, tick.3, trace_snapshot)
    };
    if wrapped {
      if let Some(runtime) = &trace_snapshot {
        trace_runtime("loop-cycle", runtime, Instant::now());
      }
    }
    if let Some(snapshot) = snapshot_to_apply {
      apply_remap_snapshot(&mut state.lock().unwrap(), snapshot);
    }
    for (original_note, output) in releases {
      if output_gate.release_source(OutputSource::Playback { original_note }, output) {
        if let Some(runtime) = &trace_snapshot {
          trace_midi_event(
            "midi-output playback-release",
            &[0x80 | output.channel, output.note, 0],
            runtime,
            Instant::now(),
          );
        }
      }
    }
    for event in events {
      let status = match event.kind {
        RecordedEventKind::NoteOn => 0x90 | event.output_channel,
        RecordedEventKind::NoteOff => 0x80 | event.output_channel,
      };
      if output_gate.send_note_message(
        OutputSource::Playback {
          original_note: event.original_note,
        },
        &[status, event.output_note, event.velocity],
      ) {
        if let Some(runtime) = &trace_snapshot {
          trace_midi_event(
            "midi-output playback",
            &[status, event.output_note, event.velocity],
            runtime,
            Instant::now(),
          );
        }
      }
    }
    thread::sleep(PLAYBACK_SLEEP);
  }
}

fn playback_tick(
  recorder: &mut RecordRuntime,
  now: Instant,
) -> (
  Vec<RecordedEvent>,
  Vec<(u8, OutputNote)>,
  Option<RemapSnapshot>,
  bool,
) {
  if !recorder.playback {
    return (vec![], vec![], None, false);
  }
  let Some(duration) = recorder.slot.loop_duration else {
    recorder.playback = false;
    return (vec![], vec![], None, false);
  };
  let Some(start) = recorder.playback_start else {
    return (vec![], vec![], None, false);
  };
  let elapsed_total = now.duration_since(start);
  let elapsed = wrap_duration(elapsed_total, duration);
  let wrapped = elapsed < recorder
    .slot
    .events
    .get(recorder.next_playback_event.saturating_sub(1))
    .map(|event| event.elapsed)
    .unwrap_or(Duration::ZERO)
    || elapsed_total >= duration && recorder.next_playback_event >= recorder.slot.events.len();
  if wrapped {
    recorder.next_playback_event = 0;
    recorder.stop_flash_until = Some(now + CONTROL_FLASH);
  }
  let mut due = vec![];
  while recorder
    .slot
    .events
    .get(recorder.next_playback_event)
    .is_some_and(|event| event.elapsed <= elapsed)
  {
    let event = recorder.slot.events[recorder.next_playback_event];
    match event.kind {
      RecordedEventKind::NoteOn => {
        recorder.playback_down.insert(event.original_note, event.output());
      }
      RecordedEventKind::NoteOff => {
        recorder.playback_down.remove(&event.original_note);
      }
    }
    due.push(event);
    recorder.next_playback_event += 1;
  }
  (due, vec![], None, wrapped)
}

fn wrap_duration(elapsed: Duration, duration: Duration) -> Duration {
  if duration.is_zero() {
    return Duration::ZERO;
  }
  Duration::from_nanos((elapsed.as_nanos() % duration.as_nanos()) as u64)
}

fn wrap_sub(elapsed: Duration, sub: Duration, duration: Duration) -> Duration {
  if elapsed >= sub {
    elapsed - sub
  } else {
    duration - (sub - elapsed)
  }
}

fn duration_contains(from: Duration, to: Duration, needle: Duration, duration: Duration) -> bool {
  if from <= to {
    from <= needle && needle <= to
  } else {
    needle >= from || needle <= to || duration.is_zero()
  }
}

fn note_is_down_at(slot: &RecordingSlot, original_note: u8, at: Duration, duration: Duration) -> bool {
  if duration.is_zero() {
    return false;
  }
  let matching: Vec<_> = slot
    .events
    .iter()
    .filter(|event| event.original_note == original_note)
    .collect();
  let latest_before = matching
    .iter()
    .copied()
    .filter(|event| event.elapsed <= at)
    .max_by_key(|event| event.elapsed);
  let latest = latest_before.or_else(|| matching.iter().copied().max_by_key(|event| event.elapsed));
  latest.is_some_and(|event| event.kind == RecordedEventKind::NoteOn)
}

fn recorded_down_at(slot: &RecordingSlot, at: Duration) -> HashMap<u8, OutputNote> {
  let mut down = HashMap::new();
  for event in slot.events.iter().filter(|event| event.elapsed <= at) {
    match event.kind {
      RecordedEventKind::NoteOn => {
        down.insert(event.original_note, event.output());
      }
      RecordedEventKind::NoteOff => {
        down.remove(&event.original_note);
      }
    }
  }
  down
}

fn matching_note_off_index(events: &[RecordedEvent], on_index: usize) -> Option<usize> {
  let on = events[on_index];
  events
    .iter()
    .enumerate()
    .cycle()
    .skip(on_index + 1)
    .take(events.len().saturating_sub(1))
    .find(|(_, event)| {
      event.original_note == on.original_note && event.kind == RecordedEventKind::NoteOff
    })
    .map(|(index, _)| index)
}

fn latest_note_on_index(
  events: &[RecordedEvent],
  original_note: u8,
  elapsed: Duration,
) -> Option<usize> {
  events
    .iter()
    .enumerate()
    .filter(|(_, event)| {
      event.original_note == original_note
        && event.kind == RecordedEventKind::NoteOn
        && event.elapsed <= elapsed
    })
    .max_by_key(|(_, event)| event.elapsed)
    .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
  use super::*;
  use super::super::state::LooseState;

  fn snapshot() -> RemapSnapshot {
    RemapSnapshot {
      map: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
      deltas: [0; 12],
      loose: [LooseState::Fixed; 12],
    }
  }

  fn event(elapsed_ms: u64, note: u8, kind: RecordedEventKind) -> RecordedEvent {
    RecordedEvent {
      elapsed: Duration::from_millis(elapsed_ms),
      original_note: note,
      output_channel: 0,
      output_note: note,
      kind,
      velocity: 64,
    }
  }

  #[test]
  fn empty_armed_start_begins_recording_without_playback() {
    let mut runtime = RecordRuntime::new();
    runtime.armed = true;
    runtime.key_down(RecordControl::Start, Instant::now(), snapshot());
    assert!(runtime.recording);
    assert!(!runtime.playback);
  }

  #[test]
  fn non_empty_armed_start_begins_playback_plus_overdub() {
    let mut runtime = RecordRuntime::new();
    runtime.slot.loop_duration = Some(Duration::from_secs(1));
    runtime.armed = true;
    runtime.key_down(RecordControl::Start, Instant::now(), snapshot());
    assert!(runtime.recording);
    assert!(runtime.playback);
  }

  #[test]
  fn stop_while_overdubbing_stops_everything_and_resets() {
    let now = Instant::now();
    let mut runtime = RecordRuntime::new();
    runtime.slot.loop_duration = Some(Duration::from_secs(1));
    runtime.armed = true;
    runtime.key_down(RecordControl::Start, now, snapshot());
    runtime.key_down(RecordControl::Stop, now + Duration::from_millis(50), snapshot());
    assert!(!runtime.recording);
    assert!(!runtime.playback);
    assert!(!runtime.armed);
    assert!(runtime.playback_start.is_none());
  }

  #[test]
  fn loop_while_recording_finalizes_and_starts_playback() {
    let now = Instant::now();
    let mut runtime = RecordRuntime::new();
    runtime.armed = true;
    runtime.key_down(RecordControl::Start, now, snapshot());
    runtime.key_down(RecordControl::Loop, now + Duration::from_millis(150), snapshot());
    assert!(!runtime.recording);
    assert!(runtime.playback);
    assert_eq!(runtime.slot.loop_duration, Some(Duration::from_millis(150)));
  }

  #[test]
  fn start_then_loop_within_100ms_clears_slot_and_disarms() {
    let now = Instant::now();
    let mut runtime = RecordRuntime::new();
    runtime.slot.loop_duration = Some(Duration::from_secs(1));
    runtime.armed = true;
    runtime.key_down(RecordControl::Start, now, snapshot());
    runtime.key_down(RecordControl::Loop, now + Duration::from_millis(50), snapshot());
    assert!(runtime.slot.is_empty());
    assert!(!runtime.recording);
    assert!(!runtime.playback);
    assert!(!runtime.armed);
  }

  #[test]
  fn loops_shorter_than_10ms_are_ignored() {
    let now = Instant::now();
    let mut runtime = RecordRuntime::new();
    runtime.armed = true;
    runtime.key_down(RecordControl::Start, now, snapshot());
    runtime.key_down(RecordControl::Loop, now + Duration::from_millis(9), snapshot());
    assert!(runtime.slot.is_empty());
  }

  #[test]
  fn note_on_over_down_note_inserts_wrapped_note_off() {
    let now = Instant::now();
    let mut runtime = RecordRuntime::new();
    runtime.slot.loop_duration = Some(Duration::from_millis(100));
    runtime.slot.events = vec![event(0, 60, RecordedEventKind::NoteOn)];
    runtime.armed = true;
    runtime.key_down(RecordControl::Start, now, snapshot());
    runtime.record_live_event(event(5, 60, RecordedEventKind::NoteOn));
    assert!(runtime.slot.events.iter().any(|e| {
      e.kind == RecordedEventKind::NoteOff && e.elapsed == Duration::from_millis(95)
    }));
  }

  #[test]
  fn note_on_over_down_note_inserts_non_wrapped_note_off() {
    let now = Instant::now();
    let mut runtime = RecordRuntime::new();
    runtime.slot.loop_duration = Some(Duration::from_millis(100));
    runtime.slot.events = vec![event(0, 60, RecordedEventKind::NoteOn)];
    runtime.armed = true;
    runtime.key_down(RecordControl::Start, now, snapshot());
    runtime.record_live_event(event(30, 60, RecordedEventKind::NoteOn));
    assert!(runtime.slot.events.iter().any(|e| {
      e.kind == RecordedEventKind::NoteOff && e.elapsed == Duration::from_millis(20)
    }));
  }

  #[test]
  fn erase_ons_removes_swept_note_ons_and_paired_offs() {
    let mut runtime = RecordRuntime::new();
    runtime.recording = true;
    runtime.erase_ons_held = true;
    runtime.slot.loop_duration = Some(Duration::from_millis(100));
    runtime.slot.events = vec![
      event(20, 60, RecordedEventKind::NoteOn),
      event(40, 60, RecordedEventKind::NoteOff),
    ];
    runtime.last_erase_elapsed = Some(Duration::from_millis(10));
    runtime.erase_swept_to(Duration::from_millis(30));
    assert!(runtime.slot.events.is_empty());
  }

  #[test]
  fn end_all_inserts_note_offs_for_recorded_down_notes_only() {
    let now = Instant::now();
    let mut runtime = RecordRuntime::new();
    runtime.recording = true;
    runtime.recording_start = Some(now);
    runtime.slot.events = vec![event(0, 60, RecordedEventKind::NoteOn)];
    runtime.recorded_down.insert(60, OutputNote { channel: 0, note: 60 });
    runtime.end_all(now + Duration::from_millis(30));
    assert_eq!(
      runtime.slot.events.iter().filter(|e| e.kind == RecordedEventKind::NoteOff).count(),
      1
    );
  }

  #[test]
  fn rscm_returns_snapshot_on_playback_start_and_toggle() {
    let now = Instant::now();
    let mut runtime = RecordRuntime::new();
    runtime.slot.loop_duration = Some(Duration::from_secs(1));
    runtime.slot.stored_map = Some(snapshot());
    runtime.rscm = true;
    assert!(runtime.key_down(RecordControl::Start, now, snapshot()).apply_snapshot.is_some());
    runtime.rscm = false;
    assert!(runtime.key_down(RecordControl::Rscm, now, snapshot()).apply_snapshot.is_some());
  }

  #[test]
  fn output_gate_sends_one_note_on_and_one_final_note_off_for_shared_playback_sources() {
    let (tx, rx) = mpsc::channel();
    let mut gate = OutputGate::new(tx);
    let output = [0x90, 60, 100];
    gate.send_note_message(OutputSource::Playback { original_note: 1 }, &output);
    gate.send_note_message(OutputSource::Playback { original_note: 2 }, &output);
    gate.send_note_message(OutputSource::Playback { original_note: 1 }, &[0x80, 60, 0]);
    gate.send_note_message(OutputSource::Playback { original_note: 2 }, &[0x80, 60, 0]);
    let messages: Vec<Vec<u8>> = rx.try_iter().collect();
    assert_eq!(messages, vec![vec![0x90, 60, 100], vec![0x80, 60, 0]]);
  }

  #[test]
  fn releasing_one_of_two_sources_does_not_emit_note_off() {
    let (tx, rx) = mpsc::channel();
    let mut gate = OutputGate::new(tx);
    gate.send_note_message(OutputSource::Playback { original_note: 1 }, &[0x90, 60, 100]);
    gate.send_note_message(OutputSource::Playback { original_note: 2 }, &[0x90, 60, 100]);
    gate.send_note_message(OutputSource::Playback { original_note: 1 }, &[0x80, 60, 0]);
    let messages: Vec<Vec<u8>> = rx.try_iter().collect();
    assert_eq!(messages, vec![vec![0x90, 60, 100]]);
  }
}
