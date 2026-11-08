//! The editor's whole state and every command it obeys: the song, the live
//! grids, the transport (stopped, or rolling with or without a recording),
//! the selection and clipboard, undo, and the sound. The I/O threads and the
//! window call into one `Session` behind a mutex; nothing here touches a
//! socket, a window or an audio device, so the tests drive it directly.
//!
//! Every command that changes the song saves it at once, so nothing is ever
//! unsaved; undo is how a change is taken back.

use std::time::Instant;

use super::live_grid::{Cell, LiveEvent, LiveGrids};
use super::playback::{PlaybackAction, PlayedTrack, Playback};
use super::recorder::{RecordMode, Recorder};
use super::rig_layout::EditorRig;
use super::song::{Song, SongTrack};
use super::sound::Sound;
use super::track::Millis;
use super::track_edit::{clear, copy, merge_take, paste_inserting, paste_over, remove, Clip};

/// A region of time across a run of adjacent tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
  pub from: Millis,
  pub to: Millis,
  pub first_track: usize,
  pub last_track: usize,
}

pub enum Transport {
  Stopped,
  Rolling {
    started: Instant,
    from: Millis,
    playback: Playback,
    recorder: Option<Recorder>,
  },
}

/// How long a blinking note stays lit, and then dark.
pub const BLINK_MILLIS: u128 = 150;

pub struct Session {
  pub song: Song,
  pub live: LiveGrids,
  pub sound: Sound,
  pub transport: Transport,
  /// Where playing and recording start.
  pub playhead: Millis,
  /// The track a recording, a paste or a track command acts on.
  pub current_track: usize,
  pub selection: Option<Selection>,
  /// One clip per track copied, top to bottom.
  pub clipboard: Vec<Clip>,
  undo: Vec<Vec<SongTrack>>,
  redo: Vec<Vec<SongTrack>>,
  /// One line for the window's status bar: the last thing done or failed.
  pub status: String,
  /// When the session began; the blink clock counts from it.
  pub born: Instant,
}

impl Session {
  pub fn new(
    song: Song,
    rig: EditorRig,
    sound: Sound,
  ) -> Session {
    let status: String = if rig.ignored.is_empty() { String::new() }
      else { format!("this rig's parts the editor ignores: {}", rig.ignored.join(", ")) };
    Session {
      song, live: LiveGrids::new(rig), sound, transport: Transport::Stopped,
      playhead: 0, current_track: 0, selection: None, clipboard: vec![],
      undo: vec![], redo: vec![], status, born: Instant::now(), } }

  pub fn is_rolling(&self) -> bool { matches!(self.transport, Transport::Rolling { .. }) }

  pub fn recording_mode(&self) -> Option<RecordMode> {
    match &self.transport {
      Transport::Rolling { recorder: Some(recorder), .. } => Some(recorder.mode),
      _ => None, } }

  /// Where the music is now: the moving position while rolling, else the
  /// playhead.
  pub fn position(
    &self,
    now: Instant,
  ) -> Millis {
    match &self.transport {
      Transport::Rolling { started, from, .. } =>
        from + now.saturating_duration_since(*started).as_millis() as Millis,
      Transport::Stopped => self.playhead, } }

  // === transport ===

  pub fn play(
    &mut self,
    now: Instant,
  ) {
    if self.is_rolling() { return; }
    self.roll(now, None); }

  /// Record from the playhead into the current track (making one if there
  /// is none). While already playing, recording starts where the music is.
  pub fn record(
    &mut self,
    now: Instant,
    mode: RecordMode,
  ) {
    if self.song.tracks.is_empty() {
      self.snapshot();
      self.current_track = self.song.insert_new(0);
      self.save(); }
    let position: Millis = self.position(now);
    match &mut self.transport {
      Transport::Rolling { recorder: Some(_), .. } => {}
      Transport::Rolling { recorder, .. } =>
        *recorder = Some(Recorder::new(mode, position, &self.live)),
      Transport::Stopped => {
        let recorder: Recorder = Recorder::new(mode, position, &self.live);
        self.roll(now, Some(recorder)); } }
    self.status = match mode {
      RecordMode::Notes => format!("recording into {}", self.current_name()),
      RecordMode::Parameters =>
        format!("overdubbing parameters into {}", self.current_name()), }; }

