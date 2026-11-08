//! What the editor's window shows, and where a click lands. Pure: it draws
//! a `Session` onto a `Canvas`; the window module owns the real window.
//!
//! Top to bottom: a status bar, a time ruler, and one lane per track. The
//! keys are listed in a help overlay ('?'), drawn by `render_help`. A lane has a header on the left (the track's name) and its
//! timeline on the right: notes as bars, higher pitches higher, over a strip
//! showing the timbre (its color) and the volume (the strip's white line).
//! Time scrolls horizontally, shared by every lane; lanes scroll vertically.

use super::canvas::{Canvas, LINE_H};
use super::recorder::RecordMode;
use super::session::{edit_scope, Frame, Selection};
use super::track_panel::{self, Row};
use super::track::{Ghosts, Millis, Note, Setting, Settings, Track, TrackKind};
use super::track_text::seconds;

pub const HEADER_W: i32 = 170;
pub const STATUS_H: i32 = 24;
pub const RULER_H: i32 = 20;
pub const LANE_H: i32 = 90;
pub const PARAMETER_H: i32 = 12;

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
/// The mark of a region whose notes make no scale ghosts.
const GHOSTLESS: u32 = 0xB07CC6;
/// Pixels between a ghostless region's hatching lines.
const HATCH_SPACING: i32 = 9;
/// Alternate scale ghost sections' bands.
const SECTION_SHADES: [u32; 2] = [0x3A3550, 0x2E3A48];
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
    ((canvas_h - Self::lanes_top()) / LANE_H).max(1) as usize }

  pub fn hit(
    &self,
    x: i32,
    y: i32,
    canvas_h: i32,
  ) -> Hit {
    if y < STATUS_H || y >= canvas_h { return Hit::Nothing; }
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
  render_boundary_lines(canvas, frame, view);
  if let Some(selection) = &frame.selection { render_selection(canvas, view, selection, lanes); }
  render_ruler(canvas, view);
  let x: i32 = view.x_of(frame.position);
  if x >= HEADER_W {
    canvas.vline(x, STATUS_H, canvas.height - STATUS_H, PLAYHEAD); }
  render_status(canvas, frame); }

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
  let speed: String = if frame.speed == 1.0 { String::new() }
    else { format!("   speed {}x", frame.speed) };
  let summary: String = format!(
    "{} s{speed}   track: {}   rig: {}   {}",
    seconds(frame.position), frame.current_name(), frame.rig_name, frame.status);
  let hint: &str = "? help";
  let hint_w: i32 = canvas.text_width(hint);
  canvas.text(110, 4, &summary, TEXT, canvas.width - 124 - hint_w);
  canvas.text(canvas.width - hint_w - 8, 4, hint, FAINT_TEXT, hint_w); }

