//! The two-monome looper runtime.
//!
//! Discover and bind both grids, open the audio stream, and run three threads
//! over a shared `LooperState`: one event loop per grid plus a playback ticker.
//! The edo grid plays through the pitch-keyed `NoteSink` and (while recording)
//! feeds the loop store; the loops grid selects the active slot and drives the
//! transport; the playback thread advances the sounding slot into the sink. Both
//! grid threads repaint every iteration, so a state change from any thread shows
//! on both grids within one tick. Pure key/LED/playback logic lives in `state.rs`
//! (unit-tested); this file is the I/O shell.
//!
//! Lock discipline: a thread holds the control lock only to mutate state and/or
//! compute an LED vector (no I/O), then drops it before UDP sends. The audio
//! thread locks only `voices`. control -> voices is the single, brief nesting.

use midi_pulse::config::{
  AmShapeFamilyConfig, Config, LoopControlKind, MonomeWindowConfig, RowRangeConfig, SinkConfig,
};
use crate::types::AmShapeFamily;
use midi_pulse::monome;
use rosc::{decoder, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod audio;
mod device;
mod edo;
#[allow(dead_code)]
mod display;
#[allow(dead_code)]
mod loop_store;
#[allow(dead_code)]
mod remap;
mod sink;
mod state;
mod timbre_editor;
mod timbre_rows;

use state::{Grid, LooperParams, LooperState};
use timbre_editor::TimbreEditor;
use timbre_rows::RowRange;

static STOP: AtomicBool = AtomicBool::new(false);

/// Dropped at the end of every worker thread -- clean return, early return, or
/// panic unwind. Setting STOP releases the sibling threads from their loops, so
/// one thread's death tears the whole runtime down instead of leaving a zombie.
struct StopOnExit;
impl Drop for StopOnExit {
  fn drop(&mut self) {
    STOP.store(true, Ordering::SeqCst);
  }
}

/// Blank a grid from an ephemeral socket (used by run() after the threads join, so
/// a panicked thread that skipped its own blank still leaves its grid dark).
fn blank_grid(device_port: u16, prefix: &str) {
  if let (Ok(sock), Ok(addr)) = (
    UdpSocket::bind(("0.0.0.0", 0)),
    format!("127.0.0.1:{device_port}").parse::<SocketAddr>(),
  ) {
    monome::send_led_all(&sock, addr, prefix, 0);
  }
}

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
  loop_slots_rect: [i32; 4],
  loop_start: (i32, i32),
  loop_stop: (i32, i32),
  loop_play: (i32, i32),
  loop_display_rect: [i32; 4],
  loop_toggle: (i32, i32),
  loop_copy: (i32, i32),
  loop_undo: (i32, i32),
  remap_center: (i32, i32),
  x_step: i32,
  y_step: i32,
  edo: i32,
  fund: f64,
  sample_rate: u32,
  buffer_frames: u32,
  amplitude: f32,
  attack: f32,
  release: f32,
  flash_ms: u64,
  quantize: Duration,
  cluster: Duration,
  am_shape_family: AmShapeFamily,
  /// The timbre editor on the edo monome, if configured (C3b occludes the edo
  /// grid). A loops-monome live editor arrives with the reflow in C5c.
  timbre_editor: Option<TimbreEditor>,
}

fn to_row_range(c: RowRangeConfig) -> RowRange {
  match c {
    RowRangeConfig::Linear { min, max } => RowRange::Linear { min, max },
    RowRangeConfig::LogFactor { least, multiplier } => RowRange::LogFactor { least, multiplier },
    RowRangeConfig::LogRange { least, greatest } => RowRange::LogRange { least, greatest },
  }
}

fn to_am_shape_family(c: AmShapeFamilyConfig) -> AmShapeFamily {
  match c {
    AmShapeFamilyConfig::SinToSquare => AmShapeFamily::SinToSquare,
    AmShapeFamilyConfig::TriToSquare => AmShapeFamily::TriToSquare,
  }
}

