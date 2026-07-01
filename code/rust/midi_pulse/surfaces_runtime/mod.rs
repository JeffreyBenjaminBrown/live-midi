//! The surfaces runtime: two independent EDO play grids (each with a scroll pad and a
//! waveform selector that re-timbres the *other* grid) plus the KMSS pedalboard as a
//! drumkit -- three surfaces at once, in one process.
//!
//! `midi_pulse.rs::main` dispatches to exactly one runtime, and the three features
//! each otherwise take over the whole app (EDO grid+synth = sawwave, the scroll pad =
//! looper, the drums = drumkit). This runtime composes them by reusing the already-
//! shared pure pieces: the sawwave voice/render engine (`crate::{types,voices,pitch}`),
//! the lib scroll math (`midi_pulse::edo_play`), the two-grid bind
//! (`midi_pulse::device_assign`), and the drumkit's own bring-up
//! (`crate::drumkit_runtime::start`, consumed -- not forked).
//!
//! Voices are keyed by `(grid, cell)` (see `synth`), so the same pitch on both grids
//! is two independent voices and a release never gates the other grid. Each grid's
//! voices sound in that grid's currently-selected waveform.
//!
//! Threads: one serialosc key/LED loop per grid, plus the drumkit's own timer/MIDI
//! threads. Shared state (the per-grid waveform) sits behind one mutex; the audio
//! voice map behind another; a grid thread reads the waveform, drops that lock, then
//! touches voices -- never nesting the two. STOP (SIGINT/SIGTERM, or a test) releases
//! the grid loops; teardown blanks both grids, drops audio, and restores the KMSS to
//! standalone mode.

pub mod audio;
mod grid;
mod synth;

use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rosc::{decoder, OscPacket, OscType};

use midi_pulse::config::{Config, MonomeWindowConfig, SinkConfig};
use midi_pulse::device_assign::assign_distinct_devices;
use midi_pulse::edo_play::{register_delta, shift_for_cell, step_for_cell};
use midi_pulse::monome::{self, DeviceInfo};

use crate::drumkit_runtime;
use crate::pitch::build_pitch_class;
use crate::types::{PitchClass, Timbre, Waveform};

use grid::{levels_for_grid, waveform_for_selector_cell};
use synth::SurfaceSink;

/// The sentinel rect (never matches a cell) for an absent scroll pad / selector.
const NO_RECT: [i32; 4] = [-1, -1, -1, -1];

static STOP: AtomicBool = AtomicBool::new(false);

/// Dropped at the end of every grid thread: setting STOP releases the siblings, so
/// one thread's death (clean, early return, or panic) tears the runtime down instead
/// of leaving a zombie grid loop.
struct StopOnExit;
impl Drop for StopOnExit {
  fn drop(&mut self) {
    STOP.store(true, Ordering::SeqCst);
  }
}

pub fn run_from_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  print_inventory(config);
  // Block SIGINT/SIGTERM and start the STOP-setting waiter BEFORE any audio/MIDI/grid
  // thread spawns, so the block is inherited by all of them (a stray default SIGINT
  // would otherwise kill the process, leaving the KMSS stuck in tether mode).
  install_signals();
  // Headless / mock runs set MIDI_PULSE_NO_AUDIO to skip the cpal stream.
  let no_audio = std::env::var_os("MIDI_PULSE_NO_AUDIO").is_some();
  STOP.store(false, Ordering::SeqCst);
  run(config, monome::detector_port(), no_audio)
}

fn print_inventory(config: &Config) {
  println!(
    "surfaces: {} monome windows across {} monomes; {} softstep windows",
    config.monome_windows.len(),
    config.monomes.len(),
    config.softstep_windows.len(),
  );
  for monome in &config.monomes {
    println!("  monome {:?} (port {}, prefix {:?}):", monome.id, monome.listen_port, monome.prefix);
    for window in config.monome_windows.iter().filter(|w| w.monome() == monome.id) {
      println!("    {:<18} rect {:?}", window.kind_name(), window.rect());
    }
  }
}

