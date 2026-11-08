//! The text form of a track: one '.track' file per track.
//!
//! A sample:
//!
//!   # an edo-editor track; rigs/editor_readme.org describes the format
//!   muted no
//!
//!   0.000 rig timbres-volume-scroll_41edo
//!   0.000 tuning 41 edo 110 hz
//!   0.000 timbre 3
//!   0.000 volume -12 dB
//!   1.250 note 37 for 0.650
//!   2.000 ghosts while-sounding
//!
//! A line starting with a time is a setting change or a note; times and
//! lengths are seconds. '#' starts a comment. The track's name is the file's
//! stem, so it is not in the file.

use super::track::{
  Ghosts, Millis, Note, Setting, SettingChange, Track, TrackKind, TrackTuning,
};

/// The first line of a boundaries track's file.
const BOUNDARIES_HEADER: &str =
  "# scale ghost section boundaries, in seconds; rigs/editor_readme.org explains them";

pub fn write_track(track: &Track) -> String {
  if track.kind == TrackKind::Boundaries { return write_boundaries(track); }
  let mut out: String = String::new();
  out.push_str("# an edo-editor track; rigs/editor_readme.org describes the format\n");
  out.push_str(&format!("muted {}\n\n", yes_no(track.muted)));
  { // changes and notes interleaved by time, changes first at a tie
    let mut changes = track.changes.iter().peekable();
    let mut notes = track.notes.iter().peekable();
    loop {
      let change_next: bool = match (changes.peek(), notes.peek()) {
        (Some(change), Some(note)) => change.at <= note.start,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => break, };
      if change_next {
        out.push_str(&change_line(changes.next().unwrap()));
      } else {
        out.push_str(&note_line(notes.next().unwrap())); } } }
  out }

/// A boundaries track: a time per line for each boundary, and a
/// 'TIME ghosts yes|no' line where ghosts are switched on or off for the
/// whole song.
fn write_boundaries(track: &Track) -> String {
  let mut lines: Vec<(Millis, String)> = track.boundaries.iter()
    .map(|boundary| (*boundary, format!("{}\n", seconds(*boundary))))
    .chain(track.changes.iter().map(|change| (change.at, change_line(change))))
    .collect();
  lines.sort_by_key(|(at, _)| *at); // stable: a boundary before a change at its time
  let mut out: String = format!("{BOUNDARIES_HEADER}\n");
  for (_, line) in lines { out.push_str(&line); }
  out }

/// Parse a boundaries track (see `write_boundaries`); '#' starts a comment.
pub fn parse_boundaries(
  name: &str,
  text: &str,
) -> Result<Track, String> {
  let mut track: Track = Track::new_boundaries(name);
  for (index, raw_line) in text.lines().enumerate() {
    let line: &str = raw_line.split('#').next().unwrap_or("").trim();
    if line.is_empty() { continue; }
    let words: Vec<&str> = line.split_whitespace().collect();
    let parsed: Result<(), String> = match words.as_slice() {
      [time] => parse_seconds(time).map(|at| track.boundaries.push(at)),
      [time, "ghosts", word] =>
        parse_ghosts(word).and_then(|mode| push_change(&mut track, time, Setting::Ghosts(mode))),
      _ => Err("unrecognized".to_string()), };
    parsed.map_err(|error| format!("{name} line {}: {error}: {raw_line}", index + 1))?; }
  track.normalize();
  Ok(track) }

fn yes_no(flag: bool) -> &'static str { if flag { "yes" } else { "no" } }

fn change_line(change: &SettingChange) -> String {
  let body: String = match &change.setting {
    Setting::Rig(rig) => format!("rig {rig}"),
    Setting::Tuning(tuning) =>
      format!("tuning {} edo {} hz", tuning.edo, trim_float(tuning.fundamental_hz)),
    Setting::Timbre(slot) => format!("timbre {slot}"),
    Setting::VolumeDb(db) => format!("volume {} dB", trim_float(*db as f64)),
    Setting::Ghosts(mode) => format!("ghosts {}", mode.word()), };
  format!("{} {body}\n", seconds(change.at)) }

fn note_line(note: &Note) -> String {
  format!("{} note {} for {}\n", seconds(note.start), note.pitch, seconds(note.length)) }

pub fn seconds(millis: Millis) -> String {
  let sign: &str = if millis < 0 { "-" } else { "" };
  format!("{sign}{}.{:03}", millis.abs() / 1000, millis.abs() % 1000) }

/// A float without trailing zeros: 110 not 110.000000, 2.5 not 2.50.
fn trim_float(value: f64) -> String {
  let text: String = format!("{value:.4}");
  let text: &str = text.trim_end_matches('0').trim_end_matches('.');
  if text == "-0" { "0".to_string() } else { text.to_string() } }

/// Parse a track. `name` becomes its name. An error names the line.
pub fn parse_track(
  name: &str,
  text: &str,
) -> Result<Track, String> {
  let mut track: Track = Track::new(name);
  for (index, raw_line) in text.lines().enumerate() {
    let line: &str = raw_line.split('#').next().unwrap_or("").trim();
    if line.is_empty() { continue; }
    parse_line(&mut track, line)
      .map_err(|error| format!("{name}.track line {}: {error}: {raw_line}", index + 1))?; }
  track.normalize();
  Ok(track) }

