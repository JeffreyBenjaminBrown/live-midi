//! The editor's window: reads the mouse and keyboard, turns them into
//! `Session` commands, and draws `view::render` at 60 frames a second.
//! Closing the window (or ctrl-q) ends the editor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use minifb::{InputCallback, Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};

use super::canvas::Canvas;
use super::recorder::RecordMode;
use super::rig_layout::{editor_rig, EditorRig};
use super::render::render_mp3;
use super::session::{edit_scope, Frame, Selection, Session};
use super::track::Millis;
use super::track_panel::{self, Request, Row};
use super::view::{render, render_help, render_track_panel, Hit, View, HEADER_W};
use crate::rig::{load_named_rig, rig_dir};

/// A drag that has moved less than this many pixels is a click.
const CLICK_SLOP_PX: f32 = 3.0;

/// The characters typed since the last frame, collected by minifb.
struct Typed(Arc<Mutex<Vec<char>>>);

impl InputCallback for Typed {
  fn add_char(
    &mut self,
    uni_char: u32,
  ) {
    if let Some(ch) = char::from_u32(uni_char) {
      self.0.lock().unwrap_or_else(|e| e.into_inner()).push(ch); } }
}

struct Drag {
  track: usize,
  at: Millis,
  x: f32,
  moved: bool,
}

pub fn run_window(
  session: &Arc<Mutex<Session>>,
  stop: &Arc<AtomicBool>,
) -> Result<(), String> {
  if std::env::var("DISPLAY").is_err() { std::env::set_var("DISPLAY", ":0"); }
  let mut window: Window = Window::new(
    "edo editor", 1280, 720, WindowOptions { resize: true, ..WindowOptions::default() })
    .map_err(|e| format!(
      "the editor's window did not open ({e}); run `xhost +local:` on the host"))?;
  window.set_target_fps(60);
  let typed: Arc<Mutex<Vec<char>>> = Arc::new(Mutex::new(vec![]));
  window.set_input_callback(Box::new(Typed(Arc::clone(&typed))));
  // a track's new name while it is being typed (F2)
  let mut renaming: Option<String> = None;
  // the key help covers the window while shown ('?' toggles, escape closes)
  let mut help_shown: bool = false;
  // the track panel's chosen row, while it shows ('p')
  let mut panel: Option<usize> = None;
  let mut canvas: Canvas = Canvas::new(1280, 720);
  let mut view: View = View::default();
  let mut drag: Option<Drag> = None;
  let rigs: Vec<String> = editor_rig_names();
  let shared: Arc<Mutex<Session>> = Arc::clone(session); // for threads the window starts
  let mut frame: Frame = Frame::default();
  while window.is_open() && !stop.load(Ordering::SeqCst) {
    let (width, height) = window.get_size();
    canvas.resize(width as i32, height as i32);
    let now: Instant = Instant::now();
    { // the lock is held for the input and the copy, never for the drawing
      let mut session: MutexGuard<Session> = session.lock().unwrap_or_else(|e| e.into_inner());
      let typed_now: Vec<char> =
        std::mem::take(&mut *typed.lock().unwrap_or_else(|e| e.into_inner()));
      if help_shown {
        if typed_now.contains(&'?') || window.is_key_pressed(Key::Escape, KeyRepeat::No) {
          help_shown = false; }
      } else if renaming.is_some() {
        type_name(&window, &mut session, &mut renaming, &typed_now, now);
      } else if let Some(row) = panel.as_mut() {
        match panel_keys(&window, &mut session, row, &rigs, now) {
          PanelOutcome::Stay => {}
          PanelOutcome::Close => panel = None,
          PanelOutcome::Rename => renaming = Some(session.current_name()), }
      } else if window.is_key_pressed(Key::P, KeyRepeat::No) && !session.song.tracks.is_empty() {
        panel = Some(0);
      } else if typed_now.contains(&'?') {
        help_shown = true;
      } else if window.is_key_pressed(Key::F2, KeyRepeat::No) && !session.song.tracks.is_empty() {
        renaming = Some(session.current_name());
      } else {
        if window.is_key_pressed(Key::E, KeyRepeat::No)
        && (window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl)) {
        start_render(Arc::clone(&shared), &mut session); }
      let keep_going: bool = handle_keys(&window, &mut session, &mut view, &rigs, now, &canvas);
        if !keep_going { break; } }
      handle_mouse(&window, &mut session, &mut view, &mut drag, now, &canvas);
      session.refresh_frame(now, &mut frame);
      if let Some(name) = &renaming {
        frame.status = format!("rename to: {name}_   (enter keeps, esc cancels)"); } }
    if frame.rolling { view.follow(frame.position, canvas.width); }
    render(&mut canvas, &frame, &view);
    if let Some(row) = panel { render_track_panel(&mut canvas, &frame, row); }
    if help_shown { render_help(&mut canvas); }
    window.update_with_buffer(&canvas.pixels, width, height).map_err(|e| e.to_string())?; }
  stop.store(true, Ordering::SeqCst);
  Ok(()) }

