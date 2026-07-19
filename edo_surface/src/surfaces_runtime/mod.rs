//! The surfaces runtime: two independent EDO play grids (each with a scroll pad and a
//! waveform selector + volume strip -- wired per rig via `controls`, which the
//! current rigs point at the strip's own grid) plus the KMSS pedalboard as a drumkit
//! -- three surfaces at once, in one process.
//!
//! `midi_pulse.rs::main` dispatches to exactly one runtime, and the three features
//! each otherwise take over the whole app (EDO grid+synth = sawwave, the scroll pad =
//! looper, the drums = drumkit). This runtime composes them by reusing the already-
//! shared pure pieces: the sawwave voice/render engine (`crate::{types,voices,pitch}`),
//! the lib scroll math (`crate::edo_play`), the two-grid bind
//! (`crate::device_assign`), and the drumkit's own bring-up
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
mod hooks;
mod keys;
mod paint;
mod pedal_volume;
mod polyrhythm;
mod pulse_window;
mod readout;
mod reload;
mod ring;
mod settings;
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

use crate::rig::Rig;
use crate::device_assign::assign_selected_devices;
use crate::edo_play::step_for_cell;
use crate::monome::{self, DeviceInfo};
use crate::monome_brightness::PulseBrightness;

use crate::drumkit_runtime;
use crate::types::VoiceMap;

// Used only by tests.rs (via `use super::*;`); gated so a non-test build doesn't warn.
#[cfg(test)]
use crate::rig::{AccreteControlKind, MonomeWindowRig, SinkRig};
#[cfg(test)]
use crate::voices::Distortion;

use accrete::AccreteState;
use ring::{GridRing, Reason};
use polyrhythm::{TempoFactorButton, PolyrhythmState};
use slide::SlideCandidates;
use grid::{
  button_level, levels_for_grid, volume_cells, volume_gain_for_pos, BRIGHT, DIM, OFF,
  SELECTOR_CELLS,
};
use synth::SurfaceSink;

