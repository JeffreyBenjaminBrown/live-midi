//! The two-monome looper runtime.
//!
//! Phase 1: discover and bind both grids, open the audio stream, and run one
//! event-loop thread per grid over a shared `LooperState`. The edo grid plays
//! through the pitch-keyed `NoteSink` and reflects sounding pitches; the loops
//! grid is blank for now. Pure key/LED logic lives in `state.rs` (unit-tested);
//! this file is the thin I/O shell (sockets, cpal, threads, teardown).
//!
//! Lock discipline: a grid thread locks `LooperState` only to apply a key and
//! compute an LED vector (both fast, no I/O), then drops it before the UDP LED
//! sends. The audio thread locks only `voices`. So the only nesting is
//! control -> voices (brief), and the realtime thread never waits on control.

use midi_pulse::config::{Config, MonomeWindowConfig, SinkConfig};
use midi_pulse::monome;
use rosc::{decoder, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod audio;
mod device;
#[allow(dead_code)]
mod edo;
#[allow(dead_code)]
mod loop_store;
// Slot / release_source are used by the Phase 2 playback path (next).
#[allow(dead_code)]
mod sink;
mod state;

use state::{Grid, LooperState};

static STOP: AtomicBool = AtomicBool::new(false);

pub fn run_from_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  print_inventory(config);
  run(config)
}

fn print_inventory(config: &Config) {
  println!(
    "looper: {} windows across {} monomes",
    config.monome_windows.len(),
    config.monomes.len(),
  );
  for monome in &config.monomes {
    println!("  monome {:?} (port {}, prefix {:?}):", monome.id, monome.listen_port, monome.prefix);
    for window in config.monome_windows.iter().filter(|w| w.monome() == monome.id) {
      println!("    {:<22} id {:?} rect {:?}", window.kind_name(), window.id(), window.rect());
    }
  }
  if let Some(looper) = &config.looper {
    println!(
      "  [looper] quantize_record_ms={} cluster_display_ms={} flash_ms={} remap_center={:?}",
      looper.quantize_record_ms, looper.cluster_display_ms, looper.flash_ms, looper.remap_center,
    );
  }
}

struct Settings {
  edo_monome: String,
  loops_monome: String,
  edo_listen_port: u16,
  loops_listen_port: u16,
  edo_prefix: String,
  loops_prefix: String,
  size: [i32; 2],
  grid_w: i32,
  grid_h: i32,
  edo_rect: [i32; 4],
  shift_rect: [i32; 4],
  x_step: i32,
  y_step: i32,
  edo: i32,
  fund: f64,
  sample_rate: u32,
  buffer_frames: u32,
  amplitude: f32,
  attack: f32,
  release: f32,
}

