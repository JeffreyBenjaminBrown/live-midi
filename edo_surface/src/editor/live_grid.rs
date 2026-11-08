//! The live state of the grids a player plays: what each grid's keys mean
//! now (its register), which notes fingers hold, and each grid's tone (its
//! timbre slot and the volume of each slot). A key press becomes a
//! `LiveEvent`; the LEDs are painted from this state plus whatever the tracks
//! are playing.
//!
//! Pure: no sockets, no audio. The I/O thread feeds it presses and sends its
//! LED levels; the session turns its events into sound and recordings.

use std::collections::{HashMap, HashSet};

use crate::edo_play::{register_delta, shift_for_cell, Shift};
use crate::timbre_slots::TIMBRE_SLOTS;

use super::rig_layout::{EditorRig, GridLayout, PieceKind};
use super::track::{Setting, Settings};

pub type Cell = (i32, i32);

/// What a key press did, for the sound and for a recording.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveEvent {
  /// Grid `grid` started sounding `pitch` (in its own tuning) at `cell`.
  NoteOn { grid: usize, cell: Cell, pitch: i32 },
  /// The finger at `cell` lifted, ending its note.
  NoteOff { grid: usize, cell: Cell },
  /// The timbre or volume of grid `grid` changed.
  Parameter { grid: usize, setting: Setting },
}

#[derive(Clone, Debug, PartialEq)]
pub struct GridTone {
  pub timbre: usize,
  pub volume_db: [f32; TIMBRE_SLOTS],
}

impl GridTone {
  pub fn current_volume_db(&self) -> f32 { self.volume_db[self.timbre] }
}

#[derive(Clone, Debug)]
pub struct LiveGrids {
  pub rig: EditorRig,
  pub registers: Vec<i32>,
  /// Per grid: the pitch each held cell is sounding.
  pub held: Vec<HashMap<Cell, i32>>,
  pub tones: Vec<GridTone>,
}

/// LED levels: 15 is full, 0 dark.
pub const BRIGHT: u8 = 15;
pub const DIM: u8 = 4;

impl LiveGrids {
  pub fn new(rig: EditorRig) -> LiveGrids {
    let count: usize = rig.grids.len();
    let tones: Vec<GridTone> = rig.grids.iter()
      .map(|grid| GridTone {
        timbre: grid.initial_timbre,
        volume_db: [grid.initial_volume_db; TIMBRE_SLOTS], })
      .collect();
    LiveGrids { rig, registers: vec![0; count], held: vec![HashMap::new(); count], tones } }

  /// Adopt another rig (or a reloaded one), keeping each grid's register and
  /// tone where the grid still exists. Held notes are dropped: the caller
  /// ends their sound.
  pub fn change_rig(
    &mut self,
    rig: EditorRig,
  ) {
    let count: usize = rig.grids.len();
    let old_tones: Vec<GridTone> = std::mem::take(&mut self.tones);
    let fresh: LiveGrids = LiveGrids::new(rig);
    self.tones = fresh.tones.into_iter().enumerate()
      .map(|(grid, tone)| old_tones.get(grid).cloned().unwrap_or(tone))
      .collect();
    self.registers.resize(count, 0);
    self.held = vec![HashMap::new(); count];
    self.rig = fresh.rig; }

  /// The settings a recording of grid `grid` starts with.
  pub fn settings(
    &self,
    grid: usize,
  ) -> Settings {
    let tone: &GridTone = &self.tones[grid];
    Settings {
      rig: Some(self.rig.name.clone()),
      tuning: Some(self.rig.grids[grid].track_tuning()),
      timbre: Some(tone.timbre),
      volume_db: Some(tone.current_volume_db()), } }

  pub fn press(
    &mut self,
    grid: usize,
    cell: Cell,
    down: bool,
  ) -> Vec<LiveEvent> {
    let Some(layout) = self.rig.grids.get(grid) else { return vec![]; };
    if !down {
      return match self.held[grid].remove(&cell) {
        Some(_) => vec![LiveEvent::NoteOff { grid, cell }],
        None => vec![], }; }
    let Some(piece) = layout.piece_at(cell) else { return vec![]; };
    match piece.kind.clone() {
      PieceKind::Notes => {
        let pitch: i32 = layout.tuning.pitch_at(self.registers[grid], cell);
        self.held[grid].insert(cell, pitch);
        vec![LiveEvent::NoteOn { grid, cell, pitch }] }
      PieceKind::Shift => {
        if let Some(shift) = shift_for_cell(piece.rect, cell) {
          let tuning = layout.tuning;
          self.registers[grid] +=
            register_delta(shift, tuning.x_step, tuning.y_step, tuning.edo); }
        vec![] }
      PieceKind::Timbre { controls } => {
        let slot: usize = piece.index_of(cell).min(TIMBRE_SLOTS - 1);
        let tone: &mut GridTone = &mut self.tones[controls];
        tone.timbre = slot;
        vec![
          LiveEvent::Parameter { grid: controls, setting: Setting::Timbre(slot) },
          LiveEvent::Parameter {
            grid: controls, setting: Setting::VolumeDb(tone.current_volume_db()) }, ] }
      PieceKind::Volume { controls, keys } => {
        let tone: &mut GridTone = &mut self.tones[controls];
        let slot: usize = tone.timbre;
        tone.volume_db[slot] = (tone.volume_db[slot] + keys.delta_db(piece.index_of(cell)))
          .clamp(keys.min_db, keys.max_db);
        vec![LiveEvent::Parameter {
          grid: controls, setting: Setting::VolumeDb(tone.volume_db[slot]) }] }
      PieceKind::Inert => vec![], } }