// The submodules below are the phase-3 split of what used to be one 2.7k-line
// mod.rs (see TODO/cleaning/2_plan.org phase 3); mod.rs itself keeps run(),
// grid_thread + GridThread, and the shared signal/STOP machinery. Everything
// is glob-imported back into this namespace so `tests.rs`'s `use super::*;`
// keeps resolving exactly what it did when it was all one file.
use hooks::*;
use keys::*;
use paint::*;
use pedal_volume::*;
use reload::*;
use settings::*;

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
  let selects: Vec<crate::rig::MonomeSelect> =
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
    let cells = volume_cells(g.overlays.volume_rect);
    if cells > 0 {
      let pos = (VOLUME_DEFAULT_COL - g.overlays.volume_rect[0]).clamp(0, cells - 1);
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
  // The per-grid ring: one `GridRing` per monome (its accrete bank, its edit-mode
  // machine, and the shared store of what rings), all under ONE lock (cleaning phase
  // 6, `1_vision` item 1). Absorbing the old separate `Vec<AccreteState>` and
  // `Vec<EditState>` handles into one removes the two-lock ordering hazard: entering
  // edit mode and sustaining a pitch write the same store, so the two can never
  // disagree about what rings, and the pedal hook (which reads and writes both) takes
  // a single lock. A bank whose `needs_holding` switch is bound NOWHERE can never
  // leave whatever mode it starts in; leaving it at the toggle default is then a trap
  // (one tap latches sustain on), so such a bank comes up momentary -- hold to
  // accrete, lift to stop.
  let ring: Arc<Mutex<Vec<GridRing>>> = Arc::new(Mutex::new(
    (0..num_grids)
      .map(|g| {
        let accrete = if grid_has_needs_holding_control(rig, &s.grids[g]) {
          AccreteState::new()
        } else {
          AccreteState::new_momentary()
        };
        GridRing::new(accrete)
      })
      .collect(),
  ));
  let held_all = Arc::new(Mutex::new(vec![HashMap::<(i32, i32), i32>::new(); num_grids]));

  // Bring up the drumkit alongside the grids, if the rig declares one. Consumed
  // from `drumkit_runtime` (not forked); kept alive for the run, restoring standalone
  // mode on drop. We own the signal handling, so the tether session is unarmed. The
  // pedal hook drives every rig-declared `accrete_control` / `tap_tempo_pedal` /
  // `pulse_factor_pedal` / `editmode_control` softstep window (`rig_pedal_actions`);
  // a rig with none of those (e.g. the standalone drumkit rig) gets an empty map, so
  // every pedal simply drums.
  let drums = if plan.drums {
    let actions = rig_pedal_actions(rig, |m| s.grids.iter().position(|g| g.monome_id == m));
    println!("surfaces: {} rig-declared pedal binding(s)", actions.len());
    let hook = rig_pedal_hook(
      actions,
      Arc::clone(&ring),
      Arc::clone(&held_all),
      Arc::clone(&voices),
      Arc::clone(&poly),
      s.tap_window,
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

  // The tuning inputs are one instrument-wide register geometry, identical for every
  // grid thread; built once and `Copy`-ed into each `GridThread` below.
  let tuning = Tuning {
    x_step: s.x_step,
    y_step: s.y_step,
    edo: s.edo,
    fund: s.fund,
    grid_w: s.grid_w,
    grid_h: s.grid_h,
  };
  // The Arc'd cross-grid handles, bundled once so each grid thread below just clones
  // the whole bundle (cheap: it's all `Arc::clone`) instead of naming every field.
  let shared = Shared {
    selected: Arc::clone(&selected),
    sounding: Arc::clone(&sounding),
    trail: Arc::clone(&trail),
    volume_pos: Arc::clone(&volume_pos),
    gains: Arc::clone(&gains),
    ring: Arc::clone(&ring),
    held_all: Arc::clone(&held_all),
    distortion_on: Arc::clone(&distortion_on),
    slide_on: Arc::clone(&slide_on),
    mono_on: Arc::clone(&mono_on),
    poly: Arc::clone(&poly),
    live: Arc::clone(&live),
    voices: Arc::clone(&voices),
  };

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
    let mut overlays = g.overlays;
    if plan.drop_selector[grid_index] {
      overlays.selector_rect = NO_RECT;
    }
    if plan.drop_volume[grid_index] {
      overlays.volume_rect = NO_RECT;
    }
    let knobs = Knobs {
      trail_clobber_radius: s.trail_clobber_radius,
      trails_max: s.trails_max,
      slide_window: s.slide_window,
      slide_duration_secs: s.slide_duration_secs,
      tap_window: s.tap_window,
      echo_input: s.echo_input,
      controls_index: g.controls_index,
      volume_controls_index: g.volume_controls_index,
    };
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
      timbres: s.timbres,
      overlays,
      editmode_clear_down: false,
      editmode_accrete_down: false,
      tuning,
      knobs,
      shared: shared.clone(),
      slide: SlideCandidates::new(),
      started: Instant::now(),
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
        Arc::clone(&gains),
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

/// The Arc'd cross-grid handles every grid thread shares: state behind a `Mutex` or
/// `AtomicBool`, so every field here is read through `&Shared` alone -- no field ever
/// needs `&mut Shared` (the interior mutability is the point). `Clone` is a bundle of
/// Arc clones (free), so a whole `Shared` can be handed to a new thread, or re-cloned
/// to dodge a borrow fight, without a second thought (cleaning phase 4: "if that means
/// more clones, whatever").
#[derive(Clone)]
struct Shared {
  /// Per-grid selected timbre slot; written by whichever grid's selector controls it.
  selected: Arc<Mutex<Vec<usize>>>,
  /// Per-grid sounding pitch-classes; the union drives cross-grid note reflection.
  sounding: Arc<Mutex<Vec<HashSet<i32>>>>,
  /// Shared recent-note trail (pitch classes), newest first.
  trail: Arc<Mutex<VecDeque<i32>>>,
  /// Per-grid volume position (column within the strip) and linear gain.
  volume_pos: Arc<Mutex<Vec<i32>>>,
  gains: Arc<Mutex<Vec<f32>>>,
  /// The per-grid ring: each `GridRing` bundles a monome's accrete bank, its edit-mode
  /// machine, and the shared store of which pitches ring (for sustain and for edit),
  /// all under ONE lock. This grid's trio + edit gestures drive `ring[grid_index]`
  /// only. Absorbs what used to be two separate `Vec<AccreteState>` /
  /// `Vec<EditState>` handles (cleaning phase 6), so sustain and edit can never
  /// disagree about what sounds and there is no cross-lock ordering to get wrong.
  ring: Arc<Mutex<Vec<GridRing>>>,
  /// Every grid's held notes (cell -> struck pitch), for accrete's capture-on-
  /// activation. Each grid thread rewrites only its own slot.
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  /// The per-grid distortion switches; this grid's toggle flips (and its LED shows)
  /// element `grid_index`, and the audio callback routes voices by them.
  distortion_on: Arc<Vec<AtomicBool>>,
  /// The per-grid slide / mono switches (this grid uses element `grid_index`).
  slide_on: Arc<Vec<AtomicBool>>,
  mono_on: Arc<Vec<AtomicBool>>,
  /// The shared polyrhythm state (tap tempo + tempo factor) and its pairing window.
  poly: Arc<Mutex<PolyrhythmState>>,
  /// The hot-reloadable parameters; refreshed into `GridThread`'s own fields when the
  /// generation moves (see `refresh_live`).
  live: Arc<Live>,
  /// The shared voice map, for the live volume rescale of the controlled grid's voices.
  voices: Arc<Mutex<VoiceMap>>,
}

/// The tuning inputs, identical across every grid in one run (one instrument-wide
/// tuning + one register geometry): the register-math step sizes, the EDO, the
/// fundamental, and the grid's cell dimensions.
#[derive(Clone, Copy)]
struct Tuning {
  x_step: i32,
  y_step: i32,
  edo: i32,
  /// Tuning fundamental (Hz), for the optional `echo_input` note echo.
  fund: f64,
  grid_w: i32,
  grid_h: i32,
}

/// The instrument-wide scalar knobs (`[trail]` / `[slide]` / `[tap_tempo]` + which
/// grid this one's overlays control), refreshed alongside `Tuning` on a hot reload.
#[derive(Clone, Copy)]
struct Knobs {
  /// Trail clobber radius as a divisor of the octave (see `Settings`).
  trail_clobber_radius: i32,
  /// Max distinct pitch classes the shared trail keeps.
  trails_max: usize,
  slide_window: Duration,
  slide_duration_secs: f32,
  tap_window: Duration,
  /// Echo each fingered note on this grid to stderr; off unless the rig sets
  /// `echo_input`. Kept off so a startup warning isn't scrolled away as you play.
  echo_input: bool,
  /// The grid index this grid's waveform selector re-timbres.
  controls_index: usize,
  /// The grid index this grid's volume strip sets the loudness of.
  volume_controls_index: usize,
}

/// Everything one grid thread owns for the run. Grouped from first principles
/// (cleaning phase 4, `TODO/cleaning/2_plan.org`): `overlays` (this grid's rects,
/// shared verbatim with `GridSettings`) + the two editmode press-LED bools that have
/// no settings-side counterpart, `tuning` + `knobs` (the register/rig-scalar inputs,
/// refreshed together on a hot reload), `shared` (the Arc'd cross-grid handles,
/// cheaply `Clone`-able), and the genuinely per-thread machinery left flat below.
struct GridThread {
  grid_index: usize,
  sock: UdpSocket,
  prefix: String,
  listen_port: u16,
  device_id: String,
  device_port: u16,
  /// A monobright grid fakes DIM by flashing; a varibright grid sends native levels.
  monobright: bool,
  /// The four selectable timbres (shared instrument-wide table, copied per thread).
  timbres: [TimbreSlot; SELECTOR_CELLS],
  overlays: Overlays,
  /// Whether each editmode_control button is physically down right now (they light
  /// while pressed, like accrete's clear). Runtime press state, not a rig setting,
  /// so it stays out of the `Overlays` shared with `GridSettings`.
  editmode_clear_down: bool,
  editmode_accrete_down: bool,
  tuning: Tuning,
  knobs: Knobs,
  shared: Shared,
  /// THIS grid's recently-released notes (slide sources) + the slide knobs.
  slide: SlideCandidates,
  /// When this runtime started. The diamond dance's phase is a pure function of
  /// elapsed time from here, so every dance on the instrument turns in step -- that
  /// is the whole reason a skipped corner is not allowed to retime its dance.
  started: Instant,
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
  let mut live_generation = rt.shared.live.generation.load(Ordering::SeqCst);

  while !STOP.load(Ordering::SeqCst) {
    // Adopt hot-reloaded parameters ('r'): cheap generation check per iteration.
    let generation = rt.shared.live.generation.load(Ordering::SeqCst);
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
    let selector_slot = current_slot(&rt.shared.selected, rt.knobs.controls_index);
    let volume_col = volume_active_col(&rt);
    let (mut buttons, sustained_classes) = accrete_view(&rt);
    let toggle = |rect, on: &[AtomicBool]| (rect, button_level(on[rt.grid_index].load(Ordering::Relaxed)));
    buttons.push(toggle(rt.overlays.distortion_rect, &rt.shared.distortion_on));
    buttons.push(toggle(rt.overlays.slide_rect, &rt.shared.slide_on));
    buttons.push(toggle(rt.overlays.mono_rect, &rt.shared.mono_on));
    // The editmode buttons: dim at rest (findable), bright while pressed.
    buttons.push((rt.overlays.editmode_clear_rect, button_level(rt.editmode_clear_down)));
    buttons.push((rt.overlays.editmode_accrete_rect, button_level(rt.editmode_accrete_down)));
    if rt.overlays.poly_rect != NO_RECT {
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
      let p = rt.shared.poly.lock().unwrap_or_else(|e| e.into_inner());
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
        let (x, y) = (rt.overlays.poly_rect[0] + dx, rt.overlays.poly_rect[1] + dy);
        buttons.push(([x, y, x, y], level));
      }
    }
    let mut sounding_classes = union_sounding(&rt.shared.sounding);
    // `sustained_classes` (from `accrete_view`) is the union of every grid's SUSTAINED
    // pitch classes, painted bright on both grids -- they are all audible. An edited note
    // is always sustained too (edited ⊆ sustained, branch-3 queue item 4), so it is
    // already in this set and needs no separate union: this supersedes the old code that
    // unioned the edit set in by hand because edit was its own, non-sustained reason.
    sounding_classes.extend(sustained_classes);
    let trail_classes = trail_set(&rt.shared.trail);

    // The diamond dance (edit-mode) and the square dance (sustained), plus the
    // off-screen indicator. All three read THIS grid's edit set and its own
    // sustained pitches: local, never mirrored from the other grid and never
    // octave-duplicated, unlike everything else painted here.
    let elapsed = rt.started.elapsed();
    // Snapshot both under one lock, then draw: the pedal hook writes these too
    // (`clear`).
    let (edited, sustained): (Vec<i32>, Vec<i32>) = {
      let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
      let store = &rings[rt.grid_index].store;
      (store.iter(Reason::Edit).collect(), store.iter(Reason::Sustain).collect())
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
    // The square dance: the same rule, but for sustained pitches, at the diagonal
    // neighbours, T/8 out of phase with the diamond. Because an edited note is always
    // sustained (edited ⊆ sustained, branch-3 queue item 4), EVERY edited note also
    // square-dances -- the 8-position circle is now the normal look of an edited note,
    // not the rare edited-and-separately-sustained case it used to be. Shares
    // `cells_for_pitch` and `dance_cells` with the diamond above, so it gets the same
    // at-most-two-images and clobber/yield compositing for free.
    for pitch in sustained.iter().copied() {
      for (x, y) in cells_for_pitch(&rt, register, pitch) {
        dance_cells.insert(dance::diagonal_cell((x, y), elapsed));
      }
    }
    // The visible pitch window, for "is that note off-screen".
    let [ex0, ey0, ex1, ey1] = rt.overlays.edo_rect;
    let corners = [
      step_for_cell(rt.tuning.x_step, rt.tuning.y_step, register, ex0, ey0),
      step_for_cell(rt.tuning.x_step, rt.tuning.y_step, register, ex1, ey1),
    ];
    let (lo, hi) = (corners[0].min(corners[1]), corners[0].max(corners[1]));
    let off = if dance::flash_on(elapsed) {
      // One signal for BOTH edit-mode and sustained notes -- Jeff's call ("in both
      // cases"), so the LED cannot say which kind you are chasing.
      dance::off_screen(edited.iter().copied().chain(sustained.iter().copied()), lo, hi)
    } else {
      dance::OffScreen::default()
    };

    let levels = levels_for_grid(
      &sounding_classes,
      &trail_classes,
      &dance_cells,
      off,
      rt.overlays.edo_rect,
      rt.overlays.selector_rect,
      selector_slot,
      rt.overlays.volume_rect,
      volume_col,
      rt.overlays.scroll_rect,
      &buttons,
      register,
      rt.tuning.x_step,
      rt.tuning.y_step,
      rt.tuning.edo,
      rt.tuning.grid_w,
      rt.tuning.grid_h,
    );

    let (grid_w, grid_h) = (rt.tuning.grid_w, rt.tuning.grid_h);
    if rt.monobright {
      // Steady frame with DIM cells dark; briefly pulse them on at ~1/32 duty. The
      // on-frame's transmit time bounds the on-period, so a heavy dim set slows the
      // effective flash into (accepted) flicker.
      send_binary_frame(&rt.sock, device, &rt.prefix, grid_w, grid_h, &levels, false, &mut last_quads);
      let now = Instant::now();
      if now >= next_pulse {
        send_binary_frame(&rt.sock, device, &rt.prefix, grid_w, grid_h, &levels, true, &mut last_quads);
        thread::sleep(DIM_PULSE.on_time);
        send_binary_frame(&rt.sock, device, &rt.prefix, grid_w, grid_h, &levels, false, &mut last_quads);
        next_pulse = now + DIM_PULSE.period;
      }
    } else {
      send_diffs(&rt.sock, device, &rt.prefix, grid_w, &levels, &mut last_levels);
    }
  }

  // run() blanks every grid authoritatively after the joins; this is best-effort.
  monome::send_led_all(&rt.sock, device, &rt.prefix, 0);
}

/// A serialosc serial id that names an old monobright "Series" grid (per-LED on/off
/// only, so it thresholds any level <= 7 to off). The newer format (e.g. `m0000102`) is
/// varibright. Both a monobright 256 and a varibright 16x16 report type "monome 256", so
/// the id -- not the type string -- is what distinguishes them. Heuristic; the hardware
/// pass confirms it (a monobright grid drops a level-4 cell to dark).
fn is_monobright(id: &str) -> bool {
  ["m40h-", "m64-", "m128-", "m256-"].iter().any(|p| id.starts_with(p))
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
