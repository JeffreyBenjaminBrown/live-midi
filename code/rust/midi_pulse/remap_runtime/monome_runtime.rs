use midi_pulse::monome;
use rosc::{decoder, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::layout::{edo_local_cell, grid_step, window_for_cell, WindowId};
use super::remap::{apply_grid_press, preimage_for_step, undo_remap};
use super::render::{
  blank_rendered_cols, led_phases, next_render_wait, render_to_monome, ColorClock, ANCHOR_COLOR,
  IMAGE_COLOR, SOUNDING_COLOR, PREIMAGE_ROW_FLASH_COLOR,
};
use super::state::{Edo31State, SoundingState};
use super::{PREFIX, STOP_REQUESTED, PREIMAGE_ROW_FLASH_MIN};

pub(crate) struct PreimageRowState {
  pub(crate) active_by_cell: HashMap<(i32, i32), usize>,
  pub(crate) counts: [u16; 12],
  pub(crate) flash_until: [Option<Instant>; 12],
}

impl PreimageRowState {
  pub(crate) fn new() -> Self {
    PreimageRowState {
      active_by_cell: HashMap::new(),
      counts: [0; 12],
      flash_until: [None; 12],
    }
  }

  fn press(&mut self, cell: (i32, i32), preimage: usize, now: Instant) {
    if let Some(old_preimage) = self.active_by_cell.insert(cell, preimage) {
      decrement_count(&mut self.counts[old_preimage]);
    }
    self.counts[preimage] += 1;
    self.flash_until[preimage] = Some(now + PREIMAGE_ROW_FLASH_MIN);
  }

  fn release(&mut self, cell: (i32, i32)) -> bool {
    let Some(preimage) = self.active_by_cell.remove(&cell) else {
      return false;
    };
    decrement_count(&mut self.counts[preimage]);
    true
  }
}

pub(crate) fn run_monome_thread(
  state: Arc<Mutex<Edo31State>>,
  sounding: Arc<Mutex<SoundingState>>,
  listen_port: u16,
) {
  let sock = UdpSocket::bind(("0.0.0.0", listen_port))
    .unwrap_or_else(|e| panic!("bind UDP :{listen_port}: {e}"));
  sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
  let mut device_info = monome::discover_device_info(&sock, listen_port)
    .expect("no monome found; is serialoscd running?");
  {
    let mut state = state.lock().unwrap();
    state.config = state
      .config
      .with_grid_size(device_info.grid_w, device_info.grid_h);
  }
  eprintln!(
    "monome: id={} type={} port={} size={}x{}",
    device_info.id, device_info.type_name, device_info.port, device_info.grid_w, device_info.grid_h,
  );
  let mut device: SocketAddr = format!("127.0.0.1:{}", device_info.port).parse().unwrap();
  monome::register(&sock, device, PREFIX, listen_port);
  let mut rendered_cols = blank_rendered_cols(&state.lock().unwrap().config);
  let mut sounding_clock = ColorClock::new(SOUNDING_COLOR, Instant::now());
  let mut anchor_clock = ColorClock::new(ANCHOR_COLOR, Instant::now());
  let mut image_clock = ColorClock::new(IMAGE_COLOR, Instant::now());
  let mut preimage_row_flash_clock = ColorClock::new(PREIMAGE_ROW_FLASH_COLOR, Instant::now());
  let mut preimage_row = PreimageRowState::new();
  let now = Instant::now();
  let state_guard = state.lock().unwrap();
  let sounding_guard = sounding.lock().unwrap();
  render_to_monome(
    &sock,
    device,
    &state_guard,
    &sounding_guard.counts,
    &preimage_row.counts,
    &preimage_row.flash_until,
    now,
    led_phases(
      sounding_clock,
      anchor_clock,
      image_clock,
      preimage_row_flash_clock,
    ),
    &mut rendered_cols,
  );
  drop(sounding_guard);
  drop(state_guard);
  let key_addr = format!("{PREFIX}/grid/key");
  let mut buf = [0u8; 2048];
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    let now = Instant::now();
    let mut dirty = false;
    dirty |= sounding_clock.advance_if_due(now);
    dirty |= anchor_clock.advance_if_due(now);
    dirty |= image_clock.advance_if_due(now);
    dirty |= preimage_row_flash_clock.advance_if_due(now);
    let sounding_guard = sounding.lock().unwrap();
    sock
      .set_read_timeout(Some(next_render_wait(
        now,
        sounding_clock,
        anchor_clock,
        image_clock,
        preimage_row_flash_clock,
        &preimage_row.flash_until,
      )))
      .unwrap();
    drop(sounding_guard);
    let state_guard = state.lock().unwrap();
    let sounding_guard = sounding.lock().unwrap();
    render_to_monome(
      &sock,
      device,
      &state_guard,
      &sounding_guard.counts,
      &preimage_row.counts,
      &preimage_row.flash_until,
      now,
      led_phases(
        sounding_clock,
        anchor_clock,
        image_clock,
        preimage_row_flash_clock,
      ),
      &mut rendered_cols,
    );
    drop(sounding_guard);
    drop(state_guard);
    let pkt = match sock.recv_from(&mut buf) {
      Ok((n, _)) => match decoder::decode_udp(&buf[..n]) {
        Ok((_, p)) => p,
        Err(_) => continue,
      },
      Err(_) => continue,
    };
    let OscPacket::Message(m) = pkt else {
      continue;
    };
    if m.addr == "/serialosc/device" && m.args.len() >= 3 {
      if let Some(OscType::Int(p)) = m.args.get(2) {
        let p = *p as u16;
        if p != device_info.port {
          device_info.port = p;
          device = format!("127.0.0.1:{p}").parse().unwrap();
          monome::register(&sock, device, PREFIX, listen_port);
          rendered_cols = blank_rendered_cols(&state.lock().unwrap().config);
          let now = Instant::now();
          let state_guard = state.lock().unwrap();
          let sounding_guard = sounding.lock().unwrap();
          render_to_monome(
            &sock,
            device,
            &state_guard,
            &sounding_guard.counts,
            &preimage_row.counts,
            &preimage_row.flash_until,
            now,
            led_phases(
              sounding_clock,
              anchor_clock,
              image_clock,
              preimage_row_flash_clock,
            ),
            &mut rendered_cols,
          );
        }
      }
      continue;
    }
    if m.addr != key_addr || m.args.len() != 3 {
      continue;
    }
    let (x, y, s) = match (m.args.first(), m.args.get(1), m.args.get(2)) {
      (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(s))) => (*x, *y, *s),
      _ => continue,
    };
    let mut state = state.lock().unwrap();
    if apply_monome_key(&mut state, &mut preimage_row, x, y, s, Instant::now()) {
      dirty = true;
    }
    if dirty {
      let sounding_guard = sounding.lock().unwrap();
      render_to_monome(
        &sock,
        device,
        &state,
        &sounding_guard.counts,
        &preimage_row.counts,
        &preimage_row.flash_until,
        Instant::now(),
        led_phases(
          sounding_clock,
          anchor_clock,
          image_clock,
          preimage_row_flash_clock,
        ),
        &mut rendered_cols,
      );
    }
  }
  monome::send_led_all(&sock, device, PREFIX, 0);
}

