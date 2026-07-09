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
mod grid;
mod polyrhythm;
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
  load_named_rig, AccreteControlKind, AmShapeFamilyRig, Rig, MonomeWindowRig,
  SinkRig, WaveformChoice,
};
use midi_pulse::device_assign::assign_available_devices;
use midi_pulse::edo_play::{register_delta, shift_for_cell, step_for_cell};
use midi_pulse::monome::{self, DeviceInfo};
use midi_pulse::monome_brightness::PulseBrightness;

use crate::drumkit_runtime;
use crate::types::{Am, AmShapeFamily, Fm, Timbre, VoiceMap, Waveform};
use crate::voices::Distortion;

use accrete::AccreteState;
use polyrhythm::{FactorButton, PolyrhythmState};
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
    tap_window: s.tap_window,
    trail_clobber_radius: s.trail_clobber_radius,
    trails_max: s.trails_max,
  }
}

/// Re-read `name`'s rig and adopt its live parameters (everything else -- window
/// layout, ports, sinks' sample rates -- needs a restart and is silently kept as
/// is). A rig that fails to load or resolve leaves the old parameters running.
fn reload_live(name: &str, live: &Live) {
  match load_named_rig(name).and_then(|rig| adopt_rig(&rig, live)) {
    Ok(()) => println!(
      "reloaded {name}: amplitude / distortion / timbres / tuning / pluck / slide / trail applied (layout + ports need a restart)",
    ),
    Err(e) => eprintln!("reload of {name} failed; keeping the running parameters: {e}"),
  }
}

