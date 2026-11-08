//! A song: a directory of tracks, one '<name>.track' file each, and a
//! 'tracks.txt' listing their names top to bottom. A track file missing from
//! 'tracks.txt' goes after the listed ones, in name order.

use std::fs;
use std::path::{Path, PathBuf};

use super::track::Track;
use super::track_text::{parse_track, write_track};

pub const ORDER_FILE: &str = "tracks.txt";
pub const TRACK_EXTENSION: &str = "track";

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
  /// The names last written, so a track removed since is deleted on save.
  saved_names: Vec<String>,
}

impl Song {
  /// Load the song in `dir`, which need not exist yet.
  pub fn load(dir: &Path) -> Result<Song, String> {
    let mut song: Song = Song {
      dir: dir.to_path_buf(), tracks: vec![], next_id: 0, saved_names: vec![] };
    if !dir.exists() { return Ok(song); }
    let mut names: Vec<String> = fs::read_dir(dir)
      .map_err(|e| format!("read {}: {e}", dir.display()))?
      .filter_map(|entry| entry.ok())
      .map(|entry| entry.path())
      .filter(|path| path.extension().is_some_and(|ext| ext == TRACK_EXTENSION))
      .filter_map(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
      .collect();
    names.sort();
    let listed: Vec<String> = fs::read_to_string(dir.join(ORDER_FILE)).unwrap_or_default()
      .lines().map(str::trim).filter(|line| !line.is_empty()).map(String::from).collect();
    let mut ordered: Vec<String> = listed.into_iter().filter(|name| names.contains(name)).collect();
    for name in names {
      if !ordered.contains(&name) { ordered.push(name); } }
    for name in &ordered {
      let path: PathBuf = dir.join(format!("{name}.{TRACK_EXTENSION}"));
      let text: String = fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
      let track: Track = parse_track(name, &text)?;
      song.push(track); }
    song.saved_names = ordered;
    Ok(song) }

  /// Write every track and the order; delete the files of removed tracks.
  pub fn save(&mut self) -> Result<(), String> {
    fs::create_dir_all(&self.dir).map_err(|e| format!("create {}: {e}", self.dir.display()))?;
    let names: Vec<String> = self.tracks.iter().map(|t| t.track.name.clone()).collect();
    for song_track in &self.tracks {
      let path: PathBuf = self.track_path(&song_track.track.name);
      let text: String = write_track(&song_track.track);
      if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?; } }
    for gone in self.saved_names.iter().filter(|name| !names.contains(name)) {
      let _ = fs::remove_file(self.track_path(gone)); }
    let order: String = names.iter().map(|name| format!("{name}\n")).collect();
    fs::write(self.dir.join(ORDER_FILE), order)
      .map_err(|e| format!("write {ORDER_FILE}: {e}"))?;
    self.saved_names = names;
    Ok(()) }

  fn track_path(
    &self,
    name: &str,
  ) -> PathBuf {
    self.dir.join(format!("{name}.{TRACK_EXTENSION}")) }

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
                   && !self.saved_names.contains(name))
      .unwrap();
    let id: u64 = self.next_id;
    self.next_id += 1;
    let index: usize = index.min(self.tracks.len());
    self.tracks.insert(index, SongTrack { id, track: Track::new(&name) });
    index }

  pub fn end(&self) -> i64 {
    self.tracks.iter().map(|t| t.track.end()).max().unwrap_or(0) }
}

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
    song.save().unwrap();
    let names: Vec<String> = song.tracks.iter().map(|t| t.track.name.clone()).collect();
    song.tracks.remove(2);
    song.save().unwrap();
    let loaded: Song = Song::load(&dir).unwrap();
    let loaded_names: Vec<String> = loaded.tracks.iter().map(|t| t.track.name.clone()).collect();
    assert_eq!(loaded_names, names[..2].to_vec());
    assert_eq!(loaded.tracks[1].track.notes.len(), 1);
    assert!(!dir.join(format!("{}.track", names[2])).exists());
    fs::remove_dir_all(&dir).unwrap(); }
}