/// Build the edo-monome timbre editor from its window config, converting the
/// lib-side row specs into runtime `RowRange`s.
fn resolve_timbre_editor(config: &Config, edo_monome: &str) -> Option<TimbreEditor> {
  config.monome_windows.iter().find_map(|w| {
    if w.monome() != edo_monome {
      return None;
    }
    let (target, rows) = w.timbre_editor_rows()?;
    let r = |i: usize| to_row_range(rows[i]);
    Some(TimbreEditor::new(
      w.rect(),
      target,
      r(0), // amplitude
      r(1), // am amplitude (depth)
      r(2), // am frequency
      r(3), // am shape
      r(4), // fm amplitude (cents)
      r(5), // fm frequency
    ))
  })
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
  let shift_rect = config
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowConfig::EdoShiftPad { monome, rect, .. } if *monome == edo_monome => Some(*rect),
      _ => None,
    })
    .unwrap_or([-1, -1, -1, -1]);
  let (loops_monome, loop_display_rect) = config
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowConfig::LoopDisplay { monome, rect, .. } => Some((monome.clone(), *rect)),
      _ => None,
    })
    .ok_or("looper needs a loop_display window")?;
  let loop_slots_rect = config
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowConfig::LoopSlots { rect, .. } => Some(*rect),
      _ => None,
    })
    .ok_or("looper needs a loop_slots window")?;
  let control_cell = |kind: LoopControlKind| {
    config.monome_windows.iter().find_map(|w| match w {
      MonomeWindowConfig::LoopControl { control, rect, .. } if *control == kind => {
        Some((rect[0], rect[1]))
      }
      _ => None,
    })
  };
  let loop_start = control_cell(LoopControlKind::Start).ok_or("looper needs loop_control start")?;
  let loop_stop = control_cell(LoopControlKind::Stop).ok_or("looper needs loop_control stop")?;
  let loop_play = control_cell(LoopControlKind::Play).ok_or("looper needs loop_control play")?;
  // Optional single-cell controls; an impossible cell means "not configured".
  let kind_cell = |pred: fn(&MonomeWindowConfig) -> bool| {
    config
      .monome_windows
      .iter()
      .find_map(|w| if pred(w) { let r = w.rect(); Some((r[0], r[1])) } else { None })
      .unwrap_or((-1, -1))
  };
  let loop_toggle = kind_cell(|w| matches!(w, MonomeWindowConfig::LoopRemapModeToggle { .. }));
  let loop_copy = kind_cell(|w| matches!(w, MonomeWindowConfig::LoopCopyButton { .. }));
  let loop_undo = kind_cell(|w| matches!(w, MonomeWindowConfig::LoopRemapUndo { .. }));

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
  let SinkConfig::CpalSynth { sample_rate, buffer_frames, amplitude, attack_secs, release_secs, .. } = sink
  else {
    return Err("looper requires a cpal_synth sink".into());
  };
  let looper = config.looper.as_ref().ok_or("looper config needs a [looper] table")?;
  let edo_cfg = config.monomes.iter().find(|m| m.id == edo_monome).ok_or("the edo monome is not declared")?;
  let loops_cfg = config.monomes.iter().find(|m| m.id == loops_monome).ok_or("the loops monome is not declared")?;
  let size = edo_cfg.select.size.unwrap_or([16, 16]);
  // Resolve the editor + shape family before moving `edo_monome` into Settings.
  let am_shape_family =
    to_am_shape_family(config.am.as_ref().map(|a| a.shape.family).unwrap_or_default());
  let timbre_editor = resolve_timbre_editor(config, &edo_monome);
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
    loop_slots_rect,
    loop_start,
    loop_stop,
    loop_play,
    loop_display_rect,
    loop_toggle,
    loop_copy,
    loop_undo,
    remap_center: (looper.remap_center[0], looper.remap_center[1]),
    x_step: tuning.x_step as i32,
    y_step: tuning.y_step as i32,
    edo: tuning.edo as i32,
    fund: tuning.fundamental_hz,
    sample_rate: *sample_rate,
    buffer_frames: *buffer_frames,
    amplitude: *amplitude,
    attack: *attack_secs,
    release: *release_secs,
    flash_ms: looper.flash_ms,
    quantize: Duration::from_millis(looper.quantize_record_ms),
    cluster: Duration::from_millis(looper.cluster_display_ms),
    am_shape_family,
    timbre_editor,
  })
}