/// Adopt `rig`'s live parameters into `live` and bump the generation. Everything
/// non-live (window layout, ports, sinks' sample rates) is silently kept as-is.
fn adopt_rig(rig: &Rig, live: &Live) -> Result<(), String> {
  let s = resolve_settings(rig).map_err(|e| e.to_string())?;
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
    distortion_scale, distortion_shape, sustain_level, decay_secs, ..
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
      erase_rect: accrete_rect_on(AccreteControlKind::Erase),
      distortion_rect,
      slide_rect,
      mono_rect,
      feet_accrete_rect,
      poly_rect,
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
    has_drums: !rig.softstep_windows.is_empty(),
    trail_clobber_radius: surfaces.trail_clobber_radius,
    trails_max: surfaces.trails_max,
    slide_window: Duration::from_millis(surfaces.slide_candidate_window_ms),
    slide_duration_secs: surfaces.slide_duration_ms as f32 / 1000.0,
    tap_window: Duration::from_millis(surfaces.tap_tempo_window_ms),
    echo_input: rig.echo_input,
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
      am: Am { depth: t.am_depth, freq: t.am_freq, shape: t.am_shape },
      fm: Fm { depth_cents: t.fm_depth_cents, freq: t.fm_freq },
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
    println!("press 'r' + Enter to hot-reload the rig (amplitude / timbres / tuning / pluck / slide / trail / distortion curve).");
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
  let assigned: Vec<Option<DeviceInfo>> = assign_available_devices(&devices, s.size, num_grids);
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
  // The polyrhythm state (tap tempo + factor): one instrument-wide machine, both
  // grids' pads.
  let poly = Arc::new(Mutex::new(PolyrhythmState::new(num_grids)));
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
  let accrete: Arc<Mutex<Vec<AccreteState>>> =
    Arc::new(Mutex::new((0..num_grids).map(|_| AccreteState::new()).collect()));
  let held_all = Arc::new(Mutex::new(vec![HashMap::<(i32, i32), i32>::new(); num_grids]));

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
    let hook = feet_accrete_hook(
      Arc::clone(&feet_accrete_on),
      [older, other],
      Arc::clone(&accrete),
      Arc::clone(&held_all),
      Arc::clone(&voices),
      s.release,
      audio.sample_rate,
    );
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
  /// The shared polyrhythm state (tap tempo + factor) and its pairing window.
  poly: Arc<Mutex<PolyrhythmState>>,
  tap_window: Duration,
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
    let toggle = |rect, on: &[AtomicBool]| (rect, button_level(on[rt.grid_index].load(Ordering::Relaxed)));
    buttons.push(toggle(rt.distortion_rect, &rt.distortion_on));
    buttons.push(toggle(rt.slide_rect, &rt.slide_on));
    buttons.push(toggle(rt.mono_rect, &rt.mono_on));
    buttons.push(toggle(rt.feet_accrete_rect, &rt.feet_accrete_on));
    if rt.poly_rect != NO_RECT {
      // The pad's six cells, all per-THIS-grid state: the factor cells show which
      // way this grid's factor leans; =1 shows this grid's pulse switch (bright
      // while its amplitude cycling is on). The tap cell blinks BLACK <-> FULLY
      // LIT (10% duty at this grid's applied tempo; misc.org "tap blink between
      // black and fully lit") whether or not cycling is on -- it is the tempo
      // display -- and only rests dim before any tempo exists, to stay findable.
      let p = rt.poly.lock().unwrap_or_else(|e| e.into_inner());
      let now = Instant::now();
      let tap_level = if p.applied_hz(rt.grid_index).is_none() {
        DIM
      } else if p.tap_blink(rt.grid_index, now) {
        BRIGHT
      } else {
        OFF
      };
      for (dx, dy, level) in [
        (0, 0, button_level(p.factor_lit(rt.grid_index, FactorButton::Times3))),
        (1, 0, button_level(p.factor_lit(rt.grid_index, FactorButton::Times2))),
        (2, 0, tap_level),
        (0, 1, button_level(p.factor_lit(rt.grid_index, FactorButton::Div3))),
        (1, 1, button_level(p.factor_lit(rt.grid_index, FactorButton::Div2))),
        (2, 1, button_level(p.factor_lit(rt.grid_index, FactorButton::Unity))),
      ] {
        let (x, y) = (rt.poly_rect[0] + dx, rt.poly_rect[1] + dy);
        buttons.push(([x, y, x, y], level));
      }
    }
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
  // The polyrhythm pad: | x3 x2 tap | /3 /2 =1 |, key-down only. The tap sets the
  // GLOBAL tempo; the factor buttons and the =1 pulse switch act on THIS grid.
  if in_overlay(rt.poly_rect, cell) {
    if press {
      let (dx, dy) = (cell.0 - rt.poly_rect[0], cell.1 - rt.poly_rect[1]);
      let now = Instant::now();
      let mut p = rt.poly.lock().unwrap_or_else(|e| e.into_inner());
      match (dx, dy) {
        (2, 0) => p.tap(now, rt.tap_window),
        (0, 0) => p.press(rt.grid_index, FactorButton::Times3, now),
        (1, 0) => p.press(rt.grid_index, FactorButton::Times2, now),
        (0, 1) => p.press(rt.grid_index, FactorButton::Div3, now),
        (1, 1) => p.press(rt.grid_index, FactorButton::Div2, now),
        (2, 1) => p.press(rt.grid_index, FactorButton::Unity, now),
        _ => {}
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
      rt.sink.release_sustained();
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
    let timbre =
      Timbre { waveform: slot.waveform, gain: slot.amplitude * gain, am: slot.am, fm: slot.fm };
    // Slide: while on, glide into this note -- legato from the voice mono just
    // cut, or, with no stolen voice, by re-triggering the nearest recently-
    // released pitch (consuming it as a source); otherwise a plain note.
    let source = if legato_from.is_none() && rt.slide_on[rt.grid_index].load(Ordering::Relaxed) {
      rt.slide.pick(pitch, Instant::now(), rt.slide_window)
    } else {
      None
    };
    // The polyrhythm pulse at THIS note's onset (fixed for the note's life):
    // this grid's applied tempo, and only while its =1 pulse switch is on.
    let pulse = rt.poly.lock().unwrap_or_else(|e| e.into_inner()).pulse_hz(rt.grid_index);
    // The note's gain = the slot's amplitude x the grid's fader; the live fader
    // rescale is ratio-based, so the slot amplitude survives later fader moves.
    // (A stolen legato voice keeps ITS timbre and gain -- it is the same voice.)
    let stole = legato_from
      .map(|from| rt.sink.note_on_legato(from, cell, pitch, rt.slide_duration_secs, pulse))
      .unwrap_or(false);
    if !stole {
      match source {
        Some(from) => {
          rt.sink.note_on_gliding(cell, pitch, from, timbre, rt.slide_duration_secs, pulse)
        }
        None => rt.sink.note_on(cell, pitch, timbre, pulse),
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
  let keep = rt.accrete.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
    .note_released_sustains(pitch);
  if keep {
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

/// The accreting condition just turned on for this grid's bank: add the notes
/// currently held ON THIS GRID to its sustained set. Snapshot the held registry
/// first, then feed the bank -- two short, non-nested locks.
fn capture_grid_held(rt: &GridThread) {
  capture_grid_held_into(&rt.held_all, &rt.accrete, rt.grid_index);
}

/// `capture_grid_held` for callers that aren't a grid thread (the pedal hook).
fn capture_grid_held_into(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  grid: usize,
) {
  let snapshot = held_pitches(held_all, grid);
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
fn feet_accrete_hook(
  feet_accrete_on: Arc<Vec<AtomicBool>>,
  triple_banks: [usize; 2],
  accrete: Arc<Mutex<Vec<AccreteState>>>,
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) -> drumkit_runtime::PedalHook {
  Arc::new(move |pedal, down| {
    let Some((triple, button)) = feet_accrete_button(pedal) else {
      return false;
    };
    let grid = triple_banks[triple];
    if !feet_accrete_on.get(grid).map(|b| b.load(Ordering::Relaxed)).unwrap_or(false) {
      return false;
    }
    // Decide under the accrete lock, touch voices after it drops (the module's
    // no-nested-locks rule), exactly like the on-grid buttons.
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
        // The pedals mirror only the trio; erase has no pedal (feet_accrete_button
        // never yields it).
        (AccreteControlKind::Erase, _) => return false,
      }
    }
    if button == AccreteControlKind::Clear && down {
      synth::release_sustained_voices(&voices, grid, release_secs, sample_rate);
    }
    if activated.accrete {
      capture_grid_held_into(&held_all, &accrete, grid);
    }
    if activated.erase {
      // A pedal needs-holding flip can activate a physically-held ERASE button.
      erase_grid_held_into(&held_all, &accrete, grid);
    }
    true
  })
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
mod tests {
  use super::*;
  use midi_pulse::rig::load_named_rig;

  /// Serialises the mock-rig tests: they share the global `STOP` and the mock rig's
  /// listen ports, so they must not run concurrently.
  static MOCK_LOCK: Mutex<()> = Mutex::new(());

  #[test]
  fn selector_writes_the_controlled_grids_timbre_slot() {
    // A grid's selector re-timbres the grid at `controls_index`, leaving every other
    // entry untouched. Wire grid 0's strip to grid 1 (cross-control is still legal
    // rig even though the current rigs self-control): a press on grid 0's cell 3
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
    let rig = load_named_rig("2-monomes_58-8-1_kmss-drums").expect("rig loads");
    let s = resolve_settings(&rig).expect("resolves without hardware");
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
      assert_eq!(g.erase_rect, [1, 14, 1, 14], "grid {:?} erase button", g.monome_id);
      assert_eq!(g.distortion_rect, [0, 1, 0, 1], "grid {:?} distortion toggle", g.monome_id);
      assert_eq!(g.slide_rect, [1, 1, 1, 1], "grid {:?} slide toggle", g.monome_id);
      assert_eq!(g.mono_rect, [1, 2, 1, 2], "grid {:?} mono toggle", g.monome_id);
      assert_eq!(g.feet_accrete_rect, [0, 14, 0, 14], "grid {:?} feet-accrete", g.monome_id);
      assert_eq!(g.poly_rect, [13, 0, 15, 1], "grid {:?} polyrhythm pad", g.monome_id);
    }
    assert_eq!(s.tap_window, Duration::from_millis(2000), "tap window from [surfaces]");
    // The [surfaces] slide knobs flow into the settings.
    assert_eq!(s.slide_window, Duration::from_millis(1000));
    assert!((s.slide_duration_secs - 0.1).abs() < 1e-6);
    // The sink's distortion curve flows into the settings.
    assert_eq!(s.distortion, Distortion { scale: 1.0, shape: 2.0 });
    // The `[surfaces]` table flows into the settings (this rig asks for 9 trails).
    assert_eq!(s.trail_clobber_radius, 27, "trail_clobber_radius from [surfaces]");
    assert_eq!(s.trails_max, 9, "trails_max from [surfaces]");
  }

  #[test]
  fn surfaces_defaults_when_table_absent() {
    // A rig that declares no `[surfaces]` table falls back to the built-in defaults
    // (1/27 octave, 7 trails), so omitting it changes nothing.
    let rig = load_named_rig("monome-edo-sawwave").expect("rig loads");
    assert!(rig.surfaces.is_none(), "no [surfaces] table declared");
    let s = rig.surfaces.unwrap_or_default();
    assert_eq!(s.trail_clobber_radius, 27, "default radius is 1/27 octave");
    assert_eq!(s.trails_max, 7, "default trail length is 7");
  }

  /// A minimal self-controlling grid for the `plan_bringup` tests: `id`'s selector
  /// (present iff `has_selector`) re-timbres `controls_index`; no other overlays.
  fn gs(id: &str, controls_index: usize, has_selector: bool) -> GridSettings {
    GridSettings {
      monome_id: id.to_string(),
      listen_port: 9000,
      prefix: format!("/{id}"),
      edo_rect: [0, 0, 15, 15],
      scroll_rect: NO_RECT,
      selector_rect: if has_selector { [0, 0, 3, 0] } else { NO_RECT },
      controls_index,
      volume_rect: NO_RECT,
      volume_controls_index: controls_index,
      clear_rect: NO_RECT,
      needs_holding_rect: NO_RECT,
      accrete_rect: NO_RECT,
      erase_rect: NO_RECT,
      distortion_rect: NO_RECT,
      slide_rect: NO_RECT,
      mono_rect: NO_RECT,
      feet_accrete_rect: NO_RECT,
      poly_rect: NO_RECT,
    }
  }

  #[test]
  fn plan_all_present_and_softstep_present_loads_everything_silently() {
    let grids = [gs("a", 0, true), gs("b", 1, true)];
    let plan = plan_bringup(&grids, &[true, true], true, true);
    assert!(plan.report.is_empty(), "nothing skipped -> empty report: {:?}", plan.report);
    assert!(plan.drums, "the SoftStep is present");
    assert!(!plan.drop_selector[0] && !plan.drop_selector[1], "self-control keeps both selectors");
  }

  #[test]
  fn plan_absent_grid_is_reported_but_the_present_one_still_loads() {
    // The common "one grid unplugged" case: grid b is gone; grid a (self-controlling)
    // still loads, and only b is reported.
    let grids = [gs("a", 0, true), gs("b", 1, true)];
    let plan = plan_bringup(&grids, &[true, false], false, false);
    assert!(plan.any_grid(), "grid a is present");
    assert!(!plan.drop_selector[0], "a self-controls a present grid -> keep its selector");
    assert_eq!(plan.report.len(), 1, "exactly one skip (grid b)");
    assert!(plan.report[0].contains("\"b\""), "names the absent grid: {:?}", plan.report[0]);
  }

  #[test]
  fn plan_cross_control_to_an_absent_grid_drops_the_selector() {
    // Grid a is present but its selector re-timbres grid b (absent): a keeps playing,
    // but the selector can't do anything, so it does not load -- and is reported.
    let grids = [gs("a", 1, true), gs("b", 1, true)];
    let plan = plan_bringup(&grids, &[true, false], false, false);
    assert!(plan.drop_selector[0], "a's selector controls absent b -> dropped");
    // Two report lines: b absent, and a's selector dropped.
    assert_eq!(plan.report.len(), 2, "{:?}", plan.report);
    assert!(
      plan.report.iter().any(|l| l.contains("waveform selector") && l.contains("\"b\"")),
      "reports the dead cross-control: {:?}",
      plan.report,
    );
  }

  #[test]
  fn plan_missing_softstep_drops_only_the_drumkit() {
    let grids = [gs("a", 0, true)];
    let plan = plan_bringup(&grids, &[true], true, false);
    assert!(!plan.drums, "no SoftStep -> no drums");
    assert!(plan.any_grid(), "the grid still loads");
    assert!(
      plan.report.iter().any(|l| l.contains("SoftStep") && l.contains("drumkit")),
      "reports the missing drumkit: {:?}",
      plan.report,
    );
  }

  #[test]
  fn plan_drums_stand_alone_when_no_grids_are_present() {
    // The SoftStep special case: no grids, but the SoftStep is present -> the drumkit
    // loads on its own (both grids reported absent). `run`'s no-gear guard passes
    // because drums load.
    let grids = [gs("a", 0, true), gs("b", 1, true)];
    let plan = plan_bringup(&grids, &[false, false], true, true);
    assert!(!plan.any_grid(), "no grids present");
    assert!(plan.drums, "the SoftStep alone still brings up the drumkit");
    assert_eq!(plan.report.len(), 2, "both grids reported absent: {:?}", plan.report);
  }

  /// End-to-end against two virtual grids (the monome mock) with null audio: the whole
  /// device layer -- discovery, both grids binding, LED output, key input routing --
  /// which the pure tests cannot cover. No hardware, no sound. See MOCK-MONOME.org.
  #[test]
  fn two_grids_run_against_mock_grids() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_58-8-1_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock surfaces run error: {e}");
        }
      })
    };

    let a = mock.grid(0);
    let b = mock.grid(1);
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

  /// Robust-to-missing-gear (TODO.org): a two-grid rig with only ONE grid connected
  /// still brings up the present grid -- it discovers a single device, binds it as
  /// grid 0, and skips the absent grid 1 (named in the red report) instead of erroring
  /// out the whole run.
  #[test]
  fn one_grid_absent_still_runs_the_present_grid() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Only grid "a" exists; the 2-monome mock rig rig wants two.
    let mock = MockRig::start(0, &[GridSpec::grid_256("a")]).expect("start one mock grid");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_58-8-1_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock one-grid run error: {e}");
        }
      })
    };

    let a = mock.grid(0);
    let secs = Duration::from_secs;
    // The present grid registers and repaints even though its sibling is absent.
    assert!(wait_until(secs(5), || a.registered()), "the present grid registers");
    assert!(wait_until(secs(3), || a.generation() > 0), "first repaint");
    // Its own selector still works (self-control, its target present): triangle lit.
    assert!(wait_until(secs(3), || a.level_at(1, 0) == 15), "grid a selector shows triangle");
    // And it plays: a fingered note lights, then lingers dim in the trail on release.
    a.press(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 15), "fingered note lights on the present grid");
    a.release(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 4), "released note lingers dim");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  #[test]
  fn adopt_rig_swaps_the_live_parameters_and_bumps_the_generation() {
    // Start from the mock rig, then "edit" it (amplitude, tuning, a timbre, the
    // slide knobs) and reload: the live params reflect every change and the
    // generation moves, so grid threads and the audio callback pick them up.
    let base = load_named_rig("2-monomes_58-8-1_kmss-drums-mock").expect("rig loads");
    let s = resolve_settings(&base).expect("resolves");
    let live = Live { generation: AtomicU64::new(0), params: Mutex::new(live_params(&s)) };

    let source = std::fs::read_to_string(
      midi_pulse::rig::mock_rig_dir().join("2-monomes_58-8-1_kmss-drums-mock.toml"),
    )
    .expect("read mock toml");
    let edited = source
      .replace("amplitude = 0.15", "amplitude = 0.25")
      .replace("edo = 58", "edo = 41")
      .replace("x_step = 8", "x_step = 7")
      .replace(WAVE_SQUARE, "waveform = \"square\"\nfm_depth_cents = 25.0")
      .replace("slide_duration_ms = 100", "slide_duration_ms = 250");
    let rig = midi_pulse::rig::parse_rig(&edited).expect("edited rig parses");
    adopt_rig(&rig, &live).expect("adopts");

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
  fn the_pedal_hook_mirrors_each_banks_trio_only_while_its_toggle_is_on() {
    use crate::types::{Timbre, VoiceSource};

    // Triple 0 (pedals 1/2/3) -> grid 0's bank, triple 1 (8/9/0) -> grid 1's.
    let feet_on: Arc<Vec<AtomicBool>> =
      Arc::new((0..2).map(|_| AtomicBool::new(false)).collect());
    let accrete = Arc::new(Mutex::new(vec![AccreteState::new(), AccreteState::new()]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::new(); 2]));
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let hook = feet_accrete_hook(
      Arc::clone(&feet_on),
      [0, 1],
      Arc::clone(&accrete),
      Arc::clone(&held_all),
      Arc::clone(&voices),
      0.05,
      48000.0,
    );

    // Both toggles off: nothing is consumed; the pedals drum as usual.
    assert!(!hook(3, true), "off: pedal 3 stays a drum pad");
    assert!(!accrete.lock().unwrap()[0].accreting());

    feet_on[0].store(true, Ordering::Relaxed);
    // Unmapped pedals still drum even while on.
    assert!(!hook(4, true), "pedal 4 (closed hat) is never mirrored");
    // The other triple's grid is still off: its pedals keep drumming.
    assert!(!hook(0, true), "pedal 0 drums while grid 1's toggle is off");
    // Pedal 3 = grid 0's accrete: default needs-holding is OFF, so a tap toggles the
    // mode -- and the activation captures held notes from grid 0's registry ONLY.
    held_all.lock().unwrap()[0].insert((2, 3), 44);
    held_all.lock().unwrap()[1].insert((7, 7), 51);
    assert!(hook(3, true), "on: pedal 3 is consumed");
    hook(3, false);
    assert!(accrete.lock().unwrap()[0].accreting(), "grid 0's accrete mode toggled by foot");
    assert!(!accrete.lock().unwrap()[1].accreting(), "grid 1's bank untouched");
    assert!(
      accrete.lock().unwrap()[0].note_released_sustains(44),
      "grid 0's held note was captured on activation",
    );
    assert!(
      accrete.lock().unwrap()[1].sustained_classes(58).is_empty(),
      "grid 1's held note was NOT captured (banks are per-monome)",
    );

    // Turn grid 1's toggle on, accrete a note there, then clear it from pedal 8:
    // only grid 1's bank and drone are cleared.
    feet_on[1].store(true, Ordering::Relaxed);
    assert!(hook(0, true), "pedal 0 = grid 1's accrete, now consumed");
    hook(0, false);
    let mut a = SurfaceSink::new(0, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5);
    let mut b = SurfaceSink::new(1, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5);
    a.note_on((5, 5), 20, Timbre::default(), None);
    a.sustain_note((5, 5), 20);
    b.note_on((6, 6), 31, Timbre::default(), None);
    b.sustain_note((6, 6), 31);
    accrete.lock().unwrap()[1].note_played(31);
    assert!(hook(8, true), "pedal 8 = grid 1's clear, consumed");
    hook(8, false);
    let v = voices.lock().unwrap();
    for (src, state) in v.iter() {
      let VoiceSource::Accreted { chord, .. } = src else { continue };
      if *chord < synth::SUSTAIN_BASE {
        continue; // a fingered voice, not a drone
      }
      match chord - synth::SUSTAIN_BASE {
        0 => assert_eq!(state.target_env, 1.0, "grid 0's drone keeps ringing"),
        1 => assert_eq!(state.target_env, 0.0, "grid 1's drone released by its clear"),
        g => panic!("unexpected sustained voice for grid {g}"),
      }
    }
    assert!(
      accrete.lock().unwrap()[1].sustained_classes(58).is_empty(),
      "grid 1's set was flushed (accrete mode itself stays on -- clear never exits it)",
    );
    assert!(
      !accrete.lock().unwrap()[0].sustained_classes(58).is_empty(),
      "grid 0's set survives grid 1's clear",
    );
  }

  /// The polyrhythm pad end-to-end: cells rest dim; two quick taps set the ONE
  /// global tempo (the tap cell blinks on both grids, caught mid-flash); the
  /// factor buttons and the =1 pulse switch are PER-GRID -- a factor press lights
  /// only its own grid, a lone =1 tap turns that grid's cycling on (lit) and
  /// resets its factor, and a fast =1 double-tap turns the cycling back off.
  #[test]
  fn tap_tempo_pad_blinks_globally_and_the_pulse_switch_is_per_grid() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_58-8-1_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock polyrhythm run error: {e}");
        }
      })
    };

    let a = mock.grid(0);
    let b = mock.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");
    // At rest: tap (15,0) and the factor cells glow dim; no blink yet.
    assert!(wait_until(secs(3), || a.level_at(15, 0) == 4), "tap rests dim (no tempo)");
    assert!(wait_until(secs(3), || a.level_at(14, 0) == 4), "x2 rests dim");

    // Two taps ~200 ms apart set a 5 Hz tempo: the tap cell blinks at 10% duty
    // (20 ms on per 200 ms); polling catches an on-flash within a few seconds.
    a.press(15, 0);
    a.release(15, 0);
    thread::sleep(Duration::from_millis(200));
    a.press(15, 0);
    a.release(15, 0);
    assert!(wait_until(secs(5), || a.level_at(15, 0) == 15), "the tap cell blinks on grid a");
    assert!(wait_until(secs(5), || b.level_at(15, 0) == 15), "and on grid b (one shared tempo)");

    // x2 from grid b: PER-GRID -- b's cell lights, a's stays at rest.
    b.press(14, 0);
    b.release(14, 0);
    assert!(wait_until(secs(3), || b.level_at(14, 0) == 15), "grid b's x2 lit (its factor leans up)");
    assert_eq!(a.level_at(14, 0), 4, "grid a's x2 stays dim (factors are per-grid)");

    // A lone =1 tap on grid b: b's cycling turns ON (=1 lit) and its factor resets
    // (x2 back to dim). Grid a's switch is untouched.
    b.press(15, 1);
    b.release(15, 1);
    assert!(wait_until(secs(3), || b.level_at(15, 1) == 15), "grid b's =1 lit: cycling on");
    assert!(wait_until(secs(3), || b.level_at(14, 0) == 4), "=1 reset grid b's factor");
    assert_eq!(a.level_at(15, 1), 4, "grid a's =1 stays dim (the switch is per-grid)");

    // A fast =1 double-tap on grid b: cycling back OFF (the tempo display -- the
    // tap-cell blink -- survives). The four presses land well inside the 400 ms
    // double-tap window; the >400 ms sleep first makes tap one a LONE tap.
    thread::sleep(Duration::from_millis(500));
    b.press(15, 1);
    b.release(15, 1);
    b.press(15, 1);
    b.release(15, 1);
    assert!(wait_until(secs(3), || b.level_at(15, 1) == 4), "a fast double-tap: cycling off");
    assert!(wait_until(secs(5), || b.level_at(15, 0) == 15), "the tap cell still blinks the tempo");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// The accrete (sustain) banks end-to-end: toggle accrete mode on grid a (its trio
  /// lights on grid a ONLY -- one bank per monome), play and release a note there --
  /// it keeps ringing (bright on BOTH grids: drones are sounding everywhere) -- while
  /// a note on grid b does NOT sustain, grid b's clear does NOT touch grid a's drone,
  /// and grid a's own clear finally drops it to the dim trail.
  #[test]
  fn accrete_banks_are_per_monome_and_sustain_until_their_own_clear() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_58-8-1_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock accrete run error: {e}");
        }
      })
    };

    let a = mock.grid(0);
    let b = mock.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");

    // At rest all three buttons glow dim (findable), on both grids.
    for (x, name) in [(0, "clear"), (1, "needs_holding"), (2, "accrete")] {
      assert!(wait_until(secs(3), || a.level_at(x, 15) == 4), "grid a {name} rests dim");
      assert!(wait_until(secs(3), || b.level_at(x, 15) == 4), "grid b {name} rests dim");
    }

    // Tap accrete on grid a: needs_holding starts OFF, so key-down toggles accrete
    // mode on -- on grid a's bank ONLY (one bank per monome).
    a.press(2, 15);
    a.release(2, 15);
    assert!(wait_until(secs(3), || a.level_at(2, 15) == 15), "accrete lit on grid a");
    thread::sleep(Duration::from_millis(300)); // let grid b repaint before the negative check
    assert_eq!(b.level_at(2, 15), 4, "grid b's accrete stays dim (its own bank is off)");

    // Play and release a note on grid a: sustained, it stays BRIGHT (not trail-dim)
    // on BOTH grids -- the drone is sounding, so it reflects everywhere.
    a.press(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 15), "fingered note lights");
    a.release(5, 5);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(5, 5), 15, "released note keeps ringing bright on grid a");
    assert_eq!(b.level_at(5, 5), 15, "and reflects bright on grid b");

    // A note on grid b does NOT sustain: its bank is not accreting.
    b.press(8, 5);
    assert!(wait_until(secs(3), || b.level_at(8, 5) == 15), "grid b's note lights");
    b.release(8, 5);
    assert!(wait_until(secs(3), || b.level_at(8, 5) == 4), "and drops to the trail on release");

    // needs_holding on grid b: lights on grid b ONLY, and does NOT cancel grid a's
    // toggled mode (independent banks).
    b.press(1, 15);
    b.release(1, 15);
    assert!(wait_until(secs(3), || b.level_at(1, 15) == 15), "needs_holding lit on grid b");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(1, 15), 4, "grid a's needs_holding stays dim");
    assert_eq!(a.level_at(2, 15), 15, "grid a's accrete mode survives");

    // Clear from grid B: lights there while held, but grid a's drone keeps ringing.
    b.press(0, 15);
    assert!(wait_until(secs(3), || b.level_at(0, 15) == 15), "grid b's clear lit while held");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(0, 15), 4, "grid a's clear stays dim");
    b.release(0, 15);
    assert!(wait_until(secs(3), || b.level_at(0, 15) == 4), "clear dims on key-up");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(5, 5), 15, "grid b's clear leaves grid a's drone ringing");

    // Clear from grid A: NOW the note falls back to the dim trail on both grids.
    a.press(0, 15);
    a.release(0, 15);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 4), "cleared note drops to the trail on a");
    assert!(wait_until(secs(3), || b.level_at(5, 5) == 4), "and on b");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// The toggles end-to-end: every one (distortion / slide / mono / feet-accrete)
  /// is PER-GRID -- each rests dim, a press lights only that grid's cell, the two
  /// grids' switches are independent, and each turns off from its own grid.
  #[test]
  fn every_toggle_is_per_grid_and_independent() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_58-8-1_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock distortion run error: {e}");
        }
      })
    };

    let a = mock.grid(0);
    let b = mock.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");
    // Distortion (0,1), slide (1,1), mono (1,2), and feet-accrete (0,14) are all
    // PER-GRID: grid a's press lights grid a only, and grid b's toggle is a
    // separate switch (b's press turns b ON, not a off).
    for (x, y, name) in
      [(0, 1, "distortion"), (1, 1, "slide"), (1, 2, "mono"), (0, 14, "feet-accrete")]
    {
      assert!(wait_until(secs(3), || a.level_at(x, y) == 4 && b.level_at(x, y) == 4),
        "{name} rests dim on both grids");
      a.press(x, y);
      a.release(x, y);
      assert!(wait_until(secs(3), || a.level_at(x, y) == 15), "{name} on: lit on grid a");
      thread::sleep(Duration::from_millis(300));
      assert_eq!(b.level_at(x, y), 4, "grid b's {name} stays dim (per-grid switch)");
      b.press(x, y);
      b.release(x, y);
      assert!(wait_until(secs(3), || b.level_at(x, y) == 15), "grid b's own {name} turns b on");
      assert_eq!(a.level_at(x, y), 15, "grid a's {name} stays on -- independent switches");
      a.press(x, y);
      a.release(x, y);
      assert!(wait_until(secs(3), || a.level_at(x, y) == 4), "grid a's {name} off again");
      b.press(x, y);
      b.release(x, y);
      assert!(wait_until(secs(3), || b.level_at(x, y) == 4), "grid b's {name} off again");
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
    let mock = MockRig::start(0, &[GridSpec::grid_256("m256-9"), GridSpec::grid_256("m0000777")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_58-8-1_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock monobright run error: {e}");
        }
      })
    };

    let mono = mock.grid(0); // Series-256 serial -> fake-dim by flashing.
    let vari = mock.grid(1); // newer serial -> native levels.
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
