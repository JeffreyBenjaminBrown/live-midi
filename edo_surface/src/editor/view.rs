//! What the editor's window shows, and where a click lands. Pure: it draws
//! a `Session` onto a `Canvas`; the window module owns the real window.
//!
//! Top to bottom: a status bar, a time ruler, one lane per track, and a line
//! of key help. A lane has a header on the left (the track's name) and its
//! timeline on the right: notes as bars, higher pitches higher, over a strip
//! showing the timbre (its color) and the volume (the strip's white line).
//! Time scrolls horizontally, shared by every lane; lanes scroll vertically.

use super::canvas::{Canvas, LINE_H};
use super::recorder::RecordMode;
use super::session::{Frame, Selection};
use super::track::{Millis, Note, Setting, Settings, Track};
use super::track_text::seconds;

pub const HEADER_W: i32 = 170;
pub const STATUS_H: i32 = 24;
pub const RULER_H: i32 = 20;
pub const LANE_H: i32 = 90;
pub const PARAMETER_H: i32 = 12;
pub const HELP_H: i32 = 3 * LINE_H + 4;

const BACKGROUND: u32 = 0x1A1C21;
const LANE_BACKGROUND: [u32; 2] = [0x22252C, 0x1F2228];
const HEADER_BACKGROUND: u32 = 0x2A2E37;
const CURRENT_HEADER: u32 = 0x3C4A63;
const TEXT: u32 = 0xD8DCE4;
const FAINT_TEXT: u32 = 0x8A909C;
const GRID_LINE: u32 = 0x30343C;
const NOTE: u32 = 0x8EC5FF;
const MUTED_NOTE: u32 = 0x5A6270;
const PLAYHEAD: u32 = 0xFF5555;
const SELECTION: u32 = 0x6FA8FF;
const RIG_MARK: u32 = 0xE0B050;
/// One color per timbre slot, left to right.
const TIMBRE_COLORS: [u32; 4] = [0x4F8A6E, 0x5B6FB0, 0xA0704C, 0x9A4F7E];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
  /// The time at the timeline's left edge.
  pub left_ms: Millis,
  pub ms_per_px: f64,
  /// The track drawn in the top lane.
  pub first_lane: usize,
}

impl Default for View {
  fn default() -> View { View { left_ms: 0, ms_per_px: 10.0, first_lane: 0 } }
}

/// Where a point of the window lies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
  /// The header of track `track` (which may be past the last track).
  Header(usize),
  /// The timeline, in track `track`'s lane (which may be past the last).
  Timeline { track: usize, at: Millis },
  Ruler(Millis),
  Nothing,
}

impl View {
  pub fn lanes_top() -> i32 { STATUS_H + RULER_H }

  pub fn x_of(
    &self,
    at: Millis,
  ) -> i32 {
    HEADER_W + ((at - self.left_ms) as f64 / self.ms_per_px).round() as i32 }

  pub fn at_of(
    &self,
    x: i32,
  ) -> Millis {
    (self.left_ms + ((x - HEADER_W) as f64 * self.ms_per_px).round() as Millis).max(0) }

  pub fn visible_lanes(
    &self,
    canvas_h: i32,
  ) -> usize {
    ((canvas_h - Self::lanes_top() - HELP_H) / LANE_H).max(1) as usize }

  pub fn hit(
    &self,
    x: i32,
    y: i32,
    canvas_h: i32,
  ) -> Hit {
    if y < STATUS_H || y >= canvas_h - HELP_H { return Hit::Nothing; }
    if y < Self::lanes_top() {
      return if x >= HEADER_W { Hit::Ruler(self.at_of(x)) } else { Hit::Nothing }; }
    let track: usize = self.first_lane + ((y - Self::lanes_top()) / LANE_H) as usize;
    if x < HEADER_W { Hit::Header(track) } else { Hit::Timeline { track, at: self.at_of(x) } } }

  /// Scroll so `at` is visible, keeping some room on its right.
  pub fn follow(
    &mut self,
    at: Millis,
    canvas_w: i32,
  ) {
    let right: Millis = self.at_of(canvas_w - 40);
    if at > right || at < self.left_ms {
      self.left_ms = (at - ((canvas_w - HEADER_W) as f64 * self.ms_per_px * 0.1) as Millis).max(0); } }
}

pub fn render(
  canvas: &mut Canvas,
  frame: &Frame,
  view: &View,
) {
  canvas.fill(0, 0, canvas.width, canvas.height, BACKGROUND);
  let lanes: usize = view.visible_lanes(canvas.height);
  for lane in 0..lanes {
    let index: usize = view.first_lane + lane;
    let top: i32 = View::lanes_top() + lane as i32 * LANE_H;
    render_lane(canvas, frame, view, index, top); }
  if let Some(selection) = frame.selection { render_selection(canvas, view, &selection, lanes); }
  render_ruler(canvas, view);
  let x: i32 = view.x_of(frame.position);
  if x >= HEADER_W {
    canvas.vline(x, STATUS_H, canvas.height - STATUS_H - HELP_H, PLAYHEAD); }
  render_status(canvas, frame);
  render_help(canvas); }

