//! The body: a bank of parallel second-order resonators. This file
//! designs the bank (`BodyModes::design`), gives each mode its
//! `Resonator`, gives the bank's impulse response, from which the
//! exciter's knock is cut, and runs the bank on the engine's summed
//! output (`BodyBank`).
//!
//! The centers are log-spaced from `body.lowest_hz` to
//! `body.highest_hz`, each moved by a deterministic jitter from
//! `body.seed` so the modes do not fall on a regular grid; each
//! resonator's bandwidth is its center over `body.q`, and its gain
//! falls as `1 / sqrt(center / lowest_hz)`.

use crate::strings::noise::DeterministicNoise;
use crate::strings::params::BodyParams;
use std::f64::consts::PI;

/// The seed of the white noise `BodyBank::new` measures the bank's
/// gain with.
pub const NORMALIZATION_NOISE_SEED: u32 = 0xB0D7_0001;

/// Seconds of noise run through the bank before its gain is
/// measured, so the resonators have reached their steady state (the
/// narrowest default mode's amplitude time constant is about 0.1 s).
pub const NORMALIZATION_SETTLE_SECONDS: f32 = 0.25;

/// Seconds of noise over which the bank's gain is measured.
pub const NORMALIZATION_MEASURE_SECONDS: f32 = 0.75;

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

/// The bank running on the engine's mono sum: `(1 - mix) dry + mix
/// wet / norm`, `wet` the gain-weighted sum of every mode's
/// resonator and `norm` the bank's RMS gain on white noise, so that
/// a white input leaves at the RMS it came in with. Mix 0 is exactly
/// the dry sum.
pub struct BodyBank {
  mix: f32,
  /// Each mode's gain and its resonator.
  resonators: Vec<(f32, Resonator)>,
  /// `rms(wet) / rms(input)` for deterministic white noise.
  norm: f32, }

impl BodyBank {
  /// The bank of `params` at rest, its `norm` measured by running
  /// `NORMALIZATION_SETTLE_SECONDS` and then
  /// `NORMALIZATION_MEASURE_SECONDS` of white noise from
  /// `NORMALIZATION_NOISE_SEED` through it. The same parameters give
  /// the same bank, `norm` included.
  pub fn new(
    params: &BodyParams,
    sample_rate: f32,
  ) -> BodyBank {
    let resonators: Vec<(f32, Resonator)> = BodyModes::design(params, sample_rate)
      .modes
      .iter()
      .map(|mode| (mode.gain, mode.resonator(sample_rate)))
      .collect();
    let mut bank: BodyBank = BodyBank { mix: params.mix, resonators, norm: 1.0 };
    bank.norm = bank.measured_white_noise_gain(sample_rate);
    bank.reset();
    bank }

  /// One sample of the engine's mono sum through the body.
  pub fn process(
    &mut self,
    dry: f32,
  ) -> f32 {
    if self.mix <= 0.0 {
      return dry; }
    let wet: f32 = self.wet(dry);
    (1.0 - self.mix) * dry + self.mix * wet / self.norm }

  /// `rms(wet) / rms(input)` for white noise, measured as the
  /// constants above say. 1 for a bank with no modes or no output.
  fn measured_white_noise_gain(
    &mut self,
    sample_rate: f32,
  ) -> f32 {
    let settle_samples: usize = (NORMALIZATION_SETTLE_SECONDS * sample_rate) as usize;
    let measure_samples: usize = (NORMALIZATION_MEASURE_SECONDS * sample_rate) as usize;
    let mut noise: DeterministicNoise = DeterministicNoise::new(NORMALIZATION_NOISE_SEED);
    for _ in 0 .. settle_samples {
      self.wet(noise.next_bipolar()); }
    let (input_energy, output_energy): (f64, f64) = (0 .. measure_samples)
      .fold((0.0, 0.0), |(input_energy, output_energy), _| {
        let input: f32 = noise.next_bipolar();
        let output: f32 = self.wet(input);
        ( input_energy + (input as f64) * (input as f64),
          output_energy + (output as f64) * (output as f64) ) });
    if input_energy > 0.0 && output_energy > 0.0 {
      (output_energy / input_energy).sqrt() as f32
    } else { 1.0 }}

  pub fn norm(
    &self,
  ) -> f32 {
    self.norm }

  /// The bank's raw output: every mode's resonator, weighted by its
  /// gain, summed.
  fn wet(
    &mut self,
    input: f32,
  ) -> f32 {
    self.resonators
      .iter_mut()
      .map(|(gain, resonator)| *gain * resonator.tick(input))
      .sum() }

  /// Every resonator back at rest.
  fn reset(
    &mut self,
  ) {
    for (_, resonator) in self.resonators.iter_mut() {
      resonator.previous_inputs = [0.0; 2];
      resonator.previous_outputs = [0.0; 2]; }}}

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
