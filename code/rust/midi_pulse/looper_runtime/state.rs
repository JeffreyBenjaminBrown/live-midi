//! The looper's shared state and the pure key/LED/playback logic.
//!
//! The edo grid plays through the `NoteSink` and (while recording) feeds the loop
//! store; the loops grid selects the active slot and drives the transport; a
//! playback step advances the sounding slot's playhead into the sink. Everything
//! here is a function of state + (event, now), so it is unit-tested without
//! sockets, threads, or cpal. Time is injected as a `Duration` since the runtime
//! epoch.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::display::build_display;
use super::edo::{register_delta, shift_for_cell, step_for_cell, Shift};
use super::loop_store::{LoopStore, PlayAction, Playback};
use super::remap::{apply_fine, apply_group_transpose, selected_pitches, Selection};
use super::sink::{NoteSource, SawNoteSink};
use super::timbre_editor::TimbreEditor;
use crate::types::Timbre;

/// Monome LED levels, in the four buckets this grid actually shows (0/4/8/15).
pub const LEVEL_OFF: i32 = 0;
pub const LEVEL_DIM: i32 = 4;
pub const LEVEL_MID: i32 = 8;
pub const LEVEL_FULL: i32 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grid {
  Edo,
  Loops,
}

/// Geometry + tuning needed to build a `LooperState`.
pub struct LooperParams {
  pub x_step: i32,
  pub y_step: i32,
  pub edo: i32,
  pub grid_w: i32,
  pub grid_h: i32,
  pub edo_rect: [i32; 4],
  pub shift_rect: [i32; 4],
  pub loop_slots_rect: [i32; 4],
  pub loop_start: (i32, i32),
  pub loop_stop: (i32, i32),
  pub loop_play: (i32, i32),
  pub loop_display_rect: [i32; 4],
  pub loop_toggle: (i32, i32),
  pub loop_copy: (i32, i32),
  pub loop_undo: (i32, i32),
  pub remap_center: (i32, i32),
  pub quantize: Duration,
  pub cluster: Duration,
  /// The timbre editor occluding the edo grid (C3b), if configured.
  pub timbre_editor: Option<TimbreEditor>,
}

/// The slot currently sounding, with its playhead and the epoch time it started.
struct Playing {
  slot: usize,
  playback: Playback,
  start: Duration,
}

pub struct LooperState {
  pub sink: SawNoteSink,

  x_step: i32,
  y_step: i32,
  edo: i32,
  grid_w: i32,
  grid_h: i32,
  edo_rect: [i32; 4],
  shift_rect: [i32; 4],
  loop_slots_rect: [i32; 4],
  loop_start: (i32, i32),
  loop_stop: (i32, i32),
  loop_play: (i32, i32),
  loop_display_rect: [i32; 4],
  loop_toggle: (i32, i32),
  loop_copy: (i32, i32),
  loop_undo: (i32, i32),
  remap_center: (i32, i32),
  cluster: Duration,

  register: i32,
  down: HashMap<(i32, i32), i32>,

  loops: LoopStore,
  playing: Option<Playing>,

  /// Remap toggle: false = fine (set each note's absolute pitch), true = group
  /// transpose (transpose/duplicate the whole selection by an interval).
  group_transpose: bool,
  /// An in-progress remap, if any.
  remap: Option<Remap>,
  /// Copy gesture: Some(from) after 'copy' then a 'from' slot, awaiting 'to'.
  copy_arming: bool,
  copy_from: Option<usize>,

  /// The live timbre stamped onto fingered notes (set by the live-timbre editor).
  /// Until per-note loop timbre lands (C6), loop playback uses it too.
  live_timbre: Timbre,

  /// The timbre editor occluding the edo grid (C3b). Its rect intercepts edo
  /// presses (radio param rows) and overlays its LEDs over the play grid. In C3b
  /// both editor targets edit `live_timbre`; C7 splits `target = loop`.
  timbre_editor: Option<TimbreEditor>,
}

/// An in-progress loop remap. While present, the edo grid is in remap mode (its
/// presses pick destinations, not notes). Selection accumulates until the first
/// edo press freezes it.
struct Remap {
  selection: Selection,
  frozen: bool,
  /// Resolved at freeze, ascending: the pitches to remap (fine) or transpose
  /// (group transpose).
  targets: Vec<i32>,
  /// Fine: index of the next target to remap.
  next: usize,
  /// Fine: accumulated old -> new substitutions.
  mapping: Vec<(i32, i32)>,
  /// True for a group-transpose session (transpose/duplicate) vs fine.
  group_transpose: bool,
  /// The play register at freeze, restored on exit.
  saved_register: i32,
}

/// A press in the loop display: a whole pitch-row, a whole time-column, or a cell.
enum Pick {
  Row(usize),
  Col(usize),
  Cell(usize, usize),
}

impl LooperState {
  pub fn new(sink: SawNoteSink, p: LooperParams) -> Self {
    let [sx0, sy0, sx1, sy1] = p.loop_slots_rect;
    let n_slots = (((sx1 - sx0 + 1).max(0)) * ((sy1 - sy0 + 1).max(0))) as usize;
    LooperState {
      sink,
      x_step: p.x_step,
      y_step: p.y_step,
      edo: p.edo,
      grid_w: p.grid_w,
      grid_h: p.grid_h,
      edo_rect: p.edo_rect,
      shift_rect: p.shift_rect,
      loop_slots_rect: p.loop_slots_rect,
      loop_start: p.loop_start,
      loop_stop: p.loop_stop,
      loop_play: p.loop_play,
      loop_display_rect: p.loop_display_rect,
      loop_toggle: p.loop_toggle,
      loop_copy: p.loop_copy,
      loop_undo: p.loop_undo,
      remap_center: p.remap_center,
      cluster: p.cluster,
      register: 0,
      down: HashMap::new(),
      loops: LoopStore::new(n_slots, p.quantize),
      playing: None,
      group_transpose: false,
      remap: None,
      copy_arming: false,
      copy_from: None,
      live_timbre: Timbre::default(),
      timbre_editor: p.timbre_editor,
    }
  }