fn render_status(
  canvas: &mut Canvas,
  frame: &Frame,
) {
  canvas.fill(0, 0, canvas.width, STATUS_H, 0x111317);
  let (label, color): (&str, u32) = match (frame.rolling, frame.recording) {
    (_, Some(RecordMode::Notes)) => ("RECORDING", 0xC03030),
    (_, Some(RecordMode::Parameters)) => ("OVERDUB", 0xD07020),
    (true, None) => ("PLAYING", 0x308040),
    (false, None) => ("STOPPED", 0x444A55), };
  canvas.fill(4, 3, 96, STATUS_H - 6, color);
  canvas.text(10, 4, label, 0xFFFFFF, 90);
  let summary: String = format!(
    "{} s   track: {}   rig: {}   {}",
    seconds(frame.position), frame.current_name(), frame.rig_name, frame.status);
  canvas.text(110, 4, &summary, TEXT, canvas.width - 114); }

fn render_help(canvas: &mut Canvas) {
  let top: i32 = canvas.height - HELP_H;
  canvas.fill(0, top, canvas.width, HELP_H, 0x111317);
  let lines: [&str; 3] = [
    "space play/stop   r record   o overdub parameters   esc stop   home to start",
    "n new track   ctrl-k delete track   m mute   up/down choose track   g next rig   l reload rigs",
    "drag select   ctrl-c/x/v copy/cut/paste   ctrl-shift-v insert   del clear   \
     shift-del remove time   ctrl-z/y undo/redo   wheel scroll   +/- zoom", ];
  for (row, line) in lines.iter().enumerate() {
    canvas.text(6, top + 2 + row as i32 * LINE_H, line, FAINT_TEXT, canvas.width - 12); } }

fn render_ruler(
  canvas: &mut Canvas,
  view: &View,
) {
  let top: i32 = STATUS_H;
  canvas.fill(0, top, canvas.width, RULER_H, 0x15171B);
  let step: Millis = ruler_step(view.ms_per_px);
  let first: Millis = (view.left_ms / step) * step;
  let mut at: Millis = first;
  while view.x_of(at) < canvas.width {
    let x: i32 = view.x_of(at);
    if x >= HEADER_W {
      canvas.vline(x, top + RULER_H - 6, 6, FAINT_TEXT);
      canvas.text(x + 3, top + 2, &ruler_label(at, step), FAINT_TEXT, 80); }
    at += step; } }

/// The ruler's spacing: the smallest of a few round steps that leaves at
/// least 70 pixels between labels.
fn ruler_step(ms_per_px: f64) -> Millis {
  [100, 250, 500, 1000, 2000, 5000, 10_000, 30_000, 60_000, 300_000]
    .into_iter()
    .find(|step| *step as f64 / ms_per_px >= 70.0)
    .unwrap_or(600_000) }

fn ruler_label(
  at: Millis,
  step: Millis,
) -> String {
  if step % 1000 == 0 { format!("{}s", at / 1000) }
  else { format!("{:.2}s", at as f64 / 1000.0).replace(".00s", "s") } }

fn render_selection(
  canvas: &mut Canvas,
  view: &View,
  selection: &Selection,
  lanes: usize,
) {
  let first_lane: usize = selection.first_track.max(view.first_lane);
  let last_lane: usize = selection.last_track.min(view.first_lane + lanes - 1);
  if first_lane > last_lane { return; }
  let x0: i32 = view.x_of(selection.from).max(HEADER_W);
  let x1: i32 = view.x_of(selection.to).min(canvas.width);
  let y0: i32 = View::lanes_top() + (first_lane - view.first_lane) as i32 * LANE_H;
  let y1: i32 = View::lanes_top() + (last_lane - view.first_lane + 1) as i32 * LANE_H;
  canvas.shade(x0, y0, (x1 - x0).max(1), y1 - y0, SELECTION, 60);
  canvas.vline(x0, y0, y1 - y0, SELECTION);
  canvas.vline(x1, y0, y1 - y0, SELECTION); }

