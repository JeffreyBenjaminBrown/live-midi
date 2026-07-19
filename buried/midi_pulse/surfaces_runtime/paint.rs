//! The LED painting path: the per-grid button/sounding-class views fed to
//! `grid::levels_for_grid` (`accrete_view`, `union_sounding`, `publish_sounding`,
//! `trail_set`, `volume_active_col`), and sending the computed levels to the
//! device (`send_binary_frame`, `send_diffs`, `blank_grid`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};

use midi_pulse::monome;

use super::grid::{button_level, volume_cells, ButtonOverlay, BRIGHT, DIM};
use super::ring::Reason;
use super::GridThread;

/// One lock: this grid's accrete-trio LED view (its OWN bank's state) plus the
/// union of every grid's sustained pitch classes (which paint bright on every
/// grid -- they are all sounding, like the cross-grid note reflection).
pub(super) fn accrete_view(rt: &GridThread) -> (Vec<ButtonOverlay>, HashSet<i32>) {
  let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
  let s = &rings[rt.grid_index].accrete;
  let buttons = vec![
    (rt.overlays.clear_rect, button_level(s.clear_lit())),
    (rt.overlays.needs_holding_rect, button_level(s.needs_holding_lit())),
    (rt.overlays.accrete_rect, button_level(s.accrete_lit())),
    (rt.overlays.erase_rect, button_level(s.erase_lit())),
  ];
  let mut classes = HashSet::new();
  for gr in rings.iter() {
    classes.extend(gr.store.classes(Reason::Sustain, rt.tuning.edo));
  }
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
/// DIM cells are lit this sub-frame (the monobright fake-dim flash toggles it); BRIGHT is
/// always on, OFF always off. Cheap: a whole 16x16 frame is at most 4 messages, so this
/// sustains the flash where per-cell writes would swamp the serial link.
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
          let on = level == BRIGHT || (level == DIM && dim_on);
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
