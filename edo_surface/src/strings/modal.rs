//! A modal voice: the strings engine's voice for a note above the
//! crossover (`modal.crossover_hz`), where a delay line is too short
//! for its loss, dispersion and tuning filters. It is a bank of
//! `modal.mode_count` decaying resonators at the stiff string's
//! partials, and a junction member like a loop: per sample the
//! renderer reads its arriving wave (the bank's output), runs the
//! junction, and writes back its input, `-v_b + excitation + drive +
//! howl`, which every mode takes.
//!
//! The modes. Mode k (1, 2, ...) sits at `f_k = k f0 sqrt((1 + B k^2)
//! / (1 + B))`, `B = inharmonicity_b(f0)`: the stiff string's
//! partials `k f0' sqrt(1 + B k^2)` with `f0'` chosen so the first is
//! at `f0`, as a loop tunes its fundamental to `f0` whatever its
//! dispersion. Modes at or above `HIGHEST_MODE_FRACTION_OF_SAMPLE_RATE`
//! of the sample rate are left out. Mode 1 decays with `T60(f0)`,
//! and mode k with `T60(f0) * ratio ^ x_k`, `ratio =
//! string.high_frequency_t60_ratio` and `x_k = log2(f_k / f0) /
//! log2(high_frequency_hz / f0)` clamped to [0, 1]: log-linear in
//! frequency from `T60(f0)` at `f0` to `T60(f0) * ratio` at
//! `high_frequency_hz` and beyond, as the loop's loss filter aims.
//! With the reference at or below `f0` every mode decays with
//! `T60(f0)`, as the loop's loss is then flat. The pole radius is
//! `r_k = exp(-ln(1000) / (T60_k * sample_rate))`.
//!
//! Each mode is the resonator `beta (r cos(theta) z^-1 - r^2 z^-2)
//! / (1 - 2 r cos(theta) z^-1 + r^2 z^-2)`, `theta = 2 pi f_k /
//! sample_rate`, whose impulse response is `beta r^n cos(n theta)`
//! for `n >= 1` (and 0 at `n = 0`). Its output depends only on
//! earlier inputs, so its arriving wave is known before the junction
//! runs, as a loop's is. With `beta = 2 / P`, `P = sample_rate / f0`,
//! the bank is the Fourier series of a loop's periodic response
//! (`2 / P` per cosine harmonic of a period-`P` impulse train)
//! truncated to its modes. So:
//!
//! Its level matches a loop voice's. The dry and knock tables
//! streamed into its input come out as a loop would play them,
//! harmonic for harmonic, at the same `pick_energy`, less the
//! partials above its last mode.
//!
//! The junction, the drive and the howl see it as a loop. At mode
//! k's frequency its response is about `beta / (2 (1 - r_k))`, real
//! and positive, which is the `g / (1 - g)` of a loop of round-trip
//! gain `g = r_k ^ P`: the bridge drains it, the drive
//! (`energy::drive` on its output, with `g` its fundamental's) grows
//! it, and the howl sustains it, as they do a loop.
//!
//! It is passive as a junction member. A member whose response `G`
//! from input to arriving wave has `Re G >= -1/2` at every frequency
//! is a loop whose round-trip filter `G / (1 + G)` has gain at most
//! 1, and the junction's passivity (see 'junction.rs') covers it.
//! Each of a mode's two poles contributes `(beta / 2) Re(p / (1 -
//! p))`, `p = r e^(j (+-theta - w))`, which is at least `-(beta / 2)
//! r / (1 + r) > -beta / 4`; so `Re G > -M beta / 2 = -M f0 /
//! sample_rate` for `M` modes. Every mode is at or above `k f0`, and
//! below 0.45 of the sample rate, so `M f0 < 0.45 sample_rate` and
//! `Re G > -0.45`.

use crate::strings::body::HIGHEST_MODE_FRACTION_OF_SAMPLE_RATE;
use crate::strings::energy::{drive, precharge_waveshape};
use crate::strings::exciter::period_samples;
use crate::strings::params::{
  inharmonicity_b, round_trip_gain, t60_seconds, EnergyParams, ModalParams, StringParams,
  StringsParams, MAX_MODAL_MODES,
};
use crate::strings::voice::{ExcitationStream, RETUNE_RELATIVE_THRESHOLD};
use std::f64::consts::PI;
use std::sync::Arc;

/// One mode's design: where it sits, how long it rings, and its
/// resonator's coefficients.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ModeDesign {
  pub frequency_hz: f32,
  pub t60_seconds: f32,
  /// `r_k`.
  pub pole_radius: f32,
  /// `2 r cos(theta)`.
  two_r_cos: f32,
  /// `r^2`.
  r_squared: f32,
  /// `cos(theta)`.
  cos_angle: f32,
  /// `beta r cos(theta)`.
  output_from_w: f32,
  /// `beta r^2`.
  output_from_w1: f32,
  /// `beta`.
  output_gain: f32, }

