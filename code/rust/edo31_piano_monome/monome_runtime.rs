use midi_pulse::monome;
use rosc::{decoder, OscPacket, OscType};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::layout::{window_for_cell, WindowId};
use crate::remap::{apply_grid_press, undo_remap};
use crate::render::{
  blank_rendered_cols, led_phases, next_render_wait, render_to_monome, ColorClock, ANCHOR_COLOR,
  IMAGE_COLOR, SOUNDING_COLOR,
};
use crate::state::{Edo31State, SoundingState};
use crate::{PREFIX, STOP_REQUESTED};

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
  render_to_monome(
    &sock,
    device,
    &state.lock().unwrap(),
    &sounding.lock().unwrap().counts,
    led_phases(sounding_clock, anchor_clock, image_clock),
    &mut rendered_cols,
  );
  let key_addr = format!("{PREFIX}/grid/key");
  let mut buf = [0u8; 2048];
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    let now = Instant::now();
    let mut dirty = false;
    dirty |= sounding_clock.advance_if_due(now);
    dirty |= anchor_clock.advance_if_due(now);
    dirty |= image_clock.advance_if_due(now);
    sock
      .set_read_timeout(Some(next_render_wait(
        now,
        sounding_clock,
        anchor_clock,
        image_clock,
      )))
      .unwrap();
    render_to_monome(
      &sock,
      device,
      &state.lock().unwrap(),
      &sounding.lock().unwrap().counts,
      led_phases(sounding_clock, anchor_clock, image_clock),
      &mut rendered_cols,
    );
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
          render_to_monome(
            &sock,
            device,
            &state.lock().unwrap(),
            &sounding.lock().unwrap().counts,
            led_phases(sounding_clock, anchor_clock, image_clock),
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
    if s != 1 {
      continue;
    }
    let mut state = state.lock().unwrap();
    if apply_monome_press(&mut state, x, y) {
      dirty = true;
    }
    if dirty {
      render_to_monome(
        &sock,
        device,
        &state,
        &sounding.lock().unwrap().counts,
        led_phases(sounding_clock, anchor_clock, image_clock),
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