fn render_lane(
  canvas: &mut Canvas,
  frame: &Frame,
  view: &View,
  index: usize,
  top: i32,
) {
  canvas.fill(HEADER_W, top, canvas.width - HEADER_W, LANE_H, LANE_BACKGROUND[index % 2]);
  canvas.hline(0, top + LANE_H - 1, canvas.width, GRID_LINE);
  let Some(song_track) = frame.tracks.get(index) else {
    canvas.fill(0, top, HEADER_W - 1, LANE_H - 1, BACKGROUND);
    return; };
  let track: &Track = &song_track.track;
  let header: u32 = if index == frame.current_track { CURRENT_HEADER } else { HEADER_BACKGROUND };
  canvas.fill(0, top, HEADER_W - 1, LANE_H - 1, header);
  canvas.text(6, top + 4, &track.name, TEXT, HEADER_W - 12);
  canvas.text(6, top + 4 + LINE_H, &format!("{} notes", track.notes.len()),
    FAINT_TEXT, HEADER_W - 12);
  { // the rig and tuning in force at the left edge
    let settings: Settings = track.settings_at(view.left_ms);
    let tuning: String = settings.tuning.map_or("no tuning".into(), |t| format!("{} edo", t.edo));
    canvas.text(6, top + 4 + 2 * LINE_H, &tuning, RIG_MARK, HEADER_W - 12);
    canvas.text(6, top + 4 + 3 * LINE_H, settings.rig.as_deref().unwrap_or("no rig"),
      RIG_MARK, HEADER_W - 12); }
  if track.muted { canvas.text(HEADER_W - 50, top + 4, "MUTED", 0xE07070, 48); }
  if frame.recording.is_some() && index == frame.current_track {
    canvas.text(HEADER_W - 42, top + 4 + LINE_H, "REC", 0xFF6060, 40); }
  render_parameters(canvas, view, track, top + LANE_H - 1 - PARAMETER_H);
  render_rig_marks(canvas, view, track, top);
  render_notes(canvas, view, track, top + 4, LANE_H - PARAMETER_H - 10); }

/// The timbre as the strip's color, the volume (-60..0 dB) as a white line
/// across it.
fn render_parameters(
  canvas: &mut Canvas,
  view: &View,
  track: &Track,
  top: i32,
) {
  let mut settings: Settings = track.settings_before(view.left_ms);
  let mut from: Millis = view.left_ms;
  let right: Millis = view.at_of(canvas.width);
  let mut changes = track.changes.iter().filter(|c| c.at >= view.left_ms && c.at <= right).peekable();
  loop {
    let until: Millis = changes.peek().map_or(right, |change| change.at);
    let (x0, x1) = (view.x_of(from).max(HEADER_W), view.x_of(until).min(canvas.width));
    if let Some(slot) = settings.timbre {
      canvas.fill(x0, top, x1 - x0, PARAMETER_H, TIMBRE_COLORS[slot % TIMBRE_COLORS.len()]); }
    if let Some(db) = settings.volume_db {
      let height: i32 = (((db + 60.0) / 60.0).clamp(0.0, 1.0) * (PARAMETER_H - 2) as f32) as i32;
      canvas.hline(x0, top + PARAMETER_H - 1 - height, x1 - x0, 0xFFFFFF); }
    let Some(change) = changes.next() else { break; };
    settings.apply(&change.setting);
    from = change.at; } }

/// A gold mark where the rig or tuning changes after the track's start,
/// labelled. (The header names the ones in force at the left edge.)
fn render_rig_marks(
  canvas: &mut Canvas,
  view: &View,
  track: &Track,
  top: i32,
) {
  for change in &track.changes {
    let label: String = match &change.setting {
      Setting::Rig(rig) => rig.clone(),
      Setting::Tuning(tuning) => format!("{} edo", tuning.edo),
      _ => continue, };
    let x: i32 = view.x_of(change.at);
    let is_first: bool = !track.changes.iter()
      .any(|c| c.at < change.at && c.setting.kind() == change.setting.kind());
    if is_first || x < HEADER_W || x >= canvas.width { continue; }
    canvas.vline(x, top, LANE_H - 1, RIG_MARK);
    let row: i32 = if matches!(change.setting, Setting::Rig(_)) { 0 } else { 1 };
    let y: i32 = top + LANE_H - PARAMETER_H - 2 * LINE_H + row * LINE_H - 2;
    let width: i32 = canvas.text_width(&label).min(200) + 4;
    canvas.fill(x + 1, y, width, LINE_H, 0x15171B);
    canvas.text(x + 3, y, &label, RIG_MARK, 200); } }