/// One mode's all-pole state: `w[n - 1]` and `w[n - 2]` of `w[n] =
/// x[n - 1] + 2 r cos(theta) w[n - 1] - r^2 w[n - 2]`; the output is
/// `beta (r cos(theta) w[n] - r^2 w[n - 1])`.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
struct ModeState {
  w1: f32,
  w2: f32, }

pub struct ModalVoice {
  /// The voice's pitch, Hz: mode 1's frequency.
  f0: f32,
  sample_rate: f32,
  /// Engine time, in samples, of the block in which the voice's id
  /// first appeared.
  birth: u64,
  /// The first `mode_count` are in use.
  designs: [ModeDesign; MAX_MODAL_MODES as usize],
  states: [ModeState; MAX_MODAL_MODES as usize],
  mode_count: usize,
  /// Mode 1's round-trip gain at `f0`, `10 ^ (-3 / (f0 T60(f0)))`,
  /// which the drive takes as a loop's.
  round_trip_gain: f32,
  /// The input written last sample, which every mode reads this one.
  pending_input: f32,
  excitation: ExcitationStream,
  /// This sample's gain coefficient, set by the renderer.
  gain: f32,
  /// The energy parameters at birth: the voice's window and drive.
  energy: EnergyParams,
  /// This sample's energy factor, set by the renderer.
  energy_factor: f32,
  /// This sample's `pedal(t) ^ e`, set by the renderer.
  pedal_power: f32, }

impl ModalVoice {
  /// A voice at `f0` Hz born at engine sample `birth`, its modes at
  /// rest, and the two tables to stream in (empty for a voice that
  /// rings only by sympathy).
  pub fn new(
    params: &StringsParams,
    f0: f32,
    sample_rate: f32,
    birth: u64,
    dry_table: Vec<f32>,
    knock_table: Arc<[f32]>,
  ) -> ModalVoice {
    let (mode_count, designs): (usize, [ModeDesign; MAX_MODAL_MODES as usize]) =
      design_modes(&params.string, &params.modal, f0, sample_rate);
    ModalVoice {
      f0,
      sample_rate,
      birth,
      designs,
      states: [ModeState::default(); MAX_MODAL_MODES as usize],
      mode_count,
      round_trip_gain: round_trip_gain(&params.string, f0),
      pending_input: 0.0,
      excitation: ExcitationStream::new(dry_table, knock_table),
      gain: 0.0,
      energy: params.energy,
      energy_factor: 0.0,
      pedal_power: 0.0, }}

  /// Adds the pre-charge into the input stream: one period of the
  /// waveshape of the voice's energy kind at `f0`, peaking at
  /// `amplitude` (`energy::precharge_waveshape`), which the bank
  /// then plays as a loop holding that period would. Call it at
  /// birth, before the first sample.
  pub fn precharge(
    &mut self,
    amplitude: f32,
    sample_rate: f32,
  ) {
    let waveshape: Vec<f32> = precharge_waveshape(
      self.energy.kind, self.f0, sample_rate, period_samples(self.f0, sample_rate), amplitude);
    self.excitation.add_into_dry_table(&waveshape); }

  /// Re-designs the modes at `f0` if it has moved from the voice's
  /// pitch by more than `RETUNE_RELATIVE_THRESHOLD`.
  pub fn retune_if_moved(
    &mut self,
    string: &StringParams,
    modal: &ModalParams,
    f0: f32,
  ) {
    if (f0 / self.f0 - 1.0).abs() > RETUNE_RELATIVE_THRESHOLD {
      self.retune(string, modal, f0); }}

  /// Re-designs every mode at `f0` from `string` and `modal`, keeping
  /// the states of the modes that stay; modes that join start from
  /// rest.
  pub fn retune(
    &mut self,
    string: &StringParams,
    modal: &ModalParams,
    f0: f32,
  ) {
    let (mode_count, designs): (usize, [ModeDesign; MAX_MODAL_MODES as usize]) =
      design_modes(string, modal, f0, self.sample_rate);
    for state in self.states.iter_mut().take(mode_count).skip(self.mode_count) {
      *state = ModeState::default(); }
    self.designs = designs;
    self.mode_count = mode_count;
    self.round_trip_gain = round_trip_gain(string, f0);
    self.f0 = f0; }

  /// The wave arriving at the bridge this sample: the bank's output,
  /// from the inputs written before this sample. Call it once per
  /// sample, before `write_back`.
  pub fn read_arriving(
    &mut self,
  ) -> f32 {
    let input: f32 = self.pending_input;
    let mut output: f32 = 0.0;
    for (design, state) in self.designs.iter().zip(self.states.iter_mut()).take(self.mode_count) {
      let w: f32 = input + design.two_r_cos * state.w1 - design.r_squared * state.w2;
      output += design.output_from_w * w - design.output_from_w1 * state.w1;
      state.w2 = state.w1;
      state.w1 = w; }
    output }

  /// Writes this sample's input, `-bridge_velocity + excitation +
  /// drive + howl_feedback`, which every mode reads next sample, and
  /// advances the excitation. `arriving` is this sample's output from
  /// `read_arriving`; the drive is `energy::drive` on it.
  pub fn write_back(
    &mut self,
    arriving: f32,
    bridge_velocity: f32,
    howl_feedback: f32,
  ) {
    let excitation: f32 = self.excitation.next_sample();
    let drive: f32 = drive(
      &self.energy, self.energy_factor, self.pedal_power, self.f0, self.round_trip_gain,
      arriving);
    self.pending_input = -bridge_velocity + excitation + drive + howl_feedback; }