  // ---- edo grid ----

  /// Handle a key on the edo grid. Plays live and (while recording) captures the
  /// note. Returns true if the edo LEDs may have changed.
  pub fn edo_key(&mut self, x: i32, y: i32, press: bool, now: Duration) -> bool {
    if press {
      // The timbre editor occludes its rect: a press inside it drives a radio param
      // row (mutating the live timbre) or, on a blank/inert cell, is simply swallowed
      // so it never plays or remaps. A release falls through harmlessly (editor cells
      // were never recorded in `self.down`). C7 will branch on `target = loop`.
      if let Some(ed) = self.timbre_editor.as_ref().filter(|ed| ed.contains(x, y)) {
        if let Some(param) = ed.press(x, y) {
          param.apply(&mut self.live_timbre);
        }
        return true;
      }
      if let Some(shift) = shift_for_cell(self.shift_rect, (x, y)) {
        self.register += register_delta(shift, self.x_step, self.y_step, self.edo);
        return true;
      }
      // The shift pad keeps working during remap; other edo presses pick remap
      // destinations instead of playing.
      if self.remap.is_some() {
        if in_rect(self.edo_rect, x, y) {
          self.edo_remap_press(x, y);
          return true;
        }
        return false;
      }
      if in_rect(self.edo_rect, x, y) {
        let pitch = step_for_cell(self.x_step, self.y_step, self.register, x, y);
        if let Some(old) = self.down.insert((x, y), pitch) {
          if old != pitch {
            self.sink.note_off(old, NoteSource::Live(x, y));
            self.loops.record_note(now, old, false);
          }
        }
        self.sink.note_on(pitch, NoteSource::Live(x, y), self.live_timbre);
        self.loops.record_note_with(now, pitch, true, self.live_timbre);
        return true;
      }
      false
    } else if let Some(pitch) = self.down.remove(&(x, y)) {
      self.sink.note_off(pitch, NoteSource::Live(x, y));
      self.loops.record_note(now, pitch, false);
      true
    } else {
      false
    }
  }

  /// Edo LEDs. A live-held note lights all its octave-equivalents solid. The
  /// *sounding* loop (not the active one) shows its notes *dim* throughout, and
  /// each note lights *solid* only while it is individually sounding -- so two
  /// notes in one loop light at their own separate times. Live wins over the
  /// loop. In remap mode, the cells being remapped (or the group-transpose
  /// center) are solid. The four shift-pad arrows are dim.
  pub fn edo_levels(&self) -> Vec<i32> {
    let mut levels = self.edo_levels_base();
    // The editor overlays last so its occluding rect always wins over the play grid
    // (edo_levels_base has an early return in remap mode, hence the wrapper).
    if let Some(ed) = &self.timbre_editor {
      ed.paint(&self.live_timbre, &mut levels, self.grid_w);
    }
    levels
  }

  fn edo_levels_base(&self) -> Vec<i32> {
    let mut levels = vec![LEVEL_OFF; (self.grid_w * self.grid_h) as usize];

    // Remap mode: solid for the pitches being remapped (or the transpose center).
    if let Some(r) = &self.remap {
      let pitches = if r.frozen {
        if r.group_transpose {
          vec![step_for_cell(self.x_step, self.y_step, self.register, self.remap_center.0, self.remap_center.1)]
        } else {
          r.targets.get(r.next..).map(<[i32]>::to_vec).unwrap_or_default()
        }
      } else {
        selected_pitches(&self.build_active_display(), &r.selection)
      };
      let classes: HashSet<i32> = pitches.iter().map(|p| p.rem_euclid(self.edo)).collect();
      self.paint_classes(&mut levels, |class| {
        if classes.contains(&class) {
          Some(LEVEL_FULL)
        } else {
          None
        }
      });
      return levels;
    }

    // Play mode: live notes solid; the sounding loop's notes are a dim backdrop,
    // each lit solid while it is actually sounding (an individual note-on still
    // in flight, read from the sink's ref-counts).
    let live: HashSet<i32> = self.down.values().map(|p| p.rem_euclid(self.edo)).collect();
    let (loop_classes, sounding_now): (HashSet<i32>, HashSet<i32>) = match self.loops.sounding {
      Some(slot) => {
        let backdrop = self.loops.slots[slot]
          .events
          .iter()
          .filter(|e| e.on)
          .map(|e| e.pitch.rem_euclid(self.edo))
          .collect();
        let now = self
          .sink
          .pitches_held_by(NoteSource::Slot(slot))
          .into_iter()
          .map(|p| p.rem_euclid(self.edo))
          .collect();
        (backdrop, now)
      }
      None => (HashSet::new(), HashSet::new()),
    };
    self.paint_classes(&mut levels, |class| {
      if live.contains(&class) || sounding_now.contains(&class) {
        Some(LEVEL_FULL)
      } else if loop_classes.contains(&class) {
        Some(LEVEL_DIM)
      } else {
        None
      }
    });
    levels
  }

