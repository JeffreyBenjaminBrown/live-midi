//! `VoiceState`s built and edited as the surfaces runtime's
//! `SurfaceSink` builds and edits them for a strings sink, for
//! scripts that drive the strings engine without grids: the
//! listening harness and the tests. A strike is
//! `SurfaceSink::note_on_with_phase_and_gain` with no pedals, fader
//! or pulse; a release is `SurfaceSink::note_off`.

use crate::types::{Timbre, VoiceId, VoiceState};
use crate::voices::pluck_envelope;

/// Seconds of a strike's attack ramp: a strings sink's `attack`.
pub const STRIKE_ATTACK_SECS: f32 = 0.001;

/// Seconds of a release's ramp to silence (the damper): a strings
/// sink's default `release_secs`.
pub const RELEASE_SECS: f32 = 0.05;

/// A freshly struck voice at `freq` Hz: its envelope at 0 ramping to
/// 1 over `STRIKE_ATTACK_SECS`, the pluck envelope off, every gain
/// 1, no glide, slide or factored pulse, the default timbre.
pub fn struck_voice_state(
  id: VoiceId,
  freq: f32,
  sample_rate: f32,
) -> VoiceState {
  let (sustain_env, decay_per_sample): (f32, f32) = pluck_envelope(1.0, 0.0, 1.0, sample_rate);
  VoiceState {
    id,
    freq,
    freq_target: 0.0,
    glide_per_sample: 1.0,
    factored_pulse_freq: 0.0,
    factored_pulse_phase: 0.0,
    phase: 0.0,
    env: 0.0,
    target_env: 1.0,
    ramp_per_sample: 1.0 / (STRIKE_ATTACK_SECS * sample_rate),
    pending_attack: None,
    sustain_env,
    decay_per_sample,
    timbre: Timbre::default(),
    fader_gain: 1.0,
    grid_gain: 1.0,
    grid_gain_target: 1.0,
    pedal_scale: 1.0,
    new_pedal_gain: 1.0,
    global_pedal_gain: 1.0,
    global_pedal_gain_target: 1.0,
    slide_freq_target: 0.0,
    am_phase: 0.0,
    fm_phase: 0.0,
    rel_am_phase: 0.0,
    rel_fm_phase: 0.0,
    timbre_xfade: None, }}

/// Starts `voice`'s release: its envelope ramps from where it is to
/// 0 over `RELEASE_SECS`, as a lifted finger's does.
pub fn release_voice_state(
  voice: &mut VoiceState,
  sample_rate: f32,
) {
  voice.target_env = 0.0;
  voice.ramp_per_sample = voice.env / (RELEASE_SECS * sample_rate);
  voice.pending_attack = None; }