  fn roll(
    &mut self,
    now: Instant,
    recorder: Option<Recorder>,
  ) {
    let tracks: Vec<PlayedTrack> = played_tracks(&self.song);
    let (playback, actions) = Playback::start(&tracks, self.playhead);
    self.apply_playback(&actions);
    self.transport = Transport::Rolling { started: now, from: self.playhead, playback, recorder }; }

  /// Stop; the playhead goes back to where the music started. A recording
  /// is merged into its track.
  pub fn stop(
    &mut self,
    now: Instant,
  ) {
    let stop_at: Millis = self.position(now);
    let Transport::Rolling { mut playback, recorder, .. } =
      std::mem::replace(&mut self.transport, Transport::Stopped) else { return; };
    let actions: Vec<PlaybackAction> = playback.stop();
    self.apply_playback(&actions);
    let Some(recorder) = recorder else { return; };
    let takes = recorder.finish(stop_at);
    if takes.is_empty() {
      self.status = "recorded nothing".into();
      return; }
    self.snapshot();
    let mut target: usize = self.current_track;
    for (index, (_, take)) in takes.iter().enumerate() {
      if index > 0 { target = self.song.insert_new(target + 1); }
      merge_take(&mut self.song.tracks[target].track, take); }
    self.status = format!("recorded {} take(s) into {}", takes.len(), self.current_name());
    self.save(); }

  /// Advance playback to now.
  pub fn tick(
    &mut self,
    now: Instant,
  ) {
    let position: Millis = self.position(now);
    let Transport::Rolling { playback, .. } = &mut self.transport else { return; };
    let tracks: Vec<PlayedTrack> = played_tracks(&self.song);
    let actions: Vec<PlaybackAction> = playback.advance(&tracks, position);
    self.apply_playback(&actions); }

  fn apply_playback(
    &mut self,
    actions: &[PlaybackAction],
  ) {
    for action in actions { self.sound.playback(action); } }

  // === the grids ===

  pub fn key(
    &mut self,
    now: Instant,
    grid: usize,
    cell: Cell,
    down: bool,
  ) {
    let position: Millis = self.position(now);
    for event in self.live.press(grid, cell, down) {
      let event_grid: usize = match &event {
        LiveEvent::NoteOn { grid, .. } | LiveEvent::NoteOff { grid, .. }
        | LiveEvent::Parameter { grid, .. } => *grid, };
      let hz: f64 = match &event {
        LiveEvent::NoteOn { pitch, .. } => self.live.rig.grids[event_grid].tuning.hz(*pitch),
        _ => 0.0, };
      self.sound.live(&event, &self.live.settings(event_grid), hz);
      if let Transport::Rolling { recorder: Some(recorder), .. } = &mut self.transport {
        recorder.event(position, &event); } } }

  /// Grid `grid`'s LED levels now.
  pub fn leds(
    &self,
    now: Instant,
    grid: usize,
    width: i32,
    height: i32,
  ) -> Vec<u8> {
    let blink_on: bool =
      (now.saturating_duration_since(self.born).as_millis() / BLINK_MILLIS) % 2 == 0;
    let playing: Vec<f64> = match &self.transport {
      Transport::Rolling { playback, .. } => playback.sounding_hz(),
      Transport::Stopped => vec![], };
    self.live.paint(grid, &self.live.held_hz(), &playing, blink_on, width, height) }

  // === editing ===

  /// Stop playing (not recording) before an edit; refuse during a recording.
  fn ready_to_edit(
    &mut self,
    now: Instant,
  ) -> bool {
    if self.recording_mode().is_some() {
      self.status = "stop recording first".into();
      return false; }
    self.stop(now);
    true }

