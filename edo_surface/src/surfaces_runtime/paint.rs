//! The LED painting path: the per-grid button/sounding-class views fed to
//! `grid::levels_for_grid` (`accrete_view`, `union_sounding`, `publish_sounding`,
//! `trail_set`, `volume_active_col`), and sending the computed levels to the
//! device (`send_binary_frame`, `send_diffs`, `blank_grid`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::monome;

use super::grid::{
  button_level, volume_cells, ButtonOverlay, BRIGHT, DIM, OFF, STEADY_DIM,
};
use super::ring::Reason;
use super::tone_controls::ToneTarget;
use super::{chords, compact_loops, dance, momentary_chords, GridThread, NO_RECT};

/// One lock: this grid's accrete-control LED view (its OWN bank's state) plus the
/// union of every grid's sustained pitch classes (which paint bright on every
/// grid -- they are all sounding, like the cross-grid note reflection).
pub(super) fn accrete_view(rt: &GridThread, elapsed: Duration) -> (Vec<ButtonOverlay>, HashSet<i32>) {
  let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
  let s = &rings[rt.grid_index].accrete;
  let buttons = if rt.overlays.compact_loop_rect != NO_RECT {
    let first_half = (elapsed.as_millis() / 500) % 2 == 0;
    vec![
      (
        rt.overlays.clear_rect,
        if s.clear_lit() || first_half { BRIGHT } else { OFF },
      ),
      (rt.overlays.needs_holding_rect, button_level(s.needs_holding_lit())),
      (
        rt.overlays.accrete_rect,
        if s.accrete_lit() || !first_half { BRIGHT } else { OFF },
      ),
      (rt.overlays.erase_rect, button_level(s.erase_lit())),
    ]
  } else {
    vec![
      (rt.overlays.clear_rect, button_level(s.clear_lit())),
      (rt.overlays.needs_holding_rect, button_level(s.needs_holding_lit())),
      (rt.overlays.accrete_rect, button_level(s.accrete_lit())),
      (rt.overlays.erase_rect, button_level(s.erase_lit())),
    ]
  };
  let mut classes = HashSet::new();
  for gr in rings.iter() {
    classes.extend(gr.store.classes(Reason::Sustain, rt.tuning.edo));
  }
  (buttons, classes)
}

/// One lock: this grid's chord-block LED view plus the union of every grid's live
/// chord-voice pitch classes (chord voices are sounding notes, so they reflect
/// bright on every grid exactly like the sustained classes).
///
/// The block's language (2_discussion "LEDs" + the black-arm refinement): SLOTS
/// idle DIM (empty and occupied look alike -- empty slots are inert) and an
/// ACTIVE slot is solid BRIGHT; the ARM cell is BLACK until armed, then flashes
/// on the dance clock's 150 ms half-period (Jeff: "say 100 ms ... can sync with
/// the dances if that makes for cleaner code").
pub(super) fn chord_view(rt: &GridThread, elapsed: Duration) -> (Vec<ButtonOverlay>, HashSet<i32>) {
  let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
  let mut classes = HashSet::new();
  for gr in rings.iter() {
    classes.extend(gr.chord.live_pitches().map(|p| p.rem_euclid(rt.tuning.edo)));
  }
  let rect = rt.overlays.chord_rect;
  let mut buttons = Vec::new();
  if rect != NO_RECT {
    let layer = &rings[rt.grid_index].chord;
    let (ax, ay) = chords::arm_cell(rect);
    let arm_level = if layer.armed {
      if dance::flash_on(elapsed) {
        BRIGHT
      } else {
        OFF
      }
    } else {
      // Black at rest (unlike the slots' resting dim): the arm cell only ever
      // shows "armed", so its idle state carries no information worth a glow.
      OFF
    };
    buttons.push(([ax, ay, ax, ay], arm_level));
    for slot in 0..chords::SLOTS {
      let (x, y) = chords::slot_cell(rect, slot);
      buttons.push(([x, y, x, y], if layer.active[slot] { BRIGHT } else { DIM }));
    }
  }
  (buttons, classes)
}