/// One play grid's resolved config: its monome binding + its overlay rects + which
/// grid its selector re-timbres.
struct GridSettings {
  monome_id: String,
  listen_port: u16,
  prefix: String,
  edo_rect: [i32; 4],
  scroll_rect: [i32; 4],
  selector_rect: [i32; 4],
  /// The grid index this grid's waveform selector sets (self if it has no selector).
  controls_index: usize,
}

struct Settings {
  grids: Vec<GridSettings>,
  size: [i32; 2],
  grid_w: i32,
  grid_h: i32,
  x_step: i32,
  y_step: i32,
  edo: i32,
  fund: f64,
  sample_rate: u32,
  buffer_frames: u32,
  amplitude: f32,
  oversample: u32,
  attack: f32,
  release: f32,
  has_drums: bool,
}

fn resolve_settings(config: &Config) -> Result<Settings, Box<dyn std::error::Error>> {
  // The play grids: every declared monome that carries an edo_note_grid, in config
  // order (grid index = position here).
  let grid_monomes: Vec<&str> = config
    .monomes
    .iter()
    .map(|m| m.id.as_str())
    .filter(|id| {
      config.monome_windows.iter().any(|w| {
        matches!(w, MonomeWindowConfig::EdoNoteGrid { monome, .. } if monome == id)
      })
    })
    .collect();
  if grid_monomes.is_empty() {
    return Err("a surfaces config needs at least one edo_note_grid".into());
  }
  let index_of = |monome_id: &str| grid_monomes.iter().position(|m| *m == monome_id);

  // The tuning + sink come from the first edo grid (all grids share them here).
  let (tuning_id, sink_id) = config
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowConfig::EdoNoteGrid { tuning, sink, .. } => Some((tuning.clone(), sink.clone())),
      _ => None,
    })
    .ok_or("a surfaces config needs an edo_note_grid")?;
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
  let SinkConfig::CpalSynth {
    sample_rate, buffer_frames, amplitude, attack_secs, release_secs, oversample, ..
  } = sink
  else {
    return Err("surfaces requires a cpal_synth sink for the play grids".into());
  };

  let rect_on = |monome_id: &str, pred: fn(&MonomeWindowConfig) -> bool| {
    config
      .monome_windows
      .iter()
      .find(|w| w.monome() == monome_id && pred(w))
      .map(|w| w.rect())
  };

  let mut grids = Vec::new();
  for monome_id in &grid_monomes {
    let monome_cfg = config
      .monomes
      .iter()
      .find(|m| m.id == *monome_id)
      .ok_or("a play-grid monome is not declared")?;
    let edo_rect = rect_on(monome_id, |w| matches!(w, MonomeWindowConfig::EdoNoteGrid { .. }))
      .ok_or("a play grid lost its edo_note_grid")?;
    let scroll_rect =
      rect_on(monome_id, |w| matches!(w, MonomeWindowConfig::EdoShiftPad { .. })).unwrap_or(NO_RECT);
    // The selector rect + which grid it controls.
    let selector = config.monome_windows.iter().find_map(|w| match w {
      MonomeWindowConfig::WaveformSelector { monome, rect, controls, .. } if monome == monome_id => {
        Some((*rect, controls.clone()))
      }
      _ => None,
    });
    let (selector_rect, controls_index) = match selector {
      Some((rect, controls)) => {
        let idx = index_of(&controls)
          .ok_or("waveform_selector controls a monome that is not a play grid")?;
        (rect, idx)
      }
      None => (NO_RECT, index_of(monome_id).unwrap()),
    };
    grids.push(GridSettings {
      monome_id: monome_id.to_string(),
      listen_port: monome_cfg.listen_port,
      prefix: monome_cfg.prefix.clone(),
      edo_rect,
      scroll_rect,
      selector_rect,
      controls_index,
    });
  }

  let size = config
    .monomes
    .iter()
    .find(|m| m.id == grids[0].monome_id)
    .and_then(|m| m.select.size)
    .unwrap_or([16, 16]);

  Ok(Settings {
    grids,
    size,
    grid_w: size[0],
    grid_h: size[1],
    x_step: tuning.x_step as i32,
    y_step: tuning.y_step as i32,
    edo: tuning.edo as i32,
    fund: tuning.fundamental_hz,
    sample_rate: *sample_rate,
    buffer_frames: *buffer_frames,
    amplitude: *amplitude,
    oversample: *oversample,
    attack: *attack_secs,
    release: *release_secs,
    has_drums: !config.softstep_windows.is_empty(),
  })
}