  fn selected(&mut self) -> Option<Selection> {
    let selection: Option<Selection> = self.selection
      .filter(|s| s.to > s.from && s.last_track < self.song.tracks.len());
    if selection.is_none() { self.status = "select a region first".into(); }
    selection }

  pub fn copy_selection(&mut self) {
    let Some(selection) = self.selected() else { return; };
    self.clipboard = (selection.first_track..=selection.last_track)
      .map(|index| copy(&self.song.tracks[index].track, selection.from, selection.to))
      .collect();
    self.status = format!("copied {} track(s)", self.clipboard.len()); }

  /// Empty the selection (Delete).
  pub fn clear_selection(
    &mut self,
    now: Instant,
  ) {
    self.edit_selection(now, "cleared", |track, s| clear(track, s.from, s.to)); }

  /// Delete the selected time, closing the gap (Shift+Delete).
  pub fn remove_selection(
    &mut self,
    now: Instant,
  ) {
    self.edit_selection(now, "removed", |track, s| remove(track, s.from, s.to));
    if let Some(selection) = self.selection.as_mut() { selection.to = selection.from; } }

  pub fn cut_selection(
    &mut self,
    now: Instant,
  ) {
    self.copy_selection();
    self.clear_selection(now);
    self.status = format!("cut {} track(s)", self.clipboard.len()); }

  fn edit_selection(
    &mut self,
    now: Instant,
    verb: &str,
    edit: impl Fn(&mut super::track::Track, &Selection),
  ) {
    if !self.ready_to_edit(now) { return; }
    let Some(selection) = self.selected() else { return; };
    self.snapshot();
    for index in selection.first_track..=selection.last_track {
      edit(&mut self.song.tracks[index].track, &selection); }
    self.status = format!("{verb} {}..{}", seconds(selection.from), seconds(selection.to));
    self.save(); }

  /// Paste the clipboard at the playhead, its first clip into the current
  /// track and each next one into the next track down. `inserting` moves
  /// what follows right; otherwise the clipboard replaces what it covers.
  pub fn paste(
    &mut self,
    now: Instant,
    inserting: bool,
  ) {
    if !self.ready_to_edit(now) { return; }
    if self.clipboard.is_empty() { self.status = "the clipboard is empty".into(); return; }
    self.snapshot();
    let at: Millis = self.playhead;
    let clips: Vec<Clip> = self.clipboard.clone();
    for (offset, clip) in clips.iter().enumerate() {
      let Some(song_track) = self.song.tracks.get_mut(self.current_track + offset)
        else { break; };
      if inserting { paste_inserting(&mut song_track.track, clip, at); }
      else { paste_over(&mut song_track.track, clip, at); } }
    self.status = format!("pasted at {}", seconds(at));
    self.save(); }

  /// A new, empty track below the current one, which it becomes.
  pub fn new_track(
    &mut self,
    now: Instant,
  ) {
    if !self.ready_to_edit(now) { return; }
    self.snapshot();
    let below: usize = if self.song.tracks.is_empty() { 0 } else { self.current_track + 1 };
    self.current_track = self.song.insert_new(below);
    self.status = format!("new track {}", self.current_name());
    self.save(); }

  pub fn delete_current_track(
    &mut self,
    now: Instant,
  ) {
    if !self.ready_to_edit(now) || self.song.tracks.is_empty() { return; }
    self.snapshot();
    let gone: SongTrack = self.song.tracks.remove(self.current_track);
    self.current_track = self.current_track.min(self.song.tracks.len().saturating_sub(1));
    self.selection = None;
    self.status = format!("deleted {}", gone.track.name);
    self.save(); }

  pub fn toggle_mute(
    &mut self,
    now: Instant,
  ) {
    if !self.ready_to_edit(now) { return; }
    let Some(song_track) = self.song.tracks.get_mut(self.current_track) else { return; };
    song_track.track.muted = !song_track.track.muted;
    self.save(); }

