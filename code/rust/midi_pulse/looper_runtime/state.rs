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

use super::edo::{register_delta, shift_for_cell, step_for_cell};
use super::loop_store::{LoopStore, PlayAction, Playback};
use super::sink::{NoteSource, SawNoteSink};

/// Monome LED levels, in the four buckets this grid actually shows (0/4/8/15).
pub const LEVEL_OFF: i32 = 0;
pub const LEVEL_DIM: i32 = 4;
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
  pub quantize: Duration,
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

  register: i32,
  down: HashMap<(i32, i32), i32>,

  loops: LoopStore,
  playing: Option<Playing>,
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
      register: 0,
      down: HashMap::new(),
      loops: LoopStore::new(n_slots, p.quantize),
      playing: None,
    }
  }

  // ---- edo grid ----

  /// Handle a key on the edo grid. Plays live and (while recording) captures the
  /// note. Returns true if the edo LEDs may have changed.
  pub fn edo_key(&mut self, x: i32, y: i32, press: bool, now: Duration) -> bool {
    if press {
      if let Some(shift) = shift_for_cell(self.shift_rect, (x, y)) {
        self.register += register_delta(shift, self.x_step, self.y_step, self.edo);
        return true;
      }
      if in_rect(self.edo_rect, x, y) {
        let pitch = step_for_cell(self.x_step, self.y_step, self.register, x, y);
        if let Some(old) = self.down.insert((x, y), pitch) {
          if old != pitch {
            self.sink.note_off(old, NoteSource::Live(x, y));
            self.loops.record_note(now, old, false);
          }
        }
        self.sink.note_on(pitch, NoteSource::Live(x, y));
        self.loops.record_note(now, pitch, true);
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

  /// Edo LEDs: every octave-equivalent of a sounding pitch is solid. Sounding =
  /// live-held notes plus the active loop's content (solid whether or not it is
  /// playing, per the design).
  pub fn edo_levels(&self) -> Vec<i32> {
    let mut classes: HashSet<i32> =
      self.down.values().map(|p| p.rem_euclid(self.edo)).collect();
    for event in &self.loops.slots[self.loops.active].events {
      if event.on {
        classes.insert(event.pitch.rem_euclid(self.edo));
      }
    }
    let mut levels = vec![LEVEL_OFF; (self.grid_w * self.grid_h) as usize];
    for y in 0..self.grid_h {
      for x in 0..self.grid_w {
        let idx = (y * self.grid_w + x) as usize;
        let class = step_for_cell(self.x_step, self.y_step, self.register, x, y).rem_euclid(self.edo);
        if classes.contains(&class) {
          levels[idx] = LEVEL_FULL;
        } else if shift_for_cell(self.shift_rect, (x, y)).is_some() {
          levels[idx] = LEVEL_DIM;
        }
      }
    }
    levels
  }

  // ---- loops grid ----

  /// Handle a key on the loops grid (key-up has no effect). The transport windows
  /// overlay the slot grid, so they are tested first.
  pub fn loops_key(&mut self, x: i32, y: i32, press: bool, now: Duration) -> bool {
    if !press {
      return false;
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
    if let Some(slot) = self.slot_at(x, y) {
      self.loops.set_active(slot);
      return true;
    }
    false
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
        PlayAction::On(pitch) => self.sink.note_on(pitch, source),
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
  /// empty dark. The transport cells show dim so they are findable.
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
    levels
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
        quantize: ms(70),
      },
    )
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
    assert_eq!(level_at(&s.edo_levels(), 14, 15), LEVEL_DIM);
    assert!(s.edo_key(14, 15, true, ms(0)));
    assert!(s.down.is_empty());
    assert_eq!(s.register(), 1); // down-shift = +y_step
  }

  #[test]
  fn record_then_play_makes_a_slot_sound_and_reflect_on_edo() {
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
    // The active slot's content reflects solid on the edo grid even before the
    // playhead reaches it.
    assert_eq!(level_at(&s.edo_levels(), 0, 0), LEVEL_FULL);
    // The playhead emits the note into the sink as time advances.
    s.playback_tick(ms(1100)); // 100ms into the loop -> note-on at 20ms is due
    assert!(s.sink.voice_count() >= 1);
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
}