/// Obey the keys pressed this frame; false means quit.
fn handle_keys(
  window: &Window,
  session: &mut Session,
  view: &mut View,
  rigs: &[String],
  now: Instant,
  canvas: &Canvas,
) -> bool {
  let ctrl: bool = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
  let shift: bool = window.is_key_down(Key::LeftShift) || window.is_key_down(Key::RightShift);
  let page_ms: Millis = ((canvas.width - HEADER_W) as f64 * view.ms_per_px) as Millis;
  // only navigation repeats while held: a held space must not toggle
  let repeating = |key: &Key| matches!(key,
    Key::Left | Key::Right | Key::Up | Key::Down | Key::Equal | Key::Minus
    | Key::NumPadPlus | Key::NumPadMinus);
  let pressed: Vec<Key> = window.get_keys_pressed(KeyRepeat::Yes).into_iter()
    .filter(|key| repeating(key) || window.is_key_pressed(*key, KeyRepeat::No))
    .collect();
  for key in pressed {
    match (key, ctrl, shift) {
      (Key::Q, true, _) => return false,
      (Key::Space, _, _) =>
        if session.is_rolling() { session.stop(now) } else { session.play(now) },
      (Key::Escape, _, _) => session.stop(now),
      (Key::R, false, _) => session.record(now, RecordMode::Notes),
      (Key::O, false, _) => session.record(now, RecordMode::Parameters),
      (Key::Home, _, _) => { session.set_playhead(now, 0); view.left_ms = 0; }
      (Key::N, false, _) => session.new_track(now),
      (Key::K, true, _) => session.delete_current_track(now),
      (Key::M, false, _) => session.toggle_mute(now),
      (Key::Up, _, _) => choose_track(session, view, -1, canvas),
      (Key::Down, _, _) => choose_track(session, view, 1, canvas),
      (Key::Left, _, _) => view.left_ms = (view.left_ms - page_ms / 4).max(0),
      (Key::Right, _, _) => view.left_ms += page_ms / 4,
      (Key::Equal | Key::NumPadPlus, _, _) => zoom(view, 0.8, HEADER_W),
      (Key::Minus | Key::NumPadMinus, _, _) => zoom(view, 1.25, HEADER_W),
      (Key::C, true, _) => session.copy_selection(),
      (Key::X, true, _) => session.cut_selection(now),
      (Key::V, true, inserting) => session.paste(now, inserting),
      (Key::Delete | Key::Backspace, false, false) => session.clear_selection(now),
      (Key::Delete | Key::Backspace, false, true) => session.remove_selection(now),
      (Key::Z, true, false) => session.undo(now),
      (Key::Z, true, true) | (Key::Y, true, _) => session.redo(now),
      (Key::A, true, _) => session.select_whole_tracks(),
      (Key::Key1 | Key::NumPad1, false, _) => session.set_selection_timbre(now, 0),
      (Key::Key2 | Key::NumPad2, false, _) => session.set_selection_timbre(now, 1),
      (Key::Key3 | Key::NumPad3, false, _) => session.set_selection_timbre(now, 2),
      (Key::Key4 | Key::NumPad4, false, _) => session.set_selection_timbre(now, 3),
      (Key::LeftBracket, _, false) => session.jump_to_boundary(now, false),
      (Key::RightBracket, _, false) => session.jump_to_boundary(now, true),
      (Key::LeftBracket, _, true) => session.step_speed(now, -1),
      (Key::RightBracket, _, true) => session.step_speed(now, 1),
      (Key::Backslash, _, _) => session.step_speed(now, 0),
      (Key::B, false, _) => session.add_boundary(now),
      (Key::H, false, _) => session.cycle_selection_ghosts(now),
      (Key::G, false, _) => next_rig(session, rigs, now),
      (Key::L, false, _) => reload_rig(session, now),
      _ => {} } }
  true }

