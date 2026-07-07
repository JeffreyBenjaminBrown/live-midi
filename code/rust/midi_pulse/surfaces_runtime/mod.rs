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
mod slide;
mod synth;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rosc::{decoder, OscPacket, OscType};

use midi_pulse::config::{
  load_named_config, AccreteControlKind, AmShapeFamilyConfig, Config, MonomeWindowConfig,
  SinkConfig, WaveformChoice,
};
use midi_pulse::device_assign::assign_distinct_devices;
use midi_pulse::edo_play::{register_delta, shift_for_cell, step_for_cell};
use midi_pulse::monome::{self, DeviceInfo};
use midi_pulse::monome_brightness::PulseBrightness;

use crate::drumkit_runtime;
use crate::types::{Am, AmShapeFamily, Fm, Timbre, VoiceMap, Waveform};
use crate::voices::Distortion;

use accrete::AccreteState;
use slide::SlideCandidates;
use grid::{
  levels_for_grid, slot_for_selector_cell, volume_cells, volume_gain_for_pos, ButtonOverlay,
  BRIGHT, DIM, SELECTOR_CELLS,
};
use synth::{rescale_grid_gain, SurfaceSink};

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

/// One selectable timbre behind a selector cell: the resolved form of a config
/// `[[timbres]]` entry (or a plain-waveform default). `amplitude` multiplies below
/// the grid's volume fader at note-on.
#[derive(Clone, Copy)]
pub(crate) struct TimbreSlot {
  waveform: Waveform,
  amplitude: f32,
  am: Am,
  fm: Fm,
}

/// The hot-reloadable ('r' + Enter) subset of the settings -- the scalars a running
/// instrument can adopt without rebinding grids, sockets, or streams: the synth
/// master amplitude, the distortion curve, the four timbres, the tuning, the pluck
/// envelope, and the slide/trail knobs. See TODO/misc.org "config reload".
pub(crate) struct Live {
  /// Bumped on every successful reload; grid threads refresh their copies when it
  /// moves (the audio callback reads the params fresh every callback).
  generation: AtomicU64,
  params: Mutex<LiveParams>,
}

#[derive(Clone, Copy)]
pub(crate) struct LiveParams {
  pub amplitude: f32,
  pub distortion: Distortion,
  pub timbres: [TimbreSlot; SELECTOR_CELLS],
  pub x_step: i32,
  pub y_step: i32,
  pub edo: i32,
  pub fund: f64,
  pub sustain_level: f32,
  pub decay_secs: f32,
  pub slide_window: Duration,
  pub slide_duration_secs: f32,
  pub trail_clobber_radius: i32,
  pub trails_max: usize,
}

/// The live subset of resolved settings.
fn live_params(s: &Settings) -> LiveParams {
  LiveParams {
    amplitude: s.amplitude,
    distortion: s.distortion,
    timbres: s.timbres,
    x_step: s.x_step,
    y_step: s.y_step,
    edo: s.edo,
    fund: s.fund,
    sustain_level: s.sustain_level,
    decay_secs: s.decay_secs,
    slide_window: s.slide_window,
    slide_duration_secs: s.slide_duration_secs,
    trail_clobber_radius: s.trail_clobber_radius,
    trails_max: s.trails_max,
  }
}

/// Re-read `name`'s config and adopt its live parameters (everything else -- window
/// layout, ports, sinks' sample rates -- needs a restart and is silently kept as
/// is). A config that fails to load or resolve leaves the old parameters running.
fn reload_live(name: &str, live: &Live) {
  match load_named_config(name).and_then(|config| adopt_config(&config, live)) {
    Ok(()) => println!(
      "reloaded {name}: amplitude / distortion / timbres / tuning / pluck / slide / trail applied (layout + ports need a restart)",
    ),
    Err(e) => eprintln!("reload of {name} failed; keeping the running parameters: {e}"),
  }
}

/// Adopt `config`'s live parameters into `live` and bump the generation. Everything
/// non-live (window layout, ports, sinks' sample rates) is silently kept as-is.
fn adopt_config(config: &Config, live: &Live) -> Result<(), String> {
  let s = resolve_settings(config).map_err(|e| e.to_string())?;
  *live.params.lock().unwrap_or_else(|e| e.into_inner()) = live_params(&s);
  live.generation.fetch_add(1, Ordering::SeqCst);
  Ok(())
}