  /// The pitches the fingers hold on every grid, as frequencies.
  pub fn held_hz(&self) -> Vec<f64> {
    self.held.iter().zip(&self.rig.grids)
      .flat_map(|(held, layout)| held.values().map(|pitch| layout.tuning.hz(*pitch)))
      .collect() }

  /// Grid `grid`'s LED levels, row by row. A frequency lights every cell in
  /// the pitch class (any octave) of the grid's pitch nearest to it.
  /// - `solid_hz` (the held notes) light bright, steadily.
  /// - `ghost_hz` (the scale ghosts: every pitch played in the current
  ///   section) light dim, steadily.
  /// - `blinking_hz` (the tracks' sounding notes) light bright while
  ///   `blink_on`, and otherwise fall back to dim if a ghost, else dark.
  #[allow(clippy::too_many_arguments)]
  pub fn paint(
    &self,
    grid: usize,
    solid_hz: &[f64],
    blinking_hz: &[f64],
    ghost_hz: &[f64],
    blink_on: bool,
    width: i32,
    height: i32,
  ) -> Vec<u8> {
    let layout: &GridLayout = &self.rig.grids[grid];
    let tuning = layout.track_tuning();
    let class = |hz: &f64| tuning.nearest_pitch(*hz).rem_euclid(tuning.edo);
    let solid: HashSet<i32> = solid_hz.iter().map(class).collect();
    let blinking: HashSet<i32> = blinking_hz.iter().map(class).collect();
    let ghosts: HashSet<i32> = ghost_hz.iter().map(class).collect();
    let mut levels: Vec<u8> = vec![0; (width * height) as usize];
    for y in 0..height {
      for x in 0..width {
        let Some(piece) = layout.piece_at((x, y)) else { continue; };
        levels[(y * width + x) as usize] = match piece.kind {
          PieceKind::Notes => {
            let class: i32 =
              layout.tuning.pitch_at(self.registers[grid], (x, y)).rem_euclid(tuning.edo);
            if solid.contains(&class) { BRIGHT }
            else if blink_on && blinking.contains(&class) { BRIGHT }
            else if ghosts.contains(&class) { DIM }
            else { 0 } }
          PieceKind::Timbre { controls } =>
            if piece.index_of((x, y)) == self.tones[controls].timbre { BRIGHT } else { DIM },
          // the octave keys work but stay dark (Jeff); the arrows are dim
          PieceKind::Shift => match shift_for_cell(piece.rect, (x, y)) {
            Some(Shift::OctaveDown | Shift::OctaveUp) => 0,
            _ => DIM, },
          PieceKind::Volume { .. } => DIM,
          PieceKind::Inert => 0, }; } }
    levels }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::rig::load_named_rig;
  use crate::editor::rig_layout::{editor_rig, Piece};

  const PLAIN: &str = "timbres-volume-scroll_41edo";

  fn live() -> LiveGrids {
    LiveGrids::new(editor_rig(PLAIN, &load_named_rig(PLAIN).unwrap()).unwrap()) }

  fn piece(live: &LiveGrids, wanted: fn(&PieceKind) -> bool) -> Piece {
    live.rig.grids[0].pieces.iter().find(|piece| wanted(&piece.kind)).unwrap().clone() }

  /// A cell of grid 0 that only the note grid covers.
  fn note_cell(live: &LiveGrids) -> Cell {
    (0..16).flat_map(|y| (0..16).map(move |x| (x, y)))
      .find(|cell| live.rig.grids[0].piece_at(*cell).unwrap().kind == PieceKind::Notes)
      .unwrap() }

