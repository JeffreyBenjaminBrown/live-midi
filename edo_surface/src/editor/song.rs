//! A song: a directory of tracks, one file each ('<name>.track' for notes,
//! '<name>.boundaries' for scale ghost section boundaries), and a
//! 'tracks.txt' listing their names top to bottom. A track file missing from
//! 'tracks.txt' goes after the listed ones, in name order.

use std::fs;
use std::path::{Path, PathBuf};

use super::track::{Track, TrackKind};
use super::track_text::{parse_boundaries, parse_track, write_track};

pub const ORDER_FILE: &str = "tracks.txt";
pub const TRACK_EXTENSION: &str = "track";
pub const BOUNDARIES_EXTENSION: &str = "boundaries";
/// The name a song's boundaries track gets when 'b' first makes it.
pub const BOUNDARIES_NAME: &str = "sections";

/// A track with the id the session knows it by (stable while tracks move).
#[derive(Clone, Debug, PartialEq)]
pub struct SongTrack {
  pub id: u64,
  pub track: Track,
}

#[derive(Clone, Debug)]
pub struct Song {
  pub dir: PathBuf,
  pub tracks: Vec<SongTrack>,
  next_id: u64,
  /// The file names last written, so a track removed or renamed since is
  /// deleted on save.
  saved_files: Vec<String>,
}

impl Song {
  /// Load the song in `dir`, which need not exist yet.
  pub fn load(dir: &Path) -> Result<Song, String> {
    let mut song: Song = Song {
      dir: dir.to_path_buf(), tracks: vec![], next_id: 0, saved_files: vec![] };
    if !dir.exists() { return Ok(song); }
    let mut files: Vec<(String, TrackKind)> = fs::read_dir(dir) // (stem, kind)
      .map_err(|e| format!("read {}: {e}", dir.display()))?
      .filter_map(|entry| entry.ok())
      .map(|entry| entry.path())
      .filter_map(|path| {
        let kind: TrackKind = match path.extension()?.to_str()? {
          TRACK_EXTENSION => TrackKind::Notes,
          BOUNDARIES_EXTENSION => TrackKind::Boundaries,
          _ => return None, };
        Some((path.file_stem()?.to_string_lossy().into_owned(), kind)) })
      .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
    let listed: Vec<String> = fs::read_to_string(dir.join(ORDER_FILE)).unwrap_or_default()
      .lines().map(str::trim).filter(|line| !line.is_empty()).map(String::from).collect();
    let mut ordered: Vec<String> = listed.into_iter().filter(|name| names.contains(name)).collect();
    for name in names {
      if !ordered.contains(&name) { ordered.push(name); } }
    for name in &ordered {
      let kind: TrackKind = files.iter().find(|(stem, _)| stem == name).unwrap().1;
      let path: PathBuf = dir.join(file_name(name, kind));
      let text: String = fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
      let track: Track = match kind {
        TrackKind::Notes => parse_track(name, &text)?,
        TrackKind::Boundaries => parse_boundaries(name, &text)?, };
      song.saved_files.push(file_name(name, kind));
      song.push(track); }
    Ok(song) }

  /// Write every track and the order; delete the files of removed tracks.
  pub fn save(&mut self) -> Result<(), String> {
    fs::create_dir_all(&self.dir).map_err(|e| format!("create {}: {e}", self.dir.display()))?;
    let names: Vec<String> = self.tracks.iter().map(|t| t.track.name.clone()).collect();
    let files: Vec<String> =
      self.tracks.iter().map(|t| file_name(&t.track.name, t.track.kind)).collect();
    for song_track in &self.tracks {
      let path: PathBuf = self.dir.join(file_name(&song_track.track.name, song_track.track.kind));
      let text: String = write_track(&song_track.track);
      if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?; } }
    for gone in self.saved_files.iter().filter(|file| !files.contains(file)) {
      let _ = fs::remove_file(self.dir.join(gone)); }
    let order: String = names.iter().map(|name| format!("{name}\n")).collect();
    fs::write(self.dir.join(ORDER_FILE), order)
      .map_err(|e| format!("write {ORDER_FILE}: {e}"))?;
    self.saved_files = files;
    Ok(()) }

  /// Add `track` at the bottom; returns its id.
  pub fn push(
    &mut self,
    track: Track,
  ) -> u64 {
    let id: u64 = self.next_id;
    self.next_id += 1;
    self.tracks.push(SongTrack { id, track });
    id }

  /// Insert a new empty track at `index`, named "track-N" for the first N
  /// not taken; returns its index.
  pub fn insert_new(
    &mut self,
    index: usize,
  ) -> usize {
    let name: String = (1..).map(|n| format!("track-{n}"))
      .find(|name| !self.tracks.iter().any(|t| &t.track.name == name)
                   && !self.saved_files.contains(&file_name(name, TrackKind::Notes)))
      .unwrap();
    let id: u64 = self.next_id;
    self.next_id += 1;
    let index: usize = index.min(self.tracks.len());
    self.tracks.insert(index, SongTrack { id, track: Track::new(&name) });
    index }

  /// The song's boundaries track, if it has one.
  pub fn boundaries_track(&self) -> Option<usize> {
    self.tracks.iter().position(|t| t.track.kind == TrackKind::Boundaries) }

  /// The song's boundaries track, made at the top if there is none yet.
  pub fn boundaries_track_or_new(&mut self) -> usize {
    if let Some(index) = self.boundaries_track() { return index; }
    let id: u64 = self.next_id;
    self.next_id += 1;
    let mut name: String = BOUNDARIES_NAME.to_string();
    while self.tracks.iter().any(|t| t.track.name == name) { name.push('_'); }
    self.tracks.insert(0, SongTrack { id, track: Track::new_boundaries(&name) });
    0 }

  pub fn end(&self) -> i64 {
    self.tracks.iter().map(|t| t.track.end()).max().unwrap_or(0) }
}

fn file_name(
  name: &str,
  kind: TrackKind,
) -> String {
  match kind {
    TrackKind::Notes => format!("{name}.{TRACK_EXTENSION}"),
    TrackKind::Boundaries => format!("{name}.{BOUNDARIES_EXTENSION}"), } }

#[cfg(test)]
mod tests {
  use super::*;
  use crate::editor::track::Note;

  #[test]
  fn a_saved_song_loads_back_in_order_and_forgets_removed_tracks() {
    let dir: PathBuf = std::env::temp_dir().join(format!("edo-editor-song-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut song: Song = Song::load(&dir).unwrap();
    let first: usize = song.insert_new(0);
    song.tracks[first].track.notes.push(Note { start: 0, length: 10, pitch: 1 });
    song.insert_new(0); // goes above the first
    song.insert_new(2);
    let sections: usize = song.boundaries_track_or_new();
    song.tracks[sections].track.boundaries = vec![1000, 2000];
    song.save().unwrap();
    let names: Vec<String> = song.tracks.iter().map(|t| t.track.name.clone()).collect();
    song.tracks.remove(3);
    song.save().unwrap();
    let loaded: Song = Song::load(&dir).unwrap();
    let loaded_names: Vec<String> = loaded.tracks.iter().map(|t| t.track.name.clone()).collect();
    assert_eq!(loaded_names, names[..3].to_vec());
    assert_eq!(loaded.tracks[0].track.boundaries, vec![1000, 2000]);
    assert_eq!(loaded.tracks[2].track.notes.len(), 1);
    assert!(dir.join("sections.boundaries").exists());
    assert!(!dir.join(format!("{}.track", names[3])).exists());
    fs::remove_dir_all(&dir).unwrap(); }
}