/// The pitch-only block: tone target + chord mode + thirteen slots + arm. Slots
/// are bright only while their momentary recall actually sounds.
pub(super) fn momentary_chord_view(
  rt: &GridThread,
  elapsed: Duration,
) -> (Vec<ButtonOverlay>, HashSet<i32>) {
  let rect = rt.overlays.momentary_chord_rect;
  if rect == NO_RECT {
    return (Vec::new(), HashSet::new());
  }
  let (armed, chord_mode, populated, sounding, classes) = {
    let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let mut classes = HashSet::new();
    for gr in rings.iter() {
      classes.extend(
        gr.momentary_chords.live_pitches().map(|pitch| pitch.rem_euclid(rt.tuning.edo)),
      );
    }
    let chords = &rings[rt.grid_index].momentary_chords;
    let sounding: [bool; momentary_chords::SLOTS] =
      std::array::from_fn(|slot| chords.slot_sounding(slot));
    let populated: [bool; momentary_chords::SLOTS] =
      std::array::from_fn(|slot| chords.slots[slot].is_some());
    (chords.armed, chords.chord_mode, populated, sounding, classes)
  };
  let tone_target = {
    let controls = rt.shared.tone_controls.lock().unwrap_or_else(|e| e.into_inner());
    controls.get(rt.grid_index).map(|state| state.tone_target())
  };
  let mut buttons = Vec::new();
  if rt.overlays.tone_target_rect != NO_RECT {
    let target_level = match tone_target {
      Some(ToneTarget::FingeredSustained) | None => OFF,
      Some(ToneTarget::Chord) => DIM,
      Some(ToneTarget::Loop) => BRIGHT,
    };
    buttons.push((rt.overlays.tone_target_rect, target_level));
  }
  let (mx, my) = momentary_chords::chord_mode_cell(rect);
  let mode_level =
    if chord_mode == momentary_chords::ChordMode::Momentary { BRIGHT } else { OFF };
  buttons.push(([mx, my, mx, my], mode_level));
  let (ax, ay) = momentary_chords::arm_cell(rect);
  let arm_level = if armed && (elapsed.as_millis() / 200) % 2 == 0 {
    BRIGHT
  } else {
    OFF
  };
  buttons.push(([ax, ay, ax, ay], arm_level));
  for (slot, (is_populated, is_sounding)) in
    populated.into_iter().zip(sounding).enumerate()
  {
    let (x, y) = momentary_chords::slot_cell(rect, slot);
    let level = if is_sounding {
      BRIGHT
    } else if is_populated {
      DIM
    } else {
      OFF
    };
    buttons.push(([x, y, x, y], level));
  }
  (buttons, classes)
}

pub(super) fn compact_loop_view(
  rt: &GridThread,
  elapsed: Duration,
) -> (Vec<ButtonOverlay>, HashSet<i32>) {
  let rect = rt.overlays.compact_loop_rect;
  if rect == NO_RECT {
    return (Vec::new(), HashSet::new());
  }
  let flash = (elapsed.as_millis() / 200) % 2 == 0;
  let record_play_bright = (elapsed.as_millis() / 80) % 2 == 0;
  let mut buttons = Vec::new();
  for slot in 0..compact_loops::SLOTS {
    let (x, y) = compact_loops::slot_cell(rect, slot);
    let recording_and_playing = rt.compact_loops.recording()
      && slot == rt.compact_loops.target_loop_slot
      && rt.compact_loops.slot_sounding(slot);
    let level = if recording_and_playing {
      if record_play_bright {
        BRIGHT
      } else {
        STEADY_DIM
      }
    } else if slot == rt.compact_loops.target_loop_slot {
      if flash { BRIGHT } else { OFF }
    } else if rt.compact_loops.slot_sounding(slot) {
      BRIGHT
    } else if rt.compact_loops.slots[slot].is_some() {
      DIM
    } else {
      OFF
    };
    buttons.push(([x, y, x, y], level));
  }
  let (sx, sy) = compact_loops::start_cell(rect);
  buttons.push((
    [sx, sy, sx, sy],
    if rt.compact_loops.recording() && flash { BRIGHT } else { OFF },
  ));
  let (tx, ty) = compact_loops::stop_cell(rect);
  buttons.push(([tx, ty, tx, ty], if rt.compact_stop_down { BRIGHT } else { OFF }));
  let (qx, qy) = compact_loops::select_cell(rect);
  buttons.push((
    [qx, qy, qx, qy],
    if rt.compact_loops.selecting_target_loop && flash { BRIGHT } else { OFF },
  ));
  let (px, py) = compact_loops::phase_cell(rect);
  buttons.push((
    [px, py, px, py],
    if rt.compact_loops.phase_mode == compact_loops::PhaseMode::KeepRunning && flash {
      BRIGHT
    } else {
      OFF
    },
  ));
  let classes = rt
    .compact_loops
    .live_pitches()
    .into_iter()
    .map(|pitch| pitch.rem_euclid(rt.tuning.edo))
    .collect();
  (buttons, classes)
}