/// The I/O shell. `detector_port` is the serialosc(-mock) port to discover grids on;
/// `no_audio` skips the cpal stream (headless / mock). Loops until STOP. Signal
/// handling is installed by `run_from_config`, not here, so tests can call this
/// directly and stop it by setting STOP.
fn run(config: &Config, detector_port: u16, no_audio: bool) -> Result<(), Box<dyn std::error::Error>> {
  let s = resolve_settings(config)?;
  let num_grids = s.grids.len();

  // Discover all grids on the first grid's socket, then assign each a distinct device.
  let sock0 = UdpSocket::bind(("0.0.0.0", s.grids[0].listen_port))
    .map_err(|e| format!("bind UDP :{}: {e}", s.grids[0].listen_port))?;
  sock0.set_read_timeout(Some(Duration::from_millis(50)))?;
  let devices = monome::discover_devices_via(&sock0, s.grids[0].listen_port, detector_port);
  let assigned: Vec<DeviceInfo> = assign_distinct_devices(&devices, s.size, num_grids)?;
  for (g, dev) in s.grids.iter().zip(&assigned) {
    println!("surfaces: grid {:?} -> id={:?} port={}", g.monome_id, dev.id, dev.port);
  }
  // Drain leftover /serialosc/device enumeration replies so grid 0's first recv is a
  // key event, not a stale reply.
  let mut drain = [0u8; 2048];
  while sock0.recv_from(&mut drain).is_ok() {}

  // Sockets for every grid (grid 0 reuses the discovery socket).
  let mut sockets: Vec<UdpSocket> = Vec::with_capacity(num_grids);
  sockets.push(sock0);
  for g in &s.grids[1..] {
    let sock = UdpSocket::bind(("0.0.0.0", g.listen_port))
      .map_err(|e| format!("bind UDP :{}: {e}", g.listen_port))?;
    sock.set_read_timeout(Some(Duration::from_millis(50)))?;
    sockets.push(sock);
  }

  // Shared audio: one voice map + one synth stream; each voice carries its grid's
  // waveform, and the render sums them all.
  let voices = Arc::new(Mutex::new(std::collections::HashMap::new()));
  let audio = if no_audio {
    audio::start_null(s.sample_rate)
  } else {
    audio::start(Arc::clone(&voices), s.sample_rate, s.buffer_frames, s.amplitude, s.oversample as usize)?
  };

  // Per-grid current waveform (index = grid index). Both grids read/write this; each
  // element is written only by the grid whose selector controls it.
  let waveforms = Arc::new(Mutex::new(vec![Waveform::default(); num_grids]));
  // The pitch index is identical across grids (same tuning + size); share one.
  let pc = Arc::new(build_pitch_class(s.x_step, s.y_step, s.edo, s.grid_w, s.grid_h));

  // Bring up the drumkit alongside the grids, if the config declares one. Consumed
  // from `drumkit_runtime` (not forked); kept alive for the run, restoring standalone
  // mode on drop. We own the signal handling, so the tether session is unarmed.
  let drums = if s.has_drums {
    Some(drumkit_runtime::start(config, drumkit_runtime::tether::session())?)
  } else {
    None
  };

  println!("surfaces running; Ctrl-C to exit.");

  // Spawn one key/LED loop per grid.
  let mut handles = Vec::with_capacity(num_grids);
  let assigned_ports: Vec<u16> = assigned.iter().map(|d| d.port).collect();
  for ((grid_index, sock), dev) in sockets.into_iter().enumerate().zip(&assigned) {
    let g = &s.grids[grid_index];
    let rt = GridThread {
      grid_index,
      sock,
      prefix: g.prefix.clone(),
      listen_port: g.listen_port,
      device_id: dev.id.clone(),
      device_port: dev.port,
      edo_rect: g.edo_rect,
      scroll_rect: g.scroll_rect,
      selector_rect: g.selector_rect,
      controls_index: g.controls_index,
      grid_w: s.grid_w,
      grid_h: s.grid_h,
      x_step: s.x_step,
      y_step: s.y_step,
      edo: s.edo,
      pc: Arc::clone(&pc),
      waveforms: Arc::clone(&waveforms),
      sink: SurfaceSink::new(
        grid_index,
        Arc::clone(&voices),
        s.fund,
        s.edo,
        audio.sample_rate,
        s.attack,
        s.release,
      ),
    };
    handles.push(thread::spawn(move || grid_thread(rt)));
  }

  for handle in handles {
    let _ = handle.join();
  }

  // Authoritative teardown regardless of how the threads exited.
  for (g, port) in s.grids.iter().zip(&assigned_ports) {
    blank_grid(*port, &g.prefix);
  }
  drop(audio);
  drop(drums);
  println!("surfaces stopped.");
  Ok(())
}

