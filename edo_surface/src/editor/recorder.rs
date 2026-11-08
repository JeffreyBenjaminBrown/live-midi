//! Collecting what the player does during one recording pass into a `Take`
//! per grid.

use std::collections::HashMap;

use super::live_grid::{Cell, LiveEvent, LiveGrids};
use super::track::{Millis, Note, SettingChange, Settings};
use super::track_edit::Take;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordMode {
  /// Record notes and the parameters touched.
  Notes,
  /// Record only parameters; notes played sound but are not kept.
  Parameters,
}

#[derive(Clone, Debug)]
struct GridTake {
  live_settings: Settings,
  /// Notes still held: their start and pitch.
  open: HashMap<Cell, (Millis, i32)>,
  notes: Vec<Note>,
  parameter_changes: Vec<SettingChange>,
}

#[derive(Clone, Debug)]
pub struct Recorder {
  pub mode: RecordMode,
  pub start: Millis,
  grids: Vec<GridTake>,
  /// The grids that recorded something, in the order they first did.
  active_order: Vec<usize>,
}

impl Recorder {
  pub fn new(
    mode: RecordMode,
    start: Millis,
    live: &LiveGrids,
  ) -> Recorder {
    let grids: Vec<GridTake> = (0..live.rig.grids.len())
      .map(|grid| GridTake {
        live_settings: live.settings(grid),
        open: HashMap::new(),
        notes: vec![],
        parameter_changes: vec![], })
      .collect();
    Recorder { mode, start, grids, active_order: vec![] } }

  pub fn event(
    &mut self,
    at: Millis,
    event: &LiveEvent,
  ) {
    let (grid, recorded): (usize, bool) = match event {
      LiveEvent::NoteOn { grid, cell, pitch } if self.mode == RecordMode::Notes => {
        self.grids[*grid].open.insert(*cell, (at, *pitch));
        (*grid, true) }
      LiveEvent::NoteOff { grid, cell } => {
        let take: &mut GridTake = &mut self.grids[*grid];
        if let Some((start, pitch)) = take.open.remove(cell) {
          take.notes.push(Note { start, length: (at - start).max(1), pitch }); }
        (*grid, false) }
      LiveEvent::Parameter { grid, setting } => {
        self.grids[*grid].parameter_changes.push(SettingChange { at, setting: setting.clone() });
        (*grid, true) }
      LiveEvent::NoteOn { grid, .. } => (*grid, false), };
    if recorded && !self.active_order.contains(&grid) {
      self.active_order.push(grid); } }

  /// Whether grid `grid` is the one recording into the current track: the
  /// first grid to record anything (the others get new tracks).
  pub fn records_into_current_track(
    &self,
    grid: usize,
  ) -> bool {
    self.active_order.first() == Some(&grid) }

  /// The takes of the grids that recorded something, in the order they first
  /// did, each with its grid's number. A note still held ends at `stop`.
  pub fn finish(
    self,
    stop: Millis,
  ) -> Vec<(usize, Take)> {
    let start: Millis = self.start;
    let mut grids: Vec<GridTake> = self.grids;
    self.active_order.iter().map(|grid| {
      let take: &mut GridTake = &mut grids[*grid];
      for (_, (note_start, pitch)) in take.open.drain() {
        take.notes.push(Note { start: note_start, length: (stop - note_start).max(1), pitch }); }
      (*grid, Take {
        start,
        stop,
        live_settings: take.live_settings.clone(),
        notes: std::mem::take(&mut take.notes),
        parameter_changes: std::mem::take(&mut take.parameter_changes), }) })
      .collect() }
}
