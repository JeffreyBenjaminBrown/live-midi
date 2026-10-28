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
use crate::strings::StringsParams;

use crate::voices::{Distortion, Makeup};

use super::grid::SELECTOR_CELLS;
use super::pedal_volume::PedalVolumeCurve;
use super::settings::{resolve_settings, PedalTarget, Settings, TimbreSlot};
use super::GridThread;

/// The hot-reloadable ('r' + Enter) subset of the settings -- the scalars a running
/// instrument can adopt without rebinding grids, sockets, or streams: the synth
/// master amplitude, the distortion curve, the four timbres, the tuning, the pluck
/// envelope, the slide/trail knobs, the strings engine's parameters, and the drum
/// samplers' master gains. What else a rig sets (layout, ports, a sink's kind) needs
/// a restart.
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
  /// `(what it drives, taper)`. Live so Jeff can tweak the curve_* knobs mid-play
  /// ('r' reload); the pedal thread re-reads them each poll. Rebinding or
  /// removing a pedal live also takes effect, but a rig that STARTS with no
  /// pedals never spawns the thread -- adding the first pedal needs a restart,
  /// like any other bound resource.
  pub expression_pedals: [Option<(PedalTarget, PedalVolumeCurve)>; NUM_PEDALS],
  /// Every parameter of the strings engine (`None` on a sawwave rig), read by the audio
  /// callback each block; the renderer adopts a change at once or for notes born
  /// afterward, as `StringsRenderer::render_block` says. Which engine renders is fixed
  /// at start: a reload that changes the sink's kind needs a restart.
  pub strings: Option<StringsParams>,
}

