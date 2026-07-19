//! The hot-reloadable ('r' + Enter) subset of the settings: `Live` / `LiveParams`,
//! resolving and adopting them from a freshly-loaded rig (`live_params`,
//! `live_makeup`, `reload_live`, `adopt_rig`), and refreshing one grid thread's
//! working copies when the generation moves (`refresh_live`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use midi_pulse::expression_pedals::NUM_PEDALS;
use midi_pulse::rig::{load_named_rig, Rig};

use crate::voices::{Distortion, Makeup};

use super::grid::SELECTOR_CELLS;
use super::pedal_volume::PedalVolumeCurve;
use super::settings::{resolve_settings, Settings, TimbreSlot};
use super::GridThread;

/// The hot-reloadable ('r' + Enter) subset of the settings -- the scalars a running
/// instrument can adopt without rebinding grids, sockets, or streams: the synth
/// master amplitude, the distortion curve, the four timbres, the tuning, the pluck
/// envelope, and the slide/trail knobs. See TODO/misc.org "rig reload".
pub(crate) struct Live {
  /// Bumped on every successful reload; grid threads refresh their copies when it
  /// moves (the audio callback reads the params fresh every callback).
  pub(super) generation: AtomicU64,
  pub(super) params: Mutex<LiveParams>,
  /// The distortion's makeup table. Separate from `LiveParams` (which is `Copy`, so
  /// every grid thread can snapshot it) because the table is an allocation, and built
  /// here -- off the audio thread -- because building it integrates the clipper's gain
  /// 512 times. The audio callback only clones the `Arc`.
  pub(super) makeup: Mutex<Arc<Makeup>>,
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
pub(super) fn live_params(s: &Settings) -> LiveParams {
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
pub(super) fn live_makeup(s: &Settings) -> Arc<Makeup> {
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
pub(super) fn reload_live(name: &str, live: &Live) {
  match load_named_rig(name).and_then(|rig| adopt_rig(&rig, live)) {
    Ok(()) => println!(
      "reloaded {name}: amplitude / distortion / timbres / tuning / pluck / slide / trail / pedal curves applied (layout + ports need a restart)",
    ),
    Err(e) => eprintln!("reload of {name} failed; keeping the running parameters: {e}"),
  }
}

/// Adopt `rig`'s live parameters into `live` and bump the generation. Everything
/// non-live (window layout, ports, sinks' sample rates) is silently kept as-is.
pub(super) fn adopt_rig(rig: &Rig, live: &Live) -> Result<(), String> {
  let s = resolve_settings(rig).map_err(|e| e.to_string())?;
  // Build the makeup table before taking either lock: the integration is the slow part,
  // and the audio callback holds `makeup` briefly on every callback.
  let makeup = live_makeup(&s);
  *live.params.lock().unwrap_or_else(|e| e.into_inner()) = live_params(&s);
  *live.makeup.lock().unwrap_or_else(|e| e.into_inner()) = makeup;
  live.generation.fetch_add(1, Ordering::SeqCst);
  Ok(())
}

/// Adopt the current `Live` parameters into this grid thread's working copies. An
/// `edo` change invalidates every stored pitch *class*, so the shared trail is
/// cleared (held pitches keep sounding; their classes are recomputed from the raw
/// pitch on the next publish).
pub(super) fn refresh_live(rt: &mut GridThread) {
  let p = *rt.shared.live.params.lock().unwrap_or_else(|e| e.into_inner());
  if p.edo != rt.tuning.edo {
    rt.shared.trail.lock().unwrap_or_else(|e| e.into_inner()).clear();
  }
  rt.tuning.x_step = p.x_step;
  rt.tuning.y_step = p.y_step;
  rt.tuning.edo = p.edo;
  rt.knobs.trail_clobber_radius = p.trail_clobber_radius;
  rt.knobs.trails_max = p.trails_max;
  rt.knobs.slide_window = p.slide_window;
  rt.knobs.slide_duration_secs = p.slide_duration_secs;
  rt.knobs.tap_window = p.tap_window;
  rt.timbres = p.timbres;
  rt.sink.retune(p.fund, p.edo, p.sustain_level, p.decay_secs);
}