/// Render the song to '<song dir>/<song name>.mp3' on a thread of its own,
/// reporting in the status bar when it is done.
fn start_render(
  shared: Arc<Mutex<Session>>,
  session: &mut Session,
) {
  let tracks = session.song.tracks.clone();
  let name: String = session.song.dir.file_name()
    .map_or("song".into(), |name| name.to_string_lossy().into_owned());
  let path = session.song.dir.join(format!("{name}.mp3"));
  session.status = format!("rendering {} ...", path.display());
  std::thread::spawn(move || {
    let result: Result<(), String> = render_mp3(&tracks, &path);
    shared.lock().unwrap_or_else(|e| e.into_inner()).status = match result {
      Ok(()) => format!("rendered {}", path.display()),
      Err(error) => format!("render failed: {error}"), }; }); }

enum PanelOutcome { Stay, Close, Rename }

/// The track panel's keys: up and down choose a row, left and right (with
/// shift, in bigger steps) change it, enter changes it too (or renames),
/// escape or p closes.
fn panel_keys(
  window: &Window,
  session: &mut Session,
  row: &mut usize,
  rigs: &[String],
  now: Instant,
) -> PanelOutcome {
  let Some(song_track) = session.song.tracks.get(session.current_track) else {
    return PanelOutcome::Close; };
  let rows: &[Row] = track_panel::rows(song_track.track.kind);
  *row = (*row).min(rows.len() - 1);
  if window.is_key_pressed(Key::Escape, KeyRepeat::No) || window.is_key_pressed(Key::P, KeyRepeat::No) {
    return PanelOutcome::Close; }
  if window.is_key_pressed(Key::Up, KeyRepeat::Yes) { *row = row.saturating_sub(1); }
  if window.is_key_pressed(Key::Down, KeyRepeat::Yes) { *row = (*row + 1).min(rows.len() - 1); }
  let shift: bool = window.is_key_down(Key::LeftShift) || window.is_key_down(Key::RightShift);
  let by: i32 =
    if window.is_key_pressed(Key::Left, KeyRepeat::Yes) { -1 }
    else if window.is_key_pressed(Key::Right, KeyRepeat::Yes)
      || window.is_key_pressed(Key::Enter, KeyRepeat::No) { 1 }
    else { return PanelOutcome::Stay; };
  let scope = edit_scope(&session.selection, session.current_track);
  match track_panel::step(rows[*row], &song_track.track, scope, by, shift, rigs) {
    Request::Rename => return PanelOutcome::Rename,
    Request::ToggleMute => session.toggle_mute(now),
    Request::Set(setting) => session.set_current_track_setting(now, setting),
    Request::Nothing => {} }
  PanelOutcome::Stay }

/// Typing a track's new name: characters append, backspace deletes, enter
/// renames, escape cancels.
fn type_name(
  window: &Window,
  session: &mut Session,
  renaming: &mut Option<String>,
  typed: &[char],
  now: Instant,
) {
  let Some(name) = renaming.as_mut() else { return; };
  name.extend(typed.iter().filter(|c| !c.is_control()));
  if window.is_key_pressed(Key::Backspace, KeyRepeat::Yes) { name.pop(); }
  if window.is_key_pressed(Key::Escape, KeyRepeat::No) { *renaming = None; return; }
  if window.is_key_pressed(Key::Enter, KeyRepeat::No)
    || window.is_key_pressed(Key::NumPadEnter, KeyRepeat::No) {
    let name: String = name.trim().to_string();
    session.rename_current_track(now, &name);
    *renaming = None; } }

fn choose_track(
  session: &mut Session,
  view: &mut View,
  by: i64,
  canvas: &Canvas,
) {
  if session.song.tracks.is_empty() { return; }
  let last: i64 = session.song.tracks.len() as i64 - 1;
  session.current_track = (session.current_track as i64 + by).clamp(0, last) as usize;
  let lanes: usize = view.visible_lanes(canvas.height);
  if session.current_track < view.first_lane { view.first_lane = session.current_track; }
  if session.current_track >= view.first_lane + lanes {
    view.first_lane = session.current_track + 1 - lanes; } }

/// Scale the time axis by `factor`, keeping the time under `x` in place.
fn zoom(
  view: &mut View,
  factor: f64,
  x: i32,
) {
  let anchor: Millis = view.at_of(x);
  view.ms_per_px = (view.ms_per_px * factor).clamp(0.25, 2000.0);
  view.left_ms = (anchor - ((x - HEADER_W) as f64 * view.ms_per_px) as Millis).max(0); }

