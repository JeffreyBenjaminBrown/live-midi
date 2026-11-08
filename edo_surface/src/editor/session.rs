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
use super::track::{Millis, Setting, Track, TrackKind};
use super::track_edit::{
  clear, copy, merge_take, paste_inserting, paste_over, remove, set_throughout, Clip,
};

/// A region of time across a set of tracks, adjacent or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
  pub from: Millis,
  pub to: Millis,
  /// Indices into the song's tracks, ascending, without repeats.
  pub tracks: Vec<usize>,
}

impl Selection {
  /// `[from, to)` across the adjacent tracks `first..=last`.
  pub fn across(
    from: Millis,
    to: Millis,
    first: usize,
    last: usize,
  ) -> Selection {
    Selection { from, to, tracks: (first..=last).collect() } }

  /// Add `track` to the selection, or take it out if it is in.
  pub fn toggle_track(
    &mut self,
    track: usize,
  ) {
    match self.tracks.binary_search(&track) {
      Ok(index) => { self.tracks.remove(index); }
      Err(index) => self.tracks.insert(index, track), } }
}

pub enum Transport {
  Stopped,
  Rolling {
    /// When, and at what position, the music was last at a known place: the
    /// start, or the last change of speed.
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
  /// One clip per track copied, top to bottom, each with its track's
  /// distance below the first copied track.
  pub clipboard: Vec<(usize, Clip)>,
  /// The tracks, and which one was current, before each change.
  undo: Vec<(Vec<SongTrack>, usize)>,
  redo: Vec<(Vec<SongTrack>, usize)>,
  /// One line for the window's status bar: the last thing done or failed.
  pub status: String,
  /// When the session began; the blink clock counts from it.
  pub born: Instant,
  /// Counts changes to the tracks, so a view can copy them only when they
  /// changed.
  pub revision: u64,
  /// How fast the music goes: 1 is as recorded. Recording at another speed
  /// still stores song time, so a part played slowly plays back at 1 faster.
  pub speed: f64,
}

/// The speeds `[` and `]` step through.
pub const SPEEDS: [f64; 13] =
  [0.25, 0.35, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// What the window draws, copied out of the session so drawing never holds
/// the session's lock (a key press waiting on a frame is latency).
#[derive(Clone, Debug, Default)]
pub struct Frame {
  pub revision: u64,
  pub tracks: Vec<SongTrack>,
  pub current_track: usize,
  pub selection: Option<Selection>,
  pub position: Millis,
  pub rolling: bool,
  pub recording: Option<RecordMode>,
  pub status: String,
  pub rig_name: String,
  pub speed: f64,
}

impl Frame {
  pub fn current_name(&self) -> String {
    self.tracks.get(self.current_track).map_or("no track".into(), |t| t.track.name.clone()) }
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
      undo: vec![], redo: vec![], status, born: Instant::now(), revision: 0, speed: 1.0, } }

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
      Transport::Rolling { started, from, .. } => from
        + (now.saturating_duration_since(*started).as_secs_f64() * 1000.0 * self.speed) as Millis,
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
    let no_notes_track_chosen: bool = self.song.tracks.get(self.current_track)
      .is_none_or(|t| t.track.kind != TrackKind::Notes);
    if no_notes_track_chosen && self.song.tracks.len() <= 1 {
      self.snapshot(); // an empty song, or one with only boundaries: a first notes track
      self.current_track = self.song.insert_new(self.song.tracks.len());
      self.save();
    } else if no_notes_track_chosen {
      self.status = "choose a notes track to record into".into();
      return; }
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
    { // load every rig the tracks name now, and say which ones fail
      let names: Vec<String> = self.song.tracks.iter()
        .flat_map(|t| t.track.changes.iter())
        .filter_map(|change| match &change.setting { Setting::Rig(rig) => Some(rig.clone()), _ => None })
        .collect();
      for name in names { let _ = self.sound.rigs.get(&name); }
      let failures: Vec<String> = self.sound.rigs.failures();
      if !failures.is_empty() {
        self.status = format!("silent, rig failed: {}", failures.join("; ")); } }
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