fn run(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  let s = resolve_settings(config)?;

  let edo_sock = UdpSocket::bind(("0.0.0.0", s.edo_listen_port))
    .map_err(|e| format!("bind UDP :{}: {e}", s.edo_listen_port))?;
  edo_sock.set_read_timeout(Some(Duration::from_millis(50)))?;
  let devices = monome::discover_devices(&edo_sock, s.edo_listen_port);
  let assigned = device::assign_distinct_devices(&devices, s.size, config.monomes.len())?;

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
  let edo_dev_id = edo_dev.id.clone();
  let loops_dev_id = loops_dev.id.clone();

  // Drain leftover /serialosc/device enumeration replies queued on the discovery
  // socket, so the edo thread's first recv is a key event, not a stale reply.
  let mut drain = [0u8; 2048];
  while edo_sock.recv_from(&mut drain).is_ok() {}

  let loops_sock = UdpSocket::bind(("0.0.0.0", s.loops_listen_port))
    .map_err(|e| format!("bind UDP :{}: {e}", s.loops_listen_port))?;
  loops_sock.set_read_timeout(Some(Duration::from_millis(50)))?;

  let voices = Arc::new(Mutex::new(HashMap::new()));
  let audio =
    audio::start(Arc::clone(&voices), s.sample_rate, s.buffer_frames, s.amplitude, s.am_shape_family)?;
  let looper_sink =
    sink::SawNoteSink::new(Arc::clone(&voices), s.fund, s.edo, audio.sample_rate, s.attack, s.release);
  let looper_state = Arc::new(Mutex::new(LooperState::new(
    looper_sink,
    LooperParams {
      x_step: s.x_step,
      y_step: s.y_step,
      edo: s.edo,
      grid_w: s.grid_w,
      grid_h: s.grid_h,
      edo_rect: s.edo_rect,
      shift_rect: s.shift_rect,
      loop_slots_rect: s.loop_slots_rect,
      loop_start: s.loop_start,
      loop_stop: s.loop_stop,
      loop_play: s.loop_play,
      loop_display_rect: s.loop_display_rect,
      loop_toggle: s.loop_toggle,
      loop_copy: s.loop_copy,
      loop_undo: s.loop_undo,
      remap_center: s.remap_center,
      quantize: s.quantize,
      cluster: s.cluster,
      timbre_editor: s.timbre_editor,
    },
  )));

  install_sigint();
  println!("looper running; Ctrl-C to exit.");

  let epoch = Instant::now();
  let grid_w = s.grid_w;
  let flash_ms = s.flash_ms;
  let (edo_prefix, loops_prefix) = (s.edo_prefix.clone(), s.loops_prefix.clone());
  let (edo_listen, loops_listen) = (s.edo_listen_port, s.loops_listen_port);
  let st_edo = Arc::clone(&looper_state);
  let st_loops = Arc::clone(&looper_state);
  let st_play = Arc::clone(&looper_state);

  let edo_thread = thread::spawn(move || {
    grid_thread(Grid::Edo, edo_sock, edo_prefix, edo_listen, edo_dev_id, edo_dev_port, grid_w, flash_ms, epoch, st_edo);
  });
  let loops_thread = thread::spawn(move || {
    grid_thread(Grid::Loops, loops_sock, loops_prefix, loops_listen, loops_dev_id, loops_dev_port, grid_w, flash_ms, epoch, st_loops);
  });
  let playback = thread::spawn(move || playback_thread(epoch, st_play));

  let _ = edo_thread.join();
  let _ = loops_thread.join();
  let _ = playback.join();
  // Authoritative teardown regardless of how the threads exited.
  blank_grid(edo_dev_port, &s.edo_prefix);
  blank_grid(loops_dev_port, &s.loops_prefix);
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

/// Advance the sounding slot's playhead into the sink on a steady tick.
fn playback_thread(epoch: Instant, state: Arc<Mutex<LooperState>>) {
  let _stop_on_exit = StopOnExit;
  let mut last_hb = epoch.elapsed();
  while !STOP.load(Ordering::SeqCst) {
    let now = epoch.elapsed();
    {
      let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
      st.playback_tick(now);
    }
    if now.saturating_sub(last_hb) >= Duration::from_millis(750) {
      last_hb = now;
      let st = state.lock().unwrap_or_else(|e| e.into_inner());
      let voices = st.sink.voice_count();
      if voices > 0 {
        eprintln!("[hb] {} voices={voices}", st.debug_status());
      }
    }
    thread::sleep(Duration::from_millis(3));
  }
}

#[allow(clippy::too_many_arguments)]
fn grid_thread(
  role: Grid,
  sock: UdpSocket,
  prefix: String,
  listen_port: u16,
  device_id: String,
  mut device_port: u16,
  grid_w: i32,
  flash_ms: u64,
  epoch: Instant,
  state: Arc<Mutex<LooperState>>,
) {
  // Any exit (clean, early-return, or panic) sets STOP, releasing the siblings.
  let _stop_on_exit = StopOnExit;
  let Ok(mut device) = format!("127.0.0.1:{device_port}").parse::<SocketAddr>() else {
    return;
  };
  monome::register(&sock, device, &prefix, listen_port);
  let key_addr = format!("{prefix}/grid/key");
  let mut last: Vec<i32> = vec![];
  let mut buf = [0u8; 2048];
  // Cells currently held, for debouncing: a well-behaved grid sends one s=1 per
  // press and one s=0 per release, so a duplicate is noise (a stuck or echoing
  // device can flood thousands of identical events per second).
  let mut pressed: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();

  while !STOP.load(Ordering::SeqCst) {
    // 1. Receive (may time out every 50ms so STOP and the flash phase advance).
    if let Ok((n, _)) = sock.recv_from(&mut buf) {
      if let Ok((_, OscPacket::Message(msg))) = decoder::decode_udp(&buf[..n]) {
        let now = epoch.elapsed();
        if msg.addr == "/serialosc/device" && msg.args.len() >= 3 {
          // Only adopt a re-announcement of OUR OWN device id; never a stray reply
          // for the other grid. (A loops-grid ghost can't be recovered this way --
          // discovery only queries on the edo socket; it needs a serialoscd
          // restart, see RUNTIME-NOTES.org.)
          let mine = matches!(msg.args.first(), Some(OscType::String(id)) if *id == device_id);
          if mine {
            if let Some(OscType::Int(port)) = msg.args.get(2) {
              let port = *port as u16;
              if port != device_port {
                device_port = port;
                if let Ok(addr) = format!("127.0.0.1:{port}").parse::<SocketAddr>() {
                  device = addr;
                }
                monome::register(&sock, device, &prefix, listen_port);
                last.clear();
              }
            }
          }
        } else if msg.addr == key_addr && msg.args.len() == 3 {
          if let (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(down))) =
            (msg.args.first(), msg.args.get(1), msg.args.get(2))
          {
            // serialosc sends s in {0,1}; ignore anything else.
            let press = match *down {
              1 => Some(true),
              0 => Some(false),
              _ => None,
            };
            if let Some(press) = press {
              let changed = if press {
                pressed.insert((*x, *y))
              } else {
                pressed.remove(&(*x, *y))
              };
              if changed {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                match role {
                  Grid::Edo => {
                    st.edo_key(*x, *y, press, now);
                  }
                  Grid::Loops => {
                    if st.loops_key(*x, *y, press, now) && press {
                      eprintln!("[loops] ({x},{y}) -> {}", st.debug_status());
                    }
                  }
                }
              }
            }
          }
        }
      }
    }

    // 2. Repaint every iteration: picks up state changes from any thread plus the
    // flash phase for the loops grid. send_diffs only sends cells that changed.
    let flash_on = (epoch.elapsed().as_millis() / u128::from(flash_ms.max(1))) % 2 == 0;
    let levels = {
      let st = state.lock().unwrap_or_else(|e| e.into_inner());
      match role {
        Grid::Edo => st.edo_levels(),
        Grid::Loops => st.loops_levels(flash_on),
      }
    };
    send_diffs(&sock, device, &prefix, grid_w, &levels, &mut last);
  }
  // run() blanks both grids authoritatively after the joins.
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
