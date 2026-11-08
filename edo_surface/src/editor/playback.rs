//! Playing tracks back: which notes start, end and change tone as the
//! playhead moves. Pure: it reads tracks and returns `PlaybackAction`s; the
//! sound layer turns those into voices.
//!
//! The playhead moves in half-open windows: `advance(to)` handles everything
//! at a time in [the old position, `to`). At one time, setting changes come
//! before notes, so a note sounds with the settings changed at its start.

use super::track::{Millis, Settings, Track};

#[derive(Clone, Debug, PartialEq)]
pub enum PlaybackAction {
  NoteStart { track: u64, seq: u64, hz: f64, settings: Settings },
  NoteEnd { track: u64, seq: u64 },
  /// The track's settings changed while notes may be sounding.
  SettingsChanged { track: u64, settings: Settings },
}

/// A track as playback sees it: its session-long id and its content.
pub struct PlayedTrack<'a> {
  pub id: u64,
  pub track: &'a Track,
}

#[derive(Clone, Debug)]
struct TrackPosition {
  id: u64,
  next_note: usize,
  next_change: usize,
  settings: Settings,
  /// (seq, end time, frequency) of each note this track is sounding.
  sounding: Vec<(u64, Millis, f64)>,
}

#[derive(Clone, Debug)]
pub struct Playback {
  pub position: Millis,
  next_seq: u64,
  tracks: Vec<TrackPosition>,
}

impl Playback {
  /// Start at `from`. A note that began before `from` and still sounds
  /// there starts at once.
  pub fn start(
    tracks: &[PlayedTrack],
    from: Millis,
  ) -> (Playback, Vec<PlaybackAction>) {
    let mut playback: Playback = Playback { position: from, next_seq: 0, tracks: vec![] };
    let mut actions: Vec<PlaybackAction> = vec![];
    for played in tracks {
      let track: &Track = played.track;
      let mut position: TrackPosition = TrackPosition {
        id: played.id,
        next_note: track.notes.partition_point(|note| note.start < from),
        next_change: track.changes.partition_point(|change| change.at < from),
        settings: track.settings_before(from),
        sounding: vec![], };
      if !track.muted {
        for note in track.notes[..position.next_note].iter()
          .filter(|note| note.end() > from) {
          let settings: Settings = track.settings_at(note.start);
          if let Some(tuning) = settings.tuning {
            let seq: u64 = playback.next_seq;
            playback.next_seq += 1;
            let hz: f64 = tuning.hz(note.pitch);
            position.sounding.push((seq, note.end(), hz));
            actions.push(PlaybackAction::NoteStart {
              track: played.id, seq, hz, settings: position.settings.clone() }); } } }
      playback.tracks.push(position); }
    (playback, actions) }

  /// Move the playhead to `to`, handling everything in between.
  pub fn advance(
    &mut self,
    tracks: &[PlayedTrack],
    to: Millis,
  ) -> Vec<PlaybackAction> {
    let mut actions: Vec<PlaybackAction> = vec![];
    for (position, played) in self.tracks.iter_mut().zip(tracks) {
      let track: &Track = played.track;
      position.sounding.retain(|(seq, end, _)| {
        let ended: bool = *end < to;
        if ended { actions.push(PlaybackAction::NoteEnd { track: position.id, seq: *seq }); }
        !ended });
      loop {
        let change_at: Option<Millis> = track.changes.get(position.next_change)
          .map(|change| change.at).filter(|at| *at < to);
        let note_at: Option<Millis> = track.notes.get(position.next_note)
          .map(|note| note.start).filter(|at| *at < to);
        match (change_at, note_at) {
          (Some(change), note) if note.map_or(true, |note| change <= note) => {
            position.settings.apply(&track.changes[position.next_change].setting);
            position.next_change += 1;
            if !track.changes.get(position.next_change).is_some_and(|next| next.at == change) {
              actions.push(PlaybackAction::SettingsChanged {
                track: position.id, settings: position.settings.clone() }); } }
          (_, Some(_)) => {
            let note = track.notes[position.next_note];
            position.next_note += 1;
            if track.muted { continue; }
            let Some(tuning) = position.settings.tuning else { continue; };
            let seq: u64 = self.next_seq;
            self.next_seq += 1;
            let hz: f64 = tuning.hz(note.pitch);
            if note.end() < to {
              // starts and ends inside this window: still give it its start
              actions.push(PlaybackAction::NoteStart {
                track: position.id, seq, hz, settings: position.settings.clone() });
              actions.push(PlaybackAction::NoteEnd { track: position.id, seq });
            } else {
              position.sounding.push((seq, note.end(), hz));
              actions.push(PlaybackAction::NoteStart {
                track: position.id, seq, hz, settings: position.settings.clone() }); } }
          _ => break, } } }
    self.position = to;
    actions }

