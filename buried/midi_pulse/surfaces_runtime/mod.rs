//! The surfaces runtime: two independent EDO play grids (each with a scroll pad and a
//! waveform selector + volume strip -- wired per rig via `controls`, which the
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
mod dance;
mod edit;
mod grid;
mod polyrhythm;
mod pulse_window;
mod readout;
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

use midi_pulse::rig::{
  load_named_rig, AccreteControlKind, AmShapeFamilyRig, EditmodeControlKind, MonomeWindowRig,
  PulseFactorRig, Rig, SinkRig, SoftstepWindowRig, WaveformChoice,
};
use midi_pulse::device_assign::assign_selected_devices;
use midi_pulse::edo_play::{register_delta, shift_for_cell, step_for_cell};
use midi_pulse::monome::{self, DeviceInfo};
use midi_pulse::expression_pedals::NUM_PEDALS;
use midi_pulse::monome_brightness::PulseBrightness;

use crate::drumkit_runtime;
use crate::types::{Am, AmShapeFamily, Fm, RelAm, RelFm, Timbre, VoiceMap, Waveform};
use crate::voices::{Distortion, Makeup};

use accrete::AccreteState;
use polyrhythm::{TempoFactorButton, PolyrhythmState};
use slide::SlideCandidates;
use grid::{
  button_level, levels_for_grid, slot_for_selector_cell, volume_cells, volume_gain_for_pos,
  ButtonOverlay, BRIGHT, DIM, OFF, SELECTOR_CELLS,
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

/// One selectable timbre behind a selector cell: the resolved form of a rig
/// `[[timbres]]` entry (or a plain-waveform default). `amplitude` multiplies below
/// the grid's volume fader at note-on.
#[derive(Clone, Copy)]
pub(crate) struct TimbreSlot {
  waveform: Waveform,
  amplitude: f32,
  am: Am,
  fm: Fm,
  rel_am: RelAm,
  rel_fm: RelFm,
}

/// The hot-reloadable ('r' + Enter) subset of the settings -- the scalars a running
/// instrument can adopt without rebinding grids, sockets, or streams: the synth
/// master amplitude, the distortion curve, the four timbres, the tuning, the pluck
/// envelope, and the slide/trail knobs. See TODO/misc.org "rig reload".
pub(crate) struct Live {
  /// Bumped on every successful reload; grid threads refresh their copies when it
  /// moves (the audio callback reads the params fresh every callback).
  generation: AtomicU64,
  params: Mutex<LiveParams>,
  /// The distortion's makeup table. Separate from `LiveParams` (which is `Copy`, so
  /// every grid thread can snapshot it) because the table is an allocation, and built
  /// here -- off the audio thread -- because building it integrates the clipper's gain
  /// 512 times. The audio callback only clones the `Arc`.
  makeup: Mutex<Arc<Makeup>>,
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
  pub tap_window: Duration,
  pub trail_clobber_radius: i32,
  pub trails_max: usize,
  /// The EX-P volume pedals, indexed by pedal (= MPC-20 channel - 1):
  /// `(grid index, taper)`. Live so Jeff can tweak the curve_* knobs mid-play
  /// ('r' reload); the pedal thread re-reads them each poll. Rebinding or
  /// removing a pedal live also takes effect, but a rig that STARTS with no
  /// pedals never spawns the thread -- adding the first pedal needs a restart,
  /// like any other bound resource.
  pub expression_pedals: [Option<(usize, PedalVolumeCurve)>; NUM_PEDALS],
}

/// The live subset of resolved settings.
fn live_params(s: &Settings) -> LiveParams {
  let mut expression_pedals = [None; NUM_PEDALS];
  for &(pedal, grid, curve) in &s.expression_pedals {
    if let Some(slot) = expression_pedals.get_mut(pedal) {
      *slot = Some((grid, curve));
    }
  }
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
    tap_window: s.tap_window,
    trail_clobber_radius: s.trail_clobber_radius,
    trails_max: s.trails_max,
    expression_pedals,
  }
}

/// The distortion's makeup table for these settings. Integrates the clipper's Gaussian
/// RMS gain, so it is rebuilt only on load / hot-reload, never in the audio callback.
fn live_makeup(s: &Settings) -> Arc<Makeup> {
  Arc::new(Makeup::new(
    s.distortion.shape,
    s.distortion_makeup,
    s.distortion_auto_makeup,
    s.distortion_makeup_slew_secs,
  ))
}

/// Re-read `name`'s rig and adopt its live parameters (everything else -- window
/// layout, ports, sinks' sample rates -- needs a restart and is silently kept as
/// is). A rig that fails to load or resolve leaves the old parameters running.
fn reload_live(name: &str, live: &Live) {
  match load_named_rig(name).and_then(|rig| adopt_rig(&rig, live)) {
    Ok(()) => println!(
      "reloaded {name}: amplitude / distortion / timbres / tuning / pluck / slide / trail / pedal curves applied (layout + ports need a restart)",
    ),
    Err(e) => eprintln!("reload of {name} failed; keeping the running parameters: {e}"),
  }
}