fn handle_mouse(
  window: &Window,
  session: &mut Session,
  view: &mut View,
  drag: &mut Option<Drag>,
  now: Instant,
  canvas: &Canvas,
) {
  let Some((x, y)) = window.get_mouse_pos(MouseMode::Pass) else { return; };
  let hit: Hit = view.hit(x as i32, y as i32, canvas.height);
  if let Some((wheel_x, wheel_y)) = window.get_scroll_wheel() {
    let ctrl: bool = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
    if ctrl {
      zoom(view, if wheel_y > 0.0 { 0.85 } else { 1.18 }, (x as i32).max(HEADER_W));
    } else if matches!(hit, Hit::Header(_)) {
      let lanes: i64 = session.song.tracks.len() as i64;
      let by: i64 = if wheel_y > 0.0 { -1 } else { 1 };
      view.first_lane = (view.first_lane as i64 + by).clamp(0, (lanes - 1).max(0)) as usize;
    } else {
      let pixels: f32 = if wheel_x != 0.0 { wheel_x } else { -wheel_y };
      view.left_ms = (view.left_ms + (pixels as f64 * 20.0 * view.ms_per_px) as Millis).max(0); } }
  let down: bool = window.get_mouse_down(MouseButton::Left);
  let last_track: usize = session.song.tracks.len().saturating_sub(1);
  match (down, drag.as_mut()) {
    (true, None) => {
      let clicked: Option<(usize, Millis)> = match hit {
        Hit::Header(track) => {
          let ctrl: bool =
            window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
          if track <= last_track && !session.song.tracks.is_empty() {
            if ctrl {
              if let Some(selection) = session.selection.as_mut() { selection.toggle_track(track); }
            } else { session.current_track = track; } }
          None }
        Hit::Timeline { track, at } => Some((track.min(last_track), at)),
        Hit::Ruler(at) => Some((session.current_track, at)),
        Hit::Nothing => None, };
      if let Some((track, at)) = clicked {
        if !session.song.tracks.is_empty() { session.current_track = track; }
        session.set_playhead(now, at);
        *drag = Some(Drag { track, at, x, moved: false }); } }
    (true, Some(drag)) => {
      if (x - drag.x).abs() >= CLICK_SLOP_PX { drag.moved = true; }
      if drag.moved && !session.song.tracks.is_empty() {
        let (track, at) = match hit {
          Hit::Timeline { track, at } => (track.min(last_track), at),
          _ => (drag.track, view.at_of(x as i32)), };
        session.selection = Some(Selection::across(
          drag.at.min(at), drag.at.max(at), drag.track.min(track), drag.track.max(track))); } }
    (false, Some(_)) => {
      if !drag.as_ref().unwrap().moved { session.selection = None; }
      *drag = None; }
    (false, None) => {} } }

/// Every rig the editor can play: those whose load succeeds and that have a
/// note grid, the plain synth first.
pub fn editor_rig_names() -> Vec<String> {
  let mut names: Vec<String> = std::fs::read_dir(rig_dir()).into_iter().flatten()
    .filter_map(|entry| entry.ok())
    .filter_map(|entry| {
      let name: String = entry.file_name().to_string_lossy().into_owned();
      name.strip_suffix(".org").map(String::from) })
    .filter(|name| !name.ends_with("_readme"))
    .filter(|name| load_named_rig(name).ok()
      .is_some_and(|rig| editor_rig(name, &rig).is_ok()))
    .collect();
  names.sort();
  names }

fn next_rig(
  session: &mut Session,
  rigs: &[String],
  now: Instant,
) {
  if rigs.is_empty() { return; }
  let current: Option<usize> = rigs.iter().position(|name| *name == session.live.rig.name);
  let next: &str = &rigs[current.map_or(0, |index| (index + 1) % rigs.len())];
  load_and_use(session, next, now); }

/// Read the live rig's file again (after editing it), and forget every
/// loaded rig's sound so playback reads them again too.
fn reload_rig(
  session: &mut Session,
  now: Instant,
) {
  let name: String = session.live.rig.name.clone();
  load_and_use(session, &name, now); }

fn load_and_use(
  session: &mut Session,
  name: &str,
  now: Instant,
) {
  let rig: Result<EditorRig, String> =
    load_named_rig(name).and_then(|rig| editor_rig(name, &rig));
  match rig {
    Ok(rig) => session.change_rig(now, rig),
    Err(error) => session.status = format!("rig {name} did not load: {error}"), } }