  /// End every sounding note, as on a stop.
  pub fn stop(&mut self) -> Vec<PlaybackAction> {
    let mut actions: Vec<PlaybackAction> = vec![];
    for position in self.tracks.iter_mut() {
      for (seq, _, _) in position.sounding.drain(..) {
        actions.push(PlaybackAction::NoteEnd { track: position.id, seq }); } }
    actions }

  /// The frequencies every track is sounding now.
  pub fn sounding_hz(&self) -> Vec<f64> {
    self.tracks.iter()
      .flat_map(|position| position.sounding.iter().map(|(_, _, hz)| *hz))
      .collect() }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::editor::track::{Note, Setting, SettingChange, TrackTuning};

  fn track() -> Track {
    let mut track: Track = Track::new("t");
    track.changes = vec![
      SettingChange { at: 0, setting: Setting::Tuning(
        TrackTuning { edo: 12, fundamental_hz: 100.0 }) },
      SettingChange { at: 0, setting: Setting::Timbre(0) },
      SettingChange { at: 1000, setting: Setting::Timbre(2) }, ];
    track.notes = vec![
      Note { start: 0, length: 1500, pitch: 12 },
      Note { start: 1000, length: 10, pitch: 0 }, ];
    track }

  fn starts(actions: &[PlaybackAction]) -> Vec<(f64, Option<usize>)> {
    actions.iter().filter_map(|action| match action {
      PlaybackAction::NoteStart { hz, settings, .. } => Some((*hz, settings.timbre)),
      _ => None, }).collect() }

  #[test]
  fn notes_start_and_end_as_the_playhead_passes_them() {
    let track: Track = track();
    let tracks: Vec<PlayedTrack> = vec![PlayedTrack { id: 7, track: &track }];
    let (mut playback, actions) = Playback::start(&tracks, 0);
    assert!(actions.is_empty());
    assert_eq!(starts(&playback.advance(&tracks, 10)), vec![(200.0, Some(0))]);
    assert_eq!(playback.sounding_hz(), vec![200.0]);
    let at_one_second: Vec<PlaybackAction> = playback.advance(&tracks, 1100);
    // the timbre change at 1000 comes before the short note, which starts and ends
    assert_eq!(starts(&at_one_second), vec![(100.0, Some(2))]);
    assert!(at_one_second.iter().any(|a| matches!(a, PlaybackAction::SettingsChanged { .. })));
    let ends: usize = playback.advance(&tracks, 2000).iter()
      .filter(|a| matches!(a, PlaybackAction::NoteEnd { .. })).count();
    assert_eq!(ends, 1);
    assert!(playback.sounding_hz().is_empty()); }

  #[test]
  fn starting_mid_note_sounds_it_at_once() {
    let track: Track = track();
    let tracks: Vec<PlayedTrack> = vec![PlayedTrack { id: 7, track: &track }];
    let (_, actions) = Playback::start(&tracks, 500);
    assert_eq!(starts(&actions), vec![(200.0, Some(0))]); }

  #[test]
  fn a_muted_track_sounds_nothing() {
    let mut track: Track = track();
    track.muted = true;
    let tracks: Vec<PlayedTrack> = vec![PlayedTrack { id: 7, track: &track }];
    let (mut playback, _) = Playback::start(&tracks, 0);
    assert!(starts(&playback.advance(&tracks, 3000)).is_empty()); }
}