/// Adopt `rig`'s live parameters into `live` and bump the generation. Everything
/// non-live (window layout, ports, sinks' sample rates) is silently kept as-is.
fn adopt_rig(rig: &Rig, live: &Live) -> Result<(), String> {
  let s = resolve_settings(rig).map_err(|e| e.to_string())?;
  // Build the makeup table before taking either lock: the integration is the slow part,
  // and the audio callback holds `makeup` briefly on every callback.
  let makeup = live_makeup(&s);
  *live.params.lock().unwrap_or_else(|e| e.into_inner()) = live_params(&s);
  *live.makeup.lock().unwrap_or_else(|e| e.into_inner()) = makeup;
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
    rel_am: RelAm::default(),
    rel_fm: RelFm::default(),
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

pub fn run_from_rig(
  rig: &Rig,
  reload_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
  print_inventory(rig);
  // Block SIGINT/SIGTERM and start the STOP-setting waiter BEFORE any audio/MIDI/grid
  // thread spawns, so the block is inherited by all of them (a stray default SIGINT
  // would otherwise kill the process, leaving the KMSS stuck in tether mode).
  install_signals();
  // Headless / mock runs set MIDI_PULSE_NO_AUDIO to skip the cpal stream.
  let no_audio = std::env::var_os("MIDI_PULSE_NO_AUDIO").is_some();
  STOP.store(false, Ordering::SeqCst);
  run(rig, monome::detector_port(), no_audio, reload_name)
}

fn print_inventory(rig: &Rig) {
  println!(
    "surfaces: {} monome windows across {} monomes; {} softstep windows",
    rig.monome_windows.len(),
    rig.monomes.len(),
    rig.softstep_windows.len(),
  );
  for monome in &rig.monomes {
    println!("  monome {:?} (port {}, prefix {:?}):", monome.id, monome.listen_port, monome.prefix);
    for window in rig.monome_windows.iter().filter(|w| w.monome() == monome.id) {
      println!("    {:<18} rect {:?}", window.kind_name(), window.rect());
    }
  }
}

/// One play grid's resolved rig: its monome binding + its overlay rects + which
/// grid its selector re-timbres.
struct GridSettings {
  monome_id: String,
  /// This grid's FULL device selector, not just its size. A rig that pins grids by
  /// serial (`select.id_contains`) makes the left/right assignment independent of
  /// serialosc's enumeration order -- which matters the moment anything off-grid
  /// (a foot pedal) targets "the left monome" by name.
  select: midi_pulse::rig::MonomeSelect,
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
  /// The accrete (sustain) buttons' cells, `NO_RECT` when absent. Each grid's
  /// trio (+ optional erase) drives its own per-monome accrete bank.
  clear_rect: [i32; 4],
  needs_holding_rect: [i32; 4],
  accrete_rect: [i32; 4],
  erase_rect: [i32; 4],
  /// The global-distortion toggle's cell, `NO_RECT` when absent (one shared switch).
  distortion_rect: [i32; 4],
  /// The slide / mono toggles' cells, `NO_RECT` when absent (global switches too).
  slide_rect: [i32; 4],
  mono_rect: [i32; 4],
  /// The feet-accrete (softstep-accretes) toggle's cell, `NO_RECT` when absent.
  /// Per-grid: it enables the pedal mirror for THIS grid's accrete bank.
  feet_accrete_rect: [i32; 4],
  /// The 3x2 polyrhythm pad's rect (x3/x2/tap over /3//2/=1), `NO_RECT` when absent.
  poly_rect: [i32; 4],
  /// The editmode_control buttons' cells, `NO_RECT` when absent: clear empties
  /// this grid's edit mode, accrete fills it with every sounding voice.
  editmode_clear_rect: [i32; 4],
  editmode_accrete_rect: [i32; 4],
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
  /// The four selectable timbres (rig `[[timbres]]`, or the plain waveforms).
  timbres: [TimbreSlot; SELECTOR_CELLS],
  /// The instrument-wide AM LFO morph family (rig `[am]`).
  am_shape_family: AmShapeFamily,
  /// The global distortion's curve (scale + shape from the cpal_synth sink); the
  /// on/off lives in a shared AtomicBool flipped by the distortion_toggle windows.
  distortion: Distortion,
  /// The distortion's loudness compensation (cpal_synth `distortion_makeup` /
  /// `distortion_auto_makeup`): the trim, and whether to restore the clean bus's RMS.
  distortion_makeup: f32,
  distortion_auto_makeup: bool,
  /// Lag on the applied makeup (cpal_synth `distortion_makeup_slew_ms`). 0 = exact.
  distortion_makeup_slew_secs: f32,
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
  /// The tap-tempo pairing window (`[surfaces].tap_tempo_window_ms`).
  tap_window: Duration,
  /// Echo each fingered note to stderr (top-level `echo_input`). Off by default so the
  /// startup red report of components that couldn't load stays on screen instead of
  /// scrolling away under key echoes as you play.
  echo_input: bool,
  /// The EX-P volume pedals (`[[expression_pedals]]`), resolved to
  /// `(pedal index = MPC-20 channel - 1, grid index, taper)`.
  expression_pedals: Vec<(usize, usize, PedalVolumeCurve)>,
}

fn resolve_settings(rig: &Rig) -> Result<Settings, Box<dyn std::error::Error>> {
  // The play grids: every declared monome that carries an edo_note_grid, in rig
  // order (grid index = position here).
  let grid_monomes: Vec<&str> = rig
    .monomes
    .iter()
    .map(|m| m.id.as_str())
    .filter(|id| {
      rig.monome_windows.iter().any(|w| {
        matches!(w, MonomeWindowRig::EdoNoteGrid { monome, .. } if monome == id)
      })
    })
    .collect();
  if grid_monomes.is_empty() {
    return Err("a surfaces rig needs at least one edo_note_grid".into());
  }
  let index_of = |monome_id: &str| grid_monomes.iter().position(|m| *m == monome_id);

  // The tuning + sink come from the first edo grid (all grids share them here).
  let (tuning_id, sink_id) = rig
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowRig::EdoNoteGrid { tuning, sink, .. } => Some((tuning.clone(), sink.clone())),
      _ => None,
    })
    .ok_or("a surfaces rig needs an edo_note_grid")?;
  let tuning = rig
    .tunings
    .iter()
    .find(|t| t.id == tuning_id)
    .ok_or("edo_note_grid references an unknown tuning")?;
  let sink = rig
    .sinks
    .iter()
    .find(|s| s.id() == sink_id)
    .ok_or("edo_note_grid references an unknown sink")?;
  let SinkRig::CpalSynth {
    sample_rate, buffer_frames, amplitude, attack_secs, release_secs, oversample,
    distortion_scale, distortion_shape, distortion_makeup, distortion_auto_makeup,
    distortion_makeup_slew_ms, sustain_level, decay_secs, ..
  } = sink
  else {
    return Err("surfaces requires a cpal_synth sink for the play grids".into());
  };

  let rect_on = |monome_id: &str, pred: fn(&MonomeWindowRig) -> bool| {
    rig
      .monome_windows
      .iter()
      .find(|w| w.monome() == monome_id && pred(w))
      .map(|w| w.rect())
  };

  let mut grids = Vec::new();
  for monome_id in &grid_monomes {
    let monome_cfg = rig
      .monomes
      .iter()
      .find(|m| m.id == *monome_id)
      .ok_or("a play-grid monome is not declared")?;
    let edo_rect = rect_on(monome_id, |w| matches!(w, MonomeWindowRig::EdoNoteGrid { .. }))
      .ok_or("a play grid lost its edo_note_grid")?;
    let scroll_rect =
      rect_on(monome_id, |w| matches!(w, MonomeWindowRig::EdoShiftPad { .. })).unwrap_or(NO_RECT);
    // The selector rect + which grid it controls.
    let selector = rig.monome_windows.iter().find_map(|w| match w {
      MonomeWindowRig::WaveformSelector { monome, rect, controls, .. } if monome == monome_id => {
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
    let volume = rig.monome_windows.iter().find_map(|w| match w {
      MonomeWindowRig::VolumeStrip { monome, rect, controls, .. } if monome == monome_id => {
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
      rig
        .monome_windows
        .iter()
        .find_map(|w| match w {
          MonomeWindowRig::AccreteControl { monome, rect, control, .. }
            if monome == monome_id && *control == kind =>
          {
            Some(*rect)
          }
          _ => None,
        })
        .unwrap_or(NO_RECT)
    };
    let distortion_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::DistortionToggle { monome, rect, .. } if monome == monome_id => {
          Some(*rect)
        }
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let slide_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::SlideToggle { monome, rect, .. } if monome == monome_id => Some(*rect),
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let mono_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::MonoToggle { monome, rect, .. } if monome == monome_id => Some(*rect),
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let feet_accrete_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::SoftstepAccretesToggle { monome, rect, .. } if monome == monome_id => {
          Some(*rect)
        }
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let poly_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::TapTempoPad { monome, rect, .. } if monome == monome_id => Some(*rect),
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let editmode_rect_on = |wanted: EditmodeControlKind| {
      rig
        .monome_windows
        .iter()
        .find_map(|w| match w {
          MonomeWindowRig::EditmodeControl { monome, rect, control, .. }
            if monome == monome_id && *control == wanted =>
          {
            Some(*rect)
          }
          _ => None,
        })
        .unwrap_or(NO_RECT)
    };
    grids.push(GridSettings {
      monome_id: monome_id.to_string(),
      select: monome_cfg.select.clone(),
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
      erase_rect: accrete_rect_on(AccreteControlKind::Erase),
      distortion_rect,
      slide_rect,
      mono_rect,
      feet_accrete_rect,
      poly_rect,
      editmode_clear_rect: editmode_rect_on(EditmodeControlKind::Clear),
      editmode_accrete_rect: editmode_rect_on(EditmodeControlKind::Accrete),
    });
  }

  let size = rig
    .monomes
    .iter()
    .find(|m| m.id == grids[0].monome_id)
    .and_then(|m| m.select.size)
    .unwrap_or([16, 16]);

  // The `[surfaces]` table (trail knobs); absent -> defaults, so unchanged behaviour.
  let surfaces = rig.surfaces.unwrap_or_default();

  // The EX-P volume pedals, by grid index (= position in `grids`). Validation
  // already pinned channel to 1|2, the monome to a play grid, and the curve
  // parameters to their legal ranges.
  let expression_pedals = rig
    .expression_pedals
    .iter()
    .filter_map(|p| {
      grids.iter().position(|g| g.monome_id == p.monome).map(|grid| {
        let curve = PedalVolumeCurve {
          lin_frac: p.curve_initial_lin_frac,
          exp_db: p.curve_remainder_exp_db,
        };
        (p.channel as usize - 1, grid, curve)
      })
    })
    .collect();

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
    timbres: resolve_timbre_slots(rig),
    am_shape_family: match rig.am.as_ref().map(|a| a.shape.family).unwrap_or_default() {
      AmShapeFamilyRig::SinToSquare => AmShapeFamily::SinToSquare,
      AmShapeFamilyRig::TriToSquare => AmShapeFamily::TriToSquare,
    },
    distortion: Distortion { scale: *distortion_scale, shape: *distortion_shape },
    distortion_makeup: *distortion_makeup,
    distortion_auto_makeup: *distortion_auto_makeup,
    distortion_makeup_slew_secs: *distortion_makeup_slew_ms / 1000.0,
    has_drums: !rig.softstep_windows.is_empty(),
    trail_clobber_radius: surfaces.trail_clobber_radius,
    trails_max: surfaces.trails_max,
    slide_window: Duration::from_millis(surfaces.slide_candidate_window_ms),
    slide_duration_secs: surfaces.slide_duration_ms as f32 / 1000.0,
    tap_window: Duration::from_millis(surfaces.tap_tempo_window_ms),
    echo_input: rig.echo_input,
    expression_pedals,
  })
}

/// The rig's `[[timbres]]` mapped onto the four selector slots (validation
/// guarantees exactly four when present); absent = the plain waveforms.
fn resolve_timbre_slots(rig: &Rig) -> [TimbreSlot; SELECTOR_CELLS] {
  if rig.timbres.is_empty() {
    return default_timbre_slots();
  }
  let mut slots = default_timbre_slots();
  for (slot, t) in slots.iter_mut().zip(&rig.timbres) {
    *slot = TimbreSlot {
      waveform: match t.waveform {
        WaveformChoice::Sine => Waveform::Sine,
        WaveformChoice::Triangle => Waveform::Triangle,
        WaveformChoice::Square => Waveform::Square,
        WaveformChoice::Saw => Waveform::Saw,
      },
      amplitude: t.amplitude,
      am: Am { depth: t.abs_am_depth, freq: t.abs_am_hz, shape: t.am_shape },
      fm: Fm { depth_cents: t.abs_fm_depth_cents, freq: t.abs_fm_hz },
      rel_am: RelAm { depth: t.rel_am_depth, freq: t.rel_am_freq },
      rel_fm: RelFm { depth: t.rel_fm_depth, freq: t.rel_fm_freq },
    };
  }
  slots
}

/// The bring-up decision for one run: given which configured grids have a live device
/// (and whether the SoftStep is present), what loads and what is skipped for a missing
/// dependency. The clean statement of the independence rule: *a component loads iff
/// every device it functionally depends on is present.* Pure -- unit-tested without
/// hardware (see `plan_*` tests).
struct BringUp {
  /// Per grid index: does this grid have a live device to bind?
  present: Vec<bool>,
  /// Per grid index: drop this grid's waveform selector (it cross-controls an absent
  /// grid, so it would do nothing -- its cells become plain play cells).
  drop_selector: Vec<bool>,
  /// Per grid index: drop this grid's volume strip (same reason).
  drop_volume: Vec<bool>,
  /// Bring up the KMSS drumkit? (its SoftStep is present).
  drums: bool,
  /// Human-readable red lines: every component skipped for a missing dependency, in
  /// the player's terms. Empty when everything the rig declares came up.
  report: Vec<String>,
}

impl BringUp {
  fn any_grid(&self) -> bool {
    self.present.iter().any(|p| *p)
  }
}

/// Work out [`BringUp`] from the resolved grids, which ones are present, and the
/// SoftStep's presence. The dependencies:
/// - a grid's own windows depend on that grid's device;
/// - a waveform selector / volume strip that *cross-controls* another grid also
///   depends on that grid (the "one monome controls the other's timbre" case);
/// - the drumkit depends only on its SoftStep -- never on any grid (it is a
///   standalone sample-trigger surface).
fn plan_bringup(
  grids: &[GridSettings],
  present: &[bool],
  want_drums: bool,
  softstep_present: bool,
) -> BringUp {
  let n = grids.len();
  let mut drop_selector = vec![false; n];
  let mut drop_volume = vec![false; n];
  let mut report = Vec::new();

  // Absent grids: their whole window set (play surface + every overlay) does not load.
  for (i, g) in grids.iter().enumerate() {
    if !present[i] {
      report.push(format!(
        "monome {:?} is not connected -- its play surface and all its overlays did not load",
        g.monome_id,
      ));
    }
  }
  // Cross-surface functional dependencies. A present grid whose selector/volume strip
  // controls an ABSENT grid can affect nothing, so drop it (reported).
  for (i, g) in grids.iter().enumerate() {
    if !present[i] {
      continue;
    }
    if g.selector_rect != NO_RECT && !present[g.controls_index] {
      drop_selector[i] = true;
      report.push(format!(
        "monome {:?}'s waveform selector re-timbres monome {:?}, which is not connected \
         -- the selector did not load (those cells play notes instead)",
        g.monome_id, grids[g.controls_index].monome_id,
      ));
    }
    if g.volume_rect != NO_RECT && !present[g.volume_controls_index] {
      drop_volume[i] = true;
      report.push(format!(
        "monome {:?}'s volume strip sets monome {:?}, which is not connected \
         -- the volume strip did not load",
        g.monome_id, grids[g.volume_controls_index].monome_id,
      ));
    }
  }
  // The drumkit depends only on the SoftStep.
  let drums = want_drums && softstep_present;
  if want_drums && !softstep_present {
    report.push("the SoftStep is not connected -- the drumkit did not load".to_string());
  }

  BringUp { present: present.to_vec(), drop_selector, drop_volume, drums, report }
}

/// Print the red report of components skipped for a missing dependency. Silent when
/// nothing was skipped. Coloured only on a terminal (so a redirected log stays clean).
fn print_missing_report(report: &[String]) {
  if report.is_empty() {
    return;
  }
  use std::io::IsTerminal;
  let (red, reset) = if std::io::stderr().is_terminal() {
    ("\x1b[1;31m", "\x1b[0m")
  } else {
    ("", "")
  };
  eprintln!("{red}surfaces: some components did not load (missing gear):{reset}");
  for line in report {
    eprintln!("{red}  - {line}{reset}");
  }
  eprintln!(
    "{red}  (playing what loaded; reconnect the gear and restart to get the rest){reset}"
  );
}

/// The I/O shell. `detector_port` is the serialosc(-mock) port to discover grids on;
/// `no_audio` skips the cpal stream (headless / mock). Loops until STOP. Signal
/// handling is installed by `run_from_rig`, not here, so tests can call this
/// directly and stop it by setting STOP.
fn run(
  rig: &Rig,
  detector_port: u16,
  no_audio: bool,
  reload_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
  let s = resolve_settings(rig)?;
  let num_grids = s.grids.len();
  // The hot-reloadable parameters ('r' + Enter re-reads the rig; see `Live`).
  let live = Arc::new(Live {
    generation: AtomicU64::new(0),
    params: Mutex::new(live_params(&s)),
    makeup: Mutex::new(live_makeup(&s)),
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
    println!("press 'r' + Enter to hot-reload the rig (amplitude / timbres / tuning / pluck / slide / trail / distortion curve + makeup / pedal curves).");
  }

  // Discover whatever grids are actually connected and assign each configured grid a
  // distinct live device, tolerating absence: `assign_available_devices` leaves the
  // absent grids as `None` instead of erroring. A missing grid then disables only the
  // components that depend on it; everything else still loads (TODO.org "robust to
  // missing gear"). With both grids present this is the old behaviour exactly.
  let sock0 = UdpSocket::bind(("0.0.0.0", s.grids[0].listen_port))
    .map_err(|e| format!("bind UDP :{}: {e}", s.grids[0].listen_port))?;
  sock0.set_read_timeout(Some(Duration::from_millis(50)))?;
  let devices = monome::discover_devices_via(&sock0, s.grids[0].listen_port, detector_port);
  let selects: Vec<midi_pulse::rig::MonomeSelect> =
    s.grids.iter().map(|g| g.select.clone()).collect();
  let assigned: Vec<Option<DeviceInfo>> = assign_selected_devices(&devices, &selects);
  let present: Vec<bool> = assigned.iter().map(Option::is_some).collect();
  for (g, dev) in s.grids.iter().zip(&assigned) {
    if let Some(d) = dev {
      println!("surfaces: grid {:?} -> id={:?} port={}", g.monome_id, d.id, d.port);
    }
  }
  // Drain leftover /serialosc/device enumeration replies so grid 0's first recv is a
  // key event, not a stale reply.
  let mut drain = [0u8; 2048];
  while sock0.recv_from(&mut drain).is_ok() {}

  // Is the KMSS actually plugged in? The drumkit is a *standalone* sample-trigger
  // surface -- it loads whenever its SoftStep is present, even with no grids connected
  // (TODO.org: "the softstep ... should load ... even if there are no monomes
  // present"). The probe opens nothing.
  let softstep_present = s.has_drums && drumkit_runtime::any_softstep_present(rig);

  // Decide what loads and what is skipped for a missing dependency (reported in red).
  let plan = plan_bringup(&s.grids, &present, s.has_drums, softstep_present);
  if !plan.any_grid() && !plan.drums {
    return Err(
      "no gear present: found no monome grids and no SoftStep -- nothing to run \
       (reconnect a grid or the SoftStep, or check serialosc)"
        .into(),
    );
  }

  // Sockets, one per PRESENT grid (grid 0 reuses the discovery socket; an absent grid
  // gets none, and no thread). The index space stays 0..num_grids so all the shared
  // per-grid state (the `Arc<Vec<_>>`s) keeps its indices -- absent grids just leave
  // idle slots.
  let mut sockets: Vec<Option<UdpSocket>> = Vec::with_capacity(num_grids);
  if present[0] {
    sockets.push(Some(sock0));
  } else {
    drop(sock0);
    sockets.push(None);
  }
  for (i, g) in s.grids.iter().enumerate().skip(1) {
    if present[i] {
      let sock = UdpSocket::bind(("0.0.0.0", g.listen_port))
        .map_err(|e| format!("bind UDP :{}: {e}", g.listen_port))?;
      sock.set_read_timeout(Some(Duration::from_millis(50)))?;
      sockets.push(Some(sock));
    } else {
      sockets.push(None);
    }
  }

  // Shared audio: one voice map + one synth stream; each voice carries its grid's
  // waveform and its grid's volume gain, and the render sums them all. The cpal_synth
  // sink's `amplitude` is the single master "synth volume" (both grids); the per-grid
  // volume strips are live trims that multiply below it.
  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
  // The per-grid distortion switches (misc.org "distortion / per-monome"): grid g's
  // toggle routes grid g's voices through the distorted bus in the audio callback.
  let distortion_on: Arc<Vec<AtomicBool>> =
    Arc::new((0..num_grids).map(|_| AtomicBool::new(false)).collect());
  // The per-grid slide / mono switches (each grid keeps its own candidate history
  // too; every toggle in this rig is per-monome now).
  let slide_on: Arc<Vec<AtomicBool>> =
    Arc::new((0..num_grids).map(|_| AtomicBool::new(false)).collect());
  let mono_on: Arc<Vec<AtomicBool>> =
    Arc::new((0..num_grids).map(|_| AtomicBool::new(false)).collect());
  // Feet accrete, one switch per grid: while grid g's toggle is on, the KMSS pedal
  // triple mapped to grid g acts as that grid's accrete trio instead of playing
  // samples (see the pedal hook below) -- the softstep can mirror one monome, both,
  // or neither.
  let feet_accrete_on: Arc<Vec<AtomicBool>> =
    Arc::new((0..num_grids).map(|_| AtomicBool::new(false)).collect());
  // The polyrhythm state (tap tempo + tempo factor): one instrument-wide machine,
  // both grids' pads. The base tempo is seeded at 1 Hz for every rig, so the
  // tempo-factor controls multiply something from bring-up -- a rig with no tap
  // source at all (2-monomes_2-softsteps retired its tap pedal: Jeff never set a
  // tempo with it) is not stuck waiting for a tap that can never come, and where a
  // tap source exists, tapping simply overrides the seed.
  let poly = {
    let mut p = PolyrhythmState::new(num_grids);
    p.set_fixed_tempo(1.0, Instant::now());
    Arc::new(Mutex::new(p))
  };
  // The on-screen factored-pulse window (phase 9, `TODO/many/3_plan.org`): born
  // when the =1 LED and the tap cell's blink moved off the grid onto the feet and
  // this was the only place left to see the factored-pulse state; kept now that the
  // on-grid pads are back, as the at-a-glance numeric view. Skipped entirely on a
  // drums-only bring-up (`plan.any_grid()` false) -- there is no grid's factored
  // pulse to show. Optional and non-fatal: `pulse_window::spawn` never blocks, and
  // a window that can't open just warns once and leaves the rest of the
  // instrument running.
  //
  // `no_audio` gates it too: that is this runtime's headless/mock signal (it also
  // skips the cpal stream), and a window is the same kind of thing -- a real system
  // resource a test must not reach for. Without this the mock-grid smoke test opens
  // an X11 window on whoever's display happens to be around, which makes the suite
  // depend on DISPLAY and on Jeff's per-login `xhost` grant.
  if plan.any_grid() && !no_audio {
    pulse_window::spawn(Arc::clone(&poly), num_grids);
  }
  // The EX-P volume pedals' per-grid gains: unity until a pedal first moves. The
  // pedal thread writes them (and re-aims sounding voices); note-ons read them
  // through each grid's SurfaceSink. `no_audio` gates the thread like the cpal
  // stream -- a headless/mock run must not open MIDI connections.
  let pedal_gains = Arc::new(Mutex::new(vec![1.0_f32; num_grids]));
  if !s.expression_pedals.is_empty() && !no_audio {
    let live_for_pedals = Arc::clone(&live);
    let voices_for_pedals = Arc::clone(&voices);
    let gains = Arc::clone(&pedal_gains);
    thread::spawn(move || expression_pedal_loop(live_for_pedals, voices_for_pedals, gains));
  }
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
  // The accrete (sustain) banks -- one per monome, under one lock (misc.org "two
  // monome-specific accrete banks"): a grid's trio drives only its own bank, and only
  // that grid's notes can join it. Alongside: a mirror of every grid's held notes
  // (cell -> struck pitch), so a bank's activation can capture what is fingered on
  // its grid even from the pedal hook.
  // A bank whose `needs_holding` switch is bound NOWHERE -- neither an on-grid button
  // nor a rig-declared pedal -- can never leave whatever mode it starts in. Leaving it
  // at the toggle default is then a trap: one tap of accrete and every later note
  // sustains, with nothing on the surface saying why and no way back but `clear`. So
  // such a bank is momentary: hold to accrete, lift to stop. Bind a `needs_holding`
  // control and you get the switchable bank the drums rig has always had.
  let accrete: Arc<Mutex<Vec<AccreteState>>> = Arc::new(Mutex::new(
    (0..num_grids)
      .map(|g| {
        if grid_has_needs_holding_control(rig, &s.grids[g]) {
          AccreteState::new()
        } else {
          AccreteState::new_momentary()
        }
      })
      .collect(),
  ));
  let held_all = Arc::new(Mutex::new(vec![HashMap::<(i32, i32), i32>::new(); num_grids]));
  // Each grid's edit-mode state, SHARED rather than owned by its grid thread -- the
  // pedal hook both reads it (a factored-pulse pedal retunes the edited notes when
  // there are any) and writes it (`clear` must dismiss them, or it silences a note
  // and leaves it dancing). Same shape as `accrete`, which it is inseparable from:
  // entering edit mode sustains a pitch in that bank, so the two must not disagree
  // about what rings.
  let edit: Arc<Mutex<Vec<edit::EditState>>> =
    Arc::new(Mutex::new((0..num_grids).map(|_| edit::EditState::new()).collect()));

  // Bring up the drumkit alongside the grids, if the rig declares one. Consumed
  // from `drumkit_runtime` (not forked); kept alive for the run, restoring standalone
  // mode on drop. We own the signal handling, so the tether session is unarmed. The
  // pedal hook is the feet-accrete mirror: pedals 1/2/3 drive the older (monobright)
  // grid's accrete bank and 8/9/0 the other's (Jeff's mapping, misc.org "feet
  // accrete"), each triple only while its grid's toggle is on.
  let drums = if plan.drums {
    // Which present grid is the older (monobright) one? Its accrete trio maps to pedals
    // 1/2/3, the other grid's to 8/9/0 (the feet-accrete mirror). A grid that's absent
    // never turns its feet toggle on, so its triple simply keeps drumming.
    let older = assigned
      .iter()
      .position(|d| d.as_ref().is_some_and(|d| is_monobright(&d.id)))
      .unwrap_or(0);
    let other = (0..num_grids).find(|g| *g != older).unwrap_or(older);
    // The mirror binds the FIRST declared board. With one softstep (every rig that
    // uses this hook today) that is simply "the softstep"; naming it keeps a second
    // board from mirroring the same pedals onto the same banks.
    let mirror_board = rig.softsteps.first().map(|s| s.id.clone()).unwrap_or_default();
    // A rig that declares its pedal bindings explicitly gets exactly those, and the
    // feet-accrete mirror stays out of the way: the two disagree about what a pedal
    // means (the mirror hardcodes 1/2/3 + 8/9/0 and needs an on-grid toggle), so
    // running both would make a pedal's job depend on which hook saw it first.
    let actions = rig_pedal_actions(rig, |m| s.grids.iter().position(|g| g.monome_id == m));
    let hook = if actions.is_empty() {
      feet_accrete_hook(
        mirror_board,
        Arc::clone(&feet_accrete_on),
        [older, other],
        Arc::clone(&accrete),
        Arc::clone(&held_all),
        Arc::clone(&voices),
        Arc::clone(&edit),
        s.release,
        audio.sample_rate,
      )
    } else {
      println!("surfaces: {} rig-declared pedal binding(s)", actions.len());
      rig_pedal_hook(
        actions,
        Arc::clone(&accrete),
        Arc::clone(&held_all),
        Arc::clone(&voices),
        Arc::clone(&poly),
        Arc::clone(&edit),
        s.tap_window,
        s.release,
        audio.sample_rate,
      )
    };
    Some(drumkit_runtime::start_with_hook(
      rig,
      drumkit_runtime::tether::session(),
      Some(hook),
    )?)
  } else {
    None
  };

  // The red report of everything skipped for a missing dependency, printed just before
  // "running" so it is the last thing on screen -- and, with `echo_input` off (the
  // default), it stays there while you play.
  print_missing_report(&plan.report);
  println!("surfaces running; Ctrl-C to exit.");

  // Spawn one key/LED loop per PRESENT grid (absent grids have `None` for both their
  // socket and their device, so they are skipped).
  let mut handles = Vec::with_capacity(num_grids);
  for (grid_index, sock) in sockets.into_iter().enumerate() {
    let (Some(sock), Some(dev)) = (sock, assigned[grid_index].clone()) else {
      continue;
    };
    let g = &s.grids[grid_index];
    // A cross-controlling waveform selector / volume strip whose TARGET grid is absent
    // can't do anything, so it does not load: its cells revert to plain play cells
    // (and it's named in the red report). Self-controlling strips are untouched.
    let selector_rect = if plan.drop_selector[grid_index] { NO_RECT } else { g.selector_rect };
    let volume_rect = if plan.drop_volume[grid_index] { NO_RECT } else { g.volume_rect };
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
      selector_rect,
      controls_index: g.controls_index,
      volume_rect,
      volume_controls_index: g.volume_controls_index,
      clear_rect: g.clear_rect,
      needs_holding_rect: g.needs_holding_rect,
      accrete_rect: g.accrete_rect,
      erase_rect: g.erase_rect,
      distortion_rect: g.distortion_rect,
      slide_rect: g.slide_rect,
      mono_rect: g.mono_rect,
      feet_accrete_rect: g.feet_accrete_rect,
      poly_rect: g.poly_rect,
      editmode_clear_rect: g.editmode_clear_rect,
      editmode_accrete_rect: g.editmode_accrete_rect,
      editmode_clear_down: false,
      editmode_accrete_down: false,
      grid_w: s.grid_w,
      grid_h: s.grid_h,
      x_step: s.x_step,
      y_step: s.y_step,
      edo: s.edo,
      fund: s.fund,
      echo_input: s.echo_input,
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
      poly: Arc::clone(&poly),
      tap_window: s.tap_window,
      live: Arc::clone(&live),
      slide: SlideCandidates::new(),
      edit: Arc::clone(&edit),
      started: Instant::now(),
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
        Arc::clone(&pedal_gains),
      ),
    };
    handles.push(thread::spawn(move || grid_thread(rt)));
  }

  if handles.is_empty() {
    // Drums-only (every grid absent): there is no grid loop to join, so park until a
    // signal (or a test) sets STOP, then tear down. The drumkit runs on its own MIDI /
    // timer threads meanwhile.
    while !STOP.load(Ordering::SeqCst) {
      thread::sleep(Duration::from_millis(50));
    }
  } else {
    for handle in handles {
      let _ = handle.join();
    }
  }

  // Authoritative teardown regardless of how the threads exited: blank the grids that
  // were actually brought up.
  for (g, dev) in s.grids.iter().zip(&assigned) {
    if let Some(d) = dev {
      blank_grid(d.port, &g.prefix);
    }
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
  /// The accrete (sustain) buttons' cells on this grid (`NO_RECT` if absent).
  clear_rect: [i32; 4],
  needs_holding_rect: [i32; 4],
  accrete_rect: [i32; 4],
  erase_rect: [i32; 4],
  /// The global-distortion toggle's cell on this grid (`NO_RECT` if absent).
  distortion_rect: [i32; 4],
  /// The slide / mono toggles' cells on this grid (`NO_RECT` if absent).
  slide_rect: [i32; 4],
  mono_rect: [i32; 4],
  /// The feet-accrete toggle's cell on this grid (`NO_RECT` if absent).
  feet_accrete_rect: [i32; 4],
  /// The polyrhythm pad's rect on this grid (`NO_RECT` if absent).
  poly_rect: [i32; 4],
  /// The editmode_control buttons' cells (`NO_RECT` if absent), and whether each
  /// is physically down right now (they light while pressed, like accrete's clear).
  editmode_clear_rect: [i32; 4],
  editmode_accrete_rect: [i32; 4],
  editmode_clear_down: bool,
  editmode_accrete_down: bool,
  grid_w: i32,
  grid_h: i32,
  x_step: i32,
  y_step: i32,
  edo: i32,
  /// Tuning fundamental (Hz), for the optional `echo_input` note echo.
  fund: f64,
  /// Echo each fingered note on this grid to stderr; off unless the rig sets
  /// `echo_input`. Kept off so a startup warning isn't scrolled away as you play.
  echo_input: bool,
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
  /// The accrete (sustain) banks, one per monome under one lock; this grid's trio
  /// drives `accrete[grid_index]` only.
  accrete: Arc<Mutex<Vec<AccreteState>>>,
  /// Every grid's held notes (cell -> struck pitch), for accrete's capture-on-
  /// activation. Each grid thread rewrites only its own slot.
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  /// The per-grid distortion switches; this grid's toggle flips (and its LED shows)
  /// element `grid_index`, and the audio callback routes voices by them.
  distortion_on: Arc<Vec<AtomicBool>>,
  /// The per-grid slide / mono switches (this grid uses element `grid_index`).
  slide_on: Arc<Vec<AtomicBool>>,
  mono_on: Arc<Vec<AtomicBool>>,
  /// The per-grid feet-accrete switches; this grid's toggle flips (and its LED
  /// shows) element `grid_index` -- "the softstep accretes for THIS monome".
  feet_accrete_on: Arc<Vec<AtomicBool>>,
  /// The shared polyrhythm state (tap tempo + tempo factor) and its pairing window.
  poly: Arc<Mutex<PolyrhythmState>>,
  tap_window: Duration,
  /// THIS grid's recently-released notes (slide sources) + the slide knobs.
  slide: SlideCandidates,
  /// Which of THIS grid's pitches are in per-voice edit mode. Grid-local and
  /// pitch-keyed: never mirrored to the other grid, never octave-duplicated.
  edit: Arc<Mutex<Vec<edit::EditState>>>,
  /// When this runtime started. The diamond dance's phase is a pure function of
  /// elapsed time from here, so every dance on the instrument turns in step -- that
  /// is the whole reason a skipped corner is not allowed to retime its dance.
  started: Instant,
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
    let toggle = |rect, on: &[AtomicBool]| (rect, button_level(on[rt.grid_index].load(Ordering::Relaxed)));
    buttons.push(toggle(rt.distortion_rect, &rt.distortion_on));
    buttons.push(toggle(rt.slide_rect, &rt.slide_on));
    buttons.push(toggle(rt.mono_rect, &rt.mono_on));
    buttons.push(toggle(rt.feet_accrete_rect, &rt.feet_accrete_on));
    // The editmode buttons: dim at rest (findable), bright while pressed.
    buttons.push((rt.editmode_clear_rect, button_level(rt.editmode_clear_down)));
    buttons.push((rt.editmode_accrete_rect, button_level(rt.editmode_accrete_down)));
    if rt.poly_rect != NO_RECT {
      // The pad's six cells, all per-THIS-grid state: the tempo-factor cells show
      // which way this grid's tempo factor leans; =1 shows this grid's
      // factored-pulse switch (bright while its amplitude cycling is on). The tap
      // cell blinks BLACK <-> FULLY
      // LIT (10% duty at the BASE tempo, unfactored -- so both grids' tap cells
      // blink together, showing the metronome rather than what this grid made of
      // it; misc.org "tap blink between black and fully lit") whether or not
      // cycling is on -- it is the tempo display. The dim no-tempo rest state is
      // unreachable now that the base is seeded at 1 Hz, but kept as a fallback.
      // This same state also answers to the softstep factor pedals, so the pad's
      // LEDs reflect pedal presses and vice versa -- one machine, two surfaces.
      let p = rt.poly.lock().unwrap_or_else(|e| e.into_inner());
      let now = Instant::now();
      let tap_level = if p.tapped_hz().is_none() {
        DIM
      } else if p.tap_blink(now) {
        BRIGHT
      } else {
        OFF
      };
      for (dx, dy, level) in [
        (0, 0, button_level(p.tempo_factor_lit(rt.grid_index, TempoFactorButton::Times3))),
        (1, 0, button_level(p.tempo_factor_lit(rt.grid_index, TempoFactorButton::Times2))),
        (2, 0, tap_level),
        (0, 1, button_level(p.tempo_factor_lit(rt.grid_index, TempoFactorButton::Div3))),
        (1, 1, button_level(p.tempo_factor_lit(rt.grid_index, TempoFactorButton::Div2))),
        (2, 1, button_level(p.tempo_factor_lit(rt.grid_index, TempoFactorButton::Unity))),
      ] {
        let (x, y) = (rt.poly_rect[0] + dx, rt.poly_rect[1] + dy);
        buttons.push(([x, y, x, y], level));
      }
    }
    let mut sounding_classes = union_sounding(&rt.sounding);
    sounding_classes.extend(sustained_classes);
    // An edited note RINGS -- that is the whole point -- so it paints bright like any
    // other sounding note, on both grids. Edit mode is its own reason to sound, so it
    // is not in anyone's sustained set to be picked up above; it has to be unioned in
    // here or an edited drone would go silent-looking while still audible.
    {
      let states = rt.edit.lock().unwrap_or_else(|e| e.into_inner());
      for state in states.iter() {
        sounding_classes.extend(state.pitches().map(|p| p.rem_euclid(rt.edo)));
      }
    }
    let trail_classes = trail_set(&rt.trail);

    // The diamond dances and the off-screen indicator. Both read THIS grid's edit set
    // plus its own sustained pitches: local, never mirrored from the other grid and
    // never octave-duplicated, unlike everything else painted here.
    let elapsed = rt.started.elapsed();
    // Snapshot under the lock, then draw: the pedal hook writes this too (`clear`).
    let edited: Vec<i32> = {
      let states = rt.edit.lock().unwrap_or_else(|e| e.into_inner());
      states[rt.grid_index].pitches().collect()
    };
    let mut dance_cells: HashSet<(i32, i32)> = HashSet::new();
    for pitch in edited.iter().copied() {
      // A pitch can occupy TWO cells on one grid, and Jeff wants both to dance
      // ("sometimes there are two monome buttons representing exactly the same
      // pitch"). So dance every cell that sounds it, not just the first.
      for (x, y) in cells_for_pitch(&rt, register, pitch) {
        dance_cells.insert(dance::corner_cell((x, y), elapsed));
      }
    }
    // The visible pitch window, for "is that note off-screen".
    let [ex0, ey0, ex1, ey1] = rt.edo_rect;
    let corners = [
      step_for_cell(rt.x_step, rt.y_step, register, ex0, ey0),
      step_for_cell(rt.x_step, rt.y_step, register, ex1, ey1),
    ];
    let (lo, hi) = (corners[0].min(corners[1]), corners[0].max(corners[1]));
    let off = if dance::flash_on(elapsed) {
      // One signal for BOTH edit-mode and sustained notes -- Jeff's call ("in both
      // cases"), so the LED cannot say which kind you are chasing.
      let sustained: Vec<i32> = {
        let banks = rt.accrete.lock().unwrap_or_else(|e| e.into_inner());
        banks[rt.grid_index].sustained_pitches().collect()
      };
      dance::off_screen(edited.iter().copied().chain(sustained), lo, hi)
    } else {
      dance::OffScreen::default()
    };

    let levels = levels_for_grid(
      &sounding_classes,
      &trail_classes,
      &dance_cells,
      off,
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
  // Distortion toggle: key-down flips THIS grid's switch (the audio callback routes
  // each grid's voices by its own flag); key-up does nothing.
  if in_overlay(rt.distortion_rect, cell) {
    if press {
      let _ = rt.distortion_on[rt.grid_index].fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // Slide toggle: key-down flips THIS grid's switch; key-up does nothing.
  if in_overlay(rt.slide_rect, cell) {
    if press {
      let _ = rt.slide_on[rt.grid_index].fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // Mono toggle: key-down flips THIS grid's switch; key-up does nothing.
  if in_overlay(rt.mono_rect, cell) {
    if press {
      let _ = rt.mono_on[rt.grid_index].fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // Feet-accrete toggle: key-down flips THIS grid's switch (does the softstep
  // mirror this monome's accrete bank?); key-up does nothing.
  if in_overlay(rt.feet_accrete_rect, cell) {
    if press {
      let _ = rt.feet_accrete_on[rt.grid_index].fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // The editmode buttons, through the same `editmode_press` the softstep pedals
  // run: clear empties THIS grid's edit mode (edit-only drones ring out; sustained
  // and fingered voices keep their other reasons), accrete puts every sounding
  // voice into it. Key-down only; key-up douses the LED.
  for (rect, down_flag, control) in [
    (rt.editmode_clear_rect, 0, EditmodeControlKind::Clear),
    (rt.editmode_accrete_rect, 1, EditmodeControlKind::Accrete),
  ] {
    if in_overlay(rect, cell) {
      if down_flag == 0 {
        rt.editmode_clear_down = press;
      } else {
        rt.editmode_accrete_down = press;
      }
      if press {
        let (release_secs, sample_rate) = rt.sink.release_params();
        editmode_press(
          rt.grid_index, control, &rt.edit, &rt.accrete, &rt.held_all, &rt.voices,
          release_secs, sample_rate,
        );
      }
      return;
    }
  }
  // The polyrhythm pad: | x3 x2 tap | /3 /2 =1 |, key-down only. The tap sets the
  // GLOBAL tempo; the tempo-factor buttons and the =1 factored-pulse switch act on
  // THIS grid, through the same `factored_pulse_press` as the softstep pedals --
  // so a multiplier retunes this grid's edit-mode notes exactly as a pedal would.
  if in_overlay(rt.poly_rect, cell) {
    if press {
      let (dx, dy) = (cell.0 - rt.poly_rect[0], cell.1 - rt.poly_rect[1]);
      let factor = match (dx, dy) {
        (2, 0) => {
          rt.poly.lock().unwrap_or_else(|e| e.into_inner()).tap(Instant::now(), rt.tap_window);
          None
        }
        (0, 0) => Some(TempoFactorButton::Times3),
        (1, 0) => Some(TempoFactorButton::Times2),
        (0, 1) => Some(TempoFactorButton::Div3),
        (1, 1) => Some(TempoFactorButton::Div2),
        (2, 1) => Some(TempoFactorButton::Unity),
        _ => None,
      };
      if let Some(factor) = factor {
        factored_pulse_press(rt.grid_index, factor, &rt.poly, &rt.edit, &rt.held_all, &rt.voices);
      }
    }
    return;
  }
  // The accrete (sustain) buttons. Each grid's trio acts on ITS OWN bank (misc.org
  // "two monome-specific accrete banks"). Decisions are made under the accrete lock,
  // voices are touched after it drops (the module's no-nested-locks rule).
  if in_overlay(rt.clear_rect, cell) {
    if press {
      rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].press_clear();
      // Clear removes only the SUSTAIN reason (symmetric with editmode clear): a
      // drone whose pitch is in edit mode keeps ringing -- audibly, so it is not
      // the old silent "dancing ghost" -- until editmode clear (or its exit
      // gesture) takes that reason too.
      let edited: HashSet<i32> = rt.edit.lock().unwrap_or_else(|e| e.into_inner())
        [rt.grid_index]
        .pitches()
        .collect();
      rt.sink.release_sustained(&edited);
    } else {
      rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].release_clear();
    }
    return;
  }
  if in_overlay(rt.needs_holding_rect, cell) {
    if press {
      let activated = rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
        .press_needs_holding();
      if activated.accrete {
        capture_grid_held(rt);
      }
      if activated.erase {
        erase_grid_held(rt);
      }
    }
    return;
  }
  if in_overlay(rt.accrete_rect, cell) {
    if press {
      let activated =
        rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].press_accrete();
      if activated {
        capture_grid_held(rt);
      }
    } else {
      rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].release_accrete();
    }
    return;
  }
  // The erase button (misc.org "erase button"): accrete's shape under the same
  // needs-holding switch, but pressed pitches LEAVE this grid's sustained set
  // (each keeps sounding until its own finger lifts).
  if in_overlay(rt.erase_rect, cell) {
    if press {
      let activated =
        rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].press_erase();
      if activated {
        erase_grid_held(rt);
      }
    } else {
      rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].release_erase();
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
    // Optional note echo (`echo_input`, off by default): mirrors the sawwave runtime,
    // but stays quiet unless asked so a startup warning isn't scrolled off screen.
    if rt.echo_input {
      let f = rt.fund * 2f64.powf(pitch as f64 / rt.edo as f64);
      eprintln!("press grid={} x={:>2} y={:>2} f={f:.2} Hz", rt.grid_index, cell.0, cell.1);
    }
    // Per-voice edit mode, BEFORE the play path: this press may be an edit trigger or
    // a pitch drag rather than a note, and in both of those cases it must not sound.
    if !handle_edit_press(rt, held, cell, pitch, register) {
      // A trigger or a drag: no note sounds, but `held` may have MOVED (a drag
      // re-pitches a finger's voice), so the shared maps still have to be republished
      // or the other grid reflects a pitch that is no longer sounding.
      publish_held(&rt.held_all, rt.grid_index, held);
      publish_sounding(&rt.sounding, rt.grid_index, held, rt.edo);
      return;
    }
    // Mono: a new note cuts this grid's other fingered notes first. With slide on
    // too, the nearest cut note is not released but STOLEN: its voice will glide
    // into the new pitch legato-style, with no attack re-trigger (misc.org "slide
    // when mono is on should not re-trigger the attack"). Other cuts go through
    // the ordinary release path, so accrete still captures them -- and a cut that
    // sustains (accrete) becomes a drone as usual and cannot be stolen.
    let mut legato_from: Option<(i32, i32)> = None;
    if rt.mono_on[rt.grid_index].load(Ordering::Relaxed) {
      let slide = rt.slide_on[rt.grid_index].load(Ordering::Relaxed);
      let mut others: Vec<((i32, i32), i32)> =
        held.iter().filter(|(c, _)| **c != cell).map(|(c, p)| (*c, *p)).collect();
      // The nearest cut pitch is the legato source (with mono playing, the cut is
      // a single note anyway).
      others.sort_by_key(|(_, p)| (*p - pitch).abs());
      for (i, (other, _)) in others.into_iter().enumerate() {
        if slide && i == 0 {
          if cut_for_legato(rt, held, other) {
            legato_from = Some(other);
          }
        } else {
          release_cell(rt, held, other);
        }
      }
    }
    // A manual retrigger of a sustaining pitch cuts this grid's drone and the new
    // note replaces it (misc.org "retriggering a sustaining note replaces it") --
    // no doubling. The pitch keeps its place in the accrete set, so releasing the
    // replacing note re-drones it. After the mono block, so a colliding-pitch
    // drone a mono cut just captured is cut like any other.
    rt.sink.cut_sustained(pitch);
    let slot = rt.timbres[current_slot(&rt.selected, rt.grid_index)];
    let gain = current_gain(&rt.gains, rt.grid_index);
    let timbre = Timbre {
      waveform: slot.waveform,
      gain: slot.amplitude * gain,
      am: slot.am,
      fm: slot.fm,
      rel_am: slot.rel_am,
      rel_fm: slot.rel_fm,
    };
    // Slide: while on, glide into this note -- legato from the voice mono just
    // cut, or, with no stolen voice, by re-triggering the nearest recently-
    // released pitch (consuming it as a source); otherwise a plain note.
    let source = if legato_from.is_none() && rt.slide_on[rt.grid_index].load(Ordering::Relaxed) {
      rt.slide.pick(pitch, Instant::now(), rt.slide_window)
    } else {
      None
    };
    // The factored pulse at THIS note's onset (fixed for the note's life): this
    // grid's applied tempo, and only while its =1 factored-pulse switch is on.
    let factored_pulse = rt.poly.lock().unwrap_or_else(|e| e.into_inner()).factored_pulse_hz(rt.grid_index);
    // The note's gain = the slot's amplitude x the grid's fader; the live fader
    // rescale is ratio-based, so the slot amplitude survives later fader moves.
    // (A stolen legato voice keeps ITS timbre and gain -- it is the same voice.)
    let stole = legato_from
      .map(|from| rt.sink.note_on_legato(from, cell, pitch, rt.slide_duration_secs, factored_pulse))
      .unwrap_or(false);
    if !stole {
      match source {
        Some(from) => {
          rt.sink.note_on_gliding(cell, pitch, from, timbre, rt.slide_duration_secs, factored_pulse)
        }
        None => rt.sink.note_on(cell, pitch, timbre, factored_pulse),
      }
    }
    held.insert(cell, pitch);
    rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].note_played(pitch);
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
  // A note rings without a finger for either of two independent reasons: it is
  // sustained (pedal, or the per-note button), or it is being edited. Either keeps it.
  let sustains = rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
    .note_released_sustains(pitch);
  let editing = rt.edit.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].is_editing(pitch);
  if sustains || editing {
    rt.sink.sustain_note(cell, pitch);
  } else {
    rt.sink.note_off(cell);
    let mono = rt.mono_on[rt.grid_index].load(Ordering::Relaxed);
    rt.slide.note_released(pitch, Instant::now(), mono);
  }
  held.remove(&cell);
}

/// Mono's cut of the note a slide is about to glide from: like `release_cell`,
/// but a note that would release audibly is NOT released -- its voice is left
/// sounding at `cell` for `note_on_legato` to steal, and it never becomes a slide
/// candidate (it keeps sounding). A note that sustains (accrete) becomes a drone
/// as usual and cannot be stolen. Returns true if the voice awaits stealing.
fn cut_for_legato(
  rt: &mut GridThread,
  held: &mut HashMap<(i32, i32), i32>,
  cell: (i32, i32),
) -> bool {
  let Some(pitch) = held.get(&cell).copied() else {
    return false;
  };
  let keep = rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
    .note_released_sustains(pitch);
  held.remove(&cell);
  if keep {
    rt.sink.sustain_note(cell, pitch);
    return false;
  }
  true
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
  rt.tap_window = p.tap_window;
  rt.timbres = p.timbres;
  rt.sink.retune(p.fund, p.edo, p.sustain_level, p.decay_secs);
}

/// Mirror this grid's held map into the shared per-grid registry (for accrete's
/// capture-on-activation, which the pedal hook must reach from outside this thread).
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

/// The accreting condition just turned on for this grid's bank: add every voice
/// sounding on this grid -- the notes currently held AND the pitches in edit mode
/// (queue.org: "accrete-sustain should add every fingered voice and every
/// edit-mode voice"; an edited drone is a sounding voice like any other). Snapshot
/// each registry first, then feed the bank -- short, non-nested locks.
fn capture_grid_held(rt: &GridThread) {
  capture_grid_held_into(&rt.held_all, &rt.accrete, &rt.edit, rt.grid_index);
}

/// `capture_grid_held` for callers that aren't a grid thread (the pedal hook).
fn capture_grid_held_into(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  grid: usize,
) {
  let mut snapshot = held_pitches(held_all, grid);
  snapshot.extend(
    edit
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(grid)
      .map(|e| e.pitches().collect::<Vec<i32>>())
      .unwrap_or_default(),
  );
  let mut banks = accrete.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(bank) = banks.get_mut(grid) {
    bank.capture_held(snapshot);
  }
}

/// The erase mirror of `capture_grid_held`: when the erasing condition turns on,
/// the notes currently held on this grid leave the sustained set (they keep
/// sounding under their fingers).
fn erase_grid_held(rt: &GridThread) {
  erase_grid_held_into(&rt.held_all, &rt.accrete, rt.grid_index);
}

fn erase_grid_held_into(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  grid: usize,
) {
  let snapshot = held_pitches(held_all, grid);
  let mut banks = accrete.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(bank) = banks.get_mut(grid) {
    bank.erase_held(snapshot);
  }
}

/// Snapshot one grid's currently-held pitches (its own lock; never nested).
fn held_pitches(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  grid: usize,
) -> Vec<i32> {
  let all = held_all.lock().unwrap_or_else(|e| e.into_inner());
  all.get(grid).map(|m| m.values().copied().collect()).unwrap_or_default()
}

/// Which accrete button a KMSS pedal mirrors, and for which pedal TRIPLE (0 =
/// pedals 1/2/3, 1 = pedals 8/9/0). Jeff's mapping (misc.org "feet accrete" + "two
/// monome-specific accrete banks"): the older (monobright) grid's bank -> 1/2/3,
/// the other grid's -> 8/9/0, each triple in the on-grid order clear /
/// needs-holding / accrete.
fn feet_accrete_button(pedal: u8) -> Option<(usize, AccreteControlKind)> {
  match pedal {
    1 => Some((0, AccreteControlKind::Clear)),
    2 => Some((0, AccreteControlKind::NeedsHolding)),
    3 => Some((0, AccreteControlKind::Accrete)),
    8 => Some((1, AccreteControlKind::Clear)),
    9 => Some((1, AccreteControlKind::NeedsHolding)),
    0 => Some((1, AccreteControlKind::Accrete)),
    _ => None,
  }
}

/// Build the drumkit pedal hook that mirrors the accrete trios onto the KMSS
/// (TODO/misc.org "feet accrete"). `triple_banks[t]` is the grid whose bank pedal
/// triple `t` drives (0 = pedals 1/2/3 = the older grid, 1 = pedals 8/9/0); a
/// triple mirrors only while ITS grid's feet-accrete toggle is on, so the softstep
/// can accrete for one monome, both, or neither. Consuming an event suppresses
/// that pedal's sample; a pedal whose toggle is off drums as usual. A pedal
/// "press" is the decoder's Fire (down) and its Release (up), so holding pedal 3
/// or 0 is exactly holding that bank's accrete button.
/// `softstep_id` pins the mirror to ONE board. The two triples (1/2/3 and 8/9/0)
/// distinguish the two GRIDS, not the two boards -- so with a second SoftStep
/// connected its pedal 3 would otherwise mirror this board's pedal 3, both boards
/// driving one bank. The rig-declared bindings supersede this hook; it stays for the
/// existing single-board drums rig.
#[allow(clippy::too_many_arguments)]
fn feet_accrete_hook(
  softstep_id: String,
  feet_accrete_on: Arc<Vec<AtomicBool>>,
  triple_banks: [usize; 2],
  accrete: Arc<Mutex<Vec<AccreteState>>>,
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: Arc<Mutex<VoiceMap>>,
  edit: Arc<Mutex<Vec<edit::EditState>>>,
  release_secs: f32,
  sample_rate: f32,
) -> drumkit_runtime::PedalHook {
  Arc::new(move |device, pedal, down| {
    if device != softstep_id {
      return false;
    }
    let Some((triple, button)) = feet_accrete_button(pedal) else {
      return false;
    };
    let grid = triple_banks[triple];
    if !feet_accrete_on.get(grid).map(|b| b.load(Ordering::Relaxed)).unwrap_or(false) {
      return false;
    }
    // The trio only; erase has no pedal here (feet_accrete_button never yields it).
    if button == AccreteControlKind::Erase {
      return false;
    }
    drive_accrete(
      grid, button, down, &accrete, &held_all, &voices, &edit, release_secs, sample_rate,
    )
  })
}

/// Apply one accrete-button edge to one grid's bank, from a foot or a finger.
///
/// Shared by the legacy feet-accrete mirror and the rig-declared `accrete_control`
/// pedals so the two cannot drift. Decides under the accrete lock and touches voices
/// only after it drops -- the module's no-nested-locks rule, same as the on-grid
/// buttons. Returns whether the press was consumed.
#[allow(clippy::too_many_arguments)]
fn drive_accrete(
  grid: usize,
  button: AccreteControlKind,
  down: bool,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  release_secs: f32,
  sample_rate: f32,
) -> bool {
  let mut activated = accrete::Activated::default();
  {
    let mut banks = accrete.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = banks.get_mut(grid) else {
      return false;
    };
    match (button, down) {
      (AccreteControlKind::Clear, true) => state.press_clear(),
      (AccreteControlKind::Clear, false) => state.release_clear(),
      (AccreteControlKind::NeedsHolding, true) => activated = state.press_needs_holding(),
      (AccreteControlKind::NeedsHolding, false) => {}
      (AccreteControlKind::Accrete, true) => activated.accrete = state.press_accrete(),
      (AccreteControlKind::Accrete, false) => state.release_accrete(),
      (AccreteControlKind::Erase, true) => activated.erase = state.press_erase(),
      (AccreteControlKind::Erase, false) => state.release_erase(),
    }
  }
  if button == AccreteControlKind::Clear && down {
    // Clear removes only the SUSTAIN reason (symmetric with editmode clear,
    // queue.org "accrete-editmode pedals like accrete-sustain"): a drone whose
    // pitch is in edit mode keeps ringing. This supersedes the old panic-button
    // coupling that also emptied edit mode -- that fix existed because clear used
    // to silence edited drones too, stranding SILENT notes in edit mode ("a
    // dancing ghost that won't go away"). Spared drones stay audible, so they are
    // not ghosts, and both the exit gesture and the editmode clear can still
    // dismiss them; the full kill is the two clears together.
    let edited: HashSet<i32> = edit
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(grid)
      .map(|e| e.pitches().collect())
      .unwrap_or_default();
    synth::release_sustained_voices(voices, grid, &edited, release_secs, sample_rate);
  }
  if activated.accrete {
    capture_grid_held_into(held_all, accrete, edit, grid);
  }
  if activated.erase {
    // A needs-holding flip can activate a physically-held ERASE button.
    erase_grid_held_into(held_all, accrete, grid);
  }
  true
}

/// What a rig-declared pedal does. Resolved once at bring-up from
/// `[[softstep_windows]]`, keyed by (softstep id, printed label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PedalAction {
  Accrete { grid: usize, control: AccreteControlKind },
  Tap,
  FactoredPulse { grid: usize, factor: TempoFactorButton },
  Editmode { grid: usize, control: EditmodeControlKind },
}

/// Build the (device, pedal) -> action map from the rig. `grid_of` maps a monome id
/// to its play-grid index; a pedal naming a monome that is not a play grid (or is
/// absent) is dropped, so the rig loads around missing gear like everything else.
fn rig_pedal_actions(
  rig: &Rig,
  grid_of: impl Fn(&str) -> Option<usize>,
) -> HashMap<(String, u8), PedalAction> {
  let mut map = HashMap::new();
  for window in &rig.softstep_windows {
    let action = match window {
      SoftstepWindowRig::AccreteControl { pedal, monome, control, .. } => {
        grid_of(monome).map(|grid| (*pedal, PedalAction::Accrete { grid, control: *control }))
      }
      SoftstepWindowRig::TapTempoPedal { pedal, .. } => Some((*pedal, PedalAction::Tap)),
      SoftstepWindowRig::PulseFactorPedal { pedal, monome, factor, .. } => {
        grid_of(monome).map(|grid| {
          let factor = match factor {
            PulseFactorRig::Double => TempoFactorButton::Times2,
            PulseFactorRig::Triple => TempoFactorButton::Times3,
            PulseFactorRig::Half => TempoFactorButton::Div2,
            PulseFactorRig::Third => TempoFactorButton::Div3,
            PulseFactorRig::Unity => TempoFactorButton::Unity,
          };
          (*pedal, PedalAction::FactoredPulse { grid, factor })
        })
      }
      SoftstepWindowRig::EditmodeControl { pedal, monome, control, .. } => {
        grid_of(monome).map(|grid| (*pedal, PedalAction::Editmode { grid, control: *control }))
      }
      SoftstepWindowRig::Drumkit { .. } => None,
    };
    if let Some((pedal, action)) = action {
      map.insert((window.softstep().to_string(), pedal), action);
    }
  }
  map
}

/// The rate multiplier a tempo-factor button applies to an edit-mode note. `None`
/// for `Unity`: `=1` is a switch, not a multiplier, and is never an edit control.
fn tempo_factor_ratio(factor: TempoFactorButton) -> Option<f32> {
  match factor {
    TempoFactorButton::Times2 => Some(2.0),
    TempoFactorButton::Times3 => Some(3.0),
    TempoFactorButton::Div2 => Some(0.5),
    TempoFactorButton::Div3 => Some(1.0 / 3.0),
    TempoFactorButton::Unity => None,
  }
}

/// One volume pedal's taper: the standard fader law, exponential with a linear
/// splice at the heel (rig `curve_initial_lin_frac` / `curve_remainder_exp_db`).
///
/// Loudness is roughly logarithmic in amplitude, so the perceptually even taper is
/// dB-LINEAR in travel -- an exponential. Its one flaw is that it never reaches 0,
/// which the splice fixes: over the first `lin_frac` of the travel the gain fades
/// linearly from exact silence up to the exponential's floor (`-exp_db` dB), and
/// from there the exponential covers `exp_db` dB up to unity at full toe. The two
/// halves meet, so the curve is continuous, monotonic, and pinned at 0 and 1.
/// (Tried before this: linear -- numb at the toe, a cliff at the heel -- then
/// quadratic, better but still not spread right. This is the console fader's law.)
#[derive(Clone, Copy, Debug, PartialEq)]
struct PedalVolumeCurve {
  lin_frac: f32,
  exp_db: f32,
}

impl PedalVolumeCurve {
  fn gain(&self, norm: f32) -> f32 {
    let x = norm.clamp(0.0, 1.0);
    if x <= 0.0 {
      return 0.0;
    }
    // The exponential's floor: the gain where the splice hands over.
    let floor = 10.0_f32.powf(-self.exp_db / 20.0);
    if x < self.lin_frac {
      return floor * (x / self.lin_frac);
    }
    10.0_f32.powf(-self.exp_db * (1.0 - x) / ((1.0 - self.lin_frac) * 20.0))
  }
}

/// The EX-P volume-pedal thread: poll the MPC-20 bridge reader (~100 Hz) and aim
/// each mapped grid at its pedal's position. The pedal's reliable ~1..119 CC
/// travel normalizes to a position 0..1 (`expression_pedals::normalize`), which
/// the pedal's own `PedalVolumeCurve` tapers into the amplitude factor
/// (exponential with a linear splice at the heel; full heel = silent, full toe =
/// unity). The per-sample slew that keeps a
/// sweep from zippering lives in the engine (`voices::GAIN_SLEW_SECS`) -- this
/// thread only moves TARGETS: the shared per-grid gain (for future note-ons) and
/// every sounding voice's `grid_gain_target`. A pedal that has never moved
/// contributes unity, so an unplugged pedal cannot mute its grid. A missing
/// bridge is a red report, not an error: the instrument plays on without pedals,
/// like any other absent gear.
///
/// The bindings and tapers come from `Live`, re-read every poll, so the curve_*
/// knobs are hot-reloadable ('r'): a reload bumps the generation, and every pedal
/// that has ever reported a position is re-applied through its new taper
/// immediately -- a curve tweak is audible without moving the pedal. (A rig that
/// STARTS with no pedals never spawns this thread; adding the first pedal needs a
/// restart, like any other bound resource.)
fn expression_pedal_loop(
  live: Arc<Live>,
  voices: Arc<Mutex<VoiceMap>>,
  pedal_gains: Arc<Mutex<Vec<f32>>>,
) {
  use midi_pulse::expression_pedals::{PedalReader, DEFAULT_PORT};
  let reader = match PedalReader::connect(DEFAULT_PORT) {
    Ok(r) => r,
    Err(e) => {
      eprintln!("\x1b[1;31mexpression pedals skipped: {e}\x1b[0m");
      return;
    }
  };
  println!("expression pedals: connected to {:?}", reader.port_name());
  let mut last = [f32::NAN; NUM_PEDALS];
  let mut last_generation = live.generation.load(Ordering::SeqCst);
  while !STOP.load(Ordering::SeqCst) {
    let generation = live.generation.load(Ordering::SeqCst);
    let reloaded = generation != last_generation;
    last_generation = generation;
    let map = live.params.lock().unwrap_or_else(|e| e.into_inner()).expression_pedals;
    let pedals = reader.pedals();
    for (pedal, binding) in map.iter().enumerate() {
      let Some((grid, curve)) = *binding else { continue };
      let p = pedals[pedal];
      // No CC yet = no position to trust; keep unity rather than muting the grid.
      // A reload re-applies a known position through the (possibly new) taper.
      if p.updates == 0 || (!reloaded && last[pedal] == p.norm) {
        continue;
      }
      last[pedal] = p.norm;
      let gain = curve.gain(p.norm);
      if let Some(g) = pedal_gains.lock().unwrap_or_else(|e| e.into_inner()).get_mut(grid) {
        *g = gain;
      }
      synth::set_grid_pedal_gain(&voices, grid, gain);
    }
    thread::sleep(Duration::from_millis(10));
  }
}

/// The editmode `clear` control on `grid`, from EITHER surface -- a softstep pedal
/// or the grid's own button run exactly this. Every pitch leaves edit mode; each
/// voice then rings on iff it still has another reason -- a finger, or the sustain
/// bank -- and an edit-only drone ends by the ordinary release ramp. Symmetric
/// with the sustain accrete's clear: each clear removes only its OWN reason, so a
/// note both edited and sustained survives either clear alone and dies to both
/// (the full-kill combo is the two clears together). One lock at a time, per the
/// module's rule.
fn editmode_clear(
  grid: usize,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) {
  let edited: HashSet<i32> = {
    let mut states = edit.lock().unwrap_or_else(|e| e.into_inner());
    let Some(e) = states.get_mut(grid) else { return };
    let pitches: HashSet<i32> = e.pitches().collect();
    e.clear();
    pitches
  };
  if edited.is_empty() {
    return;
  }
  let sustained: HashSet<i32> = {
    let banks = accrete.lock().unwrap_or_else(|e| e.into_inner());
    banks.get(grid).map(|b| b.sustained_pitches().collect()).unwrap_or_default()
  };
  // Only the drones whose SOLE reason was edit mode end; a sustained drone keeps
  // ringing (its bank membership is untouched), and a fingered voice was never a
  // drone at all.
  let dying: HashSet<i32> = edited.difference(&sustained).copied().collect();
  synth::end_drones_at(voices, grid, &dying, release_secs, sample_rate);
}

/// The editmode `accrete` control on `grid`: a ONE-SHOT that puts every voice
/// currently sounding on this grid -- every fingered voice and every sustained
/// voice -- into edit mode. One-shot rather than a hold, unlike the sustain
/// accrete: the moment anything is edited the grid becomes a pitch-picker (every
/// press drags instead of playing), so a "capture notes played while held" phase
/// cannot exist for edit mode. Nothing needs doing at the voice level: fingered
/// voices keep their fingers, sustained voices keep their drones, and being
/// edited is simply one more reason to ring.
fn editmode_accrete(
  grid: usize,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
) {
  let fingered = held_pitches(held_all, grid);
  let sustained: Vec<i32> = {
    let banks = accrete.lock().unwrap_or_else(|e| e.into_inner());
    banks.get(grid).map(|b| b.sustained_pitches().collect()).unwrap_or_default()
  };
  let mut states = edit.lock().unwrap_or_else(|e| e.into_inner());
  let Some(e) = states.get_mut(grid) else { return };
  for pitch in fingered.into_iter().chain(sustained) {
    e.enter(pitch);
  }
}

/// Dispatch one editmode_control press (shared by the pedal hook and the on-grid
/// buttons, so hands and feet cannot diverge). Key-down only; key-up is the
/// caller's LED business.
#[allow(clippy::too_many_arguments)]
fn editmode_press(
  grid: usize,
  control: EditmodeControlKind,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) {
  match control {
    EditmodeControlKind::Clear => {
      editmode_clear(grid, edit, accrete, voices, release_secs, sample_rate);
    }
    EditmodeControlKind::Accrete => editmode_accrete(grid, edit, accrete, held_all),
  }
}

/// A tempo-factor press on `grid`, from EITHER surface -- a softstep pedal or the
/// grid's own upper-right pad run exactly this, so hands and feet cannot diverge.
/// What a multiplier acts on depends on the grid's edit state (1_vision "per-voice
/// slow AM edit"): with notes in edit mode it retunes THOSE NOTES, leaving the
/// grid's tempo factor alone; with none it moves the tempo factor.
///
/// =1 is never an edit control -- Jeff: "the only edit controls are (*) and (/),
/// not (=)" -- so it always goes to the tempo-factor/switch dance.
fn factored_pulse_press(
  grid: usize,
  factor: TempoFactorButton,
  poly: &Arc<Mutex<PolyrhythmState>>,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
) {
  let edited: HashSet<i32> = edit
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .get(grid)
    .map(|e| e.pitches().collect())
    .unwrap_or_default();
  match tempo_factor_ratio(factor) {
    Some(ratio) if !edited.is_empty() => {
      // Multiply, don't set: "slower ones continue to be slower than faster
      // ones". Applies to ALL edited notes at once -- the deliberate
      // asymmetry against a pitch drag, which moves only the nearest (2d).
      // `held` is needed because a fingered voice is keyed by its CELL, not
      // its pitch; only a drone is keyed by pitch.
      let held: HashMap<(i32, i32), i32> = held_all
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(grid)
        .cloned()
        .unwrap_or_default();
      synth::scale_factored_pulse_rate(voices, grid, ratio, &edited, &held);
    }
    _ => {
      poly.lock().unwrap_or_else(|e| e.into_inner()).press(grid, factor, Instant::now());
    }
  }
}

/// The hook for rig-declared pedal bindings: sustain, tap tempo, and the factored pulse.
///
/// Unconditional, unlike the feet-accrete mirror -- no on-grid toggle gates it. Keyed
/// by (device, pedal) because the printed labels repeat across boards and each board
/// gives them different jobs.
#[allow(clippy::too_many_arguments)]
fn rig_pedal_hook(
  actions: HashMap<(String, u8), PedalAction>,
  accrete: Arc<Mutex<Vec<AccreteState>>>,
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: Arc<Mutex<VoiceMap>>,
  poly: Arc<Mutex<PolyrhythmState>>,
  edit: Arc<Mutex<Vec<edit::EditState>>>,
  tap_window: Duration,
  release_secs: f32,
  sample_rate: f32,
) -> drumkit_runtime::PedalHook {
  Arc::new(move |device, pedal, down| {
    let Some(action) = actions.get(&(device.to_string(), pedal)) else {
      return false;
    };
    match *action {
      PedalAction::Accrete { grid, control } => drive_accrete(
        grid, control, down, &accrete, &held_all, &voices, &edit, release_secs, sample_rate,
      ),
      // Tap and the tempo-factor buttons are key-down only, like their on-grid
      // twins. The press is still CONSUMED on key-up, or the pedal would drum on
      // release.
      PedalAction::Tap => {
        if down {
          poly.lock().unwrap_or_else(|e| e.into_inner()).tap(Instant::now(), tap_window);
        }
        true
      }
      PedalAction::FactoredPulse { grid, factor } => {
        if down {
          // Shared with the on-grid pad's factor cells: edit-mode retune vs
          // tempo-factor move, decided in one place (`factored_pulse_press`).
          factored_pulse_press(grid, factor, &poly, &edit, &held_all, &voices);
        }
        true
      }
      PedalAction::Editmode { grid, control } => {
        if down {
          // Shared with the on-grid editmode buttons (`editmode_press`).
          editmode_press(
            grid, control, &edit, &accrete, &held_all, &voices, release_secs, sample_rate,
          );
        }
        true
      }
    }
  })
}

/// Every cell of this grid's play surface that sounds exactly `pitch` under the
/// current register. Usually one, but a grid can hold the same pitch twice (with
/// `x_step = 9`, `(x, y)` and `(x+1, y-9)` collide), and Jeff wants both to dance.
fn cells_for_pitch(rt: &GridThread, register: i32, pitch: i32) -> Vec<(i32, i32)> {
  let [ex0, ey0, ex1, ey1] = rt.edo_rect;
  let mut out = vec![];
  for y in ey0..=ey1 {
    for x in ex0..=ex1 {
      if step_for_cell(rt.x_step, rt.y_step, register, x, y) == pitch {
        out.push((x, y));
      }
    }
  }
  out
}

/// Can this grid's `needs_holding` switch be flipped at all -- by an on-grid button or
/// by a rig-declared pedal? If not, the mode is fixed forever at whatever it starts
/// as, which is why the caller makes such a bank momentary rather than toggling.
fn grid_has_needs_holding_control(rig: &Rig, grid: &GridSettings) -> bool {
  if grid.needs_holding_rect != NO_RECT {
    return true;
  }
  rig.softstep_windows.iter().any(|w| {
    matches!(
      w,
      SoftstepWindowRig::AccreteControl { monome, control: AccreteControlKind::NeedsHolding, .. }
        if *monome == grid.monome_id
    )
  })
}

/// The edit-mode half of a play-cell press. Returns whether the press should fall
/// through to the ordinary play path.
///
/// Runs first because both of its outcomes are SILENT: the cell under a sounding note
/// is an edit trigger and never sounds, and while anything on this grid is being
/// edited every other press drags instead of playing (2_discussion 2b/2c).
///
/// `cell_above` is geometric, not pitch-derived: Jeff pinned the gesture to the cell
/// physically below a note so it doesn't move with the tuning. A note on the bottom
/// row therefore has no trigger cell and cannot be edited.
fn handle_edit_press(
  rt: &mut GridThread,
  held: &mut HashMap<(i32, i32), i32>,
  cell: (i32, i32),
  pitch: i32,
  register: &i32,
) -> bool {
  // Both triggers act on a NEIGHBOUR of the pressed cell, and both are geometric
  // rather than pitch-defined -- Jeff pinned them to physical position so they don't
  // move when the tuning changes. Press BELOW a note to edit it, ABOVE it to sustain
  // it. A note on the top or bottom row therefore has no trigger cell on that side.
  let on_grid = |y: i32| y >= rt.edo_rect[1] && y <= rt.edo_rect[3];
  let neighbour = |dy: i32| {
    on_grid(cell.1 + dy)
      .then(|| step_for_cell(rt.x_step, rt.y_step, *register, cell.0, cell.1 + dy))
  };
  let edit_target = neighbour(-1); // the note above the pressed cell
  let sustain_target = neighbour(1); // the note below it

  // A note rings for any of three independent reasons: a finger on it, a sustain
  // (pedal or per-note button), or being edited. Both triggers ask only "is it
  // audible", so all three count.
  let sustained: HashSet<i32> = {
    let banks = rt.accrete.lock().unwrap_or_else(|e| e.into_inner());
    banks[rt.grid_index].sustained_pitches().collect()
  };

  // Decide under the edit lock, act after it drops: the branches below take the
  // accrete lock, and this module's rule is no nested locks.
  enum Act {
    Play,
    Entered,
    Exited(i32),
    Sustain(i32, bool),
    Dragged(i32, i32),
  }
  let act = {
    let mut states = rt.edit.lock().unwrap_or_else(|e| e.into_inner());
    let editing: HashSet<i32> = states[rt.grid_index].pitches().collect();
    let is_sounding = |p: i32| {
      held.values().any(|h| *h == p) || sustained.contains(&p) || editing.contains(&p)
    };
    let e = &mut states[rt.grid_index];
    match e.classify(edit_target, sustain_target, pitch, is_sounding) {
      edit::Press::Play => Act::Play,
      edit::Press::EnterEdit { pitch } => {
        e.enter(pitch);
        Act::Entered
      }
      edit::Press::ExitEdit { pitch } => {
        e.exit(pitch);
        Act::Exited(pitch)
      }
      edit::Press::ToggleSustain { pitch } => Act::Sustain(pitch, !sustained.contains(&pitch)),
      edit::Press::Drag { from, to } => {
        e.moved(from, to);
        Act::Dragged(from, to)
      }
    }
  };

  match act {
    Act::Play => return true,
    // Entering needs nothing else: being edited is itself a reason to ring, so the
    // note simply keeps sounding when the finger lifts (`release_cell` asks).
    Act::Entered => {}
    Act::Exited(pitch) => {
      // Only the DRONE is in question here, and a drone's reasons are exactly two:
      // sustained, or edited. Not "fingered" -- a finger has its own voice, keyed by
      // its cell, which this must not touch and which dies on the ordinary release.
      //
      // So: having taken away `edited`, cut the drone unless a sustain still holds it
      // up. `cut_sustained` is a no-op when there is no drone, which is what makes
      // "exit while still holding the note" keep sounding -- that note has no drone,
      // only a finger.
      //
      // Asking "is any finger on this pitch?" instead (as this did) is a different
      // question, and answering yes to it stranded a drone that had just lost its
      // last reason -- audible forever, with nothing left that could cut it.
      if !sustained.contains(&pitch) {
        rt.sink.cut_sustained(pitch);
      }
    }
    Act::Sustain(pitch, on) => {
      let mut banks = rt.accrete.lock().unwrap_or_else(|e| e.into_inner());
      if on {
        banks[rt.grid_index].sustain_pitch(pitch);
      } else {
        banks[rt.grid_index].drop_pitch(pitch);
      }
      drop(banks);
      if !on {
        // Same rule as leaving edit mode, and for the same reason: this is about the
        // DRONE, whose reasons are sustained-or-edited. A finger is not one of them.
        let editing = rt.edit.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
          .is_editing(pitch);
        if !editing {
          rt.sink.cut_sustained(pitch);
        }
      }
      // Switching sustain ON starts nothing: a fingered note has no drone yet (it
      // gets one when the finger lifts), and a note ringing because it is edited
      // already has one.
    }
    Act::Dragged(from, to) => {
      // The finger that pressed `cell` now holds this voice, so re-home the voice to
      // `cell` as a fingered voice. It was either fingered at some old cell, or a
      // drone (its original finger having lifted while edited). Either way, adopting
      // it here is what fixes Jeff's bug: a voice dragged and then taken out of edit
      // mode used to die, because it stayed a drone that the exit cut while the drag
      // finger -- invisible to `held` -- was still down.
      let old_cell = held.iter().find(|(_, p)| **p == from).map(|(c, _)| *c);
      rt.sink.rehome_to_cell(old_cell, from, cell, to, rt.slide_duration_secs);
      if let Some(oc) = old_cell {
        held.remove(&oc);
      }
      held.insert(cell, to);
      // Accrete and the trail both track PITCHES, so a moved voice is re-filed under
      // its new one -- otherwise a clear would miss it, and the trail would keep
      // showing a pitch that is no longer sounding.
      rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].note_moved(from, to);
      push_trail(&rt.trail, to.rem_euclid(rt.edo), rt.edo, rt.trail_clobber_radius, rt.trails_max);
    }
  }
  false
}

/// One lock: this grid's accrete-trio LED view (its OWN bank's state) plus the
/// union of every bank's sustained pitch classes (which paint bright on every
/// grid -- they are all sounding, like the cross-grid note reflection).
fn accrete_view(rt: &GridThread) -> (Vec<ButtonOverlay>, HashSet<i32>) {
  let banks = rt.accrete.lock().unwrap_or_else(|e| e.into_inner());
  let s = &banks[rt.grid_index];
  let buttons = vec![
    (rt.clear_rect, button_level(s.clear_lit())),
    (rt.needs_holding_rect, button_level(s.needs_holding_lit())),
    (rt.accrete_rect, button_level(s.accrete_lit())),
    (rt.erase_rect, button_level(s.erase_lit())),
  ];
  let mut classes = HashSet::new();
  for bank in banks.iter() {
    classes.extend(bank.sustained_classes(rt.edo));
  }
  (buttons, classes)
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
mod tests;