fn parse_line(
  track: &mut Track,
  line: &str,
) -> Result<(), String> {
  let words: Vec<&str> = line.split_whitespace().collect();
  match words.as_slice() {
    ["muted", flag] => track.muted = parse_yes_no(flag)?,
    [time, "note", pitch, "for", length] => track.notes.push(Note {
      start: parse_seconds(time)?,
      length: parse_seconds(length)?,
      pitch: pitch.parse().map_err(|_| format!("bad pitch {pitch:?}"))?, }),
    [time, "rig", rig] => push_change(track, time, Setting::Rig(rig.to_string()))?,
    [time, "tuning", edo, "edo", hz, "hz"] => push_change(
      track, time, Setting::Tuning(TrackTuning {
        edo: edo.parse().map_err(|_| format!("bad edo {edo:?}"))?,
        fundamental_hz: hz.parse().map_err(|_| format!("bad hz {hz:?}"))?, }))?,
    [time, "timbre", slot] => push_change(
      track, time, Setting::Timbre(slot.parse().map_err(|_| format!("bad slot {slot:?}"))?))?,
    [time, "volume", db, "dB"] => push_change(
      track, time, Setting::VolumeDb(db.parse().map_err(|_| format!("bad dB {db:?}"))?))?,
    [time, "ghosts", word] => push_change(track, time, Setting::Ghosts(parse_ghosts(word)?))?,
    _ => return Err("unrecognized".to_string()), }
  Ok(()) }

fn push_change(
  track: &mut Track,
  time: &str,
  setting: Setting,
) -> Result<(), String> {
  track.changes.push(SettingChange { at: parse_seconds(time)?, setting });
  Ok(()) }

fn parse_ghosts(word: &str) -> Result<Ghosts, String> {
  Ghosts::from_word(word).ok_or(format!("expected yes, while-sounding or no, got {word:?}")) }

fn parse_yes_no(word: &str) -> Result<bool, String> {
  match word {
    "yes" => Ok(true),
    "no" => Ok(false),
    _ => Err(format!("expected yes or no, got {word:?}")), } }

pub fn parse_seconds(word: &str) -> Result<Millis, String> {
  let seconds: f64 = word.parse().map_err(|_| format!("bad seconds {word:?}"))?;
  Ok((seconds * 1000.0).round() as Millis) }

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_written_track_parses_back_to_itself() {
    let mut track: Track = Track::new("melody");
    track.muted = true;
    track.changes = vec![
      SettingChange { at: 0, setting: Setting::Rig("some-rig".into()) },
      SettingChange { at: 0, setting: Setting::Tuning(
        TrackTuning { edo: 41, fundamental_hz: 110.0 }) },
      SettingChange { at: 0, setting: Setting::Timbre(3) },
      SettingChange { at: 0, setting: Setting::VolumeDb(-12.0) },
      SettingChange { at: 2500, setting: Setting::VolumeDb(-2.5) },
      SettingChange { at: 3000, setting: Setting::Ghosts(Ghosts::WhileSounding) },
      SettingChange { at: 3500, setting: Setting::Ghosts(Ghosts::Never) }, ];
    track.notes = vec![
      Note { start: 1250, length: 650, pitch: 37 },
      Note { start: 3001, length: 1, pitch: -4 }, ];
    track.normalize();
    let text: String = write_track(&track);
    assert!(text.contains("1.250 note 37 for 0.650\n"), "{text}");
    assert!(text.contains("0.000 tuning 41 edo 110 hz\n"), "{text}");
    assert!(text.contains("2.500 volume -2.5 dB\n"), "{text}");
    assert!(text.contains("3.000 ghosts while-sounding\n"), "{text}");
    assert!(text.contains("3.500 ghosts no\n"), "{text}");
    assert_eq!(parse_track("melody", &text).unwrap(), track); }

  #[test]
  fn boundaries_are_one_time_per_line_and_read_back() {
    let mut track: Track = Track::new_boundaries("sections");
    track.boundaries = vec![0, 2500, 61_000];
    track.changes = vec![SettingChange { at: 2500, setting: Setting::Ghosts(Ghosts::Never) }];
    let text: String = write_track(&track);
    assert!(text.ends_with("0.000\n2.500\n2.500 ghosts no\n61.000\n"), "{text}");
    assert_eq!(parse_boundaries("sections", &text).unwrap(), track); }

  #[test]
  fn an_unknown_line_is_an_error_naming_it() {
    let error: String = parse_track("t", "muted no\n1.0 nonsense 3\n").unwrap_err();
    assert!(error.contains("line 2"), "{error}"); }

  #[test]
  fn comments_and_blank_lines_are_ignored() {
    let track: Track =
      parse_track("t", "# hi\n\n0.5 note 3 for 0.25 # a comment\n").unwrap();
    assert_eq!(track.notes, vec![Note { start: 500, length: 250, pitch: 3 }]); }
}