fn resolve_settings(config: &Config) -> Result<Settings, Box<dyn std::error::Error>> {
  let (edo_monome, tuning_id, sink_id, edo_rect) = config
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowConfig::EdoNoteGrid { monome, tuning, sink, rect, .. } => {
        Some((monome.clone(), tuning.clone(), sink.clone(), *rect))
      }
      _ => None,
    })
    .ok_or("looper needs an edo_note_grid window")?;
  // The shift pad on the edo monome (optional); an impossible rect means "none".
  let shift_rect = config
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowConfig::EdoShiftPad { monome, rect, .. } if *monome == edo_monome => Some(*rect),
      _ => None,
    })
    .unwrap_or([-1, -1, -1, -1]);
  let loops_monome = config
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowConfig::LoopDisplay { monome, .. } => Some(monome.clone()),
      _ => None,
    })
    .ok_or("looper needs a loop_display window")?;
  let tuning = config
    .tunings
    .iter()
    .find(|t| t.id == tuning_id)
    .ok_or("edo_note_grid references an unknown tuning")?;
  let sink = config
    .sinks
    .iter()
    .find(|s| s.id() == sink_id)
    .ok_or("edo_note_grid references an unknown sink")?;
  let SinkConfig::CpalSawwave { sample_rate, buffer_frames, amplitude, attack_secs, release_secs, .. } = sink
  else {
    return Err("looper requires a cpal_sawwave sink".into());
  };
  let edo_cfg = config
    .monomes
    .iter()
    .find(|m| m.id == edo_monome)
    .ok_or("the edo monome is not declared")?;
  let loops_cfg = config
    .monomes
    .iter()
    .find(|m| m.id == loops_monome)
    .ok_or("the loops monome is not declared")?;
  let size = edo_cfg.select.size.unwrap_or([16, 16]);
  Ok(Settings {
    edo_monome,
    loops_monome,
    edo_listen_port: edo_cfg.listen_port,
    loops_listen_port: loops_cfg.listen_port,
    edo_prefix: edo_cfg.prefix.clone(),
    loops_prefix: loops_cfg.prefix.clone(),
    size,
    grid_w: size[0],
    grid_h: size[1],
    edo_rect,
    shift_rect,
    x_step: tuning.x_step as i32,
    y_step: tuning.y_step as i32,
    edo: tuning.edo as i32,
    fund: tuning.fundamental_hz,
    sample_rate: *sample_rate,
    buffer_frames: *buffer_frames,
    amplitude: *amplitude,
    attack: *attack_secs,
    release: *release_secs,
  })
}

fn run(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  let s = resolve_settings(config)?;

  // Discover on a socket bound to the edo grid's listen port; reuse it as the edo
  // thread's socket (same port).
  let edo_sock = UdpSocket::bind(("0.0.0.0", s.edo_listen_port))
    .map_err(|e| format!("bind UDP :{}: {e}", s.edo_listen_port))?;
  edo_sock.set_read_timeout(Some(Duration::from_millis(50)))?;
  let devices = monome::discover_devices(&edo_sock, s.edo_listen_port);
  let assigned = device::assign_distinct_devices(&devices, s.size, config.monomes.len())?;

  // Pair each config monome with its assigned grid, then pick edo / loops by id.
  let pairs: Vec<(&str, &monome::DeviceInfo)> =
    config.monomes.iter().map(|m| m.id.as_str()).zip(assigned.iter()).collect();
  let find = |id: &str| pairs.iter().find(|(mid, _)| *mid == id).map(|(_, d)| *d);
  let edo_dev = find(&s.edo_monome).ok_or("edo monome was not assigned a grid")?;
  let loops_dev = find(&s.loops_monome).ok_or("loops monome was not assigned a grid")?;
  let edo_dev_port = edo_dev.port;
  let loops_dev_port = loops_dev.port;
  println!(
    "looper: edo -> id={:?} port={}; loops -> id={:?} port={}",
    edo_dev.id, edo_dev_port, loops_dev.id, loops_dev_port,
  );

  let loops_sock = UdpSocket::bind(("0.0.0.0", s.loops_listen_port))
    .map_err(|e| format!("bind UDP :{}: {e}", s.loops_listen_port))?;
  loops_sock.set_read_timeout(Some(Duration::from_millis(50)))?;

  // Audio + the note sink (both share one voice map).
  let voices = Arc::new(Mutex::new(HashMap::new()));
  let audio = audio::start(Arc::clone(&voices), s.sample_rate, s.buffer_frames, s.amplitude)?;
  let looper_sink =
    sink::SawNoteSink::new(Arc::clone(&voices), s.fund, s.edo, audio.sample_rate, s.attack, s.release);
  let looper_state = Arc::new(Mutex::new(LooperState::new(
    looper_sink, s.x_step, s.y_step, s.edo, s.grid_w, s.grid_h, s.edo_rect, s.shift_rect,
  )));

  install_sigint();
  println!("looper running; Ctrl-C to exit.");

  let grid_w = s.grid_w;
  let (edo_prefix, loops_prefix) = (s.edo_prefix.clone(), s.loops_prefix.clone());
  let (edo_listen, loops_listen) = (s.edo_listen_port, s.loops_listen_port);
  let st_edo = Arc::clone(&looper_state);
  let st_loops = Arc::clone(&looper_state);
  let edo_thread = thread::spawn(move || {
    grid_thread(Grid::Edo, edo_sock, edo_prefix, edo_listen, edo_dev_port, grid_w, st_edo);
  });
  let loops_thread = thread::spawn(move || {
    grid_thread(Grid::Loops, loops_sock, loops_prefix, loops_listen, loops_dev_port, grid_w, st_loops);
  });
  let _ = edo_thread.join();
  let _ = loops_thread.join();
  drop(audio);
  println!("looper stopped.");
  Ok(())
}