  /// About the energy a loop holding the bank's motion would (the sum
  /// of squares over one period): `P / 2` times the sum over modes of
  /// each one's squared output amplitude, about `beta^2 (w1^2 - 2
  /// cos(theta) w1 w2 + w2^2)` (exact for an undamped mode).
  pub fn energy(
    &self,
  ) -> f32 {
    let period_samples: f32 = self.sample_rate / self.f0;
    let squared_amplitudes: f32 = self.designs.iter()
      .zip(self.states.iter())
      .take(self.mode_count)
      .map(|(design, state)| {
        design.output_gain * design.output_gain
          * (state.w1 * state.w1 - 2.0 * design.cos_angle * state.w1 * state.w2
             + state.w2 * state.w2) })
      .sum();
    0.5 * period_samples * squared_amplitudes }

  /// Whether both tables have streamed in entirely.
  pub fn excitation_finished(
    &self,
  ) -> bool {
    self.excitation.finished() }

  /// The modes in use, mode 1 first.
  pub fn modes(
    &self,
  ) -> &[ModeDesign] {
    &self.designs[.. self.mode_count] }

  pub fn set_gain(
    &mut self,
    gain: f32,
  ) {
    self.gain = gain; }

  pub fn gain(
    &self,
  ) -> f32 {
    self.gain }

  pub fn set_energy_factor(
    &mut self,
    energy_factor: f32,
  ) {
    self.energy_factor = energy_factor; }

  pub fn set_pedal_power(
    &mut self,
    pedal_power: f32,
  ) {
    self.pedal_power = pedal_power; }

  /// The energy parameters the voice was born under.
  pub fn energy_params(
    &self,
  ) -> &EnergyParams {
    &self.energy }

  pub fn birth(
    &self,
  ) -> u64 {
    self.birth }

  pub fn f0(
    &self,
  ) -> f32 {
    self.f0 }}

/// The modes of a modal voice at `f0`, as the module comment
/// describes: how many are in use, and their designs (the rest
/// default).
pub fn design_modes(
  string: &StringParams,
  modal: &ModalParams,
  f0: f32,
  sample_rate: f32,
) -> (usize, [ModeDesign; MAX_MODAL_MODES as usize]) {
  let mut designs: [ModeDesign; MAX_MODAL_MODES as usize] =
    [ModeDesign::default(); MAX_MODAL_MODES as usize];
  let b: f64 = inharmonicity_b(string, f0) as f64;
  let highest_hz: f64 = (HIGHEST_MODE_FRACTION_OF_SAMPLE_RATE * sample_rate) as f64;
  let output_gain: f64 = 2.0 * f0 as f64 / sample_rate as f64;
  let mut mode_count: usize = 0;
  for mode_number in 1 ..= modal.mode_count.min(MAX_MODAL_MODES) {
    let k: f64 = mode_number as f64;
    let frequency_hz: f64 = f0 as f64 * k * ((1.0 + b * k * k) / (1.0 + b)).sqrt();
    if frequency_hz >= highest_hz {
      break; }
    let t60: f64 = mode_t60_seconds(string, f0, frequency_hz as f32) as f64;
    let pole_radius: f64 = (-1000.0_f64.ln() / (t60 * sample_rate as f64)).exp();
    let angle: f64 = 2.0 * PI * frequency_hz / sample_rate as f64;
    designs[mode_count] = ModeDesign {
      frequency_hz: frequency_hz as f32,
      t60_seconds: t60 as f32,
      pole_radius: pole_radius as f32,
      two_r_cos: (2.0 * pole_radius * angle.cos()) as f32,
      r_squared: (pole_radius * pole_radius) as f32,
      cos_angle: angle.cos() as f32,
      output_from_w: (output_gain * pole_radius * angle.cos()) as f32,
      output_from_w1: (output_gain * pole_radius * pole_radius) as f32,
      output_gain: output_gain as f32, };
    mode_count += 1; }
  (mode_count, designs) }

/// Seconds for a mode at `frequency_hz` of a note at `f0` to fall 60
/// dB: `T60(f0) * high_frequency_t60_ratio ^ x`, `x` the mode's
/// place on the log-frequency line from `f0` (0) to
/// `high_frequency_hz` (1), clamped to [0, 1]; `T60(f0)` for every
/// mode when the reference is at or below `f0`.
pub fn mode_t60_seconds(
  string: &StringParams,
  f0: f32,
  frequency_hz: f32,
) -> f32 {
  let fundamental_t60: f32 = t60_seconds(string, f0);
  if string.high_frequency_hz <= f0 {
    return fundamental_t60; }
  let place: f32 =
    ((frequency_hz / f0).log2() / (string.high_frequency_hz / f0).log2()).clamp(0.0, 1.0);
  fundamental_t60 * string.high_frequency_t60_ratio.powf(place) }
