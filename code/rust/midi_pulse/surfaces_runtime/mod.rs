//! The surfaces runtime: two independent EDO play grids (each with a scroll pad and a
//! waveform selector + volume strip -- wired per config via `controls`, which the
//! current rigs point at the strip's own grid) plus the KMSS pedalboard as a drumkit
//! -- three surfaces at once, in one process.
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

mod accrete;
pub mod audio;
mod grid;
mod synth;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rosc::{decoder, OscPacket, OscType};

use midi_pulse::config::{AccreteControlKind, Config, MonomeWindowConfig, SinkConfig};
use midi_pulse::device_assign::assign_distinct_devices;
use midi_pulse::edo_play::{register_delta, shift_for_cell, step_for_cell};
use midi_pulse::monome::{self, DeviceInfo};
use midi_pulse::monome_brightness::PulseBrightness;

use crate::drumkit_runtime;
use crate::types::{Timbre, VoiceMap, Waveform};
use crate::voices::Distortion;

use accrete::AccreteState;
use grid::{
  levels_for_grid, volume_cells, volume_gain_for_pos, waveform_for_selector_cell, ButtonOverlay,
  BRIGHT, DIM,
};
use synth::{set_grid_gain, SurfaceSink};

/// The volume strip's total dB span (top cell unity, bottom cell -30 dB).
const VOLUME_DB_RANGE: f32 = 30.0;
/// The startup active volume column (absolute), per `0_vision.org`: "begin at button 10
/// (which leaves 5 spots to the right for headroom)".
const VOLUME_DEFAULT_COL: i32 = 10;
/// Fake-dim flash for a monobright grid: ~1/32 duty at ~66.7 Hz (period 15 ms), matching
/// `edo12n_piano_monome_runtime` and a varibright grid's native level-4 brightness. The
/// effective on-time is transmit-bound (a full frame takes a few ms to send), so a heavy
/// dim set naturally slows the flash into visible flicker -- an accepted trade (see the
/// visuals discussion doc).
const DIM_PULSE: PulseBrightness = PulseBrightness::one_thirty_second(15_000);

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
  volume_rect: [i32; 4],
  /// The grid index this grid's volume strip sets (self if it has no volume strip).
  volume_controls_index: usize,
  /// The three accrete (sustain) buttons' cells, `NO_RECT` when absent. Both grids'
  /// buttons drive ONE shared accrete state.
  clear_rect: [i32; 4],
  needs_holding_rect: [i32; 4],
  accrete_rect: [i32; 4],
  /// The global-distortion toggle's cell, `NO_RECT` when absent (one shared switch).
  distortion_rect: [i32; 4],
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
  /// The synth master gain -- the single "synth volume" knob, from the cpal_synth sink's
  /// `amplitude` (applies to both grids; the per-grid volume strips trim below it).
  amplitude: f32,
  oversample: u32,
  attack: f32,
  release: f32,
  /// The pluck envelope (cpal_synth `sustain_level` / `decay_secs`): fresh strikes
  /// peak, then decay toward the sustain so they ring out over held notes.
  sustain_level: f32,
  decay_secs: f32,
  /// The global distortion's curve (scale + shape from the cpal_synth sink); the
  /// on/off lives in a shared AtomicBool flipped by the distortion_toggle windows.
  distortion: Distortion,
  has_drums: bool,
  /// Trail clobber radius as a divisor of the octave (`[surfaces].trail_clobber_radius`):
  /// a played note clears trailed classes within `edo / this` steps of it.
  trail_clobber_radius: i32,
  /// Max distinct pitch classes in the shared trail (`[surfaces].trails_max`).
  trails_max: usize,
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
    sample_rate, buffer_frames, amplitude, attack_secs, release_secs, oversample,
    distortion_scale, distortion_shape, sustain_level, decay_secs, ..
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
    // The volume strip rect + which grid it sets the loudness of (like the selector).
    let volume = config.monome_windows.iter().find_map(|w| match w {
      MonomeWindowConfig::VolumeStrip { monome, rect, controls, .. } if monome == monome_id => {
        Some((*rect, controls.clone()))
      }
      _ => None,
    });
    let (volume_rect, volume_controls_index) = match volume {
      Some((rect, controls)) => {
        let idx = index_of(&controls)
          .ok_or("volume_strip controls a monome that is not a play grid")?;
        (rect, idx)
      }
      None => (NO_RECT, index_of(monome_id).unwrap()),
    };
    // The accrete (sustain) buttons, one rect per control kind (validation already
    // guarantees a monome has either the whole trio or none of it).
    let accrete_rect_on = |kind: AccreteControlKind| {
      config
        .monome_windows
        .iter()
        .find_map(|w| match w {
          MonomeWindowConfig::AccreteControl { monome, rect, control, .. }
            if monome == monome_id && *control == kind =>
          {
            Some(*rect)
          }
          _ => None,
        })
        .unwrap_or(NO_RECT)
    };
    let distortion_rect = config
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowConfig::DistortionToggle { monome, rect, .. } if monome == monome_id => {
          Some(*rect)
        }
        _ => None,
      })
      .unwrap_or(NO_RECT);
    grids.push(GridSettings {
      monome_id: monome_id.to_string(),
      listen_port: monome_cfg.listen_port,
      prefix: monome_cfg.prefix.clone(),
      edo_rect,
      scroll_rect,
      selector_rect,
      controls_index,
      volume_rect,
      volume_controls_index,
      clear_rect: accrete_rect_on(AccreteControlKind::Clear),
      needs_holding_rect: accrete_rect_on(AccreteControlKind::NeedsHolding),
      accrete_rect: accrete_rect_on(AccreteControlKind::Accrete),
      distortion_rect,
    });
  }

  let size = config
    .monomes
    .iter()
    .find(|m| m.id == grids[0].monome_id)
    .and_then(|m| m.select.size)
    .unwrap_or([16, 16]);

  // The `[surfaces]` table (trail knobs); absent -> defaults, so unchanged behaviour.
  let surfaces = config.surfaces.unwrap_or_default();

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
    sustain_level: *sustain_level,
    decay_secs: *decay_secs,
    distortion: Distortion { scale: *distortion_scale, shape: *distortion_shape },
    has_drums: !config.softstep_windows.is_empty(),
    trail_clobber_radius: surfaces.trail_clobber_radius,
    trails_max: surfaces.trails_max,
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
  // waveform and its grid's volume gain, and the render sums them all. The cpal_synth
  // sink's `amplitude` is the single master "synth volume" (both grids); the per-grid
  // volume strips are live trims that multiply below it.
  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
  // The global distortion on/off, shared by every grid's toggle and the audio callback.
  let distortion_on = Arc::new(AtomicBool::new(false));
  let audio = if no_audio {
    audio::start_null(s.sample_rate)
  } else {
    audio::start(
      Arc::clone(&voices),
      s.sample_rate,
      s.buffer_frames,
      s.amplitude,
      s.oversample as usize,
      s.distortion,
      Arc::clone(&distortion_on),
    )?
  };

  // Per-grid current waveform (index = grid index). Each element is written only by the
  // grid whose selector controls it; every grid reads all of it (cross-grid reflection).
  let waveforms = Arc::new(Mutex::new(vec![Waveform::default(); num_grids]));
  // Per-grid sounding pitch-classes (union drives cross-grid note reflection), the
  // shared recent-note trail (dim backdrop), and the per-grid volume state (position +
  // linear gain), each defaulting to column `VOLUME_DEFAULT_COL` of its own strip.
  let sounding = Arc::new(Mutex::new(vec![HashSet::<i32>::new(); num_grids]));
  let trail = Arc::new(Mutex::new(VecDeque::<i32>::with_capacity(s.trails_max)));
  let mut volume_pos_init = Vec::with_capacity(num_grids);
  let mut gains_init = Vec::with_capacity(num_grids);
  for g in &s.grids {
    let cells = volume_cells(g.volume_rect);
    if cells > 0 {
      let pos = (VOLUME_DEFAULT_COL - g.volume_rect[0]).clamp(0, cells - 1);
      volume_pos_init.push(pos);
      gains_init.push(volume_gain_for_pos(pos, cells, VOLUME_DB_RANGE));
    } else {
      volume_pos_init.push(0);
      gains_init.push(1.0);
    }
  }
  let volume_pos = Arc::new(Mutex::new(volume_pos_init));
  let gains = Arc::new(Mutex::new(gains_init));
  // The shared accrete (sustain) state -- one instrument-wide machine, driven by both
  // grids' button trios -- and a mirror of every grid's held notes (cell -> struck
  // pitch), so an accrete activation can capture what is fingered on ALL grids.
  let accrete = Arc::new(Mutex::new(AccreteState::new()));
  let held_all = Arc::new(Mutex::new(vec![HashMap::<(i32, i32), i32>::new(); num_grids]));

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
      // A monobright grid (an old Series 256) can't dim a single LED, so it fakes DIM by
      // flashing; a varibright grid sends native levels. Keyed on the serial id, which --
      // unlike the type string ("monome 256" for both) -- distinguishes them.
      monobright: is_monobright(&dev.id),
      edo_rect: g.edo_rect,
      scroll_rect: g.scroll_rect,
      selector_rect: g.selector_rect,
      controls_index: g.controls_index,
      volume_rect: g.volume_rect,
      volume_controls_index: g.volume_controls_index,
      clear_rect: g.clear_rect,
      needs_holding_rect: g.needs_holding_rect,
      accrete_rect: g.accrete_rect,
      distortion_rect: g.distortion_rect,
      grid_w: s.grid_w,
      grid_h: s.grid_h,
      x_step: s.x_step,
      y_step: s.y_step,
      edo: s.edo,
      trail_clobber_radius: s.trail_clobber_radius,
      trails_max: s.trails_max,
      waveforms: Arc::clone(&waveforms),
      sounding: Arc::clone(&sounding),
      trail: Arc::clone(&trail),
      volume_pos: Arc::clone(&volume_pos),
      gains: Arc::clone(&gains),
      accrete: Arc::clone(&accrete),
      held_all: Arc::clone(&held_all),
      distortion_on: Arc::clone(&distortion_on),
      voices: Arc::clone(&voices),
      sink: SurfaceSink::new(
        grid_index,
        Arc::clone(&voices),
        s.fund,
        s.edo,
        audio.sample_rate,
        s.attack,
        s.release,
        s.sustain_level,
        s.decay_secs,
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
  /// A monobright grid fakes DIM by flashing; a varibright grid sends native levels.
  monobright: bool,
  edo_rect: [i32; 4],
  scroll_rect: [i32; 4],
  selector_rect: [i32; 4],
  /// The grid index this grid's waveform selector re-timbres.
  controls_index: usize,
  volume_rect: [i32; 4],
  /// The grid index this grid's volume strip sets the loudness of.
  volume_controls_index: usize,
  /// The three accrete (sustain) buttons' cells on this grid (`NO_RECT` if absent).
  clear_rect: [i32; 4],
  needs_holding_rect: [i32; 4],
  accrete_rect: [i32; 4],
  /// The global-distortion toggle's cell on this grid (`NO_RECT` if absent).
  distortion_rect: [i32; 4],
  grid_w: i32,
  grid_h: i32,
  x_step: i32,
  y_step: i32,
  edo: i32,
  /// Trail clobber radius as a divisor of the octave (see `Settings`).
  trail_clobber_radius: i32,
  /// Max distinct pitch classes the shared trail keeps.
  trails_max: usize,
  waveforms: Arc<Mutex<Vec<Waveform>>>,
  /// Per-grid sounding pitch-classes; the union drives cross-grid note reflection.
  sounding: Arc<Mutex<Vec<HashSet<i32>>>>,
  /// Shared recent-note trail (pitch classes), newest first.
  trail: Arc<Mutex<VecDeque<i32>>>,
  /// Per-grid volume position (column within the strip) and linear gain.
  volume_pos: Arc<Mutex<Vec<i32>>>,
  gains: Arc<Mutex<Vec<f32>>>,
  /// The shared accrete (sustain) state; either grid's buttons act on it.
  accrete: Arc<Mutex<AccreteState>>,
  /// Every grid's held notes (cell -> struck pitch), for accrete's capture-on-
  /// activation. Each grid thread rewrites only its own slot.
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  /// The global distortion on/off (both grids' toggles + the audio callback).
  distortion_on: Arc<AtomicBool>,
  /// The shared voice map, for the live volume rescale of the controlled grid's voices.
  voices: Arc<Mutex<VoiceMap>>,
  sink: SurfaceSink,
}