/// Everything one grid thread owns for the run.
struct GridThread {
  grid_index: usize,
  sock: UdpSocket,
  prefix: String,
  listen_port: u16,
  device_id: String,
  device_port: u16,
  edo_rect: [i32; 4],
  scroll_rect: [i32; 4],
  selector_rect: [i32; 4],
  controls_index: usize,
  grid_w: i32,
  grid_h: i32,
  x_step: i32,
  y_step: i32,
  edo: i32,
  pc: Arc<PitchClass>,
  waveforms: Arc<Mutex<Vec<Waveform>>>,
  sink: SurfaceSink,
}

fn grid_thread(mut rt: GridThread) {
  // Any exit (clean, early-return, or panic) sets STOP, releasing the siblings.
  let _stop_on_exit = StopOnExit;
  let Ok(mut device) = format!("127.0.0.1:{}", rt.device_port).parse::<SocketAddr>() else {
    return;
  };
  monome::register(&rt.sock, device, &rt.prefix, rt.listen_port);
  let key_addr = format!("{}/grid/key", rt.prefix);

  let mut register: i32 = 0;
  let mut held: HashSet<(i32, i32)> = HashSet::new();
  // A well-behaved grid sends one s=1 per press and one s=0 per release; track the
  // pressed set so a stuck/echoing device's duplicates are dropped.
  let mut pressed: HashSet<(i32, i32)> = HashSet::new();
  let mut last_levels: Vec<i32> = vec![];
  let mut buf = [0u8; 2048];

  while !STOP.load(Ordering::SeqCst) {
    if let Ok((n, _)) = rt.sock.recv_from(&mut buf) {
      if let Ok((_, OscPacket::Message(msg))) = decoder::decode_udp(&buf[..n]) {
        if msg.addr == "/serialosc/device" && msg.args.len() >= 3 {
          // Only adopt a re-announcement of OUR OWN device id (never a stray reply
          // for another grid). Discovery only queried on grid 0's socket, so a ghost
          // on another grid needs a serialoscd restart (RUNTIME-NOTES.org).
          let mine = matches!(msg.args.first(), Some(OscType::String(id)) if *id == rt.device_id);
          if mine {
            if let Some(OscType::Int(port)) = msg.args.get(2) {
              let port = *port as u16;
              if port != rt.device_port {
                rt.device_port = port;
                if let Ok(addr) = format!("127.0.0.1:{port}").parse::<SocketAddr>() {
                  device = addr;
                }
                monome::register(&rt.sock, device, &rt.prefix, rt.listen_port);
                last_levels.clear();
              }
            }
          }
        } else if msg.addr == key_addr && msg.args.len() == 3 {
          if let (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(down))) =
            (msg.args.first(), msg.args.get(1), msg.args.get(2))
          {
            let press = match *down {
              1 => Some(true),
              0 => Some(false),
              _ => None,
            };
            if let Some(press) = press {
              let cell = (*x, *y);
              let changed = if press { pressed.insert(cell) } else { pressed.remove(&cell) };
              if changed {
                handle_key(&mut rt, &mut register, &mut held, cell, press);
              }
            }
          }
        }
      }
    }

    // Repaint (send only changed cells). The selector shows the waveform it controls.
    let selector_waveform = current_waveform(&rt.waveforms, rt.controls_index);
    let levels = levels_for_grid(
      &rt.pc,
      &held,
      rt.edo_rect,
      rt.selector_rect,
      selector_waveform,
      rt.scroll_rect,
      rt.grid_w,
      rt.grid_h,
    );
    send_diffs(&rt.sock, device, &rt.prefix, rt.grid_w, &levels, &mut last_levels);
  }

  // run() blanks every grid authoritatively after the joins; this is best-effort.
  monome::send_led_all(&rt.sock, device, &rt.prefix, 0);
}

