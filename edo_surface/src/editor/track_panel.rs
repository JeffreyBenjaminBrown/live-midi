//! The track panel ('p'): a track's parameters, one per row, to read and
//! change from the keyboard. Pure: it says what each row shows and what a
//! key on a row asks for; the window carries that out through the session.
//!
//! A parameter that changes over time (timbre, volume, ghosts, rig) is
//! edited over a scope: the selection, where it covers the track, or else
//! the whole track. The panel shows its value at the scope's start, marked
//! "varies" when it changes inside the scope.

use super::track::{Ghosts, Millis, Setting, SettingKind, Settings, Track, TrackKind};
use crate::timbre_slots::TIMBRE_SLOTS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row { Name, Muted, Ghosts, Timbre, Volume, Rig, Tuning }

/// What a key on a row asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
  Rename,
  ToggleMute,
  /// Give `Setting` to the scope.
  Set(Setting),
  Nothing,
}

/// The rows a track of `kind` has, top to bottom.
pub fn rows(kind: TrackKind) -> &'static [Row] {
  match kind {
    TrackKind::Notes =>
      &[Row::Name, Row::Muted, Row::Ghosts, Row::Timbre, Row::Volume, Row::Rig, Row::Tuning],
    TrackKind::Boundaries => &[Row::Name, Row::Ghosts], } }

pub fn label(row: Row) -> &'static str {
  match row {
    Row::Name => "name",
    Row::Muted => "muted",
    Row::Ghosts => "ghosts",
    Row::Timbre => "timbre",
    Row::Volume => "volume",
    Row::Rig => "rig",
    Row::Tuning => "tuning", } }

/// What a row shows for `track` over `scope` (`None`: the whole track).
pub fn value(
  row: Row,
  track: &Track,
  scope: Option<(Millis, Millis)>,
) -> String {
  let (from, to): (Millis, Millis) = scope.unwrap_or((0, Millis::MAX));
  let settings: Settings = track.settings_at(from);
  let varies = |kind: SettingKind| track.changes.iter()
    .any(|change| change.setting.kind() == kind && change.at > from && change.at < to);
  let with_varies = |text: String, kind: SettingKind|
    if varies(kind) { format!("{text}, varies") } else { text };
  match row {
    Row::Name => track.name.clone(),
    Row::Muted => yes_no(track.muted).into(),
    Row::Ghosts => with_varies(settings.ghost_mode().word().into(), SettingKind::Ghosts),
    Row::Timbre => with_varies(
      settings.timbre.map_or("unset".into(), |slot| slot.to_string()), SettingKind::Timbre),
    Row::Volume => with_varies(
      settings.volume_db.map_or("unset".into(), |db| format!("{db} dB")), SettingKind::Volume),
    Row::Rig => with_varies(settings.rig.unwrap_or_else(|| "none".into()), SettingKind::Rig),
    Row::Tuning => with_varies(
      settings.tuning.map_or("none".into(), |t| format!("{} edo, {} hz", t.edo, t.fundamental_hz)),
      SettingKind::Tuning), } }

/// Whether a row can be changed here (the tuning can't: changing it would
/// move every recorded pitch).
pub fn editable(row: Row) -> bool { row != Row::Tuning }

/// What `step` (-1 left, +1 right; `big` with shift) asks for on `row`.
/// `rigs` are the rigs the rig row steps through.
pub fn step(
  row: Row,
  track: &Track,
  scope: Option<(Millis, Millis)>,
  by: i32,
  big: bool,
  rigs: &[String],
) -> Request {
  let settings: Settings = track.settings_at(scope.map_or(0, |(from, _)| from));
  match row {
    Row::Name => Request::Rename,
    Row::Muted => Request::ToggleMute,
    Row::Ghosts => {
      let mode: Ghosts = settings.ghost_mode();
      Request::Set(Setting::Ghosts(if by < 0 { mode.next().next() } else { mode.next() })) }
    Row::Timbre => {
      let slot: i32 = settings.timbre.map_or(0, |slot| slot as i32 + by);
      Request::Set(Setting::Timbre(slot.clamp(0, TIMBRE_SLOTS as i32 - 1) as usize)) }
    Row::Volume => {
      let step_db: f32 = if big { 6.0 } else { 1.0 };
      let db: f32 = settings.volume_db.unwrap_or(0.0) + by as f32 * step_db;
      Request::Set(Setting::VolumeDb(db.clamp(-60.0, 0.0))) }
    Row::Rig => {
      if rigs.is_empty() { return Request::Nothing; }
      let here: Option<usize> = settings.rig.and_then(|rig| rigs.iter().position(|r| *r == rig));
      let next: usize = match here {
        Some(index) => (index as i32 + by).rem_euclid(rigs.len() as i32) as usize,
        None => 0, };
      Request::Set(Setting::Rig(rigs[next].clone())) }
    Row::Tuning => Request::Nothing, } }

fn yes_no(flag: bool) -> &'static str { if flag { "yes" } else { "no" } }

#[cfg(test)]
mod tests {
  use super::*;
  use crate::editor::track::SettingChange;

  fn track() -> Track {
    let mut track: Track = Track::new("t");
    track.changes = vec![
      SettingChange { at: 0, setting: Setting::Timbre(1) },
      SettingChange { at: 0, setting: Setting::VolumeDb(-12.0) },
      SettingChange { at: 2000, setting: Setting::Timbre(2) }, ];
    track }

  #[test]
  fn a_row_shows_its_value_at_the_scope_start_and_whether_it_varies() {
    let track: Track = track();
    assert_eq!(value(Row::Timbre, &track, None), "1, varies");
    assert_eq!(value(Row::Timbre, &track, Some((2500, 3000))), "2");
    assert_eq!(value(Row::Ghosts, &track, None), "yes");
    assert_eq!(value(Row::Volume, &track, None), "-12 dB"); }

  #[test]
  fn stepping_a_row_asks_for_the_next_value() {
    let track: Track = track();
    assert_eq!(step(Row::Timbre, &track, None, 1, false, &[]), Request::Set(Setting::Timbre(2)));
    assert_eq!(step(Row::Volume, &track, None, -1, true, &[]),
      Request::Set(Setting::VolumeDb(-18.0)));
    assert_eq!(step(Row::Ghosts, &track, None, 1, false, &[]),
      Request::Set(Setting::Ghosts(Ghosts::WhileSounding)));
    assert_eq!(step(Row::Ghosts, &track, None, -1, false, &[]),
      Request::Set(Setting::Ghosts(Ghosts::Never)));
    let rigs: Vec<String> = vec!["a".into(), "b".into()];
    assert_eq!(step(Row::Rig, &track, None, 1, false, &rigs),
      Request::Set(Setting::Rig("a".into()))); }
}