/// The startup-selected slot: 1 = triangle in the default slots, matching the old
/// `Waveform::default()` behavior (and any custom `[[timbres]]` layout's cell 1).
const DEFAULT_SLOT: usize = 1;

/// The pre-`[[timbres]]` behavior: the four plain waveforms, everything else off.
fn default_timbre_slots() -> [TimbreSlot; SELECTOR_CELLS] {
  let plain = |waveform| TimbreSlot {
    waveform,
    amplitude: 1.0,
    am: Am::default(),
    fm: Fm::default(),
  };
  [
    plain(Waveform::Sine),
    plain(Waveform::Triangle),
    plain(Waveform::Square),
    plain(Waveform::Saw),
  ]
}

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

pub fn run_from_config(
  config: &Config,
  reload_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
  print_inventory(config);
  // Block SIGINT/SIGTERM and start the STOP-setting waiter BEFORE any audio/MIDI/grid
  // thread spawns, so the block is inherited by all of them (a stray default SIGINT
  // would otherwise kill the process, leaving the KMSS stuck in tether mode).
  install_signals();
  // Headless / mock runs set MIDI_PULSE_NO_AUDIO to skip the cpal stream.
  let no_audio = std::env::var_os("MIDI_PULSE_NO_AUDIO").is_some();
  STOP.store(false, Ordering::SeqCst);
  run(config, monome::detector_port(), no_audio, reload_name)
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
  /// The slide / mono toggles' cells, `NO_RECT` when absent (global switches too).
  slide_rect: [i32; 4],
  mono_rect: [i32; 4],
  /// The feet-accrete (softstep-accretes) toggle's cell, `NO_RECT` when absent.
  feet_accrete_rect: [i32; 4],
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
  /// The four selectable timbres (config `[[timbres]]`, or the plain waveforms).
  timbres: [TimbreSlot; SELECTOR_CELLS],
  /// The instrument-wide AM LFO morph family (config `[am]`).
  am_shape_family: AmShapeFamily,
  /// The global distortion's curve (scale + shape from the cpal_synth sink); the
  /// on/off lives in a shared AtomicBool flipped by the distortion_toggle windows.
  distortion: Distortion,
  has_drums: bool,
  /// Trail clobber radius as a divisor of the octave (`[surfaces].trail_clobber_radius`):
  /// a played note clears trailed classes within `edo / this` steps of it.
  trail_clobber_radius: i32,
  /// Max distinct pitch classes in the shared trail (`[surfaces].trails_max`).
  trails_max: usize,
  /// The slide feature's knobs (`[surfaces]`): how recently a note must have been
  /// released to be a slide source, and how long the glide takes.
  slide_window: Duration,
  slide_duration_secs: f32,
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
    let slide_rect = config
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowConfig::SlideToggle { monome, rect, .. } if monome == monome_id => Some(*rect),
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let mono_rect = config
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowConfig::MonoToggle { monome, rect, .. } if monome == monome_id => Some(*rect),
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let feet_accrete_rect = config
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowConfig::SoftstepAccretesToggle { monome, rect, .. } if monome == monome_id => {
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
      slide_rect,
      mono_rect,
      feet_accrete_rect,
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
    timbres: resolve_timbre_slots(config),
    am_shape_family: match config.am.as_ref().map(|a| a.shape.family).unwrap_or_default() {
      AmShapeFamilyConfig::SinToSquare => AmShapeFamily::SinToSquare,
      AmShapeFamilyConfig::TriToSquare => AmShapeFamily::TriToSquare,
    },
    distortion: Distortion { scale: *distortion_scale, shape: *distortion_shape },
    has_drums: !config.softstep_windows.is_empty(),
    trail_clobber_radius: surfaces.trail_clobber_radius,
    trails_max: surfaces.trails_max,
    slide_window: Duration::from_millis(surfaces.slide_candidate_window_ms),
    slide_duration_secs: surfaces.slide_duration_ms as f32 / 1000.0,
  })
}