fn install_sigint() {
  extern "C" fn on_sigint(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
  }
  unsafe {
    libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
  }
}

#[allow(clippy::too_many_arguments)]
fn grid_thread(
  role: Grid,
  sock: UdpSocket,
  prefix: String,
  listen_port: u16,
  mut device_port: u16,
  grid_w: i32,
  state: Arc<Mutex<LooperState>>,
) {
  let Ok(mut device) = format!("127.0.0.1:{device_port}").parse::<SocketAddr>() else {
    return;
  };
  monome::register(&sock, device, &prefix, listen_port);
  let key_addr = format!("{prefix}/grid/key");
  let mut last: Vec<i32> = vec![];
  paint(role, &sock, device, &prefix, grid_w, &state, &mut last);

  let mut buf = [0u8; 2048];
  while !STOP.load(Ordering::SeqCst) {
    let n = match sock.recv_from(&mut buf) {
      Ok((n, _)) => n,
      Err(_) => continue, // timeout: re-check STOP.
    };
    let msg = match decoder::decode_udp(&buf[..n]) {
      Ok((_, OscPacket::Message(m))) => m,
      _ => continue,
    };
    // serialosc may re-announce the device on a new port (ghost re-enumeration).
    if msg.addr == "/serialosc/device" && msg.args.len() >= 3 {
      if let Some(OscType::Int(port)) = msg.args.get(2) {
        let port = *port as u16;
        if port != device_port {
          device_port = port;
          if let Ok(addr) = format!("127.0.0.1:{port}").parse::<SocketAddr>() {
            device = addr;
          }
          monome::register(&sock, device, &prefix, listen_port);
          last.clear();
          paint(role, &sock, device, &prefix, grid_w, &state, &mut last);
        }
      }
      continue;
    }
    if msg.addr != key_addr || msg.args.len() != 3 {
      continue;
    }
    let (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(down))) =
      (msg.args.first(), msg.args.get(1), msg.args.get(2))
    else {
      continue;
    };
    if role == Grid::Edo {
      let result = {
        let mut st = state.lock().unwrap();
        if st.edo_key(*x, *y, *down == 1) {
          Some((st.edo_levels(), st.live_pitch_count()))
        } else {
          None
        }
      };
      if let Some((levels, sounding)) = result {
        eprintln!("[edo] ({x},{y}) {} -> {sounding} sounding", if *down == 1 { "v" } else { "^" });
        send_diffs(&sock, device, &prefix, grid_w, &levels, &mut last);
      }
    }
    // The loops grid ignores keys in Phase 1.
  }
  monome::send_led_all(&sock, device, &prefix, 0);
}

fn paint(
  role: Grid,
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  grid_w: i32,
  state: &Arc<Mutex<LooperState>>,
  last: &mut Vec<i32>,
) {
  let levels = {
    let st = state.lock().unwrap();
    match role {
      Grid::Edo => st.edo_levels(),
      Grid::Loops => st.loops_levels(),
    }
  };
  send_diffs(sock, device, prefix, grid_w, &levels, last);
}

/// Send only the cells whose level changed since `last`, then update `last`.
fn send_diffs(
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