  pub fn undo(
    &mut self,
    now: Instant,
  ) {
    if !self.ready_to_edit(now) { return; }
    let Some(tracks) = self.undo.pop() else { self.status = "nothing to undo".into(); return; };
    self.redo.push(std::mem::replace(&mut self.song.tracks, tracks));
    self.after_history_move("undone"); }

  pub fn redo(
    &mut self,
    now: Instant,
  ) {
    if !self.ready_to_edit(now) { return; }
    let Some(tracks) = self.redo.pop() else { self.status = "nothing to redo".into(); return; };
    self.undo.push(std::mem::replace(&mut self.song.tracks, tracks));
    self.after_history_move("redone"); }

  fn after_history_move(
    &mut self,
    verb: &str,
  ) {
    self.current_track = self.current_track.min(self.song.tracks.len().saturating_sub(1));
    self.status = verb.into();
    self.save(); }

  /// Move the playhead; while playing, the music jumps there.
  pub fn set_playhead(
    &mut self,
    now: Instant,
    at: Millis,
  ) {
    if self.recording_mode().is_some() { return; }
    let was_playing: bool = self.is_rolling();
    self.stop(now);
    self.playhead = at.max(0);
    if was_playing { self.play(now); } }

  /// Play another rig live from now on. Recordings made with it name it.
  pub fn change_rig(
    &mut self,
    now: Instant,
    rig: EditorRig,
  ) {
    if !self.ready_to_edit(now) { return; }
    self.sound.silence();
    self.sound.rigs.reload();
    self.status = if rig.ignored.is_empty() { format!("playing {}", rig.name) }
      else { format!("playing {}, ignoring {}", rig.name, rig.ignored.join(", ")) };
    self.live.change_rig(rig); }

  fn snapshot(&mut self) {
    self.undo.push(self.song.tracks.clone());
    self.redo.clear(); }

  fn save(&mut self) {
    if let Err(error) = self.song.save() { self.status = format!("NOT SAVED: {error}"); } }

  pub fn current_name(&self) -> String {
    self.song.tracks.get(self.current_track)
      .map_or("no track".into(), |t| t.track.name.clone()) }
}

fn played_tracks(song: &Song) -> Vec<PlayedTrack<'_>> {
  song.tracks.iter().map(|t| PlayedTrack { id: t.id, track: &t.track }).collect() }