  /// For each cell, set its level from `level_for(pitch_class)`; cells with no
  /// pitch-class level fall back to a dim shift-pad arrow, else dark.
  fn paint_classes(&self, levels: &mut [i32], level_for: impl Fn(i32) -> Option<i32>) {
    for y in 0..self.grid_h {
      for x in 0..self.grid_w {
        let idx = (y * self.grid_w + x) as usize;
        let class = step_for_cell(self.x_step, self.y_step, self.register, x, y).rem_euclid(self.edo);
        if let Some(level) = level_for(class) {
          levels[idx] = level;
        } else if matches!(
          shift_for_cell(self.shift_rect, (x, y)),
          Some(Shift::Up | Shift::Down | Shift::Left | Shift::Right)
        ) {
          // Only the four arrows are findable-dim; the octave corners stay dark.
          levels[idx] = LEVEL_DIM;
        }
      }
    }
  }

  // ---- loops grid ----

  /// Handle a key on the loops grid (key-up has no effect). The transport windows
  /// overlay the slot grid, so they are tested first.
  pub fn loops_key(&mut self, x: i32, y: i32, press: bool, now: Duration) -> bool {
    if !press {
      return false;
    }
    // Copy gesture: press copy, then a 'from' slot, then a 'to' slot. While armed
    // it intercepts slot presses (so they aren't treated as active-slot changes).
    // Pressing copy again mid-gesture cancels it, forgetting any picked origin.
    if (x, y) == self.loop_copy {
      if self.copy_arming || self.copy_from.is_some() {
        self.copy_arming = false;
        self.copy_from = None;
      } else {
        self.copy_arming = true;
      }
      return true;
    }
    if self.copy_arming {
      if let Some(slot) = self.slot_at(x, y) {
        self.copy_from = Some(slot);
        self.copy_arming = false;
      }
      return true;
    }
    if let Some(from) = self.copy_from {
      if let Some(to) = self.slot_at(x, y) {
        self.copy_slot(from, to);
      }
      self.copy_from = None;
      return true;
    }
    if (x, y) == self.loop_start {
      self.loops.start_recording(now);
      self.reconcile_playback(now);
      return true;
    }
    if (x, y) == self.loop_stop {
      self.loops.stop(now);
      self.reconcile_playback(now);
      return true;
    }
    if (x, y) == self.loop_play {
      self.loops.play(now);
      self.reconcile_playback(now);
      return true;
    }
    if (x, y) == self.loop_undo {
      let slot = self.loops.active;
      if let Some(prev) = self.loops.slots[slot].history.pop() {
        self.loops.slots[slot].events = prev;
      }
      return true;
    }
    if (x, y) == self.loop_toggle {
      self.group_transpose = !self.group_transpose;
      // Toggling back to fine ends an active group-transpose session (already baked).
      if !self.group_transpose && self.remap.as_ref().is_some_and(|r| r.group_transpose) {
        self.exit_remap();
      }
      return true;
    }
    if let Some(pick) = self.display_pick(x, y) {
      self.add_pick(pick);
      return true;
    }
    if let Some(slot) = self.slot_at(x, y) {
      self.loops.set_active(slot);
      return true;
    }
    false
  }

  // ---- remap ----

  fn build_active_display(&self) -> super::display::LoopDisplay {
    let [dx0, dy0, dx1, dy1] = self.loop_display_rect;
    let width = (dx1 - dx0).max(1) as usize;
    let height = (dy1 - dy0).max(1) as usize;
    build_display(&self.loops.slots[self.loops.active].events, self.cluster, height, width)
  }

  /// Which display pick a loops-grid cell is, if any (row picker / column picker /
  /// main-area cell). Row 0 is the bottom of the main area.
  fn display_pick(&self, x: i32, y: i32) -> Option<Pick> {
    let [dx0, dy0, dx1, dy1] = self.loop_display_rect;
    if dx1 - dx0 < 1 || dy1 - dy0 < 1 {
      return None;
    }
    if x == dx0 && y > dy0 && y <= dy1 {
      return Some(Pick::Row((dy1 - y) as usize));
    }
    if y == dy0 && x > dx0 && x <= dx1 {
      return Some(Pick::Col((x - dx0 - 1) as usize));
    }
    if x > dx0 && x <= dx1 && y > dy0 && y <= dy1 {
      return Some(Pick::Cell((dy1 - y) as usize, (x - dx0 - 1) as usize));
    }
    None
  }

  /// Add a pick to the selection: start a remap session, extend it (same mode), or
  /// switch mode (clearing the prior selection). Ignored once frozen.
  fn add_pick(&mut self, pick: Pick) {
    if self.remap.as_ref().is_some_and(|r| r.frozen) {
      return;
    }
    let extend = matches!(
      (&self.remap, &pick),
      (Some(Remap { selection: Selection::Rows(_), .. }), Pick::Row(_))
        | (Some(Remap { selection: Selection::Columns(_), .. }), Pick::Col(_))
        | (Some(Remap { selection: Selection::Cells(_), .. }), Pick::Cell(..))
    );
    if extend {
      let r = self.remap.as_mut().unwrap();
      match (&mut r.selection, pick) {
        (Selection::Rows(v), Pick::Row(i)) => {
          if !v.contains(&i) {
            v.push(i);
          }
        }
        (Selection::Columns(v), Pick::Col(i)) => {
          if !v.contains(&i) {
            v.push(i);
          }
        }
        (Selection::Cells(v), Pick::Cell(rr, cc)) => {
          if !v.contains(&(rr, cc)) {
            v.push((rr, cc));
          }
        }
        _ => {}
      }
    } else {
      let selection = match pick {
        Pick::Row(i) => Selection::Rows(vec![i]),
        Pick::Col(i) => Selection::Columns(vec![i]),
        Pick::Cell(rr, cc) => Selection::Cells(vec![(rr, cc)]),
      };
      self.remap = Some(Remap {
        selection,
        frozen: false,
        targets: vec![],
        next: 0,
        mapping: vec![],
        group_transpose: self.group_transpose,
        saved_register: 0,
      });
    }
  }