  #[test]
  fn a_note_sounds_its_cells_pitch_and_ends_when_lifted() {
    let mut live: LiveGrids = live();
    let cell: Cell = note_cell(&live);
    let pitch: i32 = live.rig.grids[0].tuning.pitch_at(0, cell);
    assert_eq!(live.press(0, cell, true), vec![LiveEvent::NoteOn { grid: 0, cell, pitch }]);
    assert_eq!(live.press(0, cell, false), vec![LiveEvent::NoteOff { grid: 0, cell }]); }

  #[test]
  fn a_held_note_lights_solid_and_a_track_note_blinks() {
    let mut live: LiveGrids = live();
    let cell: Cell = note_cell(&live);
    live.press(0, cell, true);
    let held: Vec<f64> = live.held_hz();
    let index: usize = (cell.1 * 16 + cell.0) as usize;
    assert_eq!(live.paint(0, &held, &[], &[], false, 16, 16)[index], BRIGHT);
    assert_eq!(live.paint(0, &[], &held, &[], true, 16, 16)[index], BRIGHT);
    assert_eq!(live.paint(0, &[], &held, &[], false, 16, 16)[index], 0);
    let octave_up: Vec<f64> = held.iter().map(|hz| hz * 2.0).collect();
    assert_eq!(live.paint(0, &octave_up, &[], &[], false, 16, 16)[index], BRIGHT, "octaves light"); }

  #[test]
  fn a_ghost_is_dim_and_a_playing_ghost_flashes_between_dim_and_bright() {
    let live: LiveGrids = live();
    let cell: Cell = note_cell(&live);
    let hz: Vec<f64> = vec![live.rig.grids[0].tuning.hz(live.rig.grids[0].tuning.pitch_at(0, cell))];
    let index: usize = (cell.1 * 16 + cell.0) as usize;
    assert_eq!(live.paint(0, &[], &[], &hz, true, 16, 16)[index], DIM);
    assert_eq!(live.paint(0, &[], &hz, &hz, true, 16, 16)[index], BRIGHT);
    assert_eq!(live.paint(0, &[], &hz, &hz, false, 16, 16)[index], DIM); }

  #[test]
  fn a_timbre_key_chooses_the_slot_and_reports_its_volume() {
    let mut live: LiveGrids = live();
    let timbre: Piece = piece(&live, |kind| matches!(kind, PieceKind::Timbre { .. }));
    let last_slot: Cell = (timbre.rect[2], timbre.rect[1]);
    let events: Vec<LiveEvent> = live.press(0, last_slot, true);
    let slot: usize = timbre.index_of(last_slot);
    assert_eq!(live.tones[0].timbre, slot);
    assert!(events.contains(&LiveEvent::Parameter { grid: 0, setting: Setting::Timbre(slot) })); }

  #[test]
  fn a_volume_key_moves_the_current_slots_volume_within_its_limits() {
    let mut live: LiveGrids = live();
    let volume: Piece = piece(&live, |kind| matches!(kind, PieceKind::Volume { .. }));
    let PieceKind::Volume { keys, .. } = volume.kind else { unreachable!() };
    let loudest_key: Cell = (volume.rect[2], volume.rect[1]);
    for _ in 0..20 { live.press(0, loudest_key, true); }
    assert_eq!(live.tones[0].current_volume_db(), keys.max_db);
    let before: f32 = live.tones[0].current_volume_db();
    live.press(0, (volume.rect[0], volume.rect[1]), true);
    assert_eq!(live.tones[0].current_volume_db(), before + keys.delta_db(0)); }

  #[test]
  fn the_shift_pad_moves_what_the_cells_sound() {
    let mut live: LiveGrids = live();
    let shift: Piece = piece(&live, |kind| *kind == PieceKind::Shift);
    let octave_up: Cell = (shift.rect[0] + 2, shift.rect[1]);
    live.press(0, octave_up, true);
    assert_eq!(live.registers[0], live.rig.grids[0].tuning.edo); }

  #[test]
  fn the_octave_keys_are_dark_and_the_arrows_dim() {
    let live: LiveGrids = live();
    let shift: Piece = piece(&live, |kind| *kind == PieceKind::Shift);
    let levels: Vec<u8> = live.paint(0, &[], &[], &[], false, 16, 16);
    for cell in (shift.rect[1]..=shift.rect[3])
      .flat_map(|y| (shift.rect[0]..=shift.rect[2]).map(move |x| (x, y))) {
      let level: u8 = levels[(cell.1 * 16 + cell.0) as usize];
      match shift_for_cell(shift.rect, cell) {
        Some(Shift::OctaveDown | Shift::OctaveUp) => assert_eq!(level, 0, "{cell:?}"),
        _ => assert_eq!(level, DIM, "{cell:?}"), } } }
}
