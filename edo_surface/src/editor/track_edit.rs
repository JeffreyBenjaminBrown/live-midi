//! Editing a track over a region of time: copy, clear, remove, paste, and
//! merging a recorded take.
//!
//! Two rules hold throughout.
//! - A note belongs where it starts. A region holds the notes that start in
//!   it, at their full length; clearing a region also cuts short a note that
//!   started before it and would sound into it.
//! - An edit never changes the settings after the region. Whatever an edit
//!   does to the settings inside a region, a change at the region's end
//!   restores what was in force there before.

use super::track::{
  Millis, Note, Setting, SettingChange, SettingKind, Settings, Track, TrackKind, TrackTuning,
  SETTING_KINDS,
};

/// A piece of one track, positioned from 0.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Clip {
  /// The kind of track it came from, and the only kind it pastes into.
  pub kind: TrackKind,
  pub length: Millis,
  /// The settings in force where the clip was copied from.
  pub start_settings: Settings,
  /// The changes after the clip's start, with times from its start.
  pub changes: Vec<SettingChange>,
  /// The notes starting in the clip, with times from its start.
  pub notes: Vec<Note>,
  /// The section boundaries in the clip, with times from its start.
  pub boundaries: Vec<Millis>,
}

/// The region `[from, to)` of `track` as a clip.
pub fn copy(
  track: &Track,
  from: Millis,
  to: Millis,
) -> Clip {
  Clip {
    kind: track.kind,
    length: to - from,
    start_settings: track.settings_at(from),
    changes: track.changes.iter()
      .filter(|change| change.at > from && change.at < to)
      .map(|change| SettingChange { at: change.at - from, setting: change.setting.clone() })
      .collect(),
    notes: track.notes.iter()
      .filter(|note| note.start >= from && note.start < to)
      .map(|note| Note { start: note.start - from, ..*note })
      .collect(),
    boundaries: track.boundaries.iter()
      .filter(|b| **b >= from && **b < to)
      .map(|b| b - from)
      .collect(), } }

/// Empty `[from, to)`: silence there, and everything after it unchanged.
pub fn clear(
  track: &mut Track,
  from: Millis,
  to: Millis,
) {
  let settings_at_end: Settings = track.settings_before(to);
  truncate_notes_sounding_into(track, from);
  track.notes.retain(|note| note.start < from || note.start >= to);
  track.changes.retain(|change| change.at < from || change.at >= to);
  track.boundaries.retain(|b| *b < from || *b >= to);
  restore_settings_at(track, to, &settings_at_end, &SETTING_KINDS);
  track.normalize(); }

/// Delete `[from, to)` and move everything after it left to close the gap.
pub fn remove(
  track: &mut Track,
  from: Millis,
  to: Millis,
) {
  clear(track, from, to);
  shift_from(track, to, from - to);
  track.normalize(); }

/// Paste `clip` at `at`, replacing what lies under it.
pub fn paste_over(
  track: &mut Track,
  clip: &Clip,
  at: Millis,
) {
  let end: Millis = at + clip.length;
  let settings_at_end: Settings = track.settings_before(end);
  clear(track, at, end);
  track.changes.extend(clip.start_settings.as_changes_at(at));
  track.changes.extend(clip.changes.iter()
    .map(|change| SettingChange { at: change.at + at, setting: change.setting.clone() }));
  track.notes.extend(clip.notes.iter().map(|note| Note { start: note.start + at, ..*note }));
  track.boundaries.extend(clip.boundaries.iter().map(|b| b + at));
  restore_settings_at(track, end, &settings_at_end, &SETTING_KINDS);
  track.normalize(); }

/// Paste `clip` at `at`, first moving everything from `at` on right by the
/// clip's length to make room.
pub fn paste_inserting(
  track: &mut Track,
  clip: &Clip,
  at: Millis,
) {
  truncate_notes_sounding_into(track, at);
  shift_from(track, at, clip.length);
  track.normalize();
  paste_over(track, clip, at); }

/// Cut short every note that starts before `at` and sounds past it.
fn truncate_notes_sounding_into(
  track: &mut Track,
  at: Millis,
) {
  for note in track.notes.iter_mut() {
    if note.start < at && note.end() > at {
      note.length = at - note.start; } } }