/// Route one debounced key edge by which overlay (if any) it falls in.
fn handle_key(
  rt: &mut GridThread,
  register: &mut i32,
  held: &mut HashSet<(i32, i32)>,
  cell: (i32, i32),
  press: bool,
) {
  // Selector: a press sets the *controlled* grid's waveform (radio; future notes).
  if let Some(waveform) = waveform_for_selector_cell(rt.selector_rect, cell) {
    if press {
      set_waveform(&rt.waveforms, rt.controls_index, waveform);
    }
    return;
  }
  // Scroll pad: a press moves THIS grid's play register.
  if let Some(shift) = shift_for_cell(rt.scroll_rect, cell) {
    if press {
      *register += register_delta(shift, rt.x_step, rt.y_step, rt.edo);
    }
    return;
  }
  // Otherwise it is an edo play cell -- ignore presses outside the play grid.
  let [ex0, ey0, ex1, ey1] = rt.edo_rect;
  if cell.0 < ex0 || cell.0 > ex1 || cell.1 < ey0 || cell.1 > ey1 {
    return;
  }
  if press {
    let pitch = step_for_cell(rt.x_step, rt.y_step, *register, cell.0, cell.1);
    let waveform = current_waveform(&rt.waveforms, rt.grid_index);
    rt.sink.note_on(cell, pitch, Timbre { waveform, ..Timbre::default() });
    held.insert(cell);
  } else {
    rt.sink.note_off(cell);
    held.remove(&cell);
  }
}

fn current_waveform(waveforms: &Arc<Mutex<Vec<Waveform>>>, index: usize) -> Waveform {
  let guard = waveforms.lock().unwrap_or_else(|e| e.into_inner());
  guard.get(index).copied().unwrap_or_default()
}