/// One row of the help overlay: a heading, or a key and what it does.
enum HelpRow {
  Heading(&'static str),
  Key(&'static str, &'static str),
}

const HELP: &[HelpRow] = &[
  HelpRow::Heading("transport"),
  HelpRow::Key("space", "play, or stop"),
  HelpRow::Key("r", "record notes"),
  HelpRow::Key("o", "overdub timbre, volume"),
  HelpRow::Key("esc", "stop"),
  HelpRow::Key("home", "playhead to start"),
  HelpRow::Key("[, ]", "previous, next boundary"),
  HelpRow::Key("{, }", "slower, faster"),
  HelpRow::Key("\\", "normal speed"),
  HelpRow::Key("b", "section boundary"),
  HelpRow::Heading("tracks"),
  HelpRow::Key("n", "new track"),
  HelpRow::Key("p", "track parameters"),
  HelpRow::Key("f2", "rename track"),
  HelpRow::Key("ctrl-k", "delete track"),
  HelpRow::Key("m", "mute, or unmute"),
  HelpRow::Key("up, down", "choose track"),
  HelpRow::Heading("rigs"),
  HelpRow::Key("g", "next rig"),
  HelpRow::Key("l", "reload the rig"),
  HelpRow::Key("ctrl-e", "render to mp3"),
  HelpRow::Key("ctrl-q", "quit"),
  HelpRow::Heading("editing"),
  HelpRow::Key("drag", "select time"),
  HelpRow::Key("ctrl-a", "select whole tracks"),
  HelpRow::Key("1 2 3 4", "timbre of selection"),
  HelpRow::Key("h", "ghosts of selection"),
  HelpRow::Key("ctrl-click", "a header: add, drop"),
  HelpRow::Key("ctrl-c, -x", "copy, cut"),
  HelpRow::Key("ctrl-v", "paste over"),
  HelpRow::Key("ctrl-shift-v", "paste, pushing right"),
  HelpRow::Key("delete", "clear the selection"),
  HelpRow::Key("shift-delete", "remove the time"),
  HelpRow::Key("ctrl-z, -y", "undo, redo"),
  HelpRow::Heading("view"),
  HelpRow::Key("wheel", "scroll time"),
  HelpRow::Key("on headers", "scroll tracks"),
  HelpRow::Key("ctrl-wheel", "zoom"),
  HelpRow::Key("+, -", "zoom"),
  HelpRow::Key("left, right", "scroll time"),
];

/// The track panel ('p'): the current track's parameters in the help's big
/// font, the chosen row (`row`) highlighted.
pub fn render_track_panel(
  canvas: &mut Canvas,
  frame: &Frame,
  row: usize,
) {
  let Some(song_track) = frame.tracks.get(frame.current_track) else { return; };
  let track: &Track = &song_track.track;
  let scope: Option<(Millis, Millis)> = edit_scope(&frame.selection, frame.current_track);
  let rows: &[Row] = track_panel::rows(track.kind);
  canvas.shade(0, 0, canvas.width, canvas.height, 0x0B0C0F, 235);
  let line_h: i32 = (canvas.height / HELP_ROWS).max(12);
  let px: f32 = line_h as f32 / 1.25;
  let char_w: i32 = canvas.char_width_at(px);
  let margin: i32 = line_h / 2;
  let where_: String = match scope {
    Some((from, to)) => format!("from {} to {} s", seconds(from), seconds(to)),
    None => "the whole track".into(), };
  canvas.text_at_size(margin, margin, &format!("{}: {where_}", track.name),
    RIG_MARK, canvas.width - 2 * margin, px);
  let label_chars: i32 = 9;
  for (index, panel_row) in rows.iter().enumerate() {
    let y: i32 = margin + (index as i32 + 2) * line_h;
    if index == row { canvas.shade(0, y, canvas.width, line_h, SELECTION, 70); }
    let color: u32 = if track_panel::editable(*panel_row) { TEXT } else { FAINT_TEXT };
    canvas.text_at_size(margin + char_w, y, track_panel::label(*panel_row), 0xFFFFFF,
      canvas.width, px);
    canvas.text_at_size(margin + char_w * (label_chars + 2), y,
      &track_panel::value(*panel_row, track, scope), color, canvas.width, px); }
  let hint: &str =
    "up/down choose   left/right change (shift: more)   enter rename   esc or p closes";
  canvas.text_at_size(margin, canvas.height - line_h, hint, FAINT_TEXT, canvas.width, px * 0.6); }

/// The rows the help shows per screen height.
const HELP_ROWS: i32 = 20;

/// The keys over the whole window, in columns of whole groups (a heading
/// and its keys), in as big a font as fits: about twenty rows to the
/// window's height when the groups fit in that many rows' columns, smaller
/// when they need more. '?' or escape closes it.
pub fn render_help(canvas: &mut Canvas) {
  canvas.shade(0, 0, canvas.width, canvas.height, 0x0B0C0F, 235);
  let groups: Vec<&[HelpRow]> = help_groups();
  let key_chars: i32 = HELP.iter()
    .filter_map(|row| match row { HelpRow::Key(key, _) => Some(key.len() as i32), _ => None })
    .max().unwrap_or(0) + 2;
  let action_chars: i32 = HELP.iter()
    .filter_map(|row| match row { HelpRow::Key(_, action) => Some(action.len() as i32), _ => None })
    .max().unwrap_or(0);
  let column_chars: i32 = key_chars + action_chars + 3;
  let width_at_one_px: f32 = canvas.char_width_at(100.0) as f32 / 100.0;
  // try row heights from a twentieth of the window down; keep the biggest font
  let (line_h, columns, px): (i32, Vec<Vec<usize>>, f32) = (20..=40)
    .map(|rows_per_height| {
      let line_h: i32 = (canvas.height / rows_per_height).max(10);
      let margin: i32 = line_h / 2;
      let max_rows: usize = ((canvas.height - 2 * margin - line_h) / line_h).max(1) as usize;
      let columns: Vec<Vec<usize>> = pack_columns(&groups, max_rows);
      let count: i32 = columns.len() as i32;
      let column_w: i32 = (canvas.width - (count + 1) * margin) / count;
      let px: f32 = (line_h as f32 / 1.25)
        .min(column_w as f32 / (column_chars as f32 * width_at_one_px));
      (line_h, columns, px) })
    .max_by(|a, b| a.2.total_cmp(&b.2))
    .unwrap();
  let margin: i32 = line_h / 2;
  let column_w: i32 = (canvas.width - (columns.len() as i32 + 1) * margin) / columns.len() as i32;
  let char_w: i32 = canvas.char_width_at(px);
  for (column, group_indices) in columns.iter().enumerate() {
    let x: i32 = margin + column as i32 * (column_w + margin);
    let rows = group_indices.iter().flat_map(|index| groups[*index].iter());
    for (row_in_column, row) in rows.enumerate() {
      let y: i32 = margin + row_in_column as i32 * line_h;
      match row {
        HelpRow::Heading(heading) =>
          canvas.text_at_size(x, y, heading, RIG_MARK, column_w, px),
        HelpRow::Key(key, action) => {
          canvas.text_at_size(x + char_w, y, key, 0xFFFFFF, column_w, px);
          canvas.text_at_size(x + char_w * (key_chars + 1), y, action, TEXT, column_w, px); } } } }
  let close: &str = "? or esc closes this";
  canvas.text_at_size(canvas.width - margin - canvas.char_width_at(px * 0.6) * close.len() as i32,
    canvas.height - line_h, close, FAINT_TEXT, canvas.width, px * 0.6); }

/// The help rows cut into groups, each a heading and the keys under it.
fn help_groups() -> Vec<&'static [HelpRow]> {
  let starts: Vec<usize> = (0..HELP.len())
    .filter(|i| matches!(HELP[*i], HelpRow::Heading(_))).collect();
  starts.iter().enumerate()
    .map(|(n, start)| &HELP[*start..starts.get(n + 1).copied().unwrap_or(HELP.len())])
    .collect() }