/// Make `setting` hold throughout `[from, to)`, replacing that kind's
/// changes there; after `to`, what held before comes back.
pub fn set_throughout(
  track: &mut Track,
  from: Millis,
  to: Millis,
  setting: Setting,
) {
  let kind: SettingKind = setting.kind();
  let settings_at_end: Settings = track.settings_before(to);
  track.changes.retain(|change|
    change.setting.kind() != kind || change.at < from || change.at >= to);
  track.changes.push(SettingChange { at: from, setting });
  restore_settings_at(track, to, &settings_at_end, &[kind]);
  track.normalize(); }

/// Make `setting` hold over the whole track: every change of its kind goes,
/// and one at 0 takes their place.
pub fn set_whole_track(
  track: &mut Track,
  setting: Setting,
) {
  let kind: SettingKind = setting.kind();
  track.changes.retain(|change| change.setting.kind() != kind);
  track.changes.push(SettingChange { at: 0, setting });
  track.normalize(); }

/// Move every note starting at or after `from`, and every change, by `by`.
fn shift_from(
  track: &mut Track,
  from: Millis,
  by: Millis,
) {
  for note in track.notes.iter_mut().filter(|note| note.start >= from) {
    note.start += by; }
  for change in track.changes.iter_mut().filter(|change| change.at >= from) {
    change.at += by; }
  for boundary in track.boundaries.iter_mut().filter(|b| **b >= from) {
    *boundary += by; } }

/// Make each of `kinds` take its `wanted` value from `at` on, by a change at
/// `at`, unless a change of that kind is already there (it decides) or the
/// value in force already is the wanted one. An unset wanted value can't be
/// restored, so it is left alone, except one with a default (ghosts), which
/// is restored to its default.
fn restore_settings_at(
  track: &mut Track,
  at: Millis,
  wanted: &Settings,
  kinds: &[SettingKind],
) {
  let in_force: Settings = track.settings_before(at);
  for kind in kinds {
    let already_changed_there: bool = track.changes.iter()
      .any(|change| change.at == at && change.setting.kind() == *kind);
    if already_changed_there { continue; }
    if let Some(setting) = wanted.get_or_default(*kind) {
      if in_force.get_or_default(*kind).as_ref() != Some(&setting) {
        track.changes.push(SettingChange { at, setting }); } } } }

/// What one recording pass produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Take {
  pub start: Millis,
  pub stop: Millis,
  /// The live instrument's settings when the pass began.
  pub live_settings: Settings,
  /// Pitches in `live_settings.tuning`; a note still held at the stop ends there.
  pub notes: Vec<Note>,
  /// The parameter changes (timbre and volume) the player made, in time order.
  pub parameter_changes: Vec<SettingChange>,
}

/// Merge `take` into `track`.
/// - Its notes join the track's.
/// - Where it has notes, the track takes the live rig and tuning over the
///   take; notes already there are re-pitched to the nearest step of the
///   live tuning.
/// - A parameter the player touched replaces the track's values of that
///   parameter from the first touch to the stop.
/// - A parameter the track has never set takes the live value at the start,
///   so a new track sounds the way it was played.
pub fn merge_take(
  track: &mut Track,
  take: &Take,
) {
  if !take.notes.is_empty() {
    adopt_live_rig_and_tuning(track, take); }
  for kind in [SettingKind::Timbre, SettingKind::Volume] {
    let touched: Vec<&SettingChange> = take.parameter_changes.iter()
      .filter(|change| change.setting.kind() == kind)
      .collect();
    if track.settings_at(take.start).get(kind).is_none() {
      if let Some(live) = take.live_settings.get(kind) {
        track.changes.push(SettingChange { at: take.start, setting: live }); } }
    let Some(first_touch) = touched.first().map(|change| change.at) else { continue; };
    let settings_at_stop: Settings = track.settings_before(take.stop);
    track.changes.retain(|change| change.setting.kind() != kind
      || change.at < first_touch || change.at >= take.stop);
    track.changes.extend(touched.into_iter().cloned());
    restore_settings_at(track, take.stop, &settings_at_stop, &[kind]); }
  track.notes.extend(take.notes.iter().copied());
  track.normalize(); }