  /// Step the speed one place along `SPEEDS` (`by` = -1 slower, +1 faster),
  /// or back to 1 when `by` is 0. While rolling the music carries on from
  /// where it is.
  pub fn step_speed(
    &mut self,
    now: Instant,
    by: i32,
  ) {
    let here: usize = SPEEDS.iter()
      .position(|speed| *speed >= self.speed - 1e-9).unwrap_or(SPEEDS.len() - 1);
    let next: usize = if by == 0 { SPEEDS.iter().position(|s| *s == 1.0).unwrap() }
      else { (here as i32 + by).clamp(0, SPEEDS.len() as i32 - 1) as usize };
    let position: Millis = self.position(now);
    if let Transport::Rolling { started, from, .. } = &mut self.transport {
      *started = now;
      *from = position; }
    self.speed = SPEEDS[next];
    self.status = format!("speed {}x", self.speed); }

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
      let current_track_id: Option<u64> = self.song.tracks.get(self.current_track).map(|t| t.id);
      let mut retone: Option<PlaybackAction> = None;
      if let Transport::Rolling { recorder: Some(recorder), playback, .. } = &mut self.transport {
        recorder.event(position, &event);
        // an overdubbed parameter is heard on the track it is going into
        if let (LiveEvent::Parameter { setting, .. }, Some(id)) = (&event, current_track_id) {
          if recorder.records_into_current_track(event_grid) {
            retone = playback.override_parameter(id, setting); } } }
      if let Some(action) = retone { self.sound.playback(&action); } } }

  /// Grid `grid`'s LED levels now.
  pub fn leds(
    &self,
    now: Instant,
    grid: usize,
    width: i32,
    height: i32,
  ) -> Vec<u8> {
    if grid >= self.live.rig.grids.len() { // the live rig has fewer grids than are connected
      return vec![0; (width * height).max(0) as usize]; }
    let blink_on: bool =
      (now.saturating_duration_since(self.born).as_millis() / BLINK_MILLIS) % 2 == 0;
    let playing: Vec<f64> = match &self.transport {
      Transport::Rolling { playback, .. } => playback.sounding_hz(),
      Transport::Stopped => vec![], };
    let ghosts: Vec<f64> = self.ghost_hz(self.position(now));
    self.live.paint(grid, &self.live.held_hz(), &playing, &ghosts, blink_on, width, height) }

  /// The scale ghosts at `at`: the frequency of every note on every unmuted
  /// notes track that starts in the section around `at` where the track has
  /// ghosts on; none outside a section, nor where the boundaries track turns
  /// ghosts off.
  pub fn ghost_hz(
    &self,
    at: Millis,
  ) -> Vec<f64> {
    let Some(boundaries) = self.song.boundaries_track() else { return vec![]; };
    let boundaries: &Track = &self.song.tracks[boundaries].track;
    if !boundaries.settings_at(at).makes_ghosts() { return vec![]; }
    let Some((from, to)) = boundaries.section_around(at) else { return vec![]; };
    self.song.tracks.iter()
      .filter(|t| t.track.kind == TrackKind::Notes && !t.track.muted)
      .flat_map(|t| t.track.notes_with_settings())
      .filter(|(note, _, settings)|
        note.start >= from && note.start < to && settings.makes_ghosts())
      .map(|(_, hz, _)| hz)
      .collect() }

  /// Add a section boundary where the music is (making the boundaries
  /// track if there is none). Works while recording, too.
  pub fn add_boundary(
    &mut self,
    now: Instant,
  ) {
    let at: Millis = self.position(now);
    self.snapshot();
    let made: bool = self.song.boundaries_track().is_none();
    let index: usize = self.song.boundaries_track_or_new();
    if made && self.song.tracks.len() > 1 { // it went on top, so every other track moved down
      self.current_track += 1;
      if let Some(selection) = self.selection.as_mut() {
        selection.tracks.iter_mut().for_each(|track| *track += 1); } }
    let track = &mut self.song.tracks[index].track;
    track.boundaries.push(at);
    track.normalize();
    self.status = format!("boundary at {}", seconds(at));
    self.save(); }

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
    let count: usize = self.song.tracks.len();
    let selection: Option<Selection> = self.selection.clone()
      .filter(|s| s.to > s.from && !s.tracks.is_empty() && s.tracks.iter().all(|t| *t < count));
    if selection.is_none() { self.status = "select a region first".into(); }
    selection }

  pub fn copy_selection(&mut self) {
    let Some(selection) = self.selected() else { return; };
    let first: usize = selection.tracks[0];
    self.clipboard = selection.tracks.iter()
      .map(|index| (index - first,
        copy(&self.song.tracks[*index].track, selection.from, selection.to)))
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
    for index in &selection.tracks {
      edit(&mut self.song.tracks[*index].track, &selection); }
    self.status = format!("{verb} {}..{}", seconds(selection.from), seconds(selection.to));
    self.save(); }

  /// Select the whole of the selected tracks (or the current one), from the
  /// song's start to its end.
  pub fn select_whole_tracks(&mut self) {
    if self.song.tracks.is_empty() { return; }
    let tracks: Vec<usize> = match &self.selection {
      Some(selection) if !selection.tracks.is_empty() => selection.tracks.clone(),
      _ => vec![self.current_track], };
    self.selection = Some(Selection { from: 0, to: self.song.end().max(1), tracks }); }

  /// Give the selection one timbre slot throughout.
  pub fn set_selection_timbre(
    &mut self,
    now: Instant,
    slot: usize,
  ) {
    self.edit_selection(now, &format!("timbre {slot} for"), |track, s| {
      if track.kind == TrackKind::Notes {
        set_throughout(track, s.from, s.to, Setting::Timbre(slot)); } }); }

  /// Turn scale ghosts on or off over the selection: on a notes track, for
  /// the notes starting there; on the boundaries track, for every ghost
  /// while the music is there.
  pub fn set_selection_ghosts(
    &mut self,
    now: Instant,
    on: bool,
  ) {
    self.edit_selection(now, if on { "ghosts on for" } else { "ghosts off for" },
      |track, s| set_throughout(track, s.from, s.to, Setting::Ghosts(on))); }

  /// Paste the clipboard at the playhead, its first clip into the current
  /// track and each other one as far below it as it was copied from below
  /// the first. `inserting` moves what follows right; otherwise the
  /// clipboard replaces what it covers.
  pub fn paste(
    &mut self,
    now: Instant,
    inserting: bool,
  ) {
    if !self.ready_to_edit(now) { return; }
    if self.clipboard.is_empty() { self.status = "the clipboard is empty".into(); return; }
    self.snapshot();
    let at: Millis = self.playhead;
    let clips: Vec<(usize, Clip)> = self.clipboard.clone();
    for (offset, clip) in &clips {
      let Some(song_track) = self.song.tracks.get_mut(self.current_track + offset)
        else { break; };
      if song_track.track.kind != clip.kind { continue; } // boundaries go only to boundaries
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
    self.selection = None;
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
    self.status = format!("{} {}", if song_track.track.muted { "muted" } else { "unmuted" },
      song_track.track.name);
    self.save(); }

  /// Rename the current track. A name is letters, digits, '-', '_' and '.',
  /// not taken by another track.
  pub fn rename_current_track(
    &mut self,
    now: Instant,
    name: &str,
  ) {
    if !self.ready_to_edit(now) || self.song.tracks.is_empty() { return; }
    let allowed = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
    if name.is_empty() || !name.chars().all(allowed) || name.starts_with('.') {
      self.status = format!("a track name is letters, digits, - _ and . ; not {name:?}");
      return; }
    if self.song.tracks.iter().any(|t| t.track.name == name) {
      self.status = format!("there is already a track {name}");
      return; }
    self.snapshot();
    let track = &mut self.song.tracks[self.current_track].track;
    self.status = format!("renamed {} to {name}", track.name);
    track.name = name.to_string();
    self.save(); }

  pub fn undo(
    &mut self,
    now: Instant,
  ) {
    if !self.ready_to_edit(now) { return; }
    let Some((tracks, current)) = self.undo.pop()
      else { self.status = "nothing to undo".into(); return; };
    self.redo.push((std::mem::replace(&mut self.song.tracks, tracks), self.current_track));
    self.current_track = current;
    self.after_history_move("undone"); }

  pub fn redo(
    &mut self,
    now: Instant,
  ) {
    if !self.ready_to_edit(now) { return; }
    let Some((tracks, current)) = self.redo.pop()
      else { self.status = "nothing to redo".into(); return; };
    self.undo.push((std::mem::replace(&mut self.song.tracks, tracks), self.current_track));
    self.current_track = current;
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
    self.undo.push((self.song.tracks.clone(), self.current_track));
    self.redo.clear(); }

  fn save(&mut self) {
    self.revision += 1;
    if let Err(error) = self.song.save() { self.status = format!("NOT SAVED: {error}"); } }

  /// Refresh `frame` from the session; the tracks are copied only if they
  /// changed since the frame's last refresh.
  pub fn refresh_frame(
    &self,
    now: Instant,
    frame: &mut Frame,
  ) {
    if frame.revision != self.revision || frame.tracks.len() != self.song.tracks.len() {
      frame.tracks = self.song.tracks.clone();
      frame.revision = self.revision; }
    frame.current_track = self.current_track;
    frame.selection.clone_from(&self.selection);
    frame.position = self.position(now);
    frame.rolling = self.is_rolling();
    frame.recording = self.recording_mode();
    frame.status.clone_from(&self.status);
    frame.rig_name.clone_from(&self.live.rig.name);
    frame.speed = self.speed; }

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
  use crate::types::{VoiceMap, VoiceSource};

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
  fn a_selection_of_separated_tracks_pastes_with_the_same_gap() {
    let (mut session, dir) = session("separated");
    let t0: Instant = Instant::now();
    for _ in 0..4 { session.new_track(t0); }
    for track in [0, 2] {
      session.song.tracks[track].track.notes.push(Note { start: 100, length: 50, pitch: 1 }); }
    let mut selection: Selection = Selection::across(0, 500, 0, 0);
    selection.toggle_track(2);
    session.selection = Some(selection);
    session.copy_selection();
    session.current_track = 1;
    session.playhead = 1000;
    session.paste(t0, false);
    let counts: Vec<usize> = session.song.tracks.iter().map(|t| t.track.notes.len()).collect();
    assert_eq!(counts, vec![1, 1, 1, 1]); // pasted into 1 and 3, two apart as copied
    std::fs::remove_dir_all(&dir).unwrap(); }

  #[test]
  fn renaming_a_track_renames_its_file() {
    let (mut session, dir) = session("rename");
    let t0: Instant = Instant::now();
    session.new_track(t0);
    let old: String = session.current_name();
    session.rename_current_track(t0, "bass line");
    assert_eq!(session.current_name(), old, "a space is refused");
    session.rename_current_track(t0, "bass-1");
    assert!(dir.join("bass-1.track").exists());
    assert!(!dir.join(format!("{old}.track")).exists());
    std::fs::remove_dir_all(&dir).unwrap(); }

  #[test]
  fn overdubbing_started_while_playing_is_heard_and_kept() {
    let (mut session, dir) = session("overdub-while-playing");
    let t0: Instant = Instant::now();
    let cell: Cell = note_cell(&session);
    session.record(t0, RecordMode::Notes);
    session.key(at(t0, 100), 0, cell, true);
    session.key(at(t0, 2000), 0, cell, false);
    session.stop(at(t0, 2100));
    let timbre_piece = session.live.rig.grids[0].pieces.iter()
      .find(|p| matches!(p.kind, PieceKind::Timbre { .. })).unwrap().clone();
    let new_slot_cell: Cell = (timbre_piece.rect[2], timbre_piece.rect[1]);
    let new_slot: usize = timbre_piece.index_of(new_slot_cell);
    let t1: Instant = at(t0, 5000);
    session.play(t1);
    session.tick(at(t1, 200)); // the note is sounding
    session.record(at(t1, 300), RecordMode::Parameters);
    session.key(at(t1, 500), 0, new_slot_cell, true);
    session.tick(at(t1, 510));
    let wanted_waveform = session.sound.rigs.get(PLAIN).unwrap().timbres[new_slot].waveform;
    let heard: Vec<_> = session.sound.voices.lock().unwrap().iter()
      .filter(|(source, _)| matches!(source, VoiceSource::EditorTrackNote { .. }))
      .map(|(_, voice)| voice.timbre.waveform).collect();
    assert_eq!(heard, vec![wanted_waveform], "the playing note takes the new timbre");
    session.stop(at(t1, 1500));
    let track = &session.song.tracks[0].track;
    assert_eq!(track.settings_at(600).timbre, Some(new_slot));
    assert_eq!(track.settings_at(1600).timbre, track.settings_at(50).timbre, "restored after");
    std::fs::remove_dir_all(&dir).unwrap(); }

  #[test]
  fn a_whole_track_can_be_given_one_timbre() {
    let (mut session, dir) = session("whole-track-timbre");
    let t0: Instant = Instant::now();
    let cell: Cell = note_cell(&session);
    session.record(t0, RecordMode::Notes);
    session.key(at(t0, 100), 0, cell, true);
    session.key(at(t0, 900), 0, cell, false);
    session.stop(at(t0, 1000));
    session.select_whole_tracks();
    session.set_selection_timbre(at(t0, 1000), 3);
    let track = &session.song.tracks[0].track;
    assert!(track.notes.iter().all(|note| track.settings_at(note.start).timbre == Some(3)));
    std::fs::remove_dir_all(&dir).unwrap(); }

  #[test]
  fn at_half_speed_a_note_records_in_song_time_and_the_music_moves_half_as_fast() {
    let (mut session, dir) = session("speed");
    let t0: Instant = Instant::now();
    let cell: Cell = note_cell(&session);
    while session.speed > 0.5 { session.step_speed(t0, -1); }
    assert_eq!(session.speed, 0.5);
    session.record(t0, RecordMode::Notes);
    session.key(at(t0, 1000), 0, cell, true);
    session.key(at(t0, 2000), 0, cell, false);
    session.step_speed(at(t0, 2000), 0); // back to 1 mid-roll: no jump
    assert_eq!(session.position(at(t0, 2000)), 1000);
    assert_eq!(session.position(at(t0, 2500)), 1500);
    session.stop(at(t0, 2600));
    let note = session.song.tracks[0].track.notes[0];
    assert_eq!((note.start, note.length), (500, 500));
    std::fs::remove_dir_all(&dir).unwrap(); }

  #[test]
  fn ghosts_are_the_pitches_played_in_the_section_around_the_music() {
    let (mut session, dir) = session("ghosts");
    let t0: Instant = Instant::now();
    let cell: Cell = note_cell(&session);
    let other: Cell = (cell.0 + 1, cell.1);
    session.record(t0, RecordMode::Notes);
    session.add_boundary(at(t0, 0));
    session.key(at(t0, 100), 0, cell, true);
    session.key(at(t0, 200), 0, cell, false);
    session.key(at(t0, 900), 0, cell, true); // starts in the first section, sounds into the next
    session.add_boundary(at(t0, 1000));
    session.key(at(t0, 1050), 0, cell, false);
    session.key(at(t0, 1100), 0, other, true);
    session.key(at(t0, 1200), 0, other, false);
    session.add_boundary(at(t0, 2000));
    session.stop(at(t0, 2100));
    let sections: usize = session.song.boundaries_track().unwrap();
    assert_eq!(session.song.tracks[sections].track.boundaries, vec![0, 1000, 2000]);
    let tuning = session.live.rig.grids[0].tuning;
    let hz = |c: Cell| tuning.hz(tuning.pitch_at(0, c));
    assert_eq!(session.ghost_hz(500), vec![hz(cell), hz(cell)]);
    assert_eq!(session.ghost_hz(1500), vec![hz(other)], "only notes starting in the section");
    assert!(session.ghost_hz(2500).is_empty(), "no section after the last boundary");
    session.song.tracks[1].track.muted = true;
    assert!(session.ghost_hz(500).is_empty(), "a muted track casts no ghosts");
    session.song.tracks[1].track.muted = false;
    session.selection = Some(Selection::across(1050, 1500, 1, 1));
    session.set_selection_ghosts(at(t0, 3000), false);
    assert!(session.ghost_hz(1500).is_empty(), "ghosts turned off where that note starts");
    assert_eq!(session.ghost_hz(500).len(), 2, "and only there");
    session.selection = Some(Selection::across(0, 800, 0, 0)); // the boundaries track
    session.set_selection_ghosts(at(t0, 3000), false);
    assert!(session.ghost_hz(500).is_empty(), "the boundaries track turns every ghost off");
    assert_eq!(session.ghost_hz(900).len(), 2, "until it turns them back on");
    session.playhead = 900;
    let index: usize = (cell.1 * 16 + cell.0) as usize;
    assert_eq!(session.leds(t0, 0, 16, 16)[index], crate::editor::live_grid::DIM);
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
    session.selection = Some(Selection::across(0, 500, 0, 0));
    session.cut_selection(at(t0, 1000));
    assert!(session.song.tracks[0].track.notes.is_empty());
    session.playhead = 2000;
    session.paste(at(t0, 1000), false);
    assert_eq!(session.song.tracks[0].track.notes[0].start, 2100);
    std::fs::remove_dir_all(&dir).unwrap(); }
}