/// Fill columns with whole groups, in order, none taller than `max_rows`
/// unless a single group is.
fn pack_columns(
  groups: &[&[HelpRow]],
  max_rows: usize,
) -> Vec<Vec<usize>> {
  let mut columns: Vec<Vec<usize>> = vec![vec![]];
  let mut used: usize = 0;
  for (index, group) in groups.iter().enumerate() {
    if used > 0 && used + group.len() > max_rows {
      columns.push(vec![]);
      used = 0; }
    columns.last_mut().unwrap().push(index);
    used += group.len(); }
  columns }

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
  let x0: i32 = view.x_of(selection.from).max(HEADER_W);
  let x1: i32 = view.x_of(selection.to).min(canvas.width);
  for track in &selection.tracks {
    if *track < view.first_lane || *track >= view.first_lane + lanes { continue; }
    let y: i32 = View::lanes_top() + (track - view.first_lane) as i32 * LANE_H;
    canvas.shade(x0, y, (x1 - x0).max(1), LANE_H, SELECTION, 60);
    canvas.shade(0, y, HEADER_W - 1, LANE_H - 1, SELECTION, 40);
    canvas.vline(x0, y, LANE_H, SELECTION);
    canvas.vline(x1, y, LANE_H, SELECTION); } }

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
  if track.kind == TrackKind::Boundaries {
    canvas.text(6, top + 4 + LINE_H, &format!("{} boundaries", track.boundaries.len()),
      FAINT_TEXT, HEADER_W - 12);
    canvas.text(6, top + 4 + 2 * LINE_H, "ghost sections", FAINT_TEXT, HEADER_W - 12);
    render_sections(canvas, view, track, top);
    render_ghostless(canvas, view, track, top);
    return; }
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
  render_ghostless(canvas, view, track, top);
  render_parameters(canvas, view, track, top + LANE_H - 1 - PARAMETER_H);
  render_rig_marks(canvas, view, track, top);
  render_notes(canvas, view, track, top + 4, LANE_H - PARAMETER_H - 10); }

/// A boundaries lane: each section (between adjacent boundaries) a band,
/// alternating in shade, and each boundary a white line.
fn render_sections(
  canvas: &mut Canvas,
  view: &View,
  track: &Track,
  top: i32,
) {
  for (index, pair) in track.boundaries.windows(2).enumerate() {
    let x0: i32 = view.x_of(pair[0]).max(HEADER_W);
    let x1: i32 = view.x_of(pair[1]).min(canvas.width);
    if x1 <= x0 { continue; }
    let shade: u32 = if index % 2 == 0 { SECTION_SHADES[0] } else { SECTION_SHADES[1] };
    canvas.fill(x0, top + 8, x1 - x0, LANE_H - 17, shade); }
  for boundary in &track.boundaries {
    let x: i32 = view.x_of(*boundary);
    if x >= HEADER_W && x < canvas.width { canvas.fill(x - 1, top, 3, LANE_H - 1, 0xFFFFFF); } } }