fn seconds(at: Millis) -> String { super::track_text::seconds(at) }

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;
  use std::sync::{Arc, Mutex};
  use std::time::Duration;
  use crate::editor::rig_layout::{editor_rig, PieceKind};
  use crate::editor::track::Note;
  use crate::rig::load_named_rig;
  use crate::types::VoiceMap;

  const PLAIN: &str = "timbres-volume-scroll_41edo";

  fn session(test: &str) -> (Session, PathBuf) {
    let dir: PathBuf = std::env::temp_dir()
      .join(format!("edo-editor-session-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rig: EditorRig = editor_rig(PLAIN, &load_named_rig(PLAIN).unwrap()).unwrap();
    let sound: Sound = Sound::new(Arc::new(Mutex::new(VoiceMap::new())), 48000.0);
    (Session::new(Song::load(&dir).unwrap(), rig, sound), dir) }

  fn note_cell(session: &Session) -> Cell {
    (0..16).flat_map(|y| (0..16).map(move |x| (x, y)))
      .find(|cell| session.live.rig.grids[0].piece_at(*cell).unwrap().kind == PieceKind::Notes)
      .unwrap() }

  fn at(t0: Instant, millis: u64) -> Instant { t0 + Duration::from_millis(millis) }

  #[test]
  fn a_recorded_note_lands_where_it_was_played_and_plays_back() {
    let (mut session, dir) = session("record");
    let t0: Instant = Instant::now();
    let cell: Cell = note_cell(&session);
    session.record(t0, RecordMode::Notes);
    session.key(at(t0, 100), 0, cell, true);
    session.key(at(t0, 600), 0, cell, false);
    session.stop(at(t0, 1000));
    let pitch: i32 = session.live.rig.grids[0].tuning.pitch_at(0, cell);
    assert_eq!(session.song.tracks[0].track.notes, vec![Note { start: 100, length: 500, pitch }]);
    assert!(dir.join(format!("{}.track", session.current_name())).exists());
    session.play(t0);
    session.tick(at(t0, 200));
    let index: usize = (cell.1 * 16 + cell.0) as usize;
    let lit: Vec<u8> = (0..4).map(|i| session.leds(
      session.born + Duration::from_millis(i * BLINK_MILLIS as u64), 0, 16, 16)[index]).collect();
    assert!(lit.contains(&15) && lit.contains(&0), "a playing note blinks: {lit:?}");
    session.tick(at(t0, 700));
    assert!(session.sound.voices.lock().unwrap().values().all(|v| v.target_env == 0.0));
    std::fs::remove_dir_all(&dir).unwrap(); }

  #[test]
  fn a_second_grid_records_into_a_new_track_below() {
    let (mut session, dir) = session("two-grids");
    if session.live.rig.grids.len() < 2 { return; }
    let t0: Instant = Instant::now();
    let cell: Cell = note_cell(&session);
    session.record(t0, RecordMode::Notes);
    session.key(at(t0, 100), 1, cell, true);
    session.key(at(t0, 200), 0, cell, true);
    session.stop(at(t0, 300));
    assert_eq!(session.song.tracks.len(), 2);
    assert_eq!(session.song.tracks[0].track.notes[0].start, 100); // grid 1 acted first
    assert_eq!(session.song.tracks[1].track.notes[0].start, 200);
    std::fs::remove_dir_all(&dir).unwrap(); }

  #[test]
  fn overdubbing_parameters_keeps_the_notes_and_undo_takes_it_back() {
    let (mut session, dir) = session("overdub");
    let t0: Instant = Instant::now();
    let cell: Cell = note_cell(&session);
    session.record(t0, RecordMode::Notes);
    session.key(at(t0, 100), 0, cell, true);
    session.key(at(t0, 600), 0, cell, false);
    session.stop(at(t0, 1000));
    let before = session.song.tracks[0].track.clone();
    let timbre_piece = session.live.rig.grids[0].pieces.iter()
      .find(|p| matches!(p.kind, PieceKind::Timbre { .. })).unwrap().clone();
    let other_slot: Cell = (timbre_piece.rect[2], timbre_piece.rect[1]);
    session.record(at(t0, 2000), RecordMode::Parameters);
    session.key(at(t0, 2300), 0, other_slot, true);
    session.key(at(t0, 2350), 0, cell, true); // sounds, not recorded
    session.stop(at(t0, 2800));
    let after = &session.song.tracks[0].track;
    assert_eq!(after.notes, before.notes);
    assert_eq!(after.settings_at(300).timbre, Some(timbre_piece.index_of(other_slot)));
    assert_eq!(after.settings_at(800).timbre, before.settings_at(800).timbre);
    session.undo(at(t0, 3000));
    assert_eq!(session.song.tracks[0].track, before);
    std::fs::remove_dir_all(&dir).unwrap(); }

  #[test]
  fn cut_and_paste_moves_a_region() {
    let (mut session, dir) = session("cut-paste");
    let t0: Instant = Instant::now();
    let cell: Cell = note_cell(&session);
    session.record(t0, RecordMode::Notes);
    session.key(at(t0, 100), 0, cell, true);
    session.key(at(t0, 200), 0, cell, false);
    session.stop(at(t0, 1000));
    session.selection = Some(Selection { from: 0, to: 500, first_track: 0, last_track: 0 });
    session.cut_selection(at(t0, 1000));
    assert!(session.song.tracks[0].track.notes.is_empty());
    session.playhead = 2000;
    session.paste(at(t0, 1000), false);
    assert_eq!(session.song.tracks[0].track.notes[0].start, 2100);
    std::fs::remove_dir_all(&dir).unwrap(); }
}
