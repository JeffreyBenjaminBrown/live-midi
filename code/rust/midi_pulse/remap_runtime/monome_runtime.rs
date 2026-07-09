use midi_pulse::monome;
use rosc::{decoder, OscPacket, OscType};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::record::{RecordRuntime, SharedOutputGate};
use super::render::{
  blank_rendered_cols, led_phases, next_render_wait, render_to_monome, ColorClock, ANCHOR_COLOR,
  IMAGE_COLOR, SOUNDING_COLOR, PREIMAGE_ROW_FLASH_COLOR,
};
use super::state::{PreimageRowState, RemappableEdoState, SoundingPitchCounts};
use super::window_behavior::{behavior_for_cell, KeyContext, WindowBehavior};
use super::STOP_REQUESTED;

pub(crate) fn run_monome_thread(
  state: Arc<Mutex<RemappableEdoState>>,
  sounding: Arc<Mutex<SoundingPitchCounts>>,
  recorder: Arc<Mutex<RecordRuntime>>,
  output_gate: SharedOutputGate,
  listen_port: u16,
  prefix: String,
  select_size: Option<[i32; 2]>,
) {
  let sock = UdpSocket::bind(("0.0.0.0", listen_port))
    .unwrap_or_else(|e| panic!("bind UDP :{listen_port}: {e}"));
  sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
  let mut device_info = discover_configured_device(&sock, listen_port, select_size)
    .expect("no monome found; is serialoscd running?");
  {
    let mut state = state.lock().unwrap();
    state.rig = state
      .rig
      .with_grid_size(device_info.grid_w, device_info.grid_h);
  }
  eprintln!(
    "monome: id={} type={} port={} size={}x{}",
    device_info.id, device_info.type_name, device_info.port, device_info.grid_w, device_info.grid_h,
  );
  let mut device: SocketAddr = format!("127.0.0.1:{}", device_info.port).parse().unwrap();
  monome::register(&sock, device, &prefix, listen_port);
  let mut rendered_cols = blank_rendered_cols(&state.lock().unwrap().rig);
  let mut sounding_clock = ColorClock::new(SOUNDING_COLOR, Instant::now());
  let mut anchor_clock = ColorClock::new(ANCHOR_COLOR, Instant::now());
  let mut image_clock = ColorClock::new(IMAGE_COLOR, Instant::now());
  let mut preimage_row_flash_clock = ColorClock::new(PREIMAGE_ROW_FLASH_COLOR, Instant::now());
  let mut preimage_row = PreimageRowState::new();
  let now = Instant::now();
  let state_guard = state.lock().unwrap();
  let sounding_guard = sounding.lock().unwrap();
  let recorder_guard = recorder.lock().unwrap();
  render_to_monome(
    &sock,
    device,
    &state_guard,
    &sounding_guard,
    &recorder_guard,
    &preimage_row.counts,
    &preimage_row.flash_until,
    now,
    &prefix,
    led_phases(
      sounding_clock,
      anchor_clock,
      image_clock,
      preimage_row_flash_clock,
    ),
    &mut rendered_cols,
  );
  drop(recorder_guard);
  drop(sounding_guard);
  drop(state_guard);
  let key_addr = format!("{prefix}/grid/key");
  let mut buf = [0u8; 2048];
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    let now = Instant::now();
    let mut dirty = false;
    dirty |= sounding_clock.advance_if_due(now);
    dirty |= anchor_clock.advance_if_due(now);
    dirty |= image_clock.advance_if_due(now);
    dirty |= preimage_row_flash_clock.advance_if_due(now);
    {
      let mut recorder_guard = recorder.lock().unwrap();
      if recorder_guard.erase_ons_held {
        if let Some(elapsed) = recorder_guard
          .playback_start
          .and_then(|start| recorder_guard.slot.loop_duration.map(|duration| (start, duration)))
          .filter(|(_, duration)| !duration.is_zero())
          .map(|(start, duration)| {
            Duration::from_nanos(
              (now.duration_since(start).as_nanos() % duration.as_nanos()) as u64,
            )
          })
        {
          recorder_guard.erase_swept_to(elapsed);
        }
      }
    }
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
    let recorder_guard = recorder.lock().unwrap();
    render_to_monome(
      &sock,
      device,
      &state_guard,
      &sounding_guard,
      &recorder_guard,
      &preimage_row.counts,
      &preimage_row.flash_until,
      now,
      &prefix,
      led_phases(
        sounding_clock,
        anchor_clock,
        image_clock,
        preimage_row_flash_clock,
      ),
      &mut rendered_cols,
    );
    drop(recorder_guard);
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
          monome::register(&sock, device, &prefix, listen_port);
          rendered_cols = blank_rendered_cols(&state.lock().unwrap().rig);
          let now = Instant::now();
          let state_guard = state.lock().unwrap();
          let sounding_guard = sounding.lock().unwrap();
          let recorder_guard = recorder.lock().unwrap();
          render_to_monome(
            &sock,
            device,
            &state_guard,
            &sounding_guard,
            &recorder_guard,
            &preimage_row.counts,
            &preimage_row.flash_until,
            now,
            &prefix,
            led_phases(
              sounding_clock,
              anchor_clock,
              image_clock,
              preimage_row_flash_clock,
            ),
            &mut rendered_cols,
          );
          drop(recorder_guard);
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
    let mut recorder_guard = recorder.lock().unwrap();
    if apply_monome_key(
      &mut state,
      &mut recorder_guard,
      &output_gate,
      &mut preimage_row,
      x,
      y,
      s,
      Instant::now(),
    ) {
      dirty = true;
    }
    drop(recorder_guard);
    if dirty {
      let sounding_guard = sounding.lock().unwrap();
      let recorder_guard = recorder.lock().unwrap();
      render_to_monome(
        &sock,
        device,
        &state,
        &sounding_guard,
        &recorder_guard,
        &preimage_row.counts,
        &preimage_row.flash_until,
        Instant::now(),
        &prefix,
        led_phases(
          sounding_clock,
          anchor_clock,
          image_clock,
          preimage_row_flash_clock,
        ),
        &mut rendered_cols,
      );
      drop(recorder_guard);
    }
  }
  monome::send_led_all(&sock, device, &prefix, 0);
}