fn grid_thread(mut rt: GridThread) {
  // Any exit (clean, early-return, or panic) sets STOP, releasing the siblings.
  let _stop_on_exit = StopOnExit;
  let Ok(mut device) = format!("127.0.0.1:{}", rt.device_port).parse::<SocketAddr>() else {
    return;
  };
  monome::register(&rt.sock, device, &rt.prefix, rt.listen_port);
  // Poll fast so a monobright grid can flash its fake-dim pulse near the fusion rate.
  let _ = rt.sock.set_read_timeout(Some(Duration::from_millis(1)));
  let key_addr = format!("{}/grid/key", rt.prefix);

  let mut register: i32 = 0;
  // Held cells -> the pitch each was struck at. Reflection uses the *struck* pitch's
  // class, so a later scroll moves the lit cells while the sounding pitch stays put.
  let mut held: HashMap<(i32, i32), i32> = HashMap::new();
  // A well-behaved grid sends one s=1 per press and one s=0 per release; track the
  // pressed set so a stuck/echoing device's duplicates are dropped.
  let mut pressed: HashSet<(i32, i32)> = HashSet::new();
  // Varibright: diff native levels. Monobright: diff a binary frame per 8x8 quad.
  let mut last_levels: Vec<i32> = vec![];
  let mut last_quads: Vec<[u8; 8]> = vec![];
  let mut next_pulse = Instant::now() + DIM_PULSE.period;
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
                last_quads.clear();
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

    // Repaint. The overlays show the state of whatever grid THIS grid's strips control
    // (its own, in the current rigs); the play cells reflect the union of both grids'
    // sounding classes (bright; sustained notes count -- you hear them) and the shared
    // trail (dim), through the current register.
    let selector_waveform = current_waveform(&rt.waveforms, rt.controls_index);
    let volume_col = volume_active_col(&rt);
    let (mut buttons, sustained_classes) = accrete_view(&rt);
    buttons.push((rt.distortion_rect, rt.distortion_on.load(Ordering::Relaxed)));
    let mut sounding_classes = union_sounding(&rt.sounding);
    sounding_classes.extend(sustained_classes);
    let trail_classes = trail_set(&rt.trail);
    let levels = levels_for_grid(
      &sounding_classes,
      &trail_classes,
      rt.edo_rect,
      rt.selector_rect,
      selector_waveform,
      rt.volume_rect,
      volume_col,
      rt.scroll_rect,
      &buttons,
      register,
      rt.x_step,
      rt.y_step,
      rt.edo,
      rt.grid_w,
      rt.grid_h,
    );

    if rt.monobright {
      // Steady frame with DIM cells dark; briefly pulse them on at ~1/32 duty. The
      // on-frame's transmit time bounds the on-period, so a heavy dim set slows the
      // effective flash into (accepted) flicker.
      send_binary_frame(&rt.sock, device, &rt.prefix, rt.grid_w, rt.grid_h, &levels, false, &mut last_quads);
      let now = Instant::now();
      if now >= next_pulse {
        send_binary_frame(&rt.sock, device, &rt.prefix, rt.grid_w, rt.grid_h, &levels, true, &mut last_quads);
        thread::sleep(DIM_PULSE.on_time);
        send_binary_frame(&rt.sock, device, &rt.prefix, rt.grid_w, rt.grid_h, &levels, false, &mut last_quads);
        next_pulse = now + DIM_PULSE.period;
      }
    } else {
      send_diffs(&rt.sock, device, &rt.prefix, rt.grid_w, &levels, &mut last_levels);
    }
  }

  // run() blanks every grid authoritatively after the joins; this is best-effort.
  monome::send_led_all(&rt.sock, device, &rt.prefix, 0);
}