fn adopt_live_rig_and_tuning(
  track: &mut Track,
  take: &Take,
) {
  let settings_at_stop: Settings = track.settings_before(take.stop);
  if let Some(live_tuning) = take.live_settings.tuning {
    let old_tunings: Vec<Option<TrackTuning>> = track.notes.iter()
      .map(|note| track.settings_at(note.start).tuning)
      .collect();
    for (note, old_tuning) in track.notes.iter_mut().zip(old_tunings) {
      let in_take: bool = note.start >= take.start && note.start < take.stop;
      if let (true, Some(old_tuning)) = (in_take, old_tuning) {
        if old_tuning != live_tuning {
          note.pitch = live_tuning.nearest_pitch(old_tuning.hz(note.pitch)); } } } }
  let rig_or_tuning = |change: &SettingChange|
    matches!(change.setting, Setting::Rig(_) | Setting::Tuning(_));
  track.changes.retain(|change|
    !rig_or_tuning(change) || change.at < take.start || change.at >= take.stop);
  for kind in [SettingKind::Rig, SettingKind::Tuning] {
    if let Some(setting) = take.live_settings.get(kind) {
      track.changes.push(SettingChange { at: take.start, setting }); } }
  restore_settings_at(
    track, take.stop, &settings_at_stop, &[SettingKind::Rig, SettingKind::Tuning]);
  track.normalize(); }

#[cfg(test)]
mod tests {
  use super::*;

  fn tuning(edo: i32) -> TrackTuning { TrackTuning { edo, fundamental_hz: 110.0 } }

  fn change(at: Millis, setting: Setting) -> SettingChange { SettingChange { at, setting } }

  fn note(start: Millis, length: Millis, pitch: i32) -> Note { Note { start, length, pitch } }

  /// Notes at 0, 1000, 2000 and 3000 ms, each 500 long; timbre 0 from 0 and
  /// 1 from 1500; volume -12 from 0.
  fn sample() -> Track {
    let mut track: Track = Track::new("t");
    track.changes = vec![
      change(0, Setting::Rig("r".into())),
      change(0, Setting::Tuning(tuning(41))),
      change(0, Setting::Timbre(0)),
      change(0, Setting::VolumeDb(-12.0)),
      change(1500, Setting::Timbre(1)), ];
    track.notes = (0..4).map(|i| note(i * 1000, 500, i as i32)).collect();
    track.normalize();
    track }

  #[test]
  fn clear_empties_the_region_and_keeps_what_follows() {
    let mut track: Track = sample();
    clear(&mut track, 900, 2100);
    assert_eq!(track.notes, vec![note(0, 500, 0), note(3000, 500, 3)]);
    // the timbre change at 1500 is gone, but timbre 1 still holds after the region
    assert_eq!(track.settings_at(2100).timbre, Some(1));
    assert_eq!(track.settings_at(2000).timbre, Some(0)); }

  #[test]
  fn clear_cuts_short_a_note_sounding_into_the_region() {
    let mut track: Track = sample();
    clear(&mut track, 200, 600);
    assert_eq!(track.notes[0], note(0, 200, 0)); }

  #[test]
  fn remove_closes_the_gap() {
    let mut track: Track = sample();
    remove(&mut track, 1000, 2000);
    assert_eq!(track.notes, vec![note(0, 500, 0), note(1000, 500, 2), note(2000, 500, 3)]);
    assert_eq!(track.settings_at(1000).timbre, Some(1)); }

  #[test]
  fn copy_then_paste_over_reproduces_the_region_and_restores_after_it() {
    let mut track: Track = sample();
    let clip: Clip = copy(&track, 1000, 2000); // timbre 0 at its start, 1 from 500
    paste_over(&mut track, &clip, 3000);
    assert_eq!(track.notes.last(), Some(&note(3000, 500, 1)));
    assert_eq!(track.settings_at(3000).timbre, Some(0));
    assert_eq!(track.settings_at(3600).timbre, Some(1));
    assert_eq!(track.settings_at(4000).timbre, Some(1)); }

  #[test]
  fn paste_over_restores_the_settings_after_the_clip() {
    let mut track: Track = sample();
    let clip: Clip = copy(&track, 0, 1000); // timbre 0 throughout
    paste_over(&mut track, &clip, 1800);
    assert_eq!(track.settings_at(2000).timbre, Some(0));
    assert_eq!(track.settings_at(2800).timbre, Some(1));
    assert_eq!(track.notes.iter().filter(|n| n.start == 2000).count(), 0); }