  /// An edo-grid press during a remap session: freeze the selection on the first
  /// press, then pick this destination (fine) or interval (group transpose).
  fn edo_remap_press(&mut self, x: i32, y: i32) {
    let pressed_step = step_for_cell(self.x_step, self.y_step, self.register, x, y);
    if !self.remap.as_ref().is_some_and(|r| r.frozen) {
      self.freeze_remap();
    }
    if self.remap.as_ref().is_none_or(|r| r.targets.is_empty()) {
      self.exit_remap();
      return;
    }
    if self.remap.as_ref().is_some_and(|r| r.group_transpose) {
      self.group_transpose_press(pressed_step);
    } else {
      self.fine_press(pressed_step);
    }
  }

  fn freeze_remap(&mut self) {
    let display = self.build_active_display();
    let targets = selected_pitches(&display, &self.remap.as_ref().unwrap().selection);
    let saved = self.register;
    let group_transpose = self.group_transpose;
    let r = self.remap.as_mut().unwrap();
    r.frozen = true;
    r.targets = targets;
    r.next = 0;
    r.mapping = vec![];
    r.group_transpose = group_transpose;
    r.saved_register = saved;
  }

  fn fine_press(&mut self, pressed_step: i32) {
    let done = {
      let r = self.remap.as_mut().unwrap();
      if r.next < r.targets.len() {
        let old = r.targets[r.next];
        r.mapping.push((old, pressed_step));
        r.next += 1;
      }
      r.next >= r.targets.len()
    };
    if done {
      let (mapping, saved) = {
        let r = self.remap.as_ref().unwrap();
        (r.mapping.clone(), r.saved_register)
      };
      self.apply_to_active(|events| apply_fine(events, &mapping));
      self.register = saved;
      self.remap = None;
    }
  }

  fn group_transpose_press(&mut self, pressed_step: i32) {
    // Each press transposes the selection by (pressed - center); accumulating
    // intervals re-bakes from the snapshot, so N presses over M notes -> N*M.
    let center = step_for_cell(self.x_step, self.y_step, self.register, self.remap_center.0, self.remap_center.1);
    let interval = pressed_step - center;
    let (targets, mut intervals) = {
      let r = self.remap.as_ref().unwrap();
      (r.targets.clone(), r.mapping.iter().map(|(_, i)| *i).collect::<Vec<_>>())
    };
    intervals.push(interval);
    // Stash the new interval in `mapping` (reusing it as the interval list).
    self.remap.as_mut().unwrap().mapping.push((0, interval));
    // Re-bake from the snapshot stored in history (push it once, on the first press).
    let slot = self.loops.active;
    if intervals.len() == 1 {
      let snapshot = self.loops.slots[slot].events.clone();
      self.loops.slots[slot].history.push(snapshot);
    }
    let base = self.loops.slots[slot].history.last().cloned().unwrap_or_default();
    self.loops.slots[slot].events = apply_group_transpose(&base, &targets, &intervals);
  }

  /// Snapshot the active slot's events for undo, then replace them.
  fn apply_to_active(&mut self, f: impl FnOnce(&[super::loop_store::LoopEvent]) -> Vec<super::loop_store::LoopEvent>) {
    let slot = self.loops.active;
    let events = self.loops.slots[slot].events.clone();
    self.loops.slots[slot].history.push(events.clone());
    self.loops.slots[slot].events = f(&events);
  }

  fn exit_remap(&mut self) {
    if let Some(r) = &self.remap {
      if r.frozen {
        self.register = r.saved_register;
      }
    }
    self.remap = None;
  }

  /// Copy `from`'s contents onto `to`, leaving active and sounding unchanged. A
  /// no-op if from == to or `from` is empty (never erase `to`).
  /// TODO: when `to` is sounding, defer to its next boundary; today it swaps now.
  fn copy_slot(&mut self, from: usize, to: usize) {
    if from == to || self.loops.slots[from].loop_duration.is_none() {
      return;
    }
    let events = self.loops.slots[from].events.clone();
    let duration = self.loops.slots[from].loop_duration;
    self.loops.slots[to].events = events;
    self.loops.slots[to].loop_duration = duration;
  }

  /// Advance the sounding slot's playhead, emitting its due notes into the sink.
  pub fn playback_tick(&mut self, now: Duration) {
    let (slot, total) = match &self.playing {
      Some(p) => (p.slot, now.saturating_sub(p.start)),
      None => return,
    };
    let actions = {
      let events = &self.loops.slots[slot].events;
      self.playing.as_mut().unwrap().playback.step(events, total)
    };
    let source = NoteSource::Slot(slot);
    for action in actions {
      match action {
        PlayAction::On(pitch, timbre) => self.sink.note_on(pitch, source, timbre),
        PlayAction::Off(pitch) => self.sink.note_off(pitch, source),
        PlayAction::ReleaseAll => self.sink.release_source(source),
      }
    }
  }

  /// After a transport op changes which slot sounds, release the outgoing slot's
  /// notes and (re)arm the playhead for the new one.
  fn reconcile_playback(&mut self, now: Duration) {
    let target = self.loops.sounding;
    let current = self.playing.as_ref().map(|p| p.slot);
    if target == current {
      return;
    }
    if let Some(p) = &self.playing {
      self.sink.release_source(NoteSource::Slot(p.slot));
    }
    self.playing = target.and_then(|slot| {
      let duration = self.loops.slots[slot].loop_duration?;
      Some(Playing { slot, playback: Playback::new(duration), start: now })
    });
  }

