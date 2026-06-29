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
use super::timbre_editor::{
  EditorAction, EditorView, ParamKind, TimbreEditor, TimbreParam, EDITOR_ROWS,
};
use crate::types::Timbre;
use midi_pulse::config::TimbreTarget;

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
  pub clock_period: Duration,
  pub cluster: Duration,
  /// The timbre editor occluding the edo grid (C3b), if configured.
  pub timbre_editor: Option<TimbreEditor>,
  /// save-undo double-press window (6_plan 2.8, default 200 ms).
  pub save_undo_window: Duration,
  /// The live-timbre editor on the loops monome (C5c), if configured; the loop
  /// display reflows under it.
  pub loops_timbre_editor: Option<TimbreEditor>,
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
  /// The metronome cycle. Transport presses snap to the nearest multiple of this
  /// (measured from the epoch); recorded note-ons snap onto the same grid; one
  /// transport button flashes at this rate. 300 bpm (4*75) -> 200 ms.
  clock_period: Duration,

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

  /// Timbre slot store (C4). One entry per editor slot cell; `None` = empty. A slot
  /// press recalls into `live_timbre`; with `timbre_armed` set, the press instead
  /// captures `live_timbre` and auto-disarms. `timbre_current` is the last
  /// recalled/saved slot (its LED is bright, flashing when the live timbre diverges).
  timbre_slots: Vec<Option<Timbre>>,
  timbre_armed: bool,
  timbre_current: Option<usize>,

  /// Editor fold state (C5b). Folded is the resting state (6_plan 2.7): only the
  /// 1-row strip occludes the grid. `timbre_quick` is the rolling most-recent-4 saved
  /// timbres shown in the strip's quick cells (empty = waveform mode); newest at
  /// front. A save pushes onto it; a recall does not reorder it.
  timbre_folded: bool,
  timbre_quick: Vec<Timbre>,

  /// save-undo (C7b, 6_plan 2.8): the last save-undo press time and the double-press
  /// window. A lone tap checkpoints the active slot; a second within the window reverts.
  save_undo_last: Option<Duration>,
  save_undo_window: Duration,

  /// Play-along overrides (C7c, 6_plan 2.4 path 2): on a loop editor with no
  /// selection, holding a param row overrides that parameter; each loop note-on the
  /// playhead triggers is rewritten to the active overrides. `(param, held)`: a
  /// released override stays iff `timbre_sustain`. Toggling sustain off drops the
  /// released ones (held stay until released); toggling on is not retroactive.
  timbre_overrides: HashMap<ParamKind, (TimbreParam, bool)>,
  timbre_sustain: bool,

  /// The loop-editor display's "hold last on rests" value (6_plan 2.4): the most
  /// recent lowest-sounding loop timbre. Updated each playback tick while a note
  /// sounds; held across rests (and loop wraps); cleared when playback stops or the
  /// sounding slot changes (`reconcile_playback`). Shown when nothing is selected or
  /// currently sounding, in preference to the live timbre.
  loop_display_held: Option<Timbre>,

  /// The live-timbre editor on the loops monome (C5c, 6_plan 2.2 / 7_layout), with
  /// its own fold state. The whole looper stack (slots, transport, copy/undo/remap,
  /// and the loop display) sits BELOW it and reflows down together when it unfolds:
  /// `loops_reflow` holds the unfolded (config) positions, and `reflow_loops` shifts
  /// them by the editor's fold-height delta. `None` = no loops editor (every existing
  /// config), so the looper layout is fixed.
  loops_timbre_editor: Option<TimbreEditor>,
  loops_timbre_folded: bool,
  loops_reflow: Option<LoopsReflow>,
}