/// Each section boundary as a faint line through every lane.
fn render_boundary_lines(
  canvas: &mut Canvas,
  frame: &Frame,
  view: &View,
) {
  let Some(boundaries) = frame.tracks.iter().find(|t| t.track.kind == TrackKind::Boundaries)
    else { return; };
  for boundary in &boundaries.track.boundaries {
    let x: i32 = view.x_of(*boundary);
    if x >= HEADER_W && x < canvas.width {
      canvas.shade(x, View::lanes_top(), 1, canvas.height - View::lanes_top(), 0xFFFFFF, 70); } } }

/// Where the track's ghosts are other than the default: the lane darkened
/// and hatched, '/' for while-sounding and '\' for no, and labelled.
fn render_ghostless(
  canvas: &mut Canvas,
  view: &View,
  track: &Track,
  top: i32,
) {
  let right: Millis = view.at_of(canvas.width);
  let mut spans: Vec<(Millis, Millis, Ghosts)> = vec![]; // [from, to) and what holds
  let mut since: (Millis, Ghosts) = (view.left_ms, track.settings_before(view.left_ms).ghost_mode());
  for change in track.changes.iter().filter(|c| c.at >= view.left_ms && c.at <= right) {
    let Setting::Ghosts(mode) = change.setting else { continue; };
    spans.push((since.0, change.at, since.1));
    since = (change.at, mode); }
  spans.push((since.0, right, since.1));
  for (from, to, mode) in spans {
    let label: &str = match mode {
      Ghosts::Always => continue,
      Ghosts::WhileSounding => "while sounding",
      Ghosts::Never => "no ghosts", };
    let x0: i32 = view.x_of(from).max(HEADER_W);
    let x1: i32 = view.x_of(to).min(canvas.width);
    if x1 <= x0 { continue; }
    let height: i32 = LANE_H - PARAMETER_H - 1;
    canvas.shade(x0, top, x1 - x0, height, 0x000000, 70);
    // diagonal hatching: '/' while-sounding, '\' no; anchored to the
    // window, so it stays put as the band's edges scroll
    let rising: bool = mode == Ghosts::WhileSounding;
    for y in top..top + height {
      for x in x0..x1 {
        let diagonal: i32 = if rising { x + y } else { x - y };
        if diagonal.rem_euclid(HATCH_SPACING) == 0 { canvas.shade(x, y, 1, 1, GHOSTLESS, 160); } } }
    let label_w: i32 = canvas.text_width(label).min(x1 - x0 - 4);
    canvas.fill(x0 + 1, top + 2, label_w + 4, LINE_H, 0x15171B);
    canvas.text(x0 + 3, top + 2, label, GHOSTLESS, x1 - x0 - 4); } }

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
  let notes: Vec<(Note, f64)> = track.notes_with_hz();
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
        SettingChange { at: 5000, setting: Setting::Ghosts(Ghosts::WhileSounding) },
        SettingChange { at: 6500, setting: Setting::Ghosts(Ghosts::Never) },
        SettingChange { at: 8000, setting: Setting::Ghosts(Ghosts::Always) },
        SettingChange { at: 6000, setting: Setting::Tuning(
          TrackTuning { edo: 31, fundamental_hz: 110.0 }) }, ];
      track.notes = (0..24).map(|i| Note {
        start: i * 350, length: 300, pitch: 41 * (1 - index as i32) + (i as i32 * 7) % 30 }).collect();
      session.song.push(track); }
    let sections: usize = session.song.boundaries_track_or_new();
    session.song.tracks[sections].track.boundaries = vec![0, 2000, 4500, 8000];
    session.selection = Some(Selection::across(1500, 2600, 1, 2));
    session.playhead = 2000;
    session.song.tracks[2].track.muted = true;
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
      std::fs::write(&path, canvas.png()).unwrap();
      let mut help: Canvas = Canvas::new(1280, 720);
      render(&mut help, &frame, &View::default());
      render_help(&mut help);
      std::fs::write(format!("{}.help.png", path.to_string_lossy()), help.png()).unwrap();
      let mut panel: Canvas = Canvas::new(1280, 720);
      render(&mut panel, &frame, &View::default());
      let mut on_melody: Frame = frame.clone();
      on_melody.current_track = 1;
      render_track_panel(&mut panel, &on_melody, 2);
      std::fs::write(format!("{}.panel.png", path.to_string_lossy()), panel.png()).unwrap(); } }
}
