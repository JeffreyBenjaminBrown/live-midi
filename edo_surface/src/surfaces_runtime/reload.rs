//! The hot-reloadable ('r' + Enter) subset of the settings: `Live` / `LiveParams`,
//! resolving and adopting them from a freshly-loaded rig (`live_params`,
//! `live_makeup`, `reload_live`, `adopt_rig`), and refreshing one grid thread's
//! working copies when the generation moves (`refresh_live`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::drumkit_runtime::audio::SamplerAmplitude;
use crate::expression_pedals::NUM_PEDALS;
use crate::rig::{load_named_rig, Rig, SinkRig};

use crate::voices::{Distortion, Makeup};

use super::grid::SELECTOR_CELLS;
use super::pedal_volume::PedalVolumeCurve;
use super::settings::{resolve_settings, Settings, TimbreSlot};
use super::GridThread;

/// The hot-reloadable ('r' + Enter) subset of the settings -- the scalars a running
/// instrument can adopt without rebinding grids, sockets, or streams: the synth
/// master amplitude, the distortion curve, the four timbres, the tuning, the pluck
/// envelope, the slide/trail knobs, and the drum samplers' master gains. See
/// TODO/misc.org "rig reload".
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
  /// The running drum samplers' master gains, by sink id -- a side table rather than a
  /// `LiveParams` field because the drumkit is brought up LATER than this struct (and
  /// later than the 'r' thread that reads it), and may not exist at all. Empty until
  /// `register_samplers`; a reload before that, or on a rig with no drums, simply
  /// finds nothing to write.
  pub(super) samplers: Mutex<Vec<(String, SamplerAmplitude)>>,
}

impl Live {
  /// The live parameters as resolved from a rig at bring-up. Drum gains are NOT here:
  /// the drumkit starts later, and registers itself with `register_samplers`.
  pub(super) fn new(s: &Settings) -> Live {
    Live {
      generation: AtomicU64::new(0),
      params: Mutex::new(live_params(s)),
      makeup: Mutex::new(live_makeup(s)),
      samplers: Mutex::new(Vec::new()),
    }
  }

  /// Hand the reload path live handles on the drum samplers' gains. Called once, at
  /// drumkit bring-up.
  pub(super) fn register_samplers(&self, samplers: Vec<(String, SamplerAmplitude)>) {
    *self.samplers.lock().unwrap_or_else(|e| e.into_inner()) = samplers;
  }
}

/// Push `rig`'s `cpal_sampler` amplitudes into the running samplers, matched by sink
/// id. Returns how many were applied.
///
/// Only the scalar travels: a sink that appeared, vanished, or changed sample rate is
/// ignored, because those rebind an audio stream and need a restart -- the same rule
/// the rest of the reload follows for layout and ports.
fn adopt_sampler_amplitudes(rig: &Rig, live: &Live) -> usize {
  let cells = live.samplers.lock().unwrap_or_else(|e| e.into_inner());
  let mut applied = 0;
  for sink in &rig.sinks {
    let SinkRig::CpalSampler { id, amplitude, .. } = sink else { continue };
    for (cell_id, cell) in cells.iter() {
      if cell_id == id {
        cell.set(*amplitude);
        applied += 1;
      }
    }
  }
  applied
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
  pub retrigger_tail_detune_cents: f32,
  pub crowded_pitch_planck_deviation: f32,
  pub slide_window: Duration,
  pub slide_duration_secs: f32,
  /// The pedal-slide pitch smoother's time constant, in seconds -- read fresh by the
  /// audio callback each block, so 'r' retunes the glide feel live.
  pub slide_pedal_smoother_secs: f32,
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
    retrigger_tail_detune_cents: s.retrigger_tail_detune_cents,
    crowded_pitch_planck_deviation: s.crowded_pitch_planck_deviation,
    slide_window: s.slide_window,
    slide_duration_secs: s.slide_duration_secs,
    slide_pedal_smoother_secs: s.slide_pedal_smoother_secs,
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
      "reloaded {name}: amplitude / distortion / timbres / tuning / pluck / retrigger detune / slide / trail / pedal curves / drum volume applied (layout + ports need a restart)",
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
  // The drum gains live outside `LiveParams` (see `Live::samplers`) and outside the
  // generation: the audio callback reads their atomics directly, so no grid thread
  // needs to be told.
  adopt_sampler_amplitudes(rig, live);
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
  rt.sink.retune(
    p.fund,
    p.edo,
    p.sustain_level,
    p.decay_secs,
    p.retrigger_tail_detune_cents,
    p.crowded_pitch_planck_deviation,
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::drumkit_runtime::audio::Sampler;

  /// A `Live` as `run` builds one, minus the drum registration.
  fn live_for(rig: &Rig) -> Live {
    Live::new(&resolve_settings(rig).expect("the rig resolves"))
  }

  fn set_drum_amplitude(rig: &mut Rig, to: f32) {
    for sink in &mut rig.sinks {
      if let SinkRig::CpalSampler { amplitude, .. } = sink {
        *amplitude = to;
      }
    }
  }

  /// The point of the feature: edit the drum sink's amplitude, press 'r', and the
  /// RUNNING sampler's gain moves -- no restart, and without disturbing the stream.
  #[test]
  fn a_reload_revises_a_running_samplers_gain() {
    let mut rig = load_named_rig("2-edogrids_ss-accrete_ss-drums").expect("drums rig loads");
    let live = live_for(&rig);
    // The null sampler is the headless stream; its gain cell is the real one.
    let sampler = Sampler::start_null(48000, 1.6);
    live.register_samplers(vec![("drums".to_string(), sampler.amplitude())]);
    assert_eq!(sampler.amplitude().get(), 1.6, "starts at the rig's value");

    set_drum_amplitude(&mut rig, 0.9);
    adopt_rig(&rig, &live).expect("the edited rig adopts");
    assert_eq!(sampler.amplitude().get(), 0.9, "the running sampler took the new gain");
  }

  /// Matching is by SINK ID, not position: a gain must never land on the wrong sink.
  #[test]
  fn an_unmatched_sink_id_is_left_alone() {
    let mut rig = load_named_rig("2-edogrids_ss-accrete_ss-drums").expect("drums rig loads");
    let live = live_for(&rig);
    let sampler = Sampler::start_null(48000, 1.6);
    live.register_samplers(vec![("some-other-kit".to_string(), sampler.amplitude())]);

    set_drum_amplitude(&mut rig, 0.9);
    assert_eq!(adopt_sampler_amplitudes(&rig, &live), 0, "no sink id matched");
    assert_eq!(sampler.amplitude().get(), 1.6, "an unrelated sampler is untouched");
  }

  /// The pulse rig has no sampler at all, and a reload can also fire before the
  /// drumkit has registered. Neither may fail or panic.
  #[test]
  fn a_reload_without_registered_drums_is_harmless() {
    let rig = load_named_rig("2-edogrids_ss-accrete_ss-pulse").expect("pulse rig loads");
    let live = live_for(&rig);
    assert_eq!(adopt_sampler_amplitudes(&rig, &live), 0, "nothing registered, nothing to do");
    adopt_rig(&rig, &live).expect("a drumless reload still succeeds");
    assert_eq!(live.generation.load(Ordering::SeqCst), 1, "and still bumps the generation");
  }
}