pub(crate) fn apply_monome_press(state: &mut Edo31State, x: i32, y: i32) -> bool {
  match window_for_cell(&state.config, x, y) {
    Some(WindowId::Undo) => undo_remap(state),
    Some(WindowId::Edo) => apply_grid_press(state, x, y),
    None => false,
  }
}

pub(crate) fn apply_monome_key(
  state: &mut Edo31State,
  preimage_row: &mut PreimageRowState,
  x: i32,
  y: i32,
  s: i32,
  now: Instant,
) -> bool {
  if s == 0 {
    return preimage_row.release((x, y));
  }
  if s != 1 {
    return false;
  }
  match window_for_cell(&state.config, x, y) {
    Some(WindowId::Undo) => undo_remap(state),
    Some(WindowId::Edo) => apply_edo_key_down(state, preimage_row, x, y, now),
    None => false,
  }
}

fn apply_edo_key_down(
  state: &mut Edo31State,
  preimage_row: &mut PreimageRowState,
  x: i32,
  y: i32,
  now: Instant,
) -> bool {
  let Some((local_x, local_y)) = edo_local_cell(&state.config, x, y) else {
    return false;
  };
  let step = grid_step(&state.config, local_x, local_y);
  let preimage_before = preimage_for_step(state, step);
  let changed = apply_grid_press(state, x, y);
  let preimage_row_preimage = preimage_before.or_else(|| preimage_for_step(state, step));
  if let Some(preimage) = preimage_row_preimage {
    preimage_row.press((x, y), preimage, now);
    true
  } else {
    changed
  }
}

fn decrement_count(count: &mut u16) {
  if *count > 0 {
    *count -= 1;
  }
}