/// The live subset of resolved settings.
pub(super) fn live_params(s: &Settings) -> LiveParams {
  let mut expression_pedals = [None; NUM_PEDALS];
  for &(pedal, target, curve) in &s.expression_pedals {
    if let Some(slot) = expression_pedals.get_mut(pedal) {
      *slot = Some((target, curve));
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
    strings: s.strings,
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
      "reloaded {name}: amplitude / distortion / timbres / tuning / pluck / retrigger detune / slide / trail / pedal curves / string parameters / drum volume applied (layout + ports + the sink's kind need a restart)",
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
  use crate::rig::parse_rig;
  use crate::strings::scripted_voices::struck_voice_state;
  use crate::strings::StringsRenderer;
  use crate::types::{VoiceId, VoiceMap, VoiceSource};

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

  /// One play grid on a `cpal_strings` sink; `sink_tables` is TOML appended right
  /// after the sink's own keys (its `[sinks.<table>]` sub-tables).
  fn strings_rig(
    sink_tables: &str,
  ) -> Rig {
    parse_rig(&format!(
      r#"version = 1
id = "strings-reload"
title = "a strings sink under a play grid"
[[tunings]]
id = "main"
edo = 41
x_step = 8
y_step = 1
fundamental_hz = 110
[[monomes]]
id = "a"
listen_port = 9000
prefix = "/a"
select.size = [16, 16]
[[monome_windows]]
id = "grid-a"
monome = "a"
kind = "edo_note_grid"
rect = [0, 0, 15, 15]
tuning = "main"
sink = "strings"
[[sinks]]
id = "strings"
kind = "cpal_strings"
sample_rate = 48000
buffer_frames = 128
amplitude = 0.2
{sink_tables}"#))
      .expect("the strings rig parses") }

  /// A strings sink resolves to its engine parameters, with the sawwave-only fields
  /// inert, and every omitted table is the engine's default.
  #[test]
  fn a_strings_sink_resolves_to_its_params_with_the_sawwave_fields_inert() {
    let bare: Rig = strings_rig("");
    let bare_settings: Settings = resolve_settings(&bare).expect("the bare rig resolves");
    assert_eq!(bare_settings.strings, Some(StringsParams::default()));
    assert_eq!(bare_settings.oversample, 1);
    assert_eq!(bare_settings.sustain_level, 1.0, "the pluck envelope is off");
    assert_eq!(bare_settings.decay_secs, 0.0, "the pluck envelope is off");
    assert!(!bare_settings.distortion_auto_makeup);
    assert_eq!(bare_settings.crowded_pitch_planck_deviation, 0.0);
    assert_eq!(bare_settings.retrigger_tail_detune_cents, 0.0);
    let tuned: Rig = strings_rig("[sinks.body]\nmix = 0.0\n");
    let tuned_settings: Settings = resolve_settings(&tuned).expect("the tuned rig resolves");
    assert_eq!(tuned_settings.strings, tuned.sinks[0].strings_params());
    assert_ne!(tuned_settings.strings, bare_settings.strings); }

  /// A sawwave rig carries no strings parameters, so its audio stays the sawwave engine.
  #[test]
  fn a_sawwave_rig_resolves_without_strings_params() {
    let rig: Rig = load_named_rig("2-edogrids_ss-accrete_ss-pulse").expect("pulse rig loads");
    assert!(live_for(&rig).params.lock().unwrap().strings.is_none()); }

  /// The point of reload for the strings engine: edit `t60_seconds_at_55hz` in the rig,
  /// press 'r', and the running renderer's next block uses it.
  #[test]
  fn a_reload_moves_the_strings_renderers_decay() {
    let mut rig: Rig = strings_rig("");
    let live: Live = live_for(&rig);
    let sample_rate: f32 =
      resolve_settings(&rig).expect("the rig resolves").sample_rate as f32;
    let started: StringsParams =
      live.params.lock().unwrap().strings.expect("a strings rig has strings params");
    let mut renderer: StringsRenderer = StringsRenderer::new(started, sample_rate);
    let mut voices: VoiceMap = VoiceMap::new();
    voices.insert(
      VoiceSource::SurfaceFinger { grid: 0, cell: (0, 0) },
      struck_voice_state(0, 220.0, sample_rate));
    let mut block: [f32; 128] = [0.0; 128];
    render_live_block(&mut renderer, &mut voices, &mut block, sample_rate, &live);
    let edited_t60_seconds_at_55hz: f32 = started.string.t60_seconds_at_55hz / 2.0;
    { // The edit Jeff makes in the rig file.
      let SinkRig::CpalStrings { string, .. } = &mut rig.sinks[0] else {
        panic!("expected a cpal_strings sink, got {:?}", rig.sinks[0]); };
      string.t60_seconds_at_55hz = edited_t60_seconds_at_55hz; }
    adopt_rig(&rig, &live).expect("the edited rig adopts");
    render_live_block(&mut renderer, &mut voices, &mut block, sample_rate, &live);
    assert_eq!(renderer.params().string.t60_seconds_at_55hz, edited_t60_seconds_at_55hz);
    assert_ne!(renderer.params().string.t60_seconds_at_55hz, started.string.t60_seconds_at_55hz); }

  /// A changed `ex_post_energizing_ms` reaches the renderer at its next block and
  /// applies to notes born afterward: a note sounding at the reload keeps the window it
  /// was born with.
  #[test]
  fn a_reload_moves_the_energy_window_of_notes_born_afterward() {
    let mut rig: Rig = strings_rig("");
    let live: Live = live_for(&rig);
    let sample_rate: f32 =
      resolve_settings(&rig).expect("the rig resolves").sample_rate as f32;
    let started: StringsParams =
      live.params.lock().unwrap().strings.expect("a strings rig has strings params");
    let mut renderer: StringsRenderer = StringsRenderer::new(started, sample_rate);
    let mut voices: VoiceMap = VoiceMap::new();
    voices.insert(
      VoiceSource::SurfaceFinger { grid: 0, cell: (0, 0) },
      struck_voice_state(0, 220.0, sample_rate));
    let mut block: [f32; 128] = [0.0; 128];
    render_live_block(&mut renderer, &mut voices, &mut block, sample_rate, &live);
    let edited_ex_post_energizing_ms: f32 = started.energy.ex_post_energizing_ms * 2.0 + 1.0;
    { // The edit Jeff makes in the rig file.
      let SinkRig::CpalStrings { energy, .. } = &mut rig.sinks[0] else {
        panic!("expected a cpal_strings sink, got {:?}", rig.sinks[0]); };
      energy.ex_post_energizing_ms = edited_ex_post_energizing_ms; }
    adopt_rig(&rig, &live).expect("the edited rig adopts");
    voices.insert(
      VoiceSource::SurfaceFinger { grid: 0, cell: (1, 0) },
      struck_voice_state(1, 330.0, sample_rate));
    render_live_block(&mut renderer, &mut voices, &mut block, sample_rate, &live);
    assert_eq!(renderer.params().energy.ex_post_energizing_ms, edited_ex_post_energizing_ms);
    let window_ms = |id: VoiceId| -> f32 {
      renderer.string_voice(id).expect("the voice sounds").energy_params().ex_post_energizing_ms };
    assert_eq!(window_ms(0), started.energy.ex_post_energizing_ms, "the sounding note's window moved");
    assert_eq!(window_ms(1), edited_ex_post_energizing_ms, "the new note took the old window"); }

  /// A `string_energy` pedal over a strings sink resolves to the rig-wide energy
  /// target, on the pedal index its channel names.
  #[test]
  fn a_string_energy_pedal_resolves_to_the_rig_wide_target() {
    let rig: Rig = strings_rig("[[expression_pedals]]\nchannel = 2\nrole = \"string_energy\"\n");
    let settings: Settings = resolve_settings(&rig).expect("the rig resolves");
    let channel: usize = rig.expression_pedals[0].channel as usize;
    let resolved: Vec<(usize, PedalTarget)> =
      settings.expression_pedals.iter().map(|(pedal, target, _)| (*pedal, *target)).collect();
    assert_eq!(resolved, vec![(channel - 1, PedalTarget::StringEnergy)]); }

  /// One block as the audio callback renders it: the live amplitude and strings
  /// parameters read fresh.
  fn render_live_block(
    renderer: &mut StringsRenderer,
    voices: &mut VoiceMap,
    block: &mut [f32],
    sample_rate: f32,
    live: &Live,
  ) {
    let params: LiveParams = *live.params.lock().unwrap();
    let strings: StringsParams = params.strings.expect("the live params hold strings params");
    renderer.render_block(voices, block, 1, sample_rate, params.amplitude, &strings, f32::NAN); }
}