/// The absolute column of the active volume cell this grid should light (the *controlled*
/// grid's position, shown on this grid's strip), or -1 if this grid has no volume strip.
pub(super) fn volume_active_col(rt: &GridThread) -> i32 {
  if volume_cells(rt.overlays.volume_rect) <= 0 {
    return -1;
  }
  let pos = {
    let vp = rt.shared.volume_pos.lock().unwrap_or_else(|e| e.into_inner());
    vp.get(rt.knobs.volume_controls_index).copied().unwrap_or(0)
  };
  rt.overlays.volume_rect[0] + pos
}

/// Publish grid `index`'s currently-sounding pitch classes (register-independent: the
/// class of each held cell's *struck* pitch) for the other grid to reflect.
pub(super) fn publish_sounding(
  sounding: &Arc<Mutex<Vec<HashSet<i32>>>>,
  index: usize,
  held: &HashMap<(i32, i32), i32>,
  edo: i32,
) {
  let classes: HashSet<i32> = held.values().map(|p| p.rem_euclid(edo)).collect();
  let mut s = sounding.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(slot) = s.get_mut(index) {
    *slot = classes;
  }
}

pub(super) fn union_sounding(sounding: &Arc<Mutex<Vec<HashSet<i32>>>>) -> HashSet<i32> {
  let s = sounding.lock().unwrap_or_else(|e| e.into_inner());
  let mut u = HashSet::new();
  for set in s.iter() {
    u.extend(set.iter().copied());
  }
  u
}

pub(super) fn trail_set(trail: &Arc<Mutex<VecDeque<i32>>>) -> HashSet<i32> {
  let t = trail.lock().unwrap_or_else(|e| e.into_inner());
  t.iter().copied().collect()
}

/// Send a binary frame as changed 8x8 quads (`/grid/led/map`). `dim_on` selects whether
/// DIM cells are lit this sub-frame (the monobright fake-dim flash toggles it);
/// STEADY_DIM and BRIGHT are always on, and OFF is always off. Cheap: a whole
/// 16x16 frame is at most 4 messages, so this sustains the flash where per-cell
/// writes would swamp the serial link.
#[allow(clippy::too_many_arguments)]
pub(super) fn send_binary_frame(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  grid_w: i32,
  grid_h: i32,
  levels: &[i32],
  dim_on: bool,
  last: &mut Vec<[u8; 8]>,
) {
  let nqx = (grid_w + 7) / 8;
  let nqy = (grid_h + 7) / 8;
  let nq = (nqx * nqy) as usize;
  // On the first frame (or after a re-register clears it) the grid was just blanked, so
  // an all-zero baseline diffs correctly.
  if last.len() != nq {
    *last = vec![[0u8; 8]; nq];
  }
  for qy in 0..nqy {
    for qx in 0..nqx {
      let x_off = qx * 8;
      let y_off = qy * 8;
      let mut rows = [0u8; 8];
      for r in 0..8 {
        let y = y_off + r;
        if y >= grid_h {
          continue;
        }
        let mut byte = 0u8;
        for c in 0..8 {
          let x = x_off + c;
          if x >= grid_w {
            continue;
          }
          let level = levels[(y * grid_w + x) as usize];
          let on = level == BRIGHT || level == STEADY_DIM || (level == DIM && dim_on);
          if on {
            byte |= 1u8 << c;
          }
        }
        rows[r as usize] = byte;
      }
      let qi = (qy * nqx + qx) as usize;
      if last[qi] != rows {
        monome::send_led_map(sock, device, prefix, x_off, y_off, &rows);
        last[qi] = rows;
      }
    }
  }
}

/// Send only the cells whose level changed since `last`, then update `last`.
pub(super) fn send_diffs(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  grid_w: i32,
  levels: &[i32],
  last: &mut Vec<i32>,
) {
  for (i, &level) in levels.iter().enumerate() {
    let prev = last.get(i).copied().unwrap_or(-1);
    if prev != level {
      let x = (i as i32) % grid_w;
      let y = (i as i32) / grid_w;
      monome::send_led_level_set(sock, device, prefix, x, y, level);
    }
  }
  *last = levels.to_vec();
}

/// Blank a grid from an ephemeral socket (used by run() after the threads join, so a
/// panicked thread that skipped its own blank still leaves its grid dark).
pub(super) fn blank_grid(device_port: u16, prefix: &str) {
  if let (Ok(sock), Ok(addr)) = (
    UdpSocket::bind(("0.0.0.0", 0)),
    format!("127.0.0.1:{device_port}").parse::<SocketAddr>(),
  ) {
    monome::send_led_all(&sock, addr, prefix, 0);
  }
}
