//! The body: a bank of parallel second-order resonators. This file
//! designs the bank (`BodyModes::design`), gives each mode its
//! `Resonator`, and gives the bank's impulse response, from which
//! the exciter's knock is cut.
//!
//! The centers are log-spaced from `body.lowest_hz` to
//! `body.highest_hz`, each moved by a deterministic jitter from
//! `body.seed` so the modes do not fall on a regular grid; each
//! resonator's bandwidth is its center over `body.q`, and its gain
//! falls as `1 / sqrt(center / lowest_hz)`.

use crate::strings::noise::DeterministicNoise;
use crate::strings::params::BodyParams;
use std::f64::consts::PI;

/// Modes whose (jittered) center would land at or above this
/// fraction of the sample rate are left out of the bank: a resonator
/// that close to Nyquist is no longer a resonance.
pub const HIGHEST_MODE_FRACTION_OF_SAMPLE_RATE: f32 = 0.45;

/// Each center moves by up to this fraction of the log spacing
/// between neighbors, either way, so neighbors never swap order.
pub const CENTER_JITTER_FRACTION_OF_SPACING: f32 = 0.3;

#[derive(Clone, Debug, PartialEq)]
pub struct BodyModes {
  pub modes: Vec<BodyMode>, }

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BodyMode {
  pub center_hz: f32,
  pub bandwidth_hz: f32,
  pub gain: f32, }

/// The two-pole resonator `b0 (1 - z^-2) / (1 + a1 z^-1 + a2
/// z^-2)` with poles at radius `r = exp(-pi bandwidth / fs)` and
/// angle `2 pi center / fs`, and `b0 = (1 - r^2) / 2`, which puts
/// its peak gain near 1 and its DC and Nyquist gains at 0.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Resonator {
  b0: f32,
  a1: f32,
  a2: f32,
  previous_inputs: [f32; 2],
  previous_outputs: [f32; 2], }

impl BodyModes {
  /// The bank of `params`, as the module comment describes, without
  /// the modes that land too near Nyquist at `sample_rate`. The same
  /// parameters give the same bank every time.
  pub fn design(
    params: &BodyParams,
    sample_rate: f32,
  ) -> BodyModes {
    let mode_count: u32 = params.mode_count;
    let spacing_octaves: f32 = if mode_count > 1 {
      (params.highest_hz / params.lowest_hz).log2() / (mode_count - 1) as f32
    } else { 0.0 };
    let mut jitter: DeterministicNoise = DeterministicNoise::new(params.seed);
    let modes: Vec<BodyMode> = (0 .. mode_count)
      .map(|mode_index| {
        let jitter_octaves: f32 =
          jitter.next_bipolar() * CENTER_JITTER_FRACTION_OF_SPACING * spacing_octaves;
        let center_hz: f32 =
          params.lowest_hz * (mode_index as f32 * spacing_octaves + jitter_octaves).exp2();
        BodyMode {
          center_hz,
          bandwidth_hz: center_hz / params.q,
          gain: 1.0 / (center_hz / params.lowest_hz).sqrt(), }})
      .filter(|mode| mode.center_hz < HIGHEST_MODE_FRACTION_OF_SAMPLE_RATE * sample_rate)
      .collect();
    BodyModes { modes }}

  /// The bank's response to a unit impulse: the gain-weighted sum of
  /// every resonator's, `length_samples` long.
  pub fn impulse_response(
    &self,
    sample_rate: f32,
    length_samples: usize,
  ) -> Vec<f32> {
    let mut response: Vec<f32> = vec![0.0; length_samples];
    for mode in &self.modes {
      let mut resonator: Resonator = mode.resonator(sample_rate);
      for (index, sample) in response.iter_mut().enumerate() {
        let impulse: f32 = if index == 0 { 1.0 } else { 0.0 };
        *sample += mode.gain * resonator.tick(impulse); }}
    response }}

impl BodyMode {
  /// This mode's resonator, at rest. Its output excludes `gain`.
  pub fn resonator(
    &self,
    sample_rate: f32,
  ) -> Resonator {
    let radius: f64 = (-PI * self.bandwidth_hz as f64 / sample_rate as f64).exp();
    let angle: f64 = 2.0 * PI * self.center_hz as f64 / sample_rate as f64;
    Resonator {
      b0: ((1.0 - radius * radius) / 2.0) as f32,
      a1: (-2.0 * radius * angle.cos()) as f32,
      a2: (radius * radius) as f32,
      previous_inputs: [0.0; 2],
      previous_outputs: [0.0; 2], }}}

impl Resonator {
  pub fn tick(
    &mut self,
    input: f32,
  ) -> f32 {
    let output: f32 = self.b0 * (input - self.previous_inputs[1])
      - self.a1 * self.previous_outputs[0]
      - self.a2 * self.previous_outputs[1];
    self.previous_inputs = [input, self.previous_inputs[0]];
    self.previous_outputs = [output, self.previous_outputs[0]];
    output }}