  /// Loops-grid LEDs: active solid > sounding(non-active) flash > recorded dim >
  /// empty dark. The transport cells show dim so they are findable. The copy
  /// button is solid while a copy is armed (dim otherwise); the remap-mode toggle
  /// is solid in fine mode (dim in group transpose); undo always flashes.
  pub fn loops_levels(&self, flash_on: bool) -> Vec<i32> {
    let mut levels = vec![LEVEL_OFF; (self.grid_w * self.grid_h) as usize];
    let [x0, y0, x1, y1] = self.loop_slots_rect;
    let width = (x1 - x0 + 1).max(1);
    let n_slots = (width * (y1 - y0 + 1).max(1)) as usize;
    for slot in 0..n_slots {
      let cx = x0 + (slot as i32) % width;
      let cy = y0 + (slot as i32) / width;
      let idx = (cy * self.grid_w + cx) as usize;
      levels[idx] = if (cx, cy) == self.loop_start
        || (cx, cy) == self.loop_stop
        || (cx, cy) == self.loop_play
      {
        LEVEL_DIM
      } else {
        self.slot_level(slot, flash_on)
      };
    }
    self.render_loop_display(&mut levels);
    // The copy button: solid while a copy is armed (copy pressed, awaiting from or
    // to), dim and findable otherwise.
    let (cx, cy) = self.loop_copy;
    if cx >= 0 && cx < self.grid_w && cy >= 0 && cy < self.grid_h {
      let armed = self.copy_arming || self.copy_from.is_some();
      levels[(cy * self.grid_w + cx) as usize] = if armed { LEVEL_FULL } else { LEVEL_DIM };
    }
    // The remap-mode toggle: solid in fine mode (the default, so it starts lit),
    // dim in group-transpose mode.
    let (tx, ty) = self.loop_toggle;
    if tx >= 0 && tx < self.grid_w && ty >= 0 && ty < self.grid_h {
      levels[(ty * self.grid_w + tx) as usize] =
        if self.group_transpose { LEVEL_DIM } else { LEVEL_FULL };
    }
    // The undo button always flashes 50/50 (driven by the caller's flash phase).
    let (ux, uy) = self.loop_undo;
    if ux >= 0 && ux < self.grid_w && uy >= 0 && uy < self.grid_h {
      levels[(uy * self.grid_w + ux) as usize] = if flash_on { LEVEL_FULL } else { LEVEL_OFF };
    }
    levels
  }

  /// Paint the active slot's 2D display into its compound rect: the row-picker
  /// column and column-picker row show dim (findable), the reserved corner stays
  /// dark, and the main area lights the loop's notes (time across, pitch up, row 0
  /// lowest at the bottom).
  fn render_loop_display(&self, levels: &mut [i32]) {
    let [dx0, dy0, dx1, dy1] = self.loop_display_rect;
    if dx1 - dx0 < 1 || dy1 - dy0 < 1 {
      return;
    }
    for y in (dy0 + 1)..=dy1 {
      levels[(y * self.grid_w + dx0) as usize] = LEVEL_DIM; // row pickers
    }
    for x in (dx0 + 1)..=dx1 {
      levels[(dy0 * self.grid_w + x) as usize] = LEVEL_DIM; // column pickers
    }
    let width = (dx1 - dx0) as usize;
    let height = (dy1 - dy0) as usize;
    let events = &self.loops.slots[self.loops.active].events;
    let display = build_display(events, self.cluster, height, width);
    for (row, col) in display.lit_cells() {
      let sx = dx0 + 1 + col as i32;
      let sy = dy1 - row as i32; // row 0 (lowest pitch) at the bottom
      if sx >= dx0 + 1 && sx <= dx1 && sy >= dy0 + 1 && sy <= dy1 {
        levels[(sy * self.grid_w + sx) as usize] = LEVEL_FULL;
      }
    }
  }

  fn slot_level(&self, slot: usize, flash_on: bool) -> i32 {
    if self.loops.active == slot {
      LEVEL_FULL
    } else if self.loops.sounding == Some(slot) {
      if flash_on {
        LEVEL_FULL
      } else {
        LEVEL_OFF
      }
    } else if self.loops.is_occupied(slot) {
      LEVEL_DIM
    } else {
      LEVEL_OFF
    }
  }

  fn slot_at(&self, x: i32, y: i32) -> Option<usize> {
    let [x0, y0, x1, y1] = self.loop_slots_rect;
    if x < x0 || x > x1 || y < y0 || y > y1 {
      return None;
    }
    let width = x1 - x0 + 1;
    Some(((y - y0) * width + (x - x0)) as usize)
  }

  /// One-line transport status for the runtime log.
  pub fn debug_status(&self) -> String {
    format!(
      "active={} sounding={:?} recording={:?}",
      self.loops.active,
      self.loops.sounding,
      self.loops.recording_slot(),
    )
  }

  #[cfg(test)]
  fn register(&self) -> i32 {
    self.register
  }
}