/// The config's `[[timbres]]` mapped onto the four selector slots (validation
/// guarantees exactly four when present); absent = the plain waveforms.
fn resolve_timbre_slots(config: &Config) -> [TimbreSlot; SELECTOR_CELLS] {
  if config.timbres.is_empty() {
    return default_timbre_slots();
  }
  let mut slots = default_timbre_slots();
  for (slot, t) in slots.iter_mut().zip(&config.timbres) {
    *slot = TimbreSlot {
      waveform: match t.waveform {
        WaveformChoice::Sine => Waveform::Sine,
        WaveformChoice::Triangle => Waveform::Triangle,
        WaveformChoice::Square => Waveform::Square,
        WaveformChoice::Saw => Waveform::Saw,
      },
      amplitude: t.amplitude,
      am: Am { depth: t.am_depth, freq: t.am_freq, shape: t.am_shape },
      fm: Fm { depth_cents: t.fm_depth_cents, freq: t.fm_freq },
    };
  }
  slots
}

/// The I/O shell. `detector_port` is the serialosc(-mock) port to discover grids on;
/// `no_audio` skips the cpal stream (headless / mock). Loops until STOP. Signal
/// handling is installed by `run_from_config`, not here, so tests can call this
/// directly and stop it by setting STOP.
fn run(
  config: &Config,
  detector_port: u16,
  no_audio: bool,
  reload_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
  let s = resolve_settings(config)?;
  let num_grids = s.grids.len();
  // The hot-reloadable parameters ('r' + Enter re-reads the config; see `Live`).
  let live = Arc::new(Live {
    generation: AtomicU64::new(0),
    params: Mutex::new(live_params(&s)),
  });
  if let Some(name) = reload_name {
    let live_for_stdin = Arc::clone(&live);
    let name = name.to_string();
    thread::spawn(move || {
      // Line-based (press 'r' then Enter): raw-mode single-key input would need
      // termios surgery that could leave the terminal broken on a crash.
      for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if STOP.load(Ordering::SeqCst) {
          break;
        }
        if line.trim() == "r" {
          reload_live(&name, &live_for_stdin);
        }
      }
    });
    println!("press 'r' + Enter to hot-reload the config (amplitude / timbres / tuning / pluck / slide / trail / distortion curve).");
  }

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
  // The global slide / mono switches (each grid keeps its own candidate history,
  // but the modes are instrument-wide, like distortion).
  let slide_on = Arc::new(AtomicBool::new(false));
  let mono_on = Arc::new(AtomicBool::new(false));
  // Feet accrete: while on, the KMSS pedals 1/2/3 and 8/9/0 act as the accrete trio
  // instead of playing samples (see the pedal hook below).
  let feet_accrete_on = Arc::new(AtomicBool::new(false));
  let audio = if no_audio {
    audio::start_null(s.sample_rate)
  } else {
    audio::start(
      Arc::clone(&voices),
      s.sample_rate,
      s.buffer_frames,
      s.oversample as usize,
      s.am_shape_family,
      Arc::clone(&live),
      Arc::clone(&distortion_on),
    )?
  };

  // Per-grid selected timbre slot (index = grid index). Each element is written only
  // by the grid whose selector controls it; every grid reads all of it.
  let selected = Arc::new(Mutex::new(vec![DEFAULT_SLOT; num_grids]));
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
  // mode on drop. We own the signal handling, so the tether session is unarmed. The
  // pedal hook is the feet-accrete mirror: while its toggle is on, pedals 1/2/3 and
  // 8/9/0 drive the shared accrete state instead of playing samples.
  let drums = if s.has_drums {
    let hook = feet_accrete_hook(
      Arc::clone(&feet_accrete_on),
      Arc::clone(&accrete),
      Arc::clone(&held_all),
      Arc::clone(&voices),
      s.release,
      audio.sample_rate,
    );
    Some(drumkit_runtime::start_with_hook(
      config,
      drumkit_runtime::tether::session(),
      Some(hook),
    )?)
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
      slide_rect: g.slide_rect,
      mono_rect: g.mono_rect,
      feet_accrete_rect: g.feet_accrete_rect,
      grid_w: s.grid_w,
      grid_h: s.grid_h,
      x_step: s.x_step,
      y_step: s.y_step,
      edo: s.edo,
      trail_clobber_radius: s.trail_clobber_radius,
      trails_max: s.trails_max,
      timbres: s.timbres,
      selected: Arc::clone(&selected),
      sounding: Arc::clone(&sounding),
      trail: Arc::clone(&trail),
      volume_pos: Arc::clone(&volume_pos),
      gains: Arc::clone(&gains),
      accrete: Arc::clone(&accrete),
      held_all: Arc::clone(&held_all),
      distortion_on: Arc::clone(&distortion_on),
      slide_on: Arc::clone(&slide_on),
      mono_on: Arc::clone(&mono_on),
      feet_accrete_on: Arc::clone(&feet_accrete_on),
      live: Arc::clone(&live),
      slide: SlideCandidates::new(),
      slide_window: s.slide_window,
      slide_duration_secs: s.slide_duration_secs,
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
  /// The four selectable timbres (shared instrument-wide table, copied per thread).
  timbres: [TimbreSlot; SELECTOR_CELLS],
  volume_rect: [i32; 4],
  /// The grid index this grid's volume strip sets the loudness of.
  volume_controls_index: usize,
  /// The three accrete (sustain) buttons' cells on this grid (`NO_RECT` if absent).
  clear_rect: [i32; 4],
  needs_holding_rect: [i32; 4],
  accrete_rect: [i32; 4],
  /// The global-distortion toggle's cell on this grid (`NO_RECT` if absent).
  distortion_rect: [i32; 4],
  /// The slide / mono toggles' cells on this grid (`NO_RECT` if absent).
  slide_rect: [i32; 4],
  mono_rect: [i32; 4],
  /// The feet-accrete toggle's cell on this grid (`NO_RECT` if absent).
  feet_accrete_rect: [i32; 4],
  grid_w: i32,
  grid_h: i32,
  x_step: i32,
  y_step: i32,
  edo: i32,
  /// Trail clobber radius as a divisor of the octave (see `Settings`).
  trail_clobber_radius: i32,
  /// Max distinct pitch classes the shared trail keeps.
  trails_max: usize,
  /// Per-grid selected timbre slot; written by whichever grid's selector controls it.
  selected: Arc<Mutex<Vec<usize>>>,
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
  /// The global slide / mono switches (both grids' toggles).
  slide_on: Arc<AtomicBool>,
  mono_on: Arc<AtomicBool>,
  /// The global feet-accrete switch (pedals mirror the accrete trio while on).
  feet_accrete_on: Arc<AtomicBool>,
  /// THIS grid's recently-released notes (slide sources) + the slide knobs.
  slide: SlideCandidates,
  slide_window: Duration,
  slide_duration_secs: f32,
  /// The hot-reloadable parameters; refreshed into the fields above when the
  /// generation moves (see `refresh_live`).
  live: Arc<Live>,
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
  let mut live_generation = rt.live.generation.load(Ordering::SeqCst);

  while !STOP.load(Ordering::SeqCst) {
    // Adopt hot-reloaded parameters ('r'): cheap generation check per iteration.
    let generation = rt.live.generation.load(Ordering::SeqCst);
    if generation != live_generation {
      live_generation = generation;
      refresh_live(&mut rt);
    }
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
    let selector_slot = current_slot(&rt.selected, rt.controls_index);
    let volume_col = volume_active_col(&rt);
    let (mut buttons, sustained_classes) = accrete_view(&rt);
    buttons.push((rt.distortion_rect, rt.distortion_on.load(Ordering::Relaxed)));
    buttons.push((rt.slide_rect, rt.slide_on.load(Ordering::Relaxed)));
    buttons.push((rt.mono_rect, rt.mono_on.load(Ordering::Relaxed)));
    buttons.push((rt.feet_accrete_rect, rt.feet_accrete_on.load(Ordering::Relaxed)));
    let mut sounding_classes = union_sounding(&rt.sounding);
    sounding_classes.extend(sustained_classes);
    let trail_classes = trail_set(&rt.trail);
    let levels = levels_for_grid(
      &sounding_classes,
      &trail_classes,
      rt.edo_rect,
      rt.selector_rect,
      selector_slot,
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
  // Selector: a press sets the *controlled* grid's timbre slot (radio; future notes).
  if let Some(slot) = slot_for_selector_cell(rt.selector_rect, cell) {
    if press {
      set_slot(&rt.selected, rt.controls_index, slot);
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
  // Slide / mono toggles: key-down flips the GLOBAL switch; key-up does nothing.
  if in_overlay(rt.slide_rect, cell) {
    if press {
      let _ = rt.slide_on.fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  if in_overlay(rt.mono_rect, cell) {
    if press {
      let _ = rt.mono_on.fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  if in_overlay(rt.feet_accrete_rect, cell) {
    if press {
      let _ = rt.feet_accrete_on.fetch_xor(true, Ordering::Relaxed);
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
    // Mono: a new note cuts this grid's other fingered notes first. The cut goes
    // through the ordinary release path, so accrete still captures it and the cut
    // note lands in the slide candidate set (misc.org: with mono, the candidate
    // set is a singleton -- exactly the note this one cuts).
    if rt.mono_on.load(Ordering::Relaxed) {
      let others: Vec<(i32, i32)> = held.keys().filter(|c| **c != cell).copied().collect();
      for other in others {
        release_cell(rt, held, other);
      }
    }
    let slot = rt.timbres[current_slot(&rt.selected, rt.grid_index)];
    let gain = current_gain(&rt.gains, rt.grid_index);
    let timbre =
      Timbre { waveform: slot.waveform, gain: slot.amplitude * gain, am: slot.am, fm: slot.fm };
    // Slide: while on, re-trigger the nearest recently-released pitch and glide it
    // into this one (consuming it as a source); otherwise a plain note.
    let source = if rt.slide_on.load(Ordering::Relaxed) {
      rt.slide.pick(pitch, Instant::now(), rt.slide_window)
    } else {
      None
    };
    // The note's gain = the slot's amplitude x the grid's fader; the live fader
    // rescale is ratio-based, so the slot amplitude survives later fader moves.
    match source {
      Some(from) => rt.sink.note_on_gliding(cell, pitch, from, timbre, rt.slide_duration_secs),
      None => rt.sink.note_on(cell, pitch, timbre),
    }
    held.insert(cell, pitch);
    rt.accrete.lock().unwrap_or_else(|e| e.into_inner()).note_played(rt.grid_index, pitch);
    push_trail(&rt.trail, pitch.rem_euclid(rt.edo), rt.edo, rt.trail_clobber_radius, rt.trails_max);
  } else {
    release_cell(rt, held, cell);
  }
  publish_held(&rt.held_all, rt.grid_index, held);
  publish_sounding(&rt.sounding, rt.grid_index, held, rt.edo);
}

/// The one release path (finger up, or a mono cut): a note in the sustained set --
/// or released under a live accreting condition -- keeps ringing (its voice moves to
/// the sustain register); anything else releases normally and becomes a slide
/// candidate (sustained notes don't: they are still audible, and sliding "from" a
/// ringing drone would double it).
fn release_cell(rt: &mut GridThread, held: &mut HashMap<(i32, i32), i32>, cell: (i32, i32)) {
  let Some(pitch) = held.get(&cell).copied() else {
    rt.sink.note_off(cell);
    return;
  };
  let keep = rt
    .accrete
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .note_released_sustains(rt.grid_index, pitch);
  if keep {
    rt.sink.sustain_note(cell, pitch);
  } else {
    rt.sink.note_off(cell);
    let mono = rt.mono_on.load(Ordering::Relaxed);
    rt.slide.note_released(pitch, Instant::now(), mono);
  }
  held.remove(&cell);
}

/// Adopt the current `Live` parameters into this grid thread's working copies. An
/// `edo` change invalidates every stored pitch *class*, so the shared trail is
/// cleared (held pitches keep sounding; their classes are recomputed from the raw
/// pitch on the next publish).
fn refresh_live(rt: &mut GridThread) {
  let p = *rt.live.params.lock().unwrap_or_else(|e| e.into_inner());
  if p.edo != rt.edo {
    rt.trail.lock().unwrap_or_else(|e| e.into_inner()).clear();
  }
  rt.x_step = p.x_step;
  rt.y_step = p.y_step;
  rt.edo = p.edo;
  rt.trail_clobber_radius = p.trail_clobber_radius;
  rt.trails_max = p.trails_max;
  rt.slide_window = p.slide_window;
  rt.slide_duration_secs = p.slide_duration_secs;
  rt.timbres = p.timbres;
  rt.sink.retune(p.fund, p.edo, p.sustain_level, p.decay_secs);
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
  capture_all_held_into(&rt.held_all, &rt.accrete);
}

/// `capture_all_held` for callers that aren't a grid thread (the pedal hook).
fn capture_all_held_into(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  accrete: &Arc<Mutex<AccreteState>>,
) {
  let snapshot: Vec<(usize, i32)> = {
    let all = held_all.lock().unwrap_or_else(|e| e.into_inner());
    all
      .iter()
      .enumerate()
      .flat_map(|(g, m)| m.values().map(move |p| (g, *p)))
      .collect()
  };
  accrete.lock().unwrap_or_else(|e| e.into_inner()).capture_held(snapshot);
}

/// Which accrete button a KMSS pedal mirrors while feet-accrete is on. Jeff's
/// mapping (misc.org "feet accrete"): the older (monobright) grid's trio -> pedals
/// 1/2/3, the other grid's -> 8/9/0 -- but both grids share ONE accrete state, so
/// the two pedal triples are simply two copies of the same three buttons, in the
/// on-grid order clear / needs-holding / accrete.
fn feet_accrete_button(pedal: u8) -> Option<AccreteControlKind> {
  match pedal {
    1 | 8 => Some(AccreteControlKind::Clear),
    2 | 9 => Some(AccreteControlKind::NeedsHolding),
    3 | 0 => Some(AccreteControlKind::Accrete),
    _ => None,
  }
}

/// Build the drumkit pedal hook that mirrors the accrete trio onto the KMSS while
/// `feet_accrete_on` is set (TODO/misc.org "feet accrete"). Consuming an event
/// suppresses that pedal's sample; with the toggle off every pedal drums as usual.
/// A pedal "press" is the decoder's Fire (down) and its Release (up), so holding
/// pedal 3 or 0 is exactly holding the accrete button.
fn feet_accrete_hook(
  feet_accrete_on: Arc<AtomicBool>,
  accrete: Arc<Mutex<AccreteState>>,
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) -> drumkit_runtime::PedalHook {
  Arc::new(move |pedal, down| {
    if !feet_accrete_on.load(Ordering::Relaxed) {
      return false;
    }
    let Some(button) = feet_accrete_button(pedal) else {
      return false;
    };
    // Decide under the accrete lock, touch voices after it drops (the module's
    // no-nested-locks rule), exactly like the on-grid buttons.
    let mut activated = false;
    {
      let mut state = accrete.lock().unwrap_or_else(|e| e.into_inner());
      match (button, down) {
        (AccreteControlKind::Clear, true) => state.press_clear(),
        (AccreteControlKind::Clear, false) => state.release_clear(),
        (AccreteControlKind::NeedsHolding, true) => activated = state.press_needs_holding(),
        (AccreteControlKind::NeedsHolding, false) => {}
        (AccreteControlKind::Accrete, true) => activated = state.press_accrete(),
        (AccreteControlKind::Accrete, false) => state.release_accrete(),
      }
    }
    if button == AccreteControlKind::Clear && down {
      synth::release_sustained_voices(&voices, release_secs, sample_rate);
    }
    if activated {
      capture_all_held_into(&held_all, &accrete);
    }
    true
  })
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
  let old_gain = {
    let mut g = rt.gains.lock().unwrap_or_else(|e| e.into_inner());
    match g.get_mut(target) {
      Some(slot) => std::mem::replace(slot, gain),
      None => gain,
    }
  };
  // Ratio-rescale the sounding voices so each keeps its timbre slot's amplitude.
  rescale_grid_gain(&rt.voices, target, gain / old_gain);
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

fn current_slot(selected: &Arc<Mutex<Vec<usize>>>, index: usize) -> usize {
  let guard = selected.lock().unwrap_or_else(|e| e.into_inner());
  guard.get(index).copied().unwrap_or(DEFAULT_SLOT)
}

fn set_slot(selected: &Arc<Mutex<Vec<usize>>>, index: usize, slot: usize) {
  let mut guard = selected.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(entry) = guard.get_mut(index) {
    *entry = slot;
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
  fn selector_writes_the_controlled_grids_timbre_slot() {
    // A grid's selector re-timbres the grid at `controls_index`, leaving every other
    // entry untouched. Wire grid 0's strip to grid 1 (cross-control is still legal
    // config even though the current rigs self-control): a press on grid 0's cell 3
    // must select slot 3 for grid 1 only.
    let selected = Arc::new(Mutex::new(vec![DEFAULT_SLOT; 2]));
    set_slot(&selected, 1, 3); // grid 0's selector -> grid 1
    assert_eq!(current_slot(&selected, 1), 3, "grid 1 (controlled) got slot 3");
    assert_eq!(current_slot(&selected, 0), DEFAULT_SLOT, "grid 0 unchanged");
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
    // Each grid's selector controls its OWN grid (per TODO/misc.org, 2026-07; the
    // looper-plus-edo rig keeps its cross-surface timbre editing, which is a
    // different mechanism entirely).
    assert_eq!(s.grids[0].controls_index, 0, "grid 0's strip re-timbres grid 0");
    assert_eq!(s.grids[1].controls_index, 1, "grid 1's strip re-timbres grid 1");
    // Both grids carry a scroll pad, a selector, the accrete trio, and the
    // toggles -- but NO volume strip (dropped per misc.org "drop the amplitude
    // row": [[timbres]] amplitude replaced it).
    for g in &s.grids {
      assert_ne!(g.scroll_rect, NO_RECT, "grid {:?} has a scroll pad", g.monome_id);
      assert_ne!(g.selector_rect, NO_RECT, "grid {:?} has a selector", g.monome_id);
      assert_eq!(g.volume_rect, NO_RECT, "grid {:?} has no volume strip", g.monome_id);
      assert_eq!(g.clear_rect, [0, 15, 0, 15], "grid {:?} clear button", g.monome_id);
      assert_eq!(g.needs_holding_rect, [1, 15, 1, 15], "grid {:?} needs-holding", g.monome_id);
      assert_eq!(g.accrete_rect, [2, 15, 2, 15], "grid {:?} accrete button", g.monome_id);
      assert_eq!(g.distortion_rect, [0, 1, 0, 1], "grid {:?} distortion toggle", g.monome_id);
      assert_eq!(g.slide_rect, [1, 1, 1, 1], "grid {:?} slide toggle", g.monome_id);
      assert_eq!(g.mono_rect, [1, 2, 1, 2], "grid {:?} mono toggle", g.monome_id);
      assert_eq!(g.feet_accrete_rect, [0, 14, 0, 14], "grid {:?} feet-accrete", g.monome_id);
    }
    // The [surfaces] slide knobs flow into the settings.
    assert_eq!(s.slide_window, Duration::from_millis(1000));
    assert!((s.slide_duration_secs - 0.1).abs() < 1e-6);
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
        if let Err(e) = run(&config, detector_port, true, None) {
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

    // The old volume strip is gone (misc.org "drop the amplitude row"): its cells
    // are ordinary play cells now -- pressing (10,0) sounds a note (lights bright)
    // and releases into the dim trail rather than moving any fader.
    a.press(10, 0);
    assert!(wait_until(secs(3), || a.level_at(10, 0) == 15), "(10,0) is a play cell now");
    a.release(10, 0);
    assert!(wait_until(secs(3), || a.level_at(10, 0) == 4), "released into the trail");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  #[test]
  fn adopt_config_swaps_the_live_parameters_and_bumps_the_generation() {
    // Start from the mock config, then "edit" it (amplitude, tuning, a timbre, the
    // slide knobs) and reload: the live params reflect every change and the
    // generation moves, so grid threads and the audio callback pick them up.
    let base = load_named_config("2-monomes_58-8-1_kmss-drums-mock").expect("config loads");
    let s = resolve_settings(&base).expect("resolves");
    let live = Live { generation: AtomicU64::new(0), params: Mutex::new(live_params(&s)) };

    let source = std::fs::read_to_string(
      midi_pulse::config::mock_config_dir().join("2-monomes_58-8-1_kmss-drums-mock.toml"),
    )
    .expect("read mock toml");
    let edited = source
      .replace("amplitude = 0.15", "amplitude = 0.25")
      .replace("edo = 58", "edo = 41")
      .replace("x_step = 8", "x_step = 7")
      .replace(WAVE_SQUARE, "waveform = \"square\"\nfm_depth_cents = 25.0")
      .replace("slide_duration_ms = 100", "slide_duration_ms = 250");
    let config = midi_pulse::config::parse_config(&edited).expect("edited config parses");
    adopt_config(&config, &live).expect("adopts");

    assert_eq!(live.generation.load(Ordering::SeqCst), 1, "generation bumped");
    let p = live.params.lock().unwrap();
    assert_eq!(p.amplitude, 0.25);
    assert_eq!(p.edo, 41);
    assert_eq!(p.x_step, 7);
    assert_eq!(p.timbres[2].fm.depth_cents, 25.0, "timbre slot 2 gained vibrato");
    assert!((p.slide_duration_secs - 0.25).abs() < 1e-6);
  }

  /// The `[[timbres]]` square entry, replaced by the reload test.
  const WAVE_SQUARE: &str = "waveform = \"square\"";

  #[test]
  fn the_pedal_hook_mirrors_the_accrete_trio_only_while_on() {
    use crate::types::Timbre;

    let feet_on = Arc::new(AtomicBool::new(false));
    let accrete = Arc::new(Mutex::new(AccreteState::new()));
    let held_all = Arc::new(Mutex::new(vec![HashMap::new(); 2]));
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let hook = feet_accrete_hook(
      Arc::clone(&feet_on),
      Arc::clone(&accrete),
      Arc::clone(&held_all),
      Arc::clone(&voices),
      0.05,
      48000.0,
    );

    // Toggle off: nothing is consumed; the pedals drum as usual.
    assert!(!hook(3, true), "off: pedal 3 stays a drum pad");
    assert!(!accrete.lock().unwrap().accreting());

    feet_on.store(true, Ordering::Relaxed);
    // Unmapped pedals still drum even while on.
    assert!(!hook(4, true), "pedal 4 (closed hat) is never mirrored");
    // Pedal 3 = accrete: default needs-holding is OFF, so a tap toggles the mode --
    // and the activation captures currently-held notes from BOTH grids' registries.
    held_all.lock().unwrap()[1].insert((2, 3), 44);
    assert!(hook(3, true), "on: pedal 3 is consumed");
    hook(3, false);
    assert!(accrete.lock().unwrap().accreting(), "accrete mode toggled by foot");
    assert!(
      accrete.lock().unwrap().note_released_sustains(1, 44),
      "the held note was captured on activation",
    );
    // Sustain a voice, then clear from pedal 8 (the other triple's clear).
    let mut sink = SurfaceSink::new(0, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5);
    sink.note_on((5, 5), 20, Timbre::default());
    sink.sustain_note((5, 5), 20);
    assert!(hook(8, true), "pedal 8 = clear, consumed");
    hook(8, false);
    let v = voices.lock().unwrap();
    let drone = v.values().next().expect("the drone still ramps out");
    assert_eq!(drone.target_env, 0.0, "clear released the sustained voice");
    assert!(
      accrete.lock().unwrap().sustained_classes(58).is_empty(),
      "the set was flushed (accrete mode itself stays on -- clear never exits it)",
    );
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
        if let Err(e) = run(&config, detector_port, true, None) {
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

  /// The global toggles (distortion / slide / mono) end-to-end: each rests dim, a
  /// press lights it on BOTH grids (one global switch each), a second press (from
  /// the other grid) turns it off.
  #[test]
  fn global_toggles_mirror_across_grids() {
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
        if let Err(e) = run(&config, detector_port, true, None) {
          eprintln!("mock distortion run error: {e}");
        }
      })
    };

    let a = rig.grid(0);
    let b = rig.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");
    // (0,1) distortion, (1,1) slide, (1,2) mono, (0,14) feet-accrete -- same
    // toggle machinery each.
    for (x, y, name) in
      [(0, 1, "distortion"), (1, 1, "slide"), (1, 2, "mono"), (0, 14, "feet-accrete")]
    {
      assert!(wait_until(secs(3), || a.level_at(x, y) == 4 && b.level_at(x, y) == 4),
        "{name} rests dim on both grids");
      a.press(x, y);
      a.release(x, y);
      assert!(wait_until(secs(3), || a.level_at(x, y) == 15), "{name} on: lit on grid a");
      assert!(wait_until(secs(3), || b.level_at(x, y) == 15), "{name} on: lit on grid b (global)");
      b.press(x, y);
      b.release(x, y);
      assert!(wait_until(secs(3), || a.level_at(x, y) == 4 && b.level_at(x, y) == 4),
        "{name} off again from the other grid");
    }

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
        if let Err(e) = run(&config, detector_port, true, None) {
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