/// Route one debounced key edge by which overlay (if any) it falls in.
fn handle_key(
  rt: &mut GridThread,
  register: &mut i32,
  held: &mut HashMap<(i32, i32), i32>,
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
  // Volume strip: a press sets the *controlled* grid's loudness -- live (rescales its
  // sounding voices) and for its future notes.
  if in_overlay(rt.volume_rect, cell) {
    if press {
      set_volume(rt, cell.0);
    }
    return;
  }
  // Distortion toggle: key-down flips the GLOBAL on/off (the audio callback reads it
  // live); key-up does nothing.
  if in_overlay(rt.distortion_rect, cell) {
    if press {
      let _ = rt.distortion_on.fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // The accrete (sustain) buttons. All three act on the SHARED state, so either
  // grid's trio works. Decisions are made under the accrete lock, voices are touched
  // after it drops (the module's no-nested-locks rule).
  if in_overlay(rt.clear_rect, cell) {
    if press {
      rt.accrete.lock().unwrap_or_else(|e| e.into_inner()).press_clear();
      rt.sink.release_all_sustained();
    } else {
      rt.accrete.lock().unwrap_or_else(|e| e.into_inner()).release_clear();
    }
    return;
  }
  if in_overlay(rt.needs_holding_rect, cell) {
    if press {
      let activated =
        rt.accrete.lock().unwrap_or_else(|e| e.into_inner()).press_needs_holding();
      if activated {
        capture_all_held(rt);
      }
    }
    return;
  }
  if in_overlay(rt.accrete_rect, cell) {
    if press {
      let activated = rt.accrete.lock().unwrap_or_else(|e| e.into_inner()).press_accrete();
      if activated {
        capture_all_held(rt);
      }
    } else {
      rt.accrete.lock().unwrap_or_else(|e| e.into_inner()).release_accrete();
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
    let gain = current_gain(&rt.gains, rt.grid_index);
    rt.sink.note_on(cell, pitch, Timbre { waveform, gain, ..Timbre::default() });
    held.insert(cell, pitch);
    rt.accrete.lock().unwrap_or_else(|e| e.into_inner()).note_played(rt.grid_index, pitch);
    push_trail(&rt.trail, pitch.rem_euclid(rt.edo), rt.edo, rt.trail_clobber_radius, rt.trails_max);
  } else {
    // A note in the sustained set (or released under a live accreting condition)
    // keeps ringing: its voice moves to the sustain register instead of releasing.
    let sustains = held.get(&cell).copied().map(|pitch| {
      let keep = rt
        .accrete
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .note_released_sustains(rt.grid_index, pitch);
      (pitch, keep)
    });
    match sustains {
      Some((pitch, true)) => rt.sink.sustain_note(cell, pitch),
      _ => rt.sink.note_off(cell),
    }
    held.remove(&cell);
  }
  publish_held(&rt.held_all, rt.grid_index, held);
  publish_sounding(&rt.sounding, rt.grid_index, held, rt.edo);
}

/// Mirror this grid's held map into the shared per-grid registry (for accrete's
/// capture-on-activation, which must see BOTH grids' fingers).
fn publish_held(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  index: usize,
  held: &HashMap<(i32, i32), i32>,
) {
  let mut all = held_all.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(slot) = all.get_mut(index) {
    *slot = held.clone();
  }
}

/// The accreting condition just turned on: add every currently-held note (all grids)
/// to the sustained set. Snapshot the held registry first, then feed the accrete
/// state -- two short, non-nested locks.
fn capture_all_held(rt: &GridThread) {
  let snapshot: Vec<(usize, i32)> = {
    let all = rt.held_all.lock().unwrap_or_else(|e| e.into_inner());
    all
      .iter()
      .enumerate()
      .flat_map(|(g, m)| m.values().map(move |p| (g, *p)))
      .collect()
  };
  rt.accrete.lock().unwrap_or_else(|e| e.into_inner()).capture_held(snapshot);
}

/// One lock: the accrete buttons' LED view for this grid plus the sustained pitch
/// classes (which paint bright -- they are sounding).
fn accrete_view(rt: &GridThread) -> (Vec<ButtonOverlay>, HashSet<i32>) {
  let s = rt.accrete.lock().unwrap_or_else(|e| e.into_inner());
  (
    vec![
      (rt.clear_rect, s.clear_lit()),
      (rt.needs_holding_rect, s.needs_holding_lit()),
      (rt.accrete_rect, s.accrete_lit()),
    ],
    s.sustained_classes(rt.edo),
  )
}

/// A serialosc serial id that names an old monobright "Series" grid (per-LED on/off
/// only, so it thresholds any level <= 7 to off). The newer format (e.g. `m0000102`) is
/// varibright. Both a monobright 256 and a varibright 16x16 report type "monome 256", so
/// the id -- not the type string -- is what distinguishes them. Heuristic; the hardware
/// pass confirms it (a monobright grid drops a level-4 cell to dark).
fn is_monobright(id: &str) -> bool {
  ["m40h-", "m64-", "m128-", "m256-"].iter().any(|p| id.starts_with(p))
}

fn in_overlay(rect: [i32; 4], cell: (i32, i32)) -> bool {
  let [x0, y0, x1, y1] = rect;
  cell.0 >= x0 && cell.0 <= x1 && cell.1 >= y0 && cell.1 <= y1
}

/// Apply a volume-strip press at absolute column `pressed_x`: set the controlled grid's
/// position + gain and rescale its live voices (the fader is *live*, per Jeff).
fn set_volume(rt: &GridThread, pressed_x: i32) {
  let cells = volume_cells(rt.volume_rect);
  if cells <= 0 {
    return;
  }
  let pos = (pressed_x - rt.volume_rect[0]).clamp(0, cells - 1);
  let gain = volume_gain_for_pos(pos, cells, VOLUME_DB_RANGE);
  let target = rt.volume_controls_index;
  {
    let mut vp = rt.volume_pos.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = vp.get_mut(target) {
      *slot = pos;
    }
  }
  {
    let mut g = rt.gains.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = g.get_mut(target) {
      *slot = gain;
    }
  }
  set_grid_gain(&rt.voices, target, gain);
}

/// The absolute column of the active volume cell this grid should light (the *controlled*
/// grid's position, shown on this grid's strip), or -1 if this grid has no volume strip.
fn volume_active_col(rt: &GridThread) -> i32 {
  if volume_cells(rt.volume_rect) <= 0 {
    return -1;
  }
  let pos = {
    let vp = rt.volume_pos.lock().unwrap_or_else(|e| e.into_inner());
    vp.get(rt.volume_controls_index).copied().unwrap_or(0)
  };
  rt.volume_rect[0] + pos
}

fn current_gain(gains: &Arc<Mutex<Vec<f32>>>, index: usize) -> f32 {
  let g = gains.lock().unwrap_or_else(|e| e.into_inner());
  g.get(index).copied().unwrap_or(1.0)
}

/// Publish grid `index`'s currently-sounding pitch classes (register-independent: the
/// class of each held cell's *struck* pitch) for the other grid to reflect.
fn publish_sounding(
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

fn union_sounding(sounding: &Arc<Mutex<Vec<HashSet<i32>>>>) -> HashSet<i32> {
  let s = sounding.lock().unwrap_or_else(|e| e.into_inner());
  let mut u = HashSet::new();
  for set in s.iter() {
    u.extend(set.iter().copied());
  }
  u
}

fn trail_set(trail: &Arc<Mutex<VecDeque<i32>>>) -> HashSet<i32> {
  let t = trail.lock().unwrap_or_else(|e| e.into_inner());
  t.iter().copied().collect()
}

/// Record a just-pressed pitch class in the shared trail. The trail holds up to
/// `trails_max` *distinct* classes, newest first. Playing a note first clears its own
/// class (dedup -- so hammering one note, in any octave, never floods or erases the
/// trail) and every trailed class within `edo / clobber_radius` steps of it (neighbour
/// suppression), then adds it at the front. Both knobs come from the `[surfaces]` table.
fn push_trail(
  trail: &Arc<Mutex<VecDeque<i32>>>,
  class: i32,
  edo: i32,
  clobber_radius: i32,
  trails_max: usize,
) {
  let mut t = trail.lock().unwrap_or_else(|e| e.into_inner());
  // Keep only classes strictly outside the suppression radius (this also drops the new
  // class itself, since its distance is 0).
  t.retain(|&c| pitch_class_distance(c, class, edo) * clobber_radius > edo);
  t.push_front(class);
  while t.len() > trails_max {
    t.pop_back();
  }
}

/// The distance between two pitch classes on the octave circle, in EDO steps (0..=edo/2).
fn pitch_class_distance(a: i32, b: i32, edo: i32) -> i32 {
  let d = (a - b).rem_euclid(edo);
  d.min(edo - d)
}

/// Send a binary frame as changed 8x8 quads (`/grid/led/map`). `dim_on` selects whether
/// DIM cells are lit this sub-frame (the monobright fake-dim flash toggles it); BRIGHT is
/// always on, OFF always off. Cheap: a whole 16x16 frame is at most 4 messages, so this
/// sustains the flash where per-cell writes would swamp the serial link.
#[allow(clippy::too_many_arguments)]
fn send_binary_frame(
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

  /// Serialises the mock-rig tests: they share the global `STOP` and the mock config's
  /// listen ports, so they must not run concurrently.
  static MOCK_LOCK: Mutex<()> = Mutex::new(());

  #[test]
  fn selector_writes_the_controlled_grids_waveform_slot() {
    // A grid's selector re-timbres the grid at `controls_index`, leaving every other
    // slot untouched. Wire grid 0's strip to grid 1 (cross-control is still legal
    // config even though the current rigs self-control): a press on grid 0's saw cell
    // must set grid 1's waveform to Saw only.
    let waveforms = Arc::new(Mutex::new(vec![Waveform::default(); 2]));
    set_waveform(&waveforms, 1, Waveform::Saw); // grid 0's selector -> grid 1
    assert_eq!(current_waveform(&waveforms, 1), Waveform::Saw, "grid 1 (controlled) got saw");
    assert_eq!(current_waveform(&waveforms, 0), Waveform::Triangle, "grid 0 unchanged");
  }

  #[test]
  fn trail_dedups_by_class_and_suppresses_neighbours() {
    let edo = 58;
    // The `[surfaces]` defaults: 1/27 octave (58/27 ~= 2.1, so classes within 2 steps are
    // neighbours) and up to 7 distinct classes.
    let clobber = 27;
    let trails_max = 7;
    let trail = Arc::new(Mutex::new(VecDeque::new()));
    let snap = |t: &Arc<Mutex<VecDeque<i32>>>| -> Vec<i32> { t.lock().unwrap().iter().copied().collect() };

    // Hammering one class (octaves collapse to the same class) never floods or evicts.
    for _ in 0..7 {
      push_trail(&trail, 20, edo, clobber, trails_max);
    }
    assert_eq!(snap(&trail), vec![20], "a repeated class stays a single entry");

    // Far-apart classes accumulate, newest first.
    push_trail(&trail, 30, edo, clobber, trails_max);
    push_trail(&trail, 40, edo, clobber, trails_max);
    assert_eq!(snap(&trail), vec![40, 30, 20], "far-apart classes coexist");

    // Playing a near neighbour (2 steps from 40) erases 40 from the trail.
    push_trail(&trail, 42, edo, clobber, trails_max);
    assert_eq!(snap(&trail), vec![42, 30, 20], "a neighbour within 1/27 octave is suppressed");

    // Wrap-around neighbours count too: classes 1 and 57 are 2 apart in 58-EDO.
    push_trail(&trail, 1, edo, clobber, trails_max);
    push_trail(&trail, 57, edo, clobber, trails_max);
    assert!(!snap(&trail).contains(&1), "1 is suppressed by its wrap-around neighbour 57");

    // Never exceeds `trails_max` distinct classes.
    for c in [4, 8, 12, 16, 24, 34, 44, 54] {
      push_trail(&trail, c, edo, clobber, trails_max);
    }
    assert!(snap(&trail).len() <= trails_max, "capped at {trails_max}");
  }

  #[test]
  fn resolves_two_grids_with_self_control() {
    let config = load_named_config("2-monomes_58-8-1_kmss-drums").expect("config loads");
    let s = resolve_settings(&config).expect("resolves without hardware");
    assert_eq!(s.grids.len(), 2, "two play grids");
    assert!(s.has_drums, "the KMSS drumkit is present");
    // Each grid's selector and volume strip control its OWN grid (per TODO/misc.org,
    // 2026-07; the looper-plus-edo rig keeps its cross-surface timbre editing, which
    // is a different mechanism entirely).
    assert_eq!(s.grids[0].controls_index, 0, "grid 0's strip re-timbres grid 0");
    assert_eq!(s.grids[1].controls_index, 1, "grid 1's strip re-timbres grid 1");
    assert_eq!(s.grids[0].volume_controls_index, 0, "grid 0's volume sets grid 0");
    assert_eq!(s.grids[1].volume_controls_index, 1, "grid 1's volume sets grid 1");
    // Both grids carry a scroll pad, a selector, a volume strip, the accrete trio,
    // and the distortion toggle.
    for g in &s.grids {
      assert_ne!(g.scroll_rect, NO_RECT, "grid {:?} has a scroll pad", g.monome_id);
      assert_ne!(g.selector_rect, NO_RECT, "grid {:?} has a selector", g.monome_id);
      assert_ne!(g.volume_rect, NO_RECT, "grid {:?} has a volume strip", g.monome_id);
      assert_eq!(g.clear_rect, [0, 15, 0, 15], "grid {:?} clear button", g.monome_id);
      assert_eq!(g.needs_holding_rect, [1, 15, 1, 15], "grid {:?} needs-holding", g.monome_id);
      assert_eq!(g.accrete_rect, [2, 15, 2, 15], "grid {:?} accrete button", g.monome_id);
      assert_eq!(g.distortion_rect, [0, 1, 0, 1], "grid {:?} distortion toggle", g.monome_id);
    }
    // The sink's distortion curve flows into the settings.
    assert_eq!(s.distortion, Distortion { scale: 1.0, shape: 2.0 });
    // The `[surfaces]` table flows into the settings (this config asks for 9 trails).
    assert_eq!(s.trail_clobber_radius, 27, "trail_clobber_radius from [surfaces]");
    assert_eq!(s.trails_max, 9, "trails_max from [surfaces]");
  }

  #[test]
  fn surfaces_defaults_when_table_absent() {
    // A config that declares no `[surfaces]` table falls back to the built-in defaults
    // (1/27 octave, 7 trails), so omitting it changes nothing.
    let config = load_named_config("monome-edo-sawwave").expect("config loads");
    assert!(config.surfaces.is_none(), "no [surfaces] table declared");
    let s = config.surfaces.unwrap_or_default();
    assert_eq!(s.trail_clobber_radius, 27, "default radius is 1/27 octave");
    assert_eq!(s.trails_max, 7, "default trail length is 7");
  }

  /// End-to-end against two virtual grids (the monome mock) with null audio: the whole
  /// device layer -- discovery, both grids binding, LED output, key input routing --
  /// which the pure tests cannot cover. No hardware, no sound. See MOCK-MONOME.org.
  #[test]
  fn two_grids_run_against_mock_grids() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 15), "fingered note lights solid on grid a");
    // Cross-grid reflection (feature 3): grid a's note lights its octave-equivalents on
    // grid b too (both registers 0, so the same cell). Audio voices stay independent --
    // that is tested in synth.rs; here we check the shared LED reflection.
    assert!(wait_until(secs(3), || b.level_at(5, 5) == 15), "grid a's note reflects onto grid b");
    a.release(5, 5);
    // Trail (feature 4): a released note lingers *dim* (level 4) in the shared trail on
    // both grids, rather than going fully dark.
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 4), "released note lingers dim on grid a");
    assert!(wait_until(secs(3), || b.level_at(5, 5) == 4), "released note lingers dim on grid b");

    // Grid a's selector sets grid a's OWN waveform to SAW (cell (3,0)) -> its strip
    // repaints to show saw selected; grid b's strip (b's own waveform) is untouched.
    a.press(3, 0);
    a.release(3, 0);
    assert!(wait_until(secs(3), || a.level_at(3, 0) == 15 && a.level_at(1, 0) == 4),
      "grid a strip now shows saw selected (triangle dims)");
    assert!(wait_until(secs(3), || b.level_at(1, 0) == 15), "grid b's strip still shows its own triangle");

    // Volume strip (feature 2): grid a's strip shows grid a's own volume, starting at
    // the default column 10; pressing another cell moves its active column.
    assert!(wait_until(secs(3), || a.level_at(10, 0) == 15), "grid a shows its own default volume (col 10)");
    a.press(4, 0);
    a.release(4, 0);
    assert!(wait_until(secs(3), || a.level_at(4, 0) == 15 && a.level_at(10, 0) == 0),
      "the volume strip moves the active cell to col 4");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// The accrete (sustain) buttons end-to-end: toggle accrete mode on grid a, play a
  /// note, release it -- it keeps ringing (stays bright on BOTH grids) -- then clear
  /// from grid B (the state is shared), and the note drops to the dim trail. Also
  /// checks the buttons' own LEDs (dim at rest, bright when active, mirrored across
  /// grids).
  #[test]
  fn accrete_sustains_notes_until_cleared() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rig = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = rig.detector_port();
    let config = load_named_config("2-monomes_58-8-1_kmss-drums-mock").expect("mock config loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let config = config.clone();
      thread::spawn(move || {
        if let Err(e) = run(&config, detector_port, true) {
          eprintln!("mock accrete run error: {e}");
        }
      })
    };

    let a = rig.grid(0);
    let b = rig.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");

    // At rest all three buttons glow dim (findable), on both grids.
    for (x, name) in [(0, "clear"), (1, "needs_holding"), (2, "accrete")] {
      assert!(wait_until(secs(3), || a.level_at(x, 15) == 4), "grid a {name} rests dim");
      assert!(wait_until(secs(3), || b.level_at(x, 15) == 4), "grid b {name} rests dim");
    }

    // Tap accrete on grid a: needs_holding starts OFF, so key-down toggles accrete
    // mode on -- the button lights on BOTH grids (one shared state).
    a.press(2, 15);
    a.release(2, 15);
    assert!(wait_until(secs(3), || a.level_at(2, 15) == 15), "accrete lit on grid a");
    assert!(wait_until(secs(3), || b.level_at(2, 15) == 15), "accrete lit on grid b too");

    // Play and release a note on grid a: sustained, it stays BRIGHT (not trail-dim).
    a.press(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 15), "fingered note lights");
    a.release(5, 5);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(5, 5), 15, "released note keeps ringing bright on grid a");
    assert_eq!(b.level_at(5, 5), 15, "and reflects bright on grid b");

    // needs_holding from grid b: lights everywhere, and cancels the toggled mode
    // (accrete dims) -- but the sustained note keeps ringing.
    b.press(1, 15);
    b.release(1, 15);
    assert!(wait_until(secs(3), || a.level_at(1, 15) == 15), "needs_holding lit on grid a");
    assert!(wait_until(secs(3), || a.level_at(2, 15) == 4), "accrete mode cancelled -> dim");
    assert_eq!(a.level_at(5, 5), 15, "the sustained note survives the mode flip");

    // Clear from grid B: lit while held, and the note falls back to the dim trail.
    b.press(0, 15);
    assert!(wait_until(secs(3), || a.level_at(0, 15) == 15), "clear lit (on the other grid too)");
    b.release(0, 15);
    assert!(wait_until(secs(3), || a.level_at(0, 15) == 4), "clear dims on key-up");
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 4), "cleared note drops to the trail on a");
    assert!(wait_until(secs(3), || b.level_at(5, 5) == 4), "and on b");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// The distortion toggle end-to-end: rests dim at (0,1), a press lights it on BOTH
  /// grids (one global switch), a second press (from the other grid) turns it off.
  #[test]
  fn distortion_toggle_mirrors_across_grids() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rig = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = rig.detector_port();
    let config = load_named_config("2-monomes_58-8-1_kmss-drums-mock").expect("mock config loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let config = config.clone();
      thread::spawn(move || {
        if let Err(e) = run(&config, detector_port, true) {
          eprintln!("mock distortion run error: {e}");
        }
      })
    };

    let a = rig.grid(0);
    let b = rig.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");
    assert!(wait_until(secs(3), || a.level_at(0, 1) == 4 && b.level_at(0, 1) == 4),
      "the toggle rests dim on both grids");
    a.press(0, 1);
    a.release(0, 1);
    assert!(wait_until(secs(3), || a.level_at(0, 1) == 15), "on: lit on grid a");
    assert!(wait_until(secs(3), || b.level_at(0, 1) == 15), "on: lit on grid b (global)");
    b.press(0, 1);
    b.release(0, 1);
    assert!(wait_until(secs(3), || a.level_at(0, 1) == 4 && b.level_at(0, 1) == 4),
      "off again from the other grid");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// A monobright grid (old Series-256 serial id) can't dim a single LED, so the runtime
  /// fakes DIM by flashing binary quad frames; a varibright grid gets a steady level 4.
  /// Drive one of each and check the contrast on a scroll arrow (a DIM cell), plus that a
  /// note lights bright on the monobright grid through the binary-map path.
  #[test]
  fn monobright_grid_flashes_fake_dim() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Grid 0's id is the classic Series-256 format -> detected monobright; grid 1's is the
    // newer format -> varibright. (Both report type "monome 256"; only the id differs.)
    let rig = MockRig::start(0, &[GridSpec::grid_256("m256-9"), GridSpec::grid_256("m0000777")])
      .expect("start mock rig");
    let detector_port = rig.detector_port();
    let config = load_named_config("2-monomes_58-8-1_kmss-drums-mock").expect("mock config loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let config = config.clone();
      thread::spawn(move || {
        if let Err(e) = run(&config, detector_port, true) {
          eprintln!("mock monobright run error: {e}");
        }
      })
    };

    let mono = rig.grid(0); // Series-256 serial -> fake-dim by flashing.
    let vari = rig.grid(1); // newer serial -> native levels.
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || mono.registered() && vari.registered()), "both grids register");

    // The Down arrow (14,15) is a DIM cell. Varibright: steady native level 4.
    assert!(wait_until(secs(3), || vari.level_at(14, 15) == 4), "varibright arrow is a steady dim (level 4)");
    // Monobright: never steady 4 -- it flashes 0<->15 (binary), so we catch an on-frame.
    assert!(wait_until(secs(5), || mono.level_at(14, 15) == 15), "monobright arrow flashes on (fake dim)");
    // A note on the monobright grid lights solid via the binary-map path.
    mono.press(6, 6);
    assert!(wait_until(secs(3), || mono.level_at(6, 6) == 15), "monobright held note bright via map");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }
}