fn discover_configured_device(
  sock: &UdpSocket,
  listen_port: u16,
  select_size: Option<[i32; 2]>,
) -> Option<midi_pulse::monome::DeviceInfo> {
  let devices = monome::discover_devices(sock, listen_port);
  let selected = devices.iter().rev().find(|device| {
    select_size.is_none_or(|[w, h]| device.grid_w == w && device.grid_h == h)
  });
  if selected.is_none() && !devices.is_empty() {
    eprintln!(
      "no monome matched configured size {:?}; serialosc devices were: {:?}",
      select_size,
      devices
        .iter()
        .map(|device| format!("{} {} {}x{}", device.id, device.type_name, device.grid_w, device.grid_h))
        .collect::<Vec<_>>()
    );
  }
  selected.cloned()
}

pub(crate) fn apply_monome_press(state: &mut RemappableEdoState, x: i32, y: i32) -> bool {
  let (tx, _rx) = std::sync::mpsc::channel();
  let output_gate = SharedOutputGate::new(tx);
  let mut recorder = RecordRuntime::new();
  let mut preimage_row = PreimageRowState::new();
  apply_monome_key(
    state,
    &mut recorder,
    &output_gate,
    &mut preimage_row,
    x,
    y,
    1,
    Instant::now(),
  )
}

pub(crate) fn apply_monome_key(
  state: &mut RemappableEdoState,
  recorder: &mut RecordRuntime,
  output_gate: &SharedOutputGate,
  preimage_row: &mut PreimageRowState,
  x: i32,
  y: i32,
  s: i32,
  now: Instant,
) -> bool {
  let Some(behavior) = behavior_for_cell(&state.rig, x, y) else {
    return if s == 0 {
      preimage_row.release((x, y))
    } else {
      false
    };
  };
  let mut ctx = KeyContext {
    state,
    recorder,
    output_gate,
    preimage_row,
    now,
  };
  if s == 0 {
    return behavior.key_up(&mut ctx, x, y);
  }
  if s != 1 {
    return false;
  }
  behavior.key_down(&mut ctx, x, y)
}

#[cfg(test)]
mod tests {
  use super::*;
  use super::super::rig::{RemapRig, RemapIdiom};
  use std::sync::mpsc;

  fn test_state() -> RemappableEdoState {
    RemappableEdoState::new(RemapRig::new(
      80.0,
      12,
      1,
      0,
      RemapIdiom::Snap,
      16,
      8,
    ))
  }

  #[test]
  fn coordinate_dispatch_reaches_recording_control() {
    let (tx, _rx) = mpsc::channel();
    let gate = SharedOutputGate::new(tx);
    let mut state = test_state();
    let mut recorder = RecordRuntime::new();
    let mut preimage_row = PreimageRowState::new();

    assert!(apply_monome_key(
      &mut state,
      &mut recorder,
      &gate,
      &mut preimage_row,
      13,
      1,
      1,
      Instant::now(),
    ));

    assert!(recorder.armed);
  }

  #[test]
  fn dispatch_reaches_scale_controls_and_slots() {
    use super::super::scale::ScaleControl;
    let (tx, _rx) = mpsc::channel();
    let gate = SharedOutputGate::new(tx);
    let rig = RemapRig::new(80.0, 12, 1, 0, RemapIdiom::Snap, 16, 8)
      .with_scale_slots(Some([12, 0, 15, 3]))
      .with_scale_controls(vec![
        (ScaleControl::Store, (15, 4)),
        (ScaleControl::Empty, (14, 4)),
      ]);
    let mut state = RemappableEdoState::new(rig);
    let mut recorder = RecordRuntime::new();
    let mut preimage_row = PreimageRowState::new();

    // Press the store arm button at (15, 4).
    assert!(apply_monome_key(
      &mut state, &mut recorder, &gate, &mut preimage_row, 15, 4, 1, Instant::now(),
    ));
    assert_eq!(state.scale.armed, Some(ScaleControl::Store));

    // Press slot 0 at (12, 0): stores the current scale and disarms.
    assert!(apply_monome_key(
      &mut state, &mut recorder, &gate, &mut preimage_row, 12, 0, 1, Instant::now(),
    ));
    assert!(state.scale.slots[0].is_some());
    assert_eq!(state.scale.armed, None);
  }

  #[test]
  fn record_controls_do_not_invoke_edo_remap_actions() {
    let (tx, _rx) = mpsc::channel();
    let gate = SharedOutputGate::new(tx);
    let mut state = test_state();
    let before = state.snapshot();
    let mut recorder = RecordRuntime::new();
    let mut preimage_row = PreimageRowState::new();

    apply_monome_key(
      &mut state,
      &mut recorder,
      &gate,
      &mut preimage_row,
      13,
      0,
      1,
      Instant::now(),
    );

    assert_eq!(state.snapshot(), before);
    assert!(preimage_row.active_by_cell.is_empty());
  }
}