/// The unfolded (config) positions of every loops-monome window that reflows under the
/// live editor. `reflow_loops` shifts them up when the editor folds (7_layout.org).
#[derive(Clone, Copy)]
struct LoopsReflow {
  slots: [i32; 4],
  start: (i32, i32),
  stop: (i32, i32),
  play: (i32, i32),
  copy: (i32, i32),
  undo: (i32, i32),
  toggle: (i32, i32),
  display: [i32; 4],
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
    let editor = p.timbre_editor.as_ref().or(p.loops_timbre_editor.as_ref());
    let n_timbre_slots = editor.map_or(0, TimbreEditor::slot_count);
    // Boot the live timbre at the amplitude row's unity (its top cell), not the
    // linear gain 1.0 of Timbre::default(): with an editor present, 1.0 sits ABOVE
    // the amplitude row's ceiling, so the instrument would start louder than the row
    // can dial back to (the bug "volume starts beyond the maximum amplitude"). With
    // no editor (the plain looper) there is no amplitude row, so keep the default.
    let live_timbre = Timbre {
      gain: editor.map_or(Timbre::default().gain, TimbreEditor::amplitude_unity),
      ..Timbre::default()
    };
    // Capture the unfolded looper-stack positions (config) so the loops editor can
    // reflow them under itself. Only when a loops editor is present.
    let loops_reflow = p.loops_timbre_editor.as_ref().map(|_| LoopsReflow {
      slots: p.loop_slots_rect,
      start: p.loop_start,
      stop: p.loop_stop,
      play: p.loop_play,
      copy: p.loop_copy,
      undo: p.loop_undo,
      toggle: p.loop_toggle,
      display: p.loop_display_rect,
    });
    let mut state = LooperState {
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
      clock_period: p.clock_period,
      register: 0,
      down: HashMap::new(),
      loops: LoopStore::new(n_slots, p.clock_period),
      playing: None,
      group_transpose: false,
      remap: None,
      copy_arming: false,
      copy_from: None,
      live_timbre,
      timbre_editor: p.timbre_editor,
      timbre_slots: vec![None; n_timbre_slots],
      timbre_armed: false,
      timbre_current: None,
      timbre_folded: true, // 6_plan 2.7: folded is the resting state
      timbre_quick: Vec::new(),
      save_undo_last: None,
      save_undo_window: p.save_undo_window,
      timbre_overrides: HashMap::new(),
      timbre_sustain: false,
      loop_display_held: None,
      loops_timbre_editor: p.loops_timbre_editor,
      loops_timbre_folded: true, // folded is the resting state (6_plan 2.7)
      loops_reflow,
    };
    state.reflow_loops(); // slide the looper stack under the (folded) loops editor
    state
  }

  // ---- edo grid ----

  /// Handle a key on the edo grid. Plays live and (while recording) captures the
  /// note. Returns true if the edo LEDs may have changed.
  pub fn edo_key(&mut self, x: i32, y: i32, press: bool, now: Duration) -> bool {
    if press {
      // The timbre editor occludes its rect: a press inside it drives a radio param
      // row, the arm latch, or a slot (C4); a blank/inert cell is simply swallowed so
      // it never plays or remaps. A release falls through harmlessly (editor cells
      // were never recorded in `self.down`). C7 will branch on `target = loop`.
      let folded = self.timbre_folded;
      let hit = self
        .timbre_editor
        .as_ref()
        .filter(|ed| ed.occludes(x, y, folded))
        .map(|ed| (ed.target, ed.press(x, y, folded)));
      if let Some((target, action)) = hit {
        if matches!(action, Some(EditorAction::ToggleFold)) {
          self.timbre_folded = !self.timbre_folded;
        } else {
          self.apply_editor_action(target, action, now);
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
    } else {
      // A release inside the (occluding) editor: if it let go of a held play-along
      // param, release that override (sustain decides whether it persists). Always
      // consume it -- editor cells never entered `self.down`. [C7c]
      let folded = self.timbre_folded;
      let hit = self
        .timbre_editor
        .as_ref()
        .filter(|ed| ed.occludes(x, y, folded))
        .map(|ed| (ed.target, ed.press(x, y, folded)));
      if let Some((target, action)) = hit {
        if target == TimbreTarget::Loop {
          if let Some(EditorAction::Param(p)) = action {
            self.release_override(p.kind());
          }
        }
        return true;
      }
      if let Some(pitch) = self.down.remove(&(x, y)) {
        self.sink.note_off(pitch, NoteSource::Live(x, y));
        self.loops.record_note(now, pitch, false);
        true
      } else {
        false
      }
    }
  }

  /// Edo LEDs. A live-held note lights all its octave-equivalents solid. The
  /// *sounding* loop (not the active one) shows its notes *dim* throughout, and
  /// each note lights *solid* only while it is individually sounding -- so two
  /// notes in one loop light at their own separate times. Live wins over the
  /// loop. In remap mode, the cells being remapped (or the group-transpose
  /// center) are solid. The four shift-pad arrows are dim.
  /// A timbre-slot press (C4): recall the slot into the live timbre, or -- if armed
  /// -- capture the live timbre into the slot and auto-disarm. Either way the slot
  /// becomes current. Pressing an empty slot unarmed is a no-op (nothing to recall).
  /// Dispatch an editor action against the editor's target. For `target = live`
  /// everything edits the global live timbre. For `target = loop` (C7) a press with
  /// a loop-display selection active stamps onto the selected notes of the active
  /// loop instead; with no selection it falls back to editing the live timbre (the
  /// no-selection play-along path is C7b).
  fn apply_editor_action(&mut self, target: TimbreTarget, action: Option<EditorAction>, now: Duration) {
    match action {
      Some(EditorAction::Param(p)) => self.editor_param(target, p),
      Some(EditorAction::ToggleArm) => self.timbre_armed = !self.timbre_armed,
      Some(EditorAction::Slot(i)) => self.editor_slot(target, i),
      Some(EditorAction::ToggleFold) => {} // fold is per-editor; the key router handles it
      Some(EditorAction::QuickCell(i)) => self.editor_quick(target, i),
      Some(EditorAction::SaveUndo) => {
        if target == TimbreTarget::Loop {
          self.save_undo_press(now); // loop-only; inert in a live editor (6_plan 2.8)
        }
      }
      Some(EditorAction::ToggleSustain) => {
        if target == TimbreTarget::Loop {
          self.toggle_sustain();
        }
      }
      None => {} // blank: consumed, no effect
    }
  }

  /// Toggle sustain-the-edits. Turning it off drops released play-along overrides
  /// (held ones stay until released); turning it on is not retroactive. [6_plan 2.4]
  fn toggle_sustain(&mut self) {
    self.timbre_sustain = !self.timbre_sustain;
    if !self.timbre_sustain {
      self.timbre_overrides.retain(|_, (_, held)| *held);
    }
  }

  /// Release a play-along override (its cell was let go): it returns to tracking
  /// unless sustain keeps it. [6_plan 2.4]
  fn release_override(&mut self, kind: ParamKind) {
    if let Some((_, held)) = self.timbre_overrides.get_mut(&kind) {
      *held = false;
      if !self.timbre_sustain {
        self.timbre_overrides.remove(&kind);
      }
    }
  }

  /// The params currently overriding play-along note-ons -- none while a selection is
  /// active (that is the immediate-stamp path instead). [6_plan 2.4]
  fn active_overrides(&self) -> Vec<TimbreParam> {
    if self.timbre_overrides.is_empty() || self.current_selection_pitches().is_some() {
      return Vec::new();
    }
    self.timbre_overrides.values().map(|(p, _)| *p).collect()
  }

  /// Slide the looper stack (slots, transport, copy/undo/remap, loop display) so it
  /// sits just under the loops-monome live editor (C5c reflow, 7_layout.org). The
  /// editor is 7 rows unfolded / 1 folded, so folding moves the whole stack up by
  /// `EDITOR_ROWS - 1`; the display's top moves with it but its bottom stays at the
  /// grid edge (it grows). A no-op without a loops editor -- every existing config.
  fn reflow_loops(&mut self) {
    let Some(base) = self.loops_reflow else {
      return;
    };
    let shift = if self.loops_timbre_folded { -(EDITOR_ROWS - 1) } else { 0 };
    let cell = |(x, y): (i32, i32)| (x, y + shift);
    let rect = |[x0, y0, x1, y1]: [i32; 4]| [x0, y0 + shift, x1, y1 + shift];
    self.loop_slots_rect = rect(base.slots);
    self.loop_start = cell(base.start);
    self.loop_stop = cell(base.stop);
    self.loop_play = cell(base.play);
    self.loop_copy = cell(base.copy);
    self.loop_undo = cell(base.undo);
    self.loop_toggle = cell(base.toggle);
    // The display's top shifts with the stack; its bottom stays anchored to the grid.
    let [dx0, dy0, dx1, dy1] = base.display;
    self.loop_display_rect = [dx0, dy0 + shift, dx1, dy1];
  }

  /// The save-undo button (6_plan 2.8): a lone tap sets a restore point on the active
  /// slot (prev_committed <- committed, committed <- current); a second tap within the
  /// window reverts -- the events and committed go back to prev_committed, cancelling
  /// the checkpoint the first tap began. Both checkpoints start at the as-recorded
  /// timbres ("save 0", set at finalize).
  fn save_undo_press(&mut self, now: Duration) {
    let slot = self.loops.active;
    let is_double = self
      .save_undo_last
      .is_some_and(|t| now.saturating_sub(t) < self.save_undo_window);
    if is_double {
      if let Some(prev) = self.loops.slots[slot].prev_committed.clone() {
        self.loops.slots[slot].events = prev.clone();
        self.loops.slots[slot].committed = prev;
      }
      self.save_undo_last = None;
    } else {
      self.loops.slots[slot].prev_committed = Some(self.loops.slots[slot].committed.clone());
      self.loops.slots[slot].committed = self.loops.slots[slot].events.clone();
      self.save_undo_last = Some(now);
    }
  }

  fn editor_param(&mut self, target: TimbreTarget, p: TimbreParam) {
    if target == TimbreTarget::Loop {
      if let Some(selected) = self.current_selection_pitches() {
        self.stamp_loop(&selected, |t| p.apply(t));
      } else {
        // No selection: hold a play-along override, applied at each loop note-on the
        // playhead triggers (6_plan 2.4 path 2).
        self.timbre_overrides.insert(p.kind(), (p, true));
      }
      return;
    }
    p.apply(&mut self.live_timbre);
  }

  fn editor_slot(&mut self, target: TimbreTarget, i: usize) {
    if i >= self.timbre_slots.len() {
      return;
    }
    if self.timbre_armed {
      // Arm -> capture the *displayed* timbre (the lowest selected loop note for a
      // loop target, else the live timbre) and auto-disarm. [C4, C7 6_plan 2.3/2.4]
      let captured = self.displayed_timbre(target);
      self.timbre_slots[i] = Some(captured);
      self.timbre_current = Some(i);
      self.timbre_armed = false;
      self.push_quick(captured);
    } else if let Some(t) = self.timbre_slots[i] {
      self.timbre_current = Some(i);
      self.recall_timbre(target, t);
    }
  }

  fn editor_quick(&mut self, target: TimbreTarget, i: usize) {
    if self.timbre_quick.is_empty() {
      if let Some(w) = TimbreEditor::quick_waveform(i) {
        self.editor_param(target, TimbreParam::Waveform(w));
      }
    } else if let Some(t) = self.timbre_quick.get(i).copied() {
      self.recall_timbre(target, t);
    }
  }

  /// Recall a whole timbre onto the edit target: the live timbre, or -- loop target
  /// with a selection -- stamped onto every selected note. [6_plan 2.3/2.4]
  fn recall_timbre(&mut self, target: TimbreTarget, t: Timbre) {
    if target == TimbreTarget::Loop {
      if let Some(selected) = self.current_selection_pitches() {
        self.stamp_loop(&selected, |slot| *slot = t);
        return;
      }
    }
    self.live_timbre = t;
  }

  /// Stamp a timbre change onto every note-on of the active loop whose pitch is
  /// selected, snapshotting for undo first (the existing loop-undo button reverts
  /// it). Works on a stopped loop -- no playhead needed. [6_plan 2.4 path 1]
  fn stamp_loop(&mut self, selected: &[i32], edit: impl Fn(&mut Timbre)) {
    let sel: HashSet<i32> = selected.iter().copied().collect();
    self.apply_to_active(|events| {
      events
        .iter()
        .map(|e| {
          let mut e = *e;
          if e.on && sel.contains(&e.pitch) {
            edit(&mut e.timbre);
          }
          e
        })
        .collect()
    });
  }

  /// The active loop-display selection's pitches, if a selection is active and not
  /// frozen (a frozen selection is a pitch-remap in progress, not a timbre scope).
  fn current_selection_pitches(&self) -> Option<Vec<i32>> {
    let r = self.remap.as_ref()?;
    if r.frozen {
      return None;
    }
    let pitches = selected_pitches(&self.build_active_display(), &r.selection);
    (!pitches.is_empty()).then_some(pitches)
  }

  /// The timbre the loop editor displays (6_plan 2.4 display): under a selection, the
  /// lowest selected note's timbre; else, while the loop plays, the lowest
  /// still-sounding note of the current note-group column (playhead tracking); else on
  /// a rest the last sounded note's timbre is held (`loop_display_held`); else the live
  /// timbre (stopped / before the first note).
  fn displayed_timbre(&self, target: TimbreTarget) -> Timbre {
    if target == TimbreTarget::Loop {
      if let Some(t) = self.lowest_selected_timbre() {
        return t;
      }
      if let Some(t) = self.lowest_sounding_loop_timbre() {
        return t;
      }
      if let Some(t) = self.loop_display_held {
        return t; // hold last on a rest
      }
    }
    self.live_timbre
  }

  fn lowest_selected_timbre(&self) -> Option<Timbre> {
    let pitches = self.current_selection_pitches()?;
    let lowest = *pitches.iter().min()?;
    self.loops.slots[self.loops.active]
      .events
      .iter()
      .find(|e| e.on && e.pitch == lowest)
      .map(|e| e.timbre)
  }

  /// The timbre of the lowest pitch the sounding loop is currently holding (read from
  /// the sink's live ref-counts), i.e. the playhead's current lowest note. [6_plan 2.4]
  fn lowest_sounding_loop_timbre(&self) -> Option<Timbre> {
    let slot = self.loops.sounding?;
    let lowest = self.sink.pitches_held_by(NoteSource::Slot(slot)).into_iter().min()?;
    self.loops.slots[slot]
      .events
      .iter()
      .find(|e| e.on && e.pitch == lowest)
      .map(|e| e.timbre)
  }

  /// Push a freshly-saved timbre onto the quick-roll: newest at the front, keep four
  /// (the 4th-oldest falls off). Recall never calls this, so it does not reorder.
  fn push_quick(&mut self, t: Timbre) {
    self.timbre_quick.insert(0, t);
    self.timbre_quick.truncate(4);
  }

  pub fn edo_levels(&self, flash_on: bool) -> Vec<i32> {
    let mut levels = self.edo_levels_base();
    // The editor overlays last so its occluding rect always wins over the play grid
    // (edo_levels_base has an early return in remap mode, hence the wrapper).
    if let Some(ed) = &self.timbre_editor {
      // For a loop target with a selection, the editor displays the lowest selected
      // note's timbre (6_plan 2.4); otherwise the live timbre.
      let displayed = self.displayed_timbre(ed.target);
      let view = EditorView {
        timbre: &displayed,
        slots: &self.timbre_slots,
        current: self.timbre_current,
        armed: self.timbre_armed,
        quick: &self.timbre_quick,
        sustain: self.timbre_sustain,
      };
      ed.paint(&view, self.timbre_folded, &mut levels, self.grid_w, flash_on);
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
    // The loops-monome live editor occludes its rect (C5c): a press edits the live
    // timbre, or folds/unfolds -- which reflows the loop display under it.
    let folded = self.loops_timbre_folded;
    let hit = self
      .loops_timbre_editor
      .as_ref()
      .filter(|ed| ed.occludes(x, y, folded))
      .map(|ed| (ed.target, ed.press(x, y, folded)));
    if let Some((target, action)) = hit {
      if matches!(action, Some(EditorAction::ToggleFold)) {
        self.loops_timbre_folded = !self.loops_timbre_folded;
        self.reflow_loops();
      } else {
        self.apply_editor_action(target, action, now);
      }
      return true;
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
    if (x, y) == self.loop_start || (x, y) == self.loop_stop || (x, y) == self.loop_play {
      // The transport takes effect on the nearest metronome boundary, so loops
      // always start and end on a beat (their length is a whole number of cycles).
      let now = self.snap_to_clock(now);
      if (x, y) == self.loop_start {
        self.loops.start_recording(now);
      } else if (x, y) == self.loop_stop {
        self.loops.stop(now);
      } else {
        self.loops.play(now);
      }
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
    // The loop-display corner cell clears any active selection (6_plan 2.4, C7).
    let [dcx, dcy, _, _] = self.loop_display_rect;
    if (x, y) == (dcx, dcy) {
      self.exit_remap();
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
    let cursor_before = self.playing.as_ref().unwrap().playback.cursor();
    let actions = {
      let events = &self.loops.slots[slot].events;
      self.playing.as_mut().unwrap().playback.step(events, total)
    };
    // C7c play-along: rewrite the note-ons the playhead just crossed with the active
    // overrides -- destructive, the loop's stored timbres change live (the save-undo
    // checkpoint, not per-edit history, reverts these).
    let overrides = self.active_overrides();
    if !overrides.is_empty() {
      let wrapped = actions.iter().any(|a| matches!(a, PlayAction::ReleaseAll));
      let cursor_after = self.playing.as_ref().unwrap().playback.cursor();
      let len = self.loops.slots[slot].events.len();
      let indices: Vec<usize> = if wrapped {
        (cursor_before..len).chain(0..cursor_after).collect()
      } else {
        (cursor_before..cursor_after).collect()
      };
      for i in indices {
        if self.loops.slots[slot].events[i].on {
          for p in &overrides {
            p.apply(&mut self.loops.slots[slot].events[i].timbre);
          }
        }
      }
    }
    let source = NoteSource::Slot(slot);
    for action in actions {
      match action {
        PlayAction::On(pitch, mut timbre) => {
          for p in &overrides {
            p.apply(&mut timbre);
          }
          self.sink.note_on(pitch, source, timbre);
        }
        PlayAction::Off(pitch) => self.sink.note_off(pitch, source),
        PlayAction::ReleaseAll => self.sink.release_source(source),
      }
    }
    // "Hold last on rests": capture the lowest sounding note's timbre while one plays;
    // on a rest keep the previous (held across rests and loop wraps). [6_plan 2.4]
    self.loop_display_held = self.lowest_sounding_loop_timbre().or(self.loop_display_held);
  }

  /// Round an epoch time to the nearest metronome boundary (a multiple of
  /// `clock_period`). The cycle is free-running from the epoch, so this is the same
  /// grid the flashing transport button and the record-time note-on snap use.
  fn snap_to_clock(&self, now: Duration) -> Duration {
    let p = self.clock_period.as_nanos();
    if p == 0 {
      return now;
    }
    let n = now.as_nanos();
    let snapped = ((n + p / 2) / p) * p;
    Duration::from_nanos(snapped.min(u128::from(u64::MAX)) as u64)
  }

  /// After a transport op changes which slot sounds, release the outgoing slot's
  /// notes and (re)arm the playhead for the new one.
  fn reconcile_playback(&mut self, now: Duration) {
    let target = self.loops.sounding;
    let current = self.playing.as_ref().map(|p| p.slot);
    if target == current {
      return;
    }
    // The held display value belonged to the outgoing slot; drop it on stop / switch.
    self.loop_display_held = None;
    if let Some(p) = &self.playing {
      self.sink.release_source(NoteSource::Slot(p.slot));
    }
    self.playing = target.and_then(|slot| {
      let duration = self.loops.slots[slot].loop_duration?;
      Some(Playing { slot, playback: Playback::new(duration), start: now })
    });
  }

  /// Loops-grid LEDs: active solid > sounding(non-active) flash > recorded dim >
  /// empty dark. The state-relevant transport cell flashes on the metronome
  /// (`clock_on`) -- record while recording, play while a loop sounds, else stop --
  /// the other two show dim so they stay findable. The copy button is solid while a
  /// copy is armed (dim otherwise); the remap-mode toggle is solid in fine mode (dim
  /// in group transpose); undo is dim and findable. The only flashing transport-row
  /// button is the state-relevant R/S/P one.
  pub fn loops_levels(&self, flash_on: bool, clock_on: bool) -> Vec<i32> {
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
        self.transport_level((cx, cy), clock_on)
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
    // The undo button: dim and findable. It does not flash -- the only flashing
    // transport-row button is the state-relevant R/S/P one (see `transport_level`).
    let (ux, uy) = self.loop_undo;
    if ux >= 0 && ux < self.grid_w && uy >= 0 && uy < self.grid_h {
      levels[(uy * self.grid_w + ux) as usize] = LEVEL_DIM;
    }
    // The loops-monome live editor overlays last (C5c), occluding whatever it covers.
    if let Some(ed) = &self.loops_timbre_editor {
      let displayed = self.displayed_timbre(ed.target); // live target -> live timbre
      let view = EditorView {
        timbre: &displayed,
        slots: &self.timbre_slots,
        current: self.timbre_current,
        armed: self.timbre_armed,
        quick: &self.timbre_quick,
        sustain: self.timbre_sustain,
      };
      ed.paint(&view, self.loops_timbre_folded, &mut levels, self.grid_w, flash_on);
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

  /// The level for a transport cell. The one that reflects the current activity --
  /// record while recording, play while a loop sounds, stop when idle -- flashes
  /// full/off on the metronome (`clock_on`); the other two stay dim and findable.
  fn transport_level(&self, cell: (i32, i32), clock_on: bool) -> i32 {
    let flasher = if self.loops.is_recording() {
      self.loop_start
    } else if self.loops.sounding.is_some() {
      self.loop_play
    } else {
      self.loop_stop
    };
    if cell == flasher {
      if clock_on {
        LEVEL_FULL
      } else {
        LEVEL_OFF
      }
    } else {
      LEVEL_DIM
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
  // transport on (0,3)/(1,3)/(2,3). No timbre editors by default.
  fn base_params() -> LooperParams {
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
      clock_period: ms(200),
      cluster: ms(100),
      timbre_editor: None,
      save_undo_window: ms(200),
      loops_timbre_editor: None,
    }
  }

  fn state() -> LooperState {
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(Map::new()));
    let sink = SawNoteSink::new(voices, 80.0, 58, 48000.0, 0.003, 0.05);
    LooperState::new(sink, base_params())
  }

  /// A state whose edo grid carries a timbre editor occluding the top 7 rows
  /// (rect [0,0,15,6]) with the 6_plan 5 default row ranges, unfolded.
  fn state_with_target(target: TimbreTarget) -> LooperState {
    use super::super::timbre_editor::TimbreEditor;
    use super::super::timbre_rows::RowRange;
    let editor = TimbreEditor::new(
      [0, 0, 15, 6],
      target,
      RowRange::LogRange { least: 0.0009, greatest: 0.15 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 5.0, multiplier: 2.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
    );
    let mut s = state();
    // Match what `new()` does from LooperParams: size the slot store to the editor.
    s.timbre_slots = vec![None; editor.slot_count()];
    s.timbre_editor = Some(editor);
    // Most tests exercise the full (unfolded) editor; C5b fold tests re-fold.
    s.timbre_folded = false;
    s
  }

  /// A live-target editor: every press edits the global live timbre (C3b..C5b).
  fn state_with_editor() -> LooperState {
    state_with_target(TimbreTarget::Live)
  }

  /// A loop-target editor: presses edit the active loop via selection-stamp /
  /// play-along (C7).
  fn state_with_loop_editor() -> LooperState {
    state_with_target(TimbreTarget::Loop)
  }

  /// A state with a live-target editor on the LOOPS monome (C5c): the loop display
  /// reflows under it. The display base is the test geometry [0,5,15,15].
  fn state_with_loops_editor() -> LooperState {
    use super::super::timbre_editor::TimbreEditor;
    use super::super::timbre_rows::RowRange;
    let editor = TimbreEditor::new(
      [0, 0, 15, 6],
      TimbreTarget::Live,
      RowRange::LogRange { least: 0.0009, greatest: 0.15 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 5.0, multiplier: 2.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
    );
    let mut s = state();
    s.timbre_slots = vec![None; editor.slot_count()];
    // 7_layout.org: the looper stack's UNFOLDED positions (below a 7-row editor).
    s.loops_reflow = Some(LoopsReflow {
      slots: [0, 7, 15, 8],
      start: (0, 8),
      stop: (1, 8),
      play: (2, 8),
      copy: (15, 7),
      undo: (15, 8),
      toggle: (14, 8),
      display: [0, 9, 15, 15],
    });
    s.loops_timbre_editor = Some(editor);
    s.loops_timbre_folded = true;
    s.reflow_loops(); // -> the folded positions (stack slides up by 6)
    s
  }

  fn level_at(levels: &[i32], x: i32, y: i32) -> i32 {
    levels[(y * 16 + x) as usize]
  }

  #[test]
  fn pressing_a_cell_lights_octave_equivalents_solid() {
    let mut s = state();
    s.edo_key(0, 0, true, ms(0));
    let levels = s.edo_levels(true);
    assert_eq!(level_at(&levels, 0, 0), LEVEL_FULL);
    assert_eq!(level_at(&levels, 7, 2), LEVEL_FULL); // step 58 = octave of step 0
    assert_eq!(level_at(&levels, 1, 0), LEVEL_OFF);
  }

  #[test]
  fn shift_pad_does_not_play_and_shows_dim() {
    let mut s = state();
    let levels = s.edo_levels(true);
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
    assert_eq!(level_at(&s.edo_levels(true), 0, 0), LEVEL_DIM, "sounding loop: dim backdrop");
    // The playhead reaches the note-on -> it lights SOLID as an individual note.
    s.playback_tick(ms(1100)); // 100ms into the loop -> note-on at 0ms is due
    assert!(s.sink.voice_count() >= 1);
    assert_eq!(level_at(&s.edo_levels(true), 0, 0), LEVEL_FULL, "solid while sounding");
    // Its octave-equivalent lights solid too (step 58 = cell (7,2)).
    assert_eq!(level_at(&s.edo_levels(true), 7, 2), LEVEL_FULL, "octave-equivalent solid");
    // After its note-off it drops back to the dim backdrop (still in the loop).
    s.playback_tick(ms(1300)); // 290ms in -> note-off at 290ms is due
    assert_eq!(level_at(&s.edo_levels(true), 0, 0), LEVEL_DIM, "dim again after note-off");
    // Stop: the loop no longer sounds, so it leaves NO backdrop (we reflect the
    // sounding loop, not the active one).
    s.loops_key(1, 3, true, ms(2000)); // stop (active slot 5 was sounding)
    assert_eq!(s.loops.sounding, None);
    assert_eq!(level_at(&s.edo_levels(true), 0, 0), LEVEL_OFF, "silent: no backdrop");
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
    assert_eq!(level_at(&s.edo_levels(true), 0, 0), LEVEL_OFF, "active-but-silent: dark");
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
    let levels = s.loops_levels(true, true);
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
    let levels = s.loops_levels(false, false);
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
  fn transport_snaps_the_loop_length_to_whole_cycles() {
    // The fixture clock is 200 ms. Pressing start at 290 ms snaps to the 200 ms
    // boundary; stop at 1310 ms snaps to 1400; the stored loop is 1200 ms = 6 cycles.
    let mut s = state();
    s.loops_key(0, 3, true, ms(290)); // start -> 200
    s.edo_key(0, 0, true, ms(400));
    s.edo_key(0, 0, false, ms(900));
    s.loops_key(1, 3, true, ms(1310)); // stop -> 1400
    assert_eq!(s.loops.slots[0].loop_duration, Some(ms(1200)));
  }

  #[test]
  fn the_state_relevant_transport_button_flashes_on_the_clock() {
    let mut s = state(); // loop_start (0,3) loop_stop (1,3) loop_play (2,3)
    let lit = |s: &LooperState, cell: (i32, i32)| level_at(&s.loops_levels(true, true), cell.0, cell.1);
    let dark = |s: &LooperState, cell: (i32, i32)| level_at(&s.loops_levels(true, false), cell.0, cell.1);

    // Idle: stop flashes (full on the lit phase, off on the dark phase); the other
    // two stay dim and findable.
    assert_eq!(lit(&s, (1, 3)), LEVEL_FULL, "idle: stop lit on the beat");
    assert_eq!(dark(&s, (1, 3)), LEVEL_OFF, "idle: stop dark off the beat");
    assert_eq!(lit(&s, (0, 3)), LEVEL_DIM, "idle: record dim");
    assert_eq!(lit(&s, (2, 3)), LEVEL_DIM, "idle: play dim");

    // Recording: record flashes, stop falls back to dim.
    s.loops_key(0, 3, true, ms(0));
    assert_eq!(lit(&s, (0, 3)), LEVEL_FULL, "recording: record lit");
    assert_eq!(dark(&s, (0, 3)), LEVEL_OFF, "recording: record dark off-beat");
    assert_eq!(lit(&s, (1, 3)), LEVEL_DIM, "recording: stop dim");

    // Playing: play flashes.
    s.edo_key(0, 0, true, ms(10));
    s.loops_key(2, 3, true, ms(1000)); // play -> the slot sounds
    assert_eq!(lit(&s, (2, 3)), LEVEL_FULL, "playing: play lit");
    assert_eq!(dark(&s, (2, 3)), LEVEL_OFF, "playing: play dark off-beat");
    assert_eq!(lit(&s, (1, 3)), LEVEL_DIM, "playing: stop dim");
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
  fn the_undo_button_is_dim_and_does_not_flash() {
    let s = state(); // loop_undo is (5,2) in the test geometry.
    // Dim and findable, and steady across both flash phases (it never flashes --
    // the only flashing transport-row button is the state-relevant R/S/P one).
    assert_eq!(level_at(&s.loops_levels(true, true), 5, 2), LEVEL_DIM);
    assert_eq!(level_at(&s.loops_levels(false, false), 5, 2), LEVEL_DIM);
  }

  #[test]
  fn the_fine_toggle_starts_lit_and_dims_in_group_transpose() {
    let mut s = state(); // loop_toggle is (5,0) in the test geometry.
    // Fine is the default, so the toggle starts lit (solid).
    assert_eq!(level_at(&s.loops_levels(true, true), 5, 0), LEVEL_FULL, "fine starts lit");
    // Toggle to group transpose -> dim (still findable).
    s.loops_key(5, 0, true, ms(0));
    assert_eq!(level_at(&s.loops_levels(true, true), 5, 0), LEVEL_DIM, "group transpose dims it");
    // Toggle back to fine -> lit again.
    s.loops_key(5, 0, true, ms(10));
    assert_eq!(level_at(&s.loops_levels(true, true), 5, 0), LEVEL_FULL, "back to fine, lit");
  }

  #[test]
  fn the_copy_button_lights_while_armed_and_a_re_press_cancels() {
    let mut s = state(); // loop_copy is (5,1) in the test geometry.
    // Idle: dim and findable.
    assert_eq!(level_at(&s.loops_levels(true, true), 5, 1), LEVEL_DIM, "idle copy dim");
    // Arm copy -> solid.
    s.loops_key(5, 1, true, ms(0));
    assert_eq!(level_at(&s.loops_levels(true, true), 5, 1), LEVEL_FULL, "armed copy solid");
    // Pick a 'from' slot (slot 0): still mid-gesture (awaiting 'to') -> still solid.
    s.loops_key(0, 0, true, ms(10));
    assert_eq!(s.copy_from, Some(0), "origin picked");
    assert_eq!(level_at(&s.loops_levels(true, true), 5, 1), LEVEL_FULL, "from picked, still solid");
    // Press copy again -> cancels and forgets the picked origin -> dim again.
    s.loops_key(5, 1, true, ms(20));
    assert_eq!(s.copy_from, None, "origin forgotten");
    assert!(!s.copy_arming, "disarmed");
    assert_eq!(level_at(&s.loops_levels(true, true), 5, 1), LEVEL_DIM, "re-press cancels");
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
  fn live_timbre_boots_at_a_reachable_amplitude() {
    // The instrument must not start louder than the amplitude row can reach. Build a
    // state through `new()` WITH an editor in the params (the real runtime path), and
    // check the booted gain is the row's top cell -- i.e. cell_for(gain) == top.
    use super::super::timbre_editor::TimbreEditor;
    use super::super::timbre_rows::RowRange;
    let amplitude = RowRange::LogRange { least: 0.0009, greatest: 0.15 };
    let editor = TimbreEditor::new(
      [0, 0, 15, 6],
      TimbreTarget::Live,
      amplitude,
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
      RowRange::Linear { min: 0.0, max: 1.0 },
      RowRange::LogFactor { least: 5.0, multiplier: 2.0 },
      RowRange::LogFactor { least: 0.25, multiplier: 2.0 },
    );
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(Map::new()));
    let sink = SawNoteSink::new(voices, 80.0, 58, 48000.0, 0.003, 0.05);
    let mut params = base_params();
    params.loops_timbre_editor = Some(editor);
    let s = LooperState::new(sink, params);
    // Starts at unity (the row's top value), not the out-of-range default gain 1.0.
    assert!((s.live_timbre.gain - 0.15).abs() < 1e-5, "boots at unity = 0.15");
    assert!(s.live_timbre.gain < 1.0, "below Timbre::default's linear 1.0");
    // And that value lands on the last cell of a 16-wide row -- it IS reachable.
    assert_eq!(amplitude.cell_for(s.live_timbre.gain, 16), 15, "boot gain is the top cell");
  }

  #[test]
  fn editor_inert_cell_swallows_the_press() {
    let mut s = state_with_editor();
    // x=6,y=0 is the sustain cell -- inert until C7, but inside the editor rect, so
    // it must be swallowed (no note, no fall-through).
    assert!(s.edo_key(6, 0, true, ms(0)), "consumed");
    assert!(s.down.is_empty(), "no note played");
    assert_eq!(s.live_timbre, Timbre::default(), "nothing changed");
  }

  #[test]
  fn editor_overlays_edo_leds_and_occludes_the_top_rows() {
    let s = state_with_editor();
    let levels = s.edo_levels(true);
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

  // ---- C4: timbre slots + save/recall ----

  #[test]
  fn slot_save_recall_round_trips_a_full_timbre_and_auto_disarms() {
    use crate::types::Waveform;
    let mut s = state_with_editor();
    // Dial a non-default timbre: Saw + a non-unity amplitude + AM depth.
    s.edo_key(4, 0, true, ms(0)); // Saw
    s.edo_key(8, 1, true, ms(1)); // amplitude row, some mid cell
    s.edo_key(15, 2, true, ms(2)); // AM depth = 1.0
    let saved = s.live_timbre;
    assert_ne!(saved, Timbre::default(), "we actually changed the timbre");
    // Arm, then capture into slot 0.
    s.edo_key(5, 0, true, ms(3)); // arm cell
    assert!(s.timbre_armed, "armed");
    s.edo_key(8, 0, true, ms(4)); // slot 0
    assert!(!s.timbre_armed, "auto-disarms after one capture");
    assert_eq!(s.timbre_slots[0], Some(saved), "slot 0 holds the captured timbre");
    assert_eq!(s.timbre_current, Some(0));
    // Move the live timbre away, then recall slot 0.
    s.edo_key(1, 0, true, ms(5)); // Sine
    assert_eq!(s.live_timbre.waveform, Waveform::Sine);
    s.edo_key(8, 0, true, ms(6)); // recall slot 0 (unarmed)
    assert_eq!(s.live_timbre, saved, "recall restores the full timbre incl. amplitude + AM");
  }

  #[test]
  fn pressing_an_empty_slot_unarmed_is_a_noop() {
    let mut s = state_with_editor();
    s.edo_key(4, 0, true, ms(0)); // Saw
    let before = s.live_timbre;
    s.edo_key(9, 0, true, ms(1)); // empty slot 1, unarmed
    assert_eq!(s.live_timbre, before, "nothing recalled");
    assert_eq!(s.timbre_current, None, "empty recall does not set current");
  }

  #[test]
  fn slot_leds_show_empty_occupied_current_and_dirty_flash() {
    let mut s = state_with_editor();
    // Save the default into slot 0 (current=0), then a Saw into slot 1 (current=1).
    s.edo_key(5, 0, true, ms(0)); // arm
    s.edo_key(8, 0, true, ms(1)); // save slot 0
    s.edo_key(4, 0, true, ms(2)); // Saw
    s.edo_key(5, 0, true, ms(3)); // arm
    s.edo_key(9, 0, true, ms(4)); // save slot 1 (current, live == slot1 -> clean)
    let levels = s.edo_levels(true);
    assert_eq!(level_at(&levels, 8, 0), LEVEL_DIM, "occupied non-current dim");
    assert_eq!(level_at(&levels, 9, 0), LEVEL_FULL, "current clean bright");
    assert_eq!(level_at(&levels, 10, 0), LEVEL_OFF, "empty slot off");
    // Diverge the live timbre from slot 1 -> the current slot flashes 50/50.
    s.edo_key(1, 0, true, ms(5)); // Sine, != slot 1 (Saw)
    assert_eq!(level_at(&s.edo_levels(true), 9, 0), LEVEL_FULL, "dirty current bright on flash");
    assert_eq!(level_at(&s.edo_levels(false), 9, 0), LEVEL_OFF, "dirty current dark off flash");
  }

  #[test]
  fn arm_cell_lights_while_armed() {
    let mut s = state_with_editor();
    assert_eq!(level_at(&s.edo_levels(true), 5, 0), LEVEL_DIM, "arm idle dim");
    s.edo_key(5, 0, true, ms(0)); // arm
    assert_eq!(level_at(&s.edo_levels(true), 5, 0), LEVEL_FULL, "armed bright");
  }

  // ---- C5b: fold + folded strip + rolling quick-cells ----

  #[test]
  fn folding_reveals_the_grid_below_the_strip() {
    let mut s = state_with_editor(); // starts unfolded
    // Unfolded: a press on row 3 (an FX row) is consumed by the editor (no note).
    s.edo_key(0, 3, true, ms(0));
    assert!(s.down.is_empty(), "unfolded editor eats the press");
    // Fold via the top-left cell.
    s.edo_key(0, 0, true, ms(1));
    assert!(s.timbre_folded, "folded");
    // Folded: row 3 falls through to the grid and plays a note.
    s.edo_key(0, 3, true, ms(2));
    assert_eq!(s.down.len(), 1, "folded reveals the row -> it plays");
  }

  #[test]
  fn folded_strip_amplitude_edits_gain() {
    let mut s = state_with_editor();
    s.edo_key(0, 0, true, ms(0)); // fold
    // Folded amplitude lives at x>=5; the far cell is unity (~0.15).
    s.edo_key(15, 0, true, ms(1));
    assert!((s.live_timbre.gain - 0.15).abs() < 1e-4, "folded amplitude edits gain");
  }

  #[test]
  fn quick_cells_set_waveforms_before_any_save() {
    use crate::types::Waveform;
    let mut s = state_with_editor();
    s.edo_key(0, 0, true, ms(0)); // fold
    assert!(s.timbre_quick.is_empty(), "waveform mode");
    s.edo_key(3, 0, true, ms(1)); // quick cell 2 = Square (waveform mode)
    assert_eq!(s.live_timbre.waveform, Waveform::Square);
  }

  #[test]
  fn saving_a_timbre_rolls_the_quick_cells_and_recall_works() {
    use crate::types::Waveform;
    let mut s = state_with_editor(); // unfolded for the save gesture
    // Save a Saw into slot 0.
    s.edo_key(4, 0, true, ms(0)); // Saw
    let saw = s.live_timbre;
    s.edo_key(5, 0, true, ms(1)); // arm
    s.edo_key(8, 0, true, ms(2)); // slot 0 -> capture (rolls into quick)
    assert_eq!(s.timbre_quick, vec![saw], "save pushes onto the quick-roll");
    // Save a Sine into slot 1; newest goes to the front.
    s.edo_key(1, 0, true, ms(3)); // Sine
    let sine = s.live_timbre;
    s.edo_key(5, 0, true, ms(4)); // arm
    s.edo_key(9, 0, true, ms(5)); // slot 1
    assert_eq!(s.timbre_quick, vec![sine, saw], "newest at front");
    // Fold and recall the 2nd quick cell (index 1 = saw).
    s.edo_key(0, 0, true, ms(6)); // fold
    s.edo_key(2, 0, true, ms(7)); // quick cell 1 -> recall saw
    assert_eq!(s.live_timbre.waveform, Waveform::Saw, "quick recall restores the timbre");
  }

  #[test]
  fn quick_roll_keeps_only_four() {
    let mut s = state_with_editor();
    // Save five distinct timbres via slots 0..5.
    for (i, wf) in [0i32, 1, 2, 3, 1].iter().enumerate() {
      s.edo_key(1 + wf, 0, true, ms(i as u64 * 10)); // pick a waveform cell
      s.edo_key(5, 0, true, ms(i as u64 * 10 + 1)); // arm
      s.edo_key(8 + i as i32, 0, true, ms(i as u64 * 10 + 2)); // slot i
    }
    assert_eq!(s.timbre_quick.len(), 4, "quick-roll holds at most four");
  }

  // ---- C7a: loop-timbre manipulation -- selection stamp (path 1) ----

  /// Record a one-note loop (pitch at edo (px,8), below the editor) into the active
  /// slot and stop. The display rect is [0,5,15,15]: row picker x=0, column picker
  /// y=5, corner (0,5).
  fn record_one_note(s: &mut LooperState, px: i32) {
    s.loops_key(0, 3, true, ms(0)); // start
    s.edo_key(px, 8, true, ms(10)); // a real note (row 8 is below the editor)
    s.edo_key(px, 8, false, ms(200));
    s.loops_key(1, 3, true, ms(1000)); // stop
  }

  #[test]
  fn loop_selection_stamp_changes_the_selected_note_and_undo_reverts() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor(); // unfolded
    record_one_note(&mut s, 0);
    s.loops_key(0, 15, true, ms(1100)); // select the single (bottom) display row
    assert!(s.remap.is_some(), "selection active");
    // Stamp Saw via the editor control-row Saw cell (x=4): loop target + selection.
    s.edo_key(4, 0, true, ms(1200));
    let wf = |s: &LooperState| s.loops.slots[0].events.iter().find(|e| e.on).unwrap().timbre.waveform;
    assert_eq!(wf(&s), Waveform::Saw, "the selected note is stamped");
    assert_eq!(s.live_timbre.waveform, Waveform::Triangle, "live timbre untouched");
    // The existing loop-undo button reverts the stamp (it snapshotted to history).
    s.loops_key(5, 2, true, ms(1300)); // loop_undo at (5,2) in the test geometry
    assert_eq!(wf(&s), Waveform::Triangle, "undo reverts the stamp");
  }

  #[test]
  fn loop_column_stamp_changes_all_notes_at_once() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    s.loops_key(0, 3, true, ms(0)); // start
    s.edo_key(0, 8, true, ms(10)); // two notes close in time -> one display column
    s.edo_key(1, 8, true, ms(15));
    s.edo_key(0, 8, false, ms(200));
    s.edo_key(1, 8, false, ms(205));
    s.loops_key(1, 3, true, ms(1000)); // stop
    s.loops_key(1, 5, true, ms(1100)); // select column 0 (picker at y=5, x=1)
    assert!(s.remap.is_some(), "column selection active");
    s.edo_key(4, 0, true, ms(1200)); // stamp Saw
    let saws = s.loops.slots[0].events.iter().filter(|e| e.on && e.timbre.waveform == Waveform::Saw).count();
    assert_eq!(saws, 2, "both notes in the column stamped at once (works on a stopped loop)");
  }

  #[test]
  fn corner_cell_clears_the_loop_selection() {
    let mut s = state_with_loop_editor();
    record_one_note(&mut s, 0);
    s.loops_key(0, 15, true, ms(1100)); // select a row
    assert!(s.remap.is_some());
    s.loops_key(0, 5, true, ms(1200)); // corner (dx0,dy0)=(0,5)
    assert!(s.remap.is_none(), "the corner cell clears the selection");
  }

  #[test]
  fn loop_editor_displays_the_lowest_selected_notes_timbre() {
    let mut s = state_with_loop_editor();
    record_one_note(&mut s, 0);
    s.loops_key(0, 15, true, ms(1100)); // select the note's row
    s.edo_key(4, 0, true, ms(1200)); // stamp Saw onto it
    // The editor now DISPLAYS the selected note's timbre (Saw), not live (Triangle).
    let levels = s.edo_levels(true);
    assert_eq!(level_at(&levels, 4, 0), LEVEL_FULL, "editor shows the selected note's saw");
    assert_eq!(level_at(&levels, 2, 0), LEVEL_OFF, "not the live triangle");
  }

  #[test]
  fn loop_recall_onto_a_selection_stamps_the_whole_timbre() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    // Put a Saw timbre into slot 0 (set live directly, then arm + capture).
    s.live_timbre.waveform = Waveform::Saw;
    s.edo_key(5, 0, true, ms(1)); // arm
    s.edo_key(8, 0, true, ms(2)); // capture the displayed (live, no selection) timbre
    s.live_timbre.waveform = Waveform::Triangle; // reset live
    record_one_note(&mut s, 0);
    s.loops_key(0, 15, true, ms(1100)); // select the note
    s.edo_key(8, 0, true, ms(1200)); // recall slot 0 (Saw) onto the selection
    let on = s.loops.slots[0].events.iter().find(|e| e.on).unwrap();
    assert_eq!(on.timbre.waveform, Waveform::Saw, "recall stamps the whole timbre onto the selection");
  }

  // ---- C7c: play-along (path 2) + sustain-the-edits ----

  /// Record a Saw note into slot 0, then play it (target=loop, no selection).
  fn record_and_play_saw(s: &mut LooperState) {
    s.live_timbre.waveform = crate::types::Waveform::Saw;
    record_one_note(s, 0);
    s.live_timbre.waveform = crate::types::Waveform::Triangle;
    s.loops_key(2, 3, true, ms(1000)); // play
  }

  #[test]
  fn no_selection_param_press_holds_a_play_along_override() {
    let mut s = state_with_loop_editor();
    assert!(s.remap.is_none());
    s.edo_key(1, 0, true, ms(0)); // Sine, no selection -> an override (not live, not the loop yet)
    assert_eq!(s.live_timbre.waveform, crate::types::Waveform::Triangle, "live untouched");
    assert!(!s.timbre_overrides.is_empty(), "a held override exists");
  }

  #[test]
  fn play_along_rewrites_notes_as_the_playhead_triggers_them() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    record_and_play_saw(&mut s); // the loop note is Saw
    // Hold a Sine override (no selection), then let the playhead reach the note.
    s.edo_key(1, 0, true, ms(1050)); // Sine override held
    s.playback_tick(ms(1110)); // playhead crosses the note-on -> rewritten to Sine
    let on = s.loops.slots[0].events.iter().find(|e| e.on).unwrap();
    assert_eq!(on.timbre.waveform, Waveform::Sine, "play-along rewrote the stored note");
  }

  #[test]
  fn released_override_stops_applying_without_sustain() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    record_and_play_saw(&mut s);
    s.edo_key(1, 0, true, ms(1050)); // hold Sine
    s.edo_key(1, 0, false, ms(1060)); // release (sustain OFF -> override drops)
    assert!(s.timbre_overrides.is_empty(), "released override drops without sustain");
    s.playback_tick(ms(1110)); // playhead crosses the note -> NOT rewritten
    let on = s.loops.slots[0].events.iter().find(|e| e.on).unwrap();
    assert_eq!(on.timbre.waveform, Waveform::Saw, "no override -> note keeps Saw");
  }

  #[test]
  fn sustain_keeps_a_released_override_applying() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    record_and_play_saw(&mut s);
    s.edo_key(6, 0, true, ms(1040)); // sustain-the-edits ON
    assert!(s.timbre_sustain);
    s.edo_key(1, 0, true, ms(1050)); // hold Sine
    s.edo_key(1, 0, false, ms(1060)); // release -> stays overridden (sustain on)
    assert!(!s.timbre_overrides.is_empty(), "sustain keeps the released override");
    s.playback_tick(ms(1110)); // playhead crosses the note -> rewritten to Sine
    let on = s.loops.slots[0].events.iter().find(|e| e.on).unwrap();
    assert_eq!(on.timbre.waveform, Waveform::Sine, "sustained override still applies");
    // Toggling sustain off now drops the released override.
    s.edo_key(6, 0, true, ms(1120));
    assert!(s.timbre_overrides.is_empty(), "sustain off drops released overrides");
  }

  // ---- C7b: save-undo checkpoint ----

  fn loop_wf(s: &LooperState) -> crate::types::Waveform {
    s.loops.slots[0].events.iter().find(|e| e.on).unwrap().timbre.waveform
  }

  #[test]
  fn save_undo_single_checkpoints_and_double_reverts() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    record_one_note(&mut s, 0); // committed = triangle ("save 0")
    s.loops_key(0, 15, true, ms(1100)); // select the note (selection persists through stamps)
    s.edo_key(4, 0, true, ms(1200)); // stamp Saw
    assert_eq!(loop_wf(&s), Waveform::Saw);
    s.edo_key(7, 0, true, ms(1300)); // save-undo SINGLE -> committed = Saw
    s.edo_key(3, 0, true, ms(1400)); // stamp Square (events = Square, committed = Saw)
    assert_eq!(loop_wf(&s), Waveform::Square);
    // A double-press: tap #1 (>200 ms after the save) checkpoints Square; tap #2
    // (<200 ms) cancels that checkpoint and reverts to the prior save (Saw).
    s.edo_key(7, 0, true, ms(2000)); // tap #1
    s.edo_key(7, 0, true, ms(2100)); // tap #2 (within 200 ms)
    assert_eq!(loop_wf(&s), Waveform::Saw, "double-press restores the prior save");
  }

  #[test]
  fn save_undo_lone_tap_only_sets_a_restore_point() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    record_one_note(&mut s, 0);
    s.loops_key(0, 15, true, ms(1100));
    s.edo_key(4, 0, true, ms(1200)); // stamp Saw
    s.edo_key(7, 0, true, ms(1300)); // lone save (no second tap within 200 ms)
    assert_eq!(loop_wf(&s), Waveform::Saw, "a lone tap does not revert the events");
    let committed_wf = s.loops.slots[0].committed.iter().find(|e| e.on).unwrap().timbre.waveform;
    assert_eq!(committed_wf, Waveform::Saw, "committed advanced to the saved timbre");
  }

  #[test]
  fn save_zero_is_the_as_recorded_timbres() {
    // Before any manual save, committed = the as-recorded events (record-time "save 0").
    let mut s = state_with_loop_editor();
    record_one_note(&mut s, 0);
    assert_eq!(s.loops.slots[0].committed, s.loops.slots[0].events, "save 0 = as recorded");
    assert!(s.loops.slots[0].prev_committed.is_none());
  }

  #[test]
  fn loop_editor_display_tracks_the_lowest_sounding_note() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    s.live_timbre.waveform = Waveform::Saw; // the recorded note will be Saw
    record_one_note(&mut s, 0);
    s.live_timbre.waveform = Waveform::Triangle; // diverge live from the loop note
    assert!(s.remap.is_none(), "no selection");
    s.loops_key(2, 3, true, ms(1100)); // play
    s.playback_tick(ms(1110)); // playhead reaches the note-on -> it sounds
    assert_eq!(level_at(&s.edo_levels(true), 4, 0), LEVEL_FULL, "display tracks the sounding saw note");
  }

  #[test]
  fn loop_editor_display_holds_the_last_note_on_a_rest() {
    use crate::types::Waveform;
    let mut s = state_with_loop_editor();
    s.live_timbre.waveform = Waveform::Saw; // the recorded note is Saw (note-on@0, off@200)
    record_one_note(&mut s, 0);
    s.live_timbre.waveform = Waveform::Triangle; // diverge live from the loop note
    s.loops_key(2, 3, true, ms(1100)); // play
    s.playback_tick(ms(1110)); // at the note-on -> Saw sounds
    assert_eq!(level_at(&s.edo_levels(true), 4, 0), LEVEL_FULL, "shows the sounding saw note");
    // Tick PAST the note-off into a rest: the display HOLDS the last note (Saw), not live.
    s.playback_tick(ms(1400));
    assert_eq!(level_at(&s.edo_levels(true), 4, 0), LEVEL_FULL, "holds saw on the rest");
    assert_eq!(level_at(&s.edo_levels(true), 2, 0), LEVEL_OFF, "not the live triangle");
    // Stopping clears the hold -> falls back to the live timbre (Triangle, cell 2).
    s.loops_key(1, 3, true, ms(1500)); // stop
    assert_eq!(level_at(&s.edo_levels(true), 2, 0), LEVEL_FULL, "stopped -> live triangle");
    assert_eq!(level_at(&s.edo_levels(true), 4, 0), LEVEL_OFF, "no longer holding saw");
  }

  #[test]
  fn play_along_override_is_idempotent_across_loop_passes() {
    // The review flagged (and its verifier refuted) a "cumulative drift on wrap" risk.
    // apply() is absolute assignment, so re-stamping each pass is stable -- prove it.
    let mut s = state_with_loop_editor();
    record_and_play_saw(&mut s); // loop duration ~1000 ms; note at elapsed 0
    s.edo_key(8, 1, true, ms(1050)); // hold a gain override (amplitude row, cell 8)
    s.playback_tick(ms(1110)); // pass 1: the note's gain is rewritten to the override
    let g1 = s.loops.slots[0].events.iter().find(|e| e.on).unwrap().timbre.gain;
    for t in [2110u64, 3110, 4110] {
      s.playback_tick(ms(t)); // passes 2..4 (each wraps the loop)
    }
    let g4 = s.loops.slots[0].events.iter().find(|e| e.on).unwrap().timbre.gain;
    assert_eq!(g1, g4, "gain override is idempotent across passes -- no cumulative drift");
  }

  // ---- C5c: loops-monome live editor + reflow ----

  #[test]
  fn loops_editor_fold_reflows_the_whole_looper_stack() {
    let mut s = state_with_loops_editor();
    // Folded (resting): strip row 0, slots rows 1-2, transport row 2, display rows 3-15.
    assert_eq!(s.loop_slots_rect, [0, 1, 15, 2], "folded slots");
    assert_eq!(s.loop_start, (0, 2), "folded record");
    assert_eq!(s.loop_play, (2, 2), "folded play");
    assert_eq!(s.loop_display_rect, [0, 3, 15, 15], "folded display (top reflows, bottom fixed)");
    // Unfold via the loops editor's fold cell (0,0): the whole stack slides down by 6.
    assert!(s.loops_key(0, 0, true, ms(0)));
    assert!(!s.loops_timbre_folded, "unfolded");
    assert_eq!(s.loop_slots_rect, [0, 7, 15, 8], "unfolded slots");
    assert_eq!(s.loop_start, (0, 8), "unfolded record");
    assert_eq!(s.loop_display_rect, [0, 9, 15, 15], "unfolded display (shrinks)");
    // Fold again -> the stack slides back up.
    s.loops_key(0, 0, true, ms(1));
    assert_eq!(s.loop_slots_rect, [0, 1, 15, 2], "re-folded slots");
    assert_eq!(s.loop_display_rect, [0, 3, 15, 15], "re-folded display");
  }

  #[test]
  fn loops_editor_edits_the_live_timbre() {
    use crate::types::Waveform;
    let mut s = state_with_loops_editor();
    s.loops_key(0, 0, true, ms(0)); // unfold (value rows reachable)
    s.loops_key(4, 0, true, ms(1)); // Saw waveform cell on the loops editor
    assert_eq!(s.live_timbre.waveform, Waveform::Saw, "the loops editor edits the live timbre");
  }

  #[test]
  fn loops_editor_occludes_its_rect_on_the_loops_grid() {
    let s = state_with_loops_editor(); // folded
    let levels = s.loops_levels(true, true);
    assert_eq!(level_at(&levels, 0, 0), LEVEL_MID, "fold cell findable on the loops grid");
  }
}