fn set_waveform(waveforms: &Arc<Mutex<Vec<Waveform>>>, index: usize, waveform: Waveform) {
  let mut guard = waveforms.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(slot) = guard.get_mut(index) {
    *slot = waveform;
  }
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

/// Blank a grid from an ephemeral socket (used by run() after the threads join, so a
/// panicked thread that skipped its own blank still leaves its grid dark).
fn blank_grid(device_port: u16, prefix: &str) {
  if let (Ok(sock), Ok(addr)) = (
    UdpSocket::bind(("0.0.0.0", 0)),
    format!("127.0.0.1:{device_port}").parse::<SocketAddr>(),
  ) {
    monome::send_led_all(&sock, addr, prefix, 0);
  }
}

/// Block SIGINT/SIGTERM process-wide (so every later-spawned thread inherits the
/// block) and wait for them on a dedicated thread that sets STOP -- letting the main
/// thread join the grid loops and run teardown (blank grids, drop audio, restore the
/// KMSS). A default Ctrl-C would instead kill the process, skipping that teardown.
fn install_signals() {
  unsafe {
    let mut set: libc::sigset_t = std::mem::zeroed();
    libc::sigemptyset(&mut set);
    libc::sigaddset(&mut set, libc::SIGINT);
    libc::sigaddset(&mut set, libc::SIGTERM);
    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
  }
  thread::spawn(|| {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut sig: libc::c_int = 0;
    unsafe {
      libc::sigemptyset(&mut set);
      libc::sigaddset(&mut set, libc::SIGINT);
      libc::sigaddset(&mut set, libc::SIGTERM);
      libc::sigwait(&set, &mut sig);
    }
    eprintln!("\nStopping (caught signal); blanking grids and restoring the KMSS...");
    STOP.store(true, Ordering::SeqCst);
  });
}

#[cfg(test)]
mod tests {
  use super::*;
  use midi_pulse::config::load_named_config;

  #[test]
  fn selector_writes_the_controlled_grids_waveform_slot() {
    // A grid's selector re-timbres the grid at `controls_index`, leaving its own
    // slot untouched. With grid 0's strip controlling grid 1 (the config's cross-
    // wiring), a press on grid 0's saw cell must set grid 1's waveform to Saw only.
    let waveforms = Arc::new(Mutex::new(vec![Waveform::default(); 2]));
    set_waveform(&waveforms, 1, Waveform::Saw); // grid 0's selector -> grid 1
    assert_eq!(current_waveform(&waveforms, 1), Waveform::Saw, "grid 1 (controlled) got saw");
    assert_eq!(current_waveform(&waveforms, 0), Waveform::Triangle, "grid 0 unchanged");
  }

  #[test]
  fn resolves_two_grids_with_cross_control() {
    let config = load_named_config("2-monomes_58-8-1_kmss-drums").expect("config loads");
    let s = resolve_settings(&config).expect("resolves without hardware");
    assert_eq!(s.grids.len(), 2, "two play grids");
    assert!(s.has_drums, "the KMSS drumkit is present");
    // Each grid's selector controls the OTHER grid.
    assert_eq!(s.grids[0].controls_index, 1, "grid 0's strip re-timbres grid 1");
    assert_eq!(s.grids[1].controls_index, 0, "grid 1's strip re-timbres grid 0");
    // Both grids carry a scroll pad and a selector.
    for g in &s.grids {
      assert_ne!(g.scroll_rect, NO_RECT, "grid {:?} has a scroll pad", g.monome_id);
      assert_ne!(g.selector_rect, NO_RECT, "grid {:?} has a selector", g.monome_id);
    }
  }

  /// End-to-end against two virtual grids (the monome mock) with null audio: the whole
  /// device layer -- discovery, both grids binding, LED output, key input routing --
  /// which the pure tests cannot cover. No hardware, no sound. See MOCK-MONOME.org.
  #[test]
  fn two_grids_run_against_mock_grids() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let rig = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = rig.detector_port();
    let config = load_named_config("2-monomes_58-8-1_kmss-drums-mock").expect("mock config loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let config = config.clone();
      thread::spawn(move || {
        if let Err(e) = run(&config, detector_port, true) {
          eprintln!("mock surfaces run error: {e}");
        }
      })
    };

    let a = rig.grid(0);
    let b = rig.grid(1);
    let secs = Duration::from_secs;
    // Both grids register and get a first repaint.
    assert!(
      wait_until(secs(5), || a.registered() && b.registered()),
      "both grids should register against the surfaces runtime",
    );
    assert!(wait_until(secs(3), || a.generation() > 0 && b.generation() > 0), "first repaint");

    // Each grid's selector strip lights the DEFAULT (triangle) cell bright: cell (1,0).
    assert!(wait_until(secs(3), || a.level_at(1, 0) == 15), "grid a selector: triangle bright");
    assert!(wait_until(secs(3), || b.level_at(1, 0) == 15), "grid b selector: triangle bright");

    // Finger a note on grid a (open cell, away from overlays): it lights, dark on release.
    a.press(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 15), "fingered note lights solid");
    // Independent voices: the same cell on grid b is unaffected by grid a's press.
    assert_eq!(b.level_at(5, 5), 0, "grid b is independent of grid a's note");
    a.release(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 0), "released note goes dark");

    // Grid a's selector sets grid b's waveform to SAW (cell (3,0)) -> grid a's strip
    // repaints to show saw selected.
    a.press(3, 0);
    a.release(3, 0);
    assert!(wait_until(secs(3), || a.level_at(3, 0) == 15 && a.level_at(1, 0) == 4),
      "grid a strip now shows saw selected (triangle dims)");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }
}