fn render_notes(
  canvas: &mut Canvas,
  view: &View,
  track: &Track,
  top: i32,
  height: i32,
) {
  let notes: Vec<(Note, f64)> = notes_with_hz(track);
  if notes.is_empty() { return; }
  let octaves = |hz: f64| hz.log2();
  let low: f64 = notes.iter().map(|(_, hz)| octaves(*hz)).fold(f64::INFINITY, f64::min);
  let high: f64 = notes.iter().map(|(_, hz)| octaves(*hz)).fold(f64::NEG_INFINITY, f64::max);
  let span: f64 = (high - low).max(0.25);
  let bar_h: i32 = 3;
  let color: u32 = if track.muted { MUTED_NOTE } else { NOTE };
  let right: Millis = view.at_of(canvas.width);
  for (note, hz) in notes.iter().filter(|(n, _)| n.end() >= view.left_ms && n.start <= right) {
    let y: i32 = top + ((high - octaves(*hz)) / span * (height - bar_h) as f64).round() as i32;
    let x0: i32 = view.x_of(note.start).max(HEADER_W);
    let x1: i32 = view.x_of(note.end()).min(canvas.width);
    canvas.fill(x0, y, (x1 - x0).max(2), bar_h, color);
    canvas.fill(view.x_of(note.start).max(HEADER_W), y - 1, 1, bar_h + 2, 0xFFFFFF); } }

/// Each note with its frequency in the tuning in force where it starts.
pub fn notes_with_hz(track: &Track) -> Vec<(Note, f64)> {
  let mut settings: Settings = Settings::default();
  let mut changes = track.changes.iter().peekable();
  let mut out: Vec<(Note, f64)> = Vec::with_capacity(track.notes.len());
  for note in &track.notes {
    while let Some(change) = changes.next_if(|change| change.at <= note.start) {
      settings.apply(&change.setting); }
    if let Some(tuning) = settings.tuning { out.push((*note, tuning.hz(note.pitch))); } }
  out }

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;
  use std::time::Instant;
  use crate::editor::session::Session;
  use std::sync::{Arc, Mutex};
  use crate::editor::rig_layout::editor_rig;
  use crate::editor::song::Song;
  use crate::editor::sound::Sound;
  use crate::editor::track::{SettingChange, TrackTuning};
  use crate::rig::load_named_rig;
  use crate::types::VoiceMap;

  const PLAIN: &str = "timbres-volume-scroll_41edo";

  fn sample_session() -> Session {
    let rig = editor_rig(PLAIN, &load_named_rig(PLAIN).unwrap()).unwrap();
    let sound: Sound = Sound::new(Arc::new(Mutex::new(VoiceMap::new())), 48000.0);
    let dir: PathBuf = std::env::temp_dir().join("edo-editor-view-never-saved");
    let mut session: Session = Session::new(Song::load(&dir).unwrap(), rig, sound);
    for (index, name) in ["melody", "bass"].iter().enumerate() {
      let mut track: Track = Track::new(name);
      track.changes = vec![
        SettingChange { at: 0, setting: Setting::Rig(PLAIN.into()) },
        SettingChange { at: 0, setting: Setting::Tuning(
          TrackTuning { edo: 41, fundamental_hz: 110.0 }) },
        SettingChange { at: 0, setting: Setting::Timbre(index) },
        SettingChange { at: 0, setting: Setting::VolumeDb(-12.0) },
        SettingChange { at: 3000, setting: Setting::Timbre(3) },
        SettingChange { at: 4500, setting: Setting::VolumeDb(-30.0) },
        SettingChange { at: 6000, setting: Setting::Tuning(
          TrackTuning { edo: 31, fundamental_hz: 110.0 }) }, ];
      track.notes = (0..24).map(|i| Note {
        start: i * 350, length: 300, pitch: 41 * (1 - index as i32) + (i as i32 * 7) % 30 }).collect();
      session.song.push(track); }
    session.selection = Some(Selection { from: 1500, to: 2600, first_track: 0, last_track: 1 });
    session.playhead = 2000;
    session.song.tracks[1].track.muted = true;
    session }

  #[test]
  fn a_click_lands_in_the_lane_and_time_it_was_drawn_at() {
    let view: View = View { left_ms: 1000, ms_per_px: 5.0, first_lane: 1 };
    let y: i32 = View::lanes_top() + LANE_H + 5;
    assert_eq!(view.hit(view.x_of(3000), y, 800), Hit::Timeline { track: 2, at: 3000 });
    assert_eq!(view.hit(10, y, 800), Hit::Header(2)); }

  /// Set EDO_EDITOR_SNAPSHOT to a path to get a PNG of a sample frame.
  #[test]
  fn snapshot() {
    let session: Session = sample_session();
    let mut frame: Frame = Frame::default();
    session.refresh_frame(Instant::now(), &mut frame);
    let mut canvas: Canvas = Canvas::new(1100, 400);
    render(&mut canvas, &frame, &View::default());
    if let Some(path) = std::env::var_os("EDO_EDITOR_SNAPSHOT") {
      std::fs::write(path, canvas.png()).unwrap(); } }
}