fn in_rect(rect: [i32; 4], x: i32, y: i32) -> bool {
  let [x0, y0, x1, y1] = rect;
  x >= x0 && x <= x1 && y >= y0 && y <= y1
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::types::VoiceMap;
  use std::collections::HashMap as Map;
  use std::sync::{Arc, Mutex};

  fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
  }

  // 58-8-1 on 16x16: edo grid full, shift pad lower-right, 4x4 slots top-left with
  // transport on (0,3)/(1,3)/(2,3).
  fn state() -> LooperState {
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(Map::new()));
    let sink = SawNoteSink::new(voices, 80.0, 58, 48000.0, 0.003, 0.05);
    LooperState::new(
      sink,
      LooperParams {
        x_step: 8,
        y_step: 1,
        edo: 58,
        grid_w: 16,
        grid_h: 16,
        edo_rect: [0, 0, 15, 15],
        shift_rect: [13, 14, 15, 15],
        loop_slots_rect: [0, 0, 3, 3],
        loop_start: (0, 3),
        loop_stop: (1, 3),
        loop_play: (2, 3),
        loop_display_rect: [0, 5, 15, 15],
        loop_toggle: (5, 0),
        loop_copy: (5, 1),
        loop_undo: (5, 2),
        remap_center: (7, 7),
        quantize: ms(70),
        cluster: ms(100),
        timbre_editor: None,
      },
    )
  }

  /// A state whose edo grid carries a timbre editor occluding the top 7 rows
  /// (rect [0,0,15,6]), with the 6_plan 5 default row ranges.
  fn state_with_editor() -> LooperState {
    use super::super::timbre_editor::TimbreEditor;
    use midi_pulse::config::TimbreTarget;
    use super::super::timbre_rows::RowRange;
    let mut s = state();
    s.timbre_editor = Some(TimbreEditor::new(
      [0, 0, 15, 6],
      TimbreTarget::Loop,
      RowRange::LogRange { least: 0.0009, greatest: 0.15 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 5.0, multiplier: 2.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
    ));
    s
  }

  fn level_at(levels: &[i32], x: i32, y: i32) -> i32 {
    levels[(y * 16 + x) as usize]
  }

  #[test]
  fn pressing_a_cell_lights_octave_equivalents_solid() {
    let mut s = state();
    s.edo_key(0, 0, true, ms(0));
    let levels = s.edo_levels();
    assert_eq!(level_at(&levels, 0, 0), LEVEL_FULL);
    assert_eq!(level_at(&levels, 7, 2), LEVEL_FULL); // step 58 = octave of step 0
    assert_eq!(level_at(&levels, 1, 0), LEVEL_OFF);
  }

  #[test]
  fn shift_pad_does_not_play_and_shows_dim() {
    let mut s = state();
    let levels = s.edo_levels();
    // The four arrows are dim; the two octave corners are dark (keyboard-like).
    assert_eq!(level_at(&levels, 14, 15), LEVEL_DIM, "down arrow dim");
    assert_eq!(level_at(&levels, 13, 15), LEVEL_DIM, "left arrow dim");
    assert_eq!(level_at(&levels, 13, 14), LEVEL_OFF, "octave-down corner dark");
    assert_eq!(level_at(&levels, 15, 14), LEVEL_OFF, "octave-up corner dark");
    assert!(s.edo_key(14, 15, true, ms(0)));
    assert!(s.down.is_empty());
    assert_eq!(s.register(), 1); // down-shift = +y_step
  }

  #[test]
  fn sounding_loop_dims_and_each_note_lights_solid_while_it_sounds() {
    let mut s = state();
    // Select slot 5 on the loops grid, record an edo note, then play.
    let cell = |slot: i32| ((slot % 4), (slot / 4)); // 4-wide slot grid
    let (sx, sy) = cell(5);
    s.loops_key(sx, sy, true, ms(0));
    s.loops_key(0, 3, true, ms(10)); // start recording
    s.edo_key(0, 0, true, ms(20)); // play step 0
    s.edo_key(0, 0, false, ms(300));
    s.loops_key(2, 3, true, ms(1000)); // play -> slot 5 sounds
    assert_eq!(s.loops.sounding, Some(5));
    // Before the playhead reaches the note it is a DIM backdrop (not yet sounding).
    assert_eq!(level_at(&s.edo_levels(), 0, 0), LEVEL_DIM, "sounding loop: dim backdrop");
    // The playhead reaches the note-on -> it lights SOLID as an individual note.
    s.playback_tick(ms(1100)); // 100ms into the loop -> note-on at 0ms is due
    assert!(s.sink.voice_count() >= 1);
    assert_eq!(level_at(&s.edo_levels(), 0, 0), LEVEL_FULL, "solid while sounding");
    // Its octave-equivalent lights solid too (step 58 = cell (7,2)).
    assert_eq!(level_at(&s.edo_levels(), 7, 2), LEVEL_FULL, "octave-equivalent solid");
    // After its note-off it drops back to the dim backdrop (still in the loop).
    s.playback_tick(ms(1300)); // 290ms in -> note-off at 290ms is due
    assert_eq!(level_at(&s.edo_levels(), 0, 0), LEVEL_DIM, "dim again after note-off");
    // Stop: the loop no longer sounds, so it leaves NO backdrop (we reflect the
    // sounding loop, not the active one).
    s.loops_key(1, 3, true, ms(2000)); // stop (active slot 5 was sounding)
    assert_eq!(s.loops.sounding, None);
    assert_eq!(level_at(&s.edo_levels(), 0, 0), LEVEL_OFF, "silent: no backdrop");
  }

  #[test]
  fn the_active_but_silent_loop_does_not_reflect_on_edo() {
    let mut s = state();
    // Record a note into slot 0 and stop (occupied, active, but NOT sounding).
    s.loops_key(0, 3, true, ms(0)); // start (slot 0 active)
    s.edo_key(0, 0, true, ms(10));
    s.edo_key(0, 0, false, ms(200));
    s.loops_key(1, 3, true, ms(1000)); // stop -> slot 0 occupied, silent
    // The active loop is not sounding, so the edo grid shows nothing for it.
    assert_eq!(level_at(&s.edo_levels(), 0, 0), LEVEL_OFF, "active-but-silent: dark");
  }

  #[test]
  fn loops_grid_shows_active_solid_and_recorded_dim() {
    let mut s = state();
    // Record into slot 0 and stop (occupied, not sounding), then make slot 1 active.
    s.loops_key(0, 3, true, ms(0)); // start (slot 0 active by default)
    s.edo_key(0, 0, true, ms(10));
    s.loops_key(1, 3, true, ms(500)); // stop -> slot 0 occupied, idle
    let one = |slot: i32| (slot % 4, slot / 4);
    let (ax, ay) = one(1);
    s.loops_key(ax, ay, true, ms(600)); // slot 1 active
    let levels = s.loops_levels(true);
    assert_eq!(level_at(&levels, ax, ay), LEVEL_FULL, "active slot solid");
    // slot 0's cell is (0,0) but that's overlaid by... no, (0,0) is a slot, (0,3)
    // is the transport. slot 0 cell = (0,0).
    assert_eq!(level_at(&levels, 0, 0), LEVEL_DIM, "recorded idle slot dim");
    // a transport cell shows dim.
    assert_eq!(level_at(&levels, 0, 3), LEVEL_DIM);
  }

  #[test]
  fn the_active_loop_renders_into_the_display_area() {
    let mut s = state();
    // Record one note (pitch 0) into the active slot (slot 0).
    s.loops_key(0, 3, true, ms(0)); // start
    s.edo_key(0, 0, true, ms(10));
    s.edo_key(0, 0, false, ms(200));
    s.loops_key(1, 3, true, ms(1000)); // stop -> stored in slot 0 (active)
    let levels = s.loops_levels(false);
    // Display rect [0,5,15,15]: row pickers at x=0 (y>=6), column pickers at y=5
    // (x>=1), one note -> one lit cell in the main area (bottom row, first col).
    assert_eq!(level_at(&levels, 0, 6), LEVEL_DIM, "row-picker column is findable");
    assert_eq!(level_at(&levels, 1, 5), LEVEL_DIM, "column-picker row is findable");
    assert_eq!(level_at(&levels, 0, 5), LEVEL_OFF, "reserved corner stays dark");
    // The single note is the lowest pitch (row 0 -> bottom y=15) at the first
    // column (x=1).
    assert_eq!(level_at(&levels, 1, 15), LEVEL_FULL, "the loop's note");
  }

  #[test]
  fn fine_remap_changes_a_row_pitch_and_undo_restores() {
    let mut s = state();
    // Record a one-note loop (pitch 0) into the active slot.
    s.loops_key(0, 3, true, ms(0)); // start
    s.edo_key(0, 0, true, ms(10));
    s.edo_key(0, 0, false, ms(200));
    s.loops_key(1, 3, true, ms(1000)); // stop
    assert!(s.loops.slots[0].events.iter().all(|e| e.pitch == 0));
    // Select the (single, bottom) display row via its picker at (0, 15).
    s.loops_key(0, 15, true, ms(1100));
    assert!(s.remap.is_some(), "selecting a row enters remap mode");
    // Press edo cell (1,0) = step 8 -> remap pitch 0 to 8.
    s.edo_key(1, 0, true, ms(1200));
    assert!(s.remap.is_none(), "a single-target fine remap completes on one press");
    assert!(s.loops.slots[0].events.iter().all(|e| e.pitch == 8), "0 -> 8");
    // Undo restores the original pitch.
    s.loops_key(5, 2, true, ms(1300));
    assert!(s.loops.slots[0].events.iter().all(|e| e.pitch == 0), "undo restores 0");
  }

  #[test]
  fn remap_press_does_not_play_a_live_note() {
    let mut s = state();
    s.loops_key(0, 3, true, ms(0));
    s.edo_key(0, 0, true, ms(10));
    s.edo_key(0, 0, false, ms(200));
    s.loops_key(1, 3, true, ms(1000));
    s.loops_key(0, 15, true, ms(1100)); // select row -> remap mode
    s.edo_key(2, 0, true, ms(1200)); // a remap destination press, NOT a played note
    assert_eq!(s.down.len(), 0, "edo presses in remap mode don't sound live notes");
  }

  #[test]
  fn the_undo_button_flashes() {
    let s = state(); // loop_undo is (5,2) in the test geometry.
    assert_eq!(level_at(&s.loops_levels(true), 5, 2), LEVEL_FULL);
    assert_eq!(level_at(&s.loops_levels(false), 5, 2), LEVEL_OFF);
  }

  #[test]
  fn the_fine_toggle_starts_lit_and_dims_in_group_transpose() {
    let mut s = state(); // loop_toggle is (5,0) in the test geometry.
    // Fine is the default, so the toggle starts lit (solid).
    assert_eq!(level_at(&s.loops_levels(true), 5, 0), LEVEL_FULL, "fine starts lit");
    // Toggle to group transpose -> dim (still findable).
    s.loops_key(5, 0, true, ms(0));
    assert_eq!(level_at(&s.loops_levels(true), 5, 0), LEVEL_DIM, "group transpose dims it");
    // Toggle back to fine -> lit again.
    s.loops_key(5, 0, true, ms(10));
    assert_eq!(level_at(&s.loops_levels(true), 5, 0), LEVEL_FULL, "back to fine, lit");
  }

  #[test]
  fn the_copy_button_lights_while_armed_and_a_re_press_cancels() {
    let mut s = state(); // loop_copy is (5,1) in the test geometry.
    // Idle: dim and findable.
    assert_eq!(level_at(&s.loops_levels(true), 5, 1), LEVEL_DIM, "idle copy dim");
    // Arm copy -> solid.
    s.loops_key(5, 1, true, ms(0));
    assert_eq!(level_at(&s.loops_levels(true), 5, 1), LEVEL_FULL, "armed copy solid");
    // Pick a 'from' slot (slot 0): still mid-gesture (awaiting 'to') -> still solid.
    s.loops_key(0, 0, true, ms(10));
    assert_eq!(s.copy_from, Some(0), "origin picked");
    assert_eq!(level_at(&s.loops_levels(true), 5, 1), LEVEL_FULL, "from picked, still solid");
    // Press copy again -> cancels and forgets the picked origin -> dim again.
    s.loops_key(5, 1, true, ms(20));
    assert_eq!(s.copy_from, None, "origin forgotten");
    assert!(!s.copy_arming, "disarmed");
    assert_eq!(level_at(&s.loops_levels(true), 5, 1), LEVEL_DIM, "re-press cancels");
  }

  #[test]
  fn copy_moves_a_loop_into_another_slot() {
    let mut s = state();
    // Record a loop into the active slot (slot 0) and stop.
    s.loops_key(0, 3, true, ms(0)); // start
    s.edo_key(0, 0, true, ms(10));
    s.edo_key(0, 0, false, ms(200));
    s.loops_key(1, 3, true, ms(1000)); // stop -> slot 0 occupied
    assert!(s.loops.is_occupied(0) && !s.loops.is_occupied(7));
    // Copy: press copy, then 'from' = slot 0, then 'to' = slot 7 (cell (3,1)).
    s.loops_key(5, 1, true, ms(1100));
    s.loops_key(0, 0, true, ms(1110)); // from
    s.loops_key(3, 1, true, ms(1120)); // to (slot 7)
    assert!(s.loops.is_occupied(7), "slot 7 received the loop");
    assert_eq!(s.loops.slots[7].events, s.loops.slots[0].events, "contents copied");
    assert_eq!(s.loops.active, 0, "active unchanged by copy");
  }

  #[test]
  fn a_cancelled_copy_leaves_a_later_slot_press_to_set_active() {
    let mut s = state();
    s.loops_key(5, 1, true, ms(0)); // arm copy
    s.loops_key(5, 1, true, ms(10)); // re-press cancels (nothing picked yet)
    // With the gesture cancelled, a slot press is a normal active-slot change.
    s.loops_key(3, 1, true, ms(20)); // slot 7
    assert_eq!(s.loops.active, 7, "slot press sets active after a cancel");
  }

  #[test]
  fn switching_active_does_not_change_sounding() {
    let mut s = state();
    s.loops_key(0, 3, true, ms(0));
    s.edo_key(0, 0, true, ms(10));
    s.loops_key(2, 3, true, ms(500)); // play slot 0
    assert_eq!(s.loops.sounding, Some(0));
    let one = |slot: i32| (slot % 4, slot / 4);
    let (bx, by) = one(7);
    s.loops_key(bx, by, true, ms(600)); // make slot 7 active
    assert_eq!(s.loops.sounding, Some(0), "sounding unchanged by active switch");
  }

  // ---- C3b: timbre editor on the edo monome ----

  #[test]
  fn editor_waveform_press_sets_live_timbre_and_plays_no_note() {
    use crate::types::Waveform;
    let mut s = state_with_editor();
    assert_eq!(s.live_timbre.waveform, Waveform::Triangle, "default");
    // Control row cell x=4 = Saw (fold=0, Sine/Triangle/Square/Saw = 1..4).
    assert!(s.edo_key(4, 0, true, ms(0)), "editor press repaints");
    assert_eq!(s.live_timbre.waveform, Waveform::Saw, "waveform dialed to saw");
    assert!(s.down.is_empty(), "an editor press never plays a live note");
  }

  #[test]
  fn editor_amplitude_press_sets_gain() {
    let mut s = state_with_editor();
    // Amplitude row (y=1), 16 wide: cell 0 = least (0.0009), cell 15 = greatest (0.15).
    s.edo_key(0, 1, true, ms(0));
    assert!((s.live_timbre.gain - 0.0009).abs() < 1e-5, "min amplitude");
    s.edo_key(15, 1, true, ms(1));
    assert!((s.live_timbre.gain - 0.15).abs() < 1e-5, "max amplitude = unity");
  }

  #[test]
  fn editor_blank_cell_swallows_the_press() {
    let mut s = state_with_editor();
    // x=10,y=0 is a slot placeholder in the control row -- inert in C3b, but inside
    // the editor rect, so it must be swallowed (no note, no fall-through).
    assert!(s.edo_key(10, 0, true, ms(0)), "consumed");
    assert!(s.down.is_empty(), "no note played");
    assert_eq!(s.live_timbre, Timbre::default(), "nothing changed");
  }

  #[test]
  fn editor_overlays_edo_leds_and_occludes_the_top_rows() {
    let s = state_with_editor();
    let levels = s.edo_levels();
    // Default waveform Triangle = control cell x=2 lit; fold cell x=0 = Mid.
    assert_eq!(level_at(&levels, 0, 0), LEVEL_MID, "fold cell findable");
    assert_eq!(level_at(&levels, 2, 0), LEVEL_FULL, "triangle lit");
    assert_eq!(level_at(&levels, 1, 0), LEVEL_OFF, "sine dark");
  }

  #[test]
  fn note_fingered_below_the_editor_carries_the_edited_timbre() {
    use crate::types::Waveform;
    let mut s = state_with_editor();
    s.edo_key(3, 0, true, ms(0)); // editor: set Square (control cell x=3)
    assert_eq!(s.live_timbre.waveform, Waveform::Square);
    // Record a note on an open instrument row (y=8, below the 7-row editor).
    s.loops_key(0, 3, true, ms(10)); // start recording (slot 0)
    s.edo_key(0, 8, true, ms(20)); // finger a note -- plays AND records
    assert_eq!(s.down.len(), 1, "a real note sounded below the editor");
    s.edo_key(0, 8, false, ms(200));
    s.loops_key(1, 3, true, ms(1000)); // stop
    let on = s.loops.slots[0].events.iter().find(|e| e.on).expect("an on event");
    assert_eq!(on.timbre.waveform, Waveform::Square, "recorded note carries editor timbre");
  }
}