  #[test]
  fn paste_inserting_moves_what_follows_right() {
    let mut track: Track = sample();
    let clip: Clip = copy(&track, 0, 1000);
    paste_inserting(&mut track, &clip, 1000);
    let starts: Vec<Millis> = track.notes.iter().map(|n| n.start).collect();
    assert_eq!(starts, vec![0, 1000, 2000, 3000, 4000]);
    assert_eq!(track.settings_at(2600).timbre, Some(1));
    assert_eq!(track.settings_at(2400).timbre, Some(0)); }

  #[test]
  fn a_region_set_to_one_timbre_keeps_it_throughout_and_restores_after() {
    let mut track: Track = sample();
    set_throughout(&mut track, 500, 2500, Setting::Timbre(3));
    assert_eq!(track.settings_at(400).timbre, Some(0));
    assert_eq!(track.settings_at(500).timbre, Some(3));
    assert_eq!(track.settings_at(1600).timbre, Some(3)); // the change at 1500 is gone
    assert_eq!(track.settings_at(2500).timbre, Some(1)); }

  #[test]
  fn boundaries_cut_paste_and_remove_like_notes() {
    let mut track: Track = Track::new_boundaries("sections");
    track.boundaries = vec![0, 1000, 2000, 3000];
    let clip: Clip = copy(&track, 900, 2100);
    assert_eq!(clip.boundaries, vec![100, 1100]);
    clear(&mut track, 900, 2100);
    assert_eq!(track.boundaries, vec![0, 3000]);
    paste_over(&mut track, &clip, 4000);
    assert_eq!(track.boundaries, vec![0, 3000, 4100, 5100]);
    remove(&mut track, 3500, 4500);
    assert_eq!(track.boundaries, vec![0, 3000, 4100]); }

  #[test]
  fn a_take_into_a_new_track_sets_everything_at_its_start() {
    let mut track: Track = Track::new("new");
    let live: Settings = Settings {
      rig: Some("r".into()), tuning: Some(tuning(41)), timbre: Some(2), volume_db: Some(-6.0),
      ghosts: None };
    let take: Take = Take {
      start: 500, stop: 2000, live_settings: live.clone(),
      notes: vec![note(700, 100, 5)],
      parameter_changes: vec![change(1000, Setting::VolumeDb(-8.0))], };
    merge_take(&mut track, &take);
    assert_eq!(track.settings_at(500), live);
    assert_eq!(track.settings_at(1000).volume_db, Some(-8.0));
    assert_eq!(track.notes, vec![note(700, 100, 5)]); }

  #[test]
  fn a_touched_parameter_latches_from_first_touch_to_the_stop() {
    let mut track: Track = sample();
    let take: Take = Take {
      start: 0, stop: 2500, live_settings: track.settings_at(0),
      notes: vec![],
      parameter_changes: vec![change(1200, Setting::Timbre(3))], };
    merge_take(&mut track, &take);
    assert_eq!(track.settings_at(1100).timbre, Some(0));
    assert_eq!(track.settings_at(1600).timbre, Some(3)); // the old change at 1500 is gone
    assert_eq!(track.settings_at(2500).timbre, Some(1)); // restored at the stop
    assert_eq!(track.notes.len(), 4); }

  #[test]
  fn a_take_in_another_tuning_repitches_the_notes_under_it() {
    let mut track: Track = sample();
    let mut live: Settings = track.settings_at(0);
    live.tuning = Some(tuning(82));
    let take: Take = Take {
      start: 900, stop: 2100, live_settings: live,
      notes: vec![note(1100, 100, 9)], parameter_changes: vec![], };
    merge_take(&mut track, &take);
    let pitches: Vec<i32> = track.notes.iter().map(|n| n.pitch).collect();
    assert_eq!(pitches, vec![0, 2, 9, 4, 3]); // 1 and 2 in 41-edo doubled
    assert_eq!(track.settings_at(1000).tuning, Some(tuning(82)));
    assert_eq!(track.settings_at(2100).tuning, Some(tuning(41))); }
}
