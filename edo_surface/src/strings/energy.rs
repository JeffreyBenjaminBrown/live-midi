//! The energy pedal's effect on a string: the pedal's history, the
//! energy window, the pre-charge a string holds at key-down, and the
//! drive that feeds it afterward.
//!
//! The window. A string voice born at engine time `T` (the block in
//! which its id first appears) has the asymmetric triangle `tri(t)`:
//! 0 at `T - ex_ante`, rising linearly to 1 at `T`, falling linearly
//! to 0 at `T + ex_post`, and 0 outside, with `ex_ante` and `ex_post`
//! the voice's `energy.ex_ante_energizing_ms` and
//! `energy.ex_post_energizing_ms`. The energy factor at `t` is
//! `tri(t) * pedal(t) ^ e`, with `e = energy.pedal_energy_exponent`
//! and `pedal(t)` the pedal's raw position in [0, 1] (0 where it never
//! reported). Only the window gates the energy: a held, a sustained
//! and a released note are driven alike while it is open, and never
//! after it closes.
//!
//! The history. The pedal thread publishes one position, which the
//! audio callback reads once per block. `PedalHistory` keeps it on a
//! fixed grid of one point per millisecond of engine time (the
//! sample clock), each point holding the position of the block that
//! contains it, for the longest `ex_ante_energizing_ms` validation
//! allows (10 s). A fixed grid makes the pre-charge's step and cost
//! the same whatever the callback's length: a block longer than a
//! millisecond fills several points with its one position, and a
//! shorter one fills a point only when it contains one.
//!
//! The pre-charge. At birth the string holds what the pedal would
//! have put into it over the ex-ante leg, each contribution decayed
//! by the string's own loss since: `A0 = energy.precharge_gain * sum_j
//! dt * tri(t_j) * pedal(t_j)^e * exp(-(T - t_j) / tau)` over the
//! history's points `t_j` in `[T - ex_ante, T]`, with `dt` the grid
//! step and `tau = T60(f0) / ln(1000)` the voice's amplitude time
//! constant. It is written into each loop as one delay line's worth
//! of the steady state the drive would have left, scaled to the peak
//! `A0 / unison_count`.
//!
//! The drive, ebow kind. During the ex-post leg each loop adds
//! `k * A * tanh(v / A)` per sample, with `v` the loop's own arriving
//! wave, `A = energy.drive_limit`, `g` the loop's round-trip gain at
//! its `f0`, and `k = max(0, exp(rate / f0) - g)`: the gain that,
//! added to `g`, makes the loop's amplitude grow by `rate` nepers per
//! second. (The drive adds to the wave after the loss, so the loop's
//! gain is `g (1 + k)` rather than `g + k`; the two differ by `(1 -
//! g) k`, a fraction of a percent of the growth for any musical
//! decay.) The pedal moves `rate` from `-lambda`, the string's own
//! decay rate (`lambda = -f0 ln g`, at which `k` is 0 and the string
//! decays as if undriven), to `energy.growth_rate_per_second_max`:
//! `rate = growth_rate_per_second_max * f - lambda * (1 - f)` with `f`
//! the energy factor. So a pedal at the heel adds nothing, a light
//! pedal lengthens the sustain, and a full one at the window's peak
//! grows every register at the same rate. The tanh bounds what the
//! drive adds, so no pedal makes a string run away.

use crate::strings::params::{EnergyKind, EnergyParams};
use std::f64::consts::PI;

/// The history's grid: points per second of engine time.
pub const HISTORY_POINTS_PER_SECOND: f64 = 1000.0;

/// Seconds of history kept: the longest `ex_ante_energizing_ms` that
/// validation allows.
pub const HISTORY_SECONDS: f64 = 10.0;

/// The ebow pre-charge waveshape is a sine at `f0` plus this much of
/// a sine at `2 f0`: nearly a sine, with weak harmonics, as a string
/// under a feedback driver settles.
pub const EBOW_SECOND_HARMONIC: f32 = 0.2;

/// The pedal's position on a fixed grid of engine time, as the module
/// comment describes.
pub struct PedalHistory {
  /// A ring: grid point `j` (engine time `j / HISTORY_POINTS_PER_SECOND`
  /// seconds) lives in slot `j % positions.len()`. Each holds a
  /// position in [0, 1], 0 where the pedal had not reported.
  positions: Vec<f32>,
  /// Engine samples between grid points.
  step_samples: f64,
  /// The points below this one have been recorded.
  recorded_point_count: u64, }

impl PedalHistory {
  /// An empty history (every point unreported) for an engine at
  /// `sample_rate`.
  pub fn new(
    sample_rate: f32,
  ) -> PedalHistory {
    let slot_count: usize = (HISTORY_SECONDS * HISTORY_POINTS_PER_SECOND).ceil() as usize + 2;
    PedalHistory {
      positions: vec![0.0; slot_count],
      step_samples: sample_rate as f64 / HISTORY_POINTS_PER_SECOND,
      recorded_point_count: 0, }}

  /// Records `pedal_position` (NaN when the pedal never reported) at
  /// every grid point not yet recorded that lies before the end of the
  /// block of `block_frames` samples starting at engine sample
  /// `block_start`. Call it once per block, before the block's births.
  pub fn record_block(
    &mut self,
    block_start: u64,
    block_frames: usize,
    pedal_position: f32,
  ) {
    let position: f32 = reported_position(pedal_position);
    let block_end: u64 = block_start + block_frames as u64;
    let points_before_block_end: u64 = (block_end as f64 / self.step_samples).ceil() as u64;
    let slot_count: u64 = self.positions.len() as u64;
    for point in self.recorded_point_count .. points_before_block_end {
      self.positions[(point % slot_count) as usize] = position; }
    self.recorded_point_count = self.recorded_point_count.max(points_before_block_end); }

  /// The position recorded at grid point `point`: 0 if it has not
  /// been recorded or has fallen out of the ring.
  pub fn position_at_point(
    &self,
    point: u64,
  ) -> f32 {
    let slot_count: u64 = self.positions.len() as u64;
    if point >= self.recorded_point_count || point + slot_count < self.recorded_point_count {
      return 0.0; }
    self.positions[(point % slot_count) as usize] }

  /// Engine samples between grid points.
  pub fn step_samples(
    &self,
  ) -> f64 {
    self.step_samples }}

/// The pre-charge amplitude `A0` of a voice born at engine sample
/// `birth` under `energy`, whose amplitude time constant is
/// `amplitude_time_constant_seconds` (`T60(f0) / ln(1000)`), from the
/// history's points in the ex-ante leg. See the module comment. 0
/// when the leg is empty or the pedal never reported in it.
pub fn precharge_amplitude(
  history: &PedalHistory,
  birth: u64,
  energy: &EnergyParams,
  amplitude_time_constant_seconds: f32,
  sample_rate: f32,
) -> f32 {
  let ex_ante_seconds: f64 = energy.ex_ante_energizing_ms as f64 / 1000.0;
  if ex_ante_seconds <= 0.0 || energy.precharge_gain <= 0.0 {
    return 0.0; }
  let step_samples: f64 = history.step_samples();
  let step_seconds: f64 = step_samples / sample_rate as f64;
  let birth_seconds: f64 = birth as f64 / sample_rate as f64;
  let ex_post_seconds: f64 = energy.ex_post_energizing_ms as f64 / 1000.0;
  let tau: f64 = amplitude_time_constant_seconds as f64;
  let first_point: u64 =
    ((birth as f64 - ex_ante_seconds * sample_rate as f64) / step_samples).ceil().max(0.0) as u64;
  let last_point: u64 = (birth as f64 / step_samples).floor() as u64;
  let decay_per_point: f64 = (-step_seconds / tau).exp();
  let mut decay_since_point: f64 = (-(birth_seconds - last_point as f64 * step_seconds) / tau).exp();
  let mut sum: f64 = 0.0;
  for point in (first_point ..= last_point).rev() {
    // Walking back from birth, each point has decayed one step more.
    let pedal: f32 = history.position_at_point(point);
    if pedal > 0.0 {
      let point_seconds: f64 = point as f64 * step_seconds;
      let triangle: f64 = tri(point_seconds, birth_seconds, ex_ante_seconds, ex_post_seconds);
      sum += triangle
        * (pedal as f64).powf(energy.pedal_energy_exponent as f64)
        * decay_since_point; }
    decay_since_point *= decay_per_point; }
  (energy.precharge_gain as f64 * step_seconds * sum) as f32 }

/// The energy factor `tri(t) * pedal ^ e` of a voice born at engine
/// sample `birth` under `energy`, at engine sample `now` (not before
/// `birth`) with the pedal at `pedal_position` (NaN when it never
/// reported, which counts as 0). 0 once the window has closed.
pub fn ex_post_energy_factor(
  energy: &EnergyParams,
  birth: u64,
  now: u64,
  sample_rate: f32,
  pedal_position: f32,
) -> f32 {
  let ex_post_samples: f64 = energy.ex_post_energizing_ms as f64 / 1000.0 * sample_rate as f64;
  let since_birth: f64 = now.saturating_sub(birth) as f64;
  if since_birth > ex_post_samples {
    return 0.0; }
  let pedal: f32 = reported_position(pedal_position);
  if pedal <= 0.0 {
    return 0.0; }
  let triangle: f32 =
    if ex_post_samples > 0.0 { (1.0 - since_birth / ex_post_samples) as f32 } else { 1.0 };
  triangle * pedal.powf(energy.pedal_energy_exponent) }

/// The asymmetric triangle at `time_seconds` for a voice born at
/// `birth_seconds`: 0 at `birth - ex_ante`, 1 at `birth`, 0 at `birth
/// + ex_post`, linear between, 0 outside. An empty leg (0 seconds)
/// leaves only the 1 at birth on its side.
pub fn tri(
  time_seconds: f64,
  birth_seconds: f64,
  ex_ante_seconds: f64,
  ex_post_seconds: f64,
) -> f64 {
  let after_birth: f64 = time_seconds - birth_seconds;
  if after_birth == 0.0 {
    1.0
  } else if after_birth < 0.0 {
    if ex_ante_seconds > 0.0 && -after_birth <= ex_ante_seconds {
      1.0 + after_birth / ex_ante_seconds } else { 0.0 }
  } else if ex_post_seconds > 0.0 && after_birth <= ex_post_seconds {
    1.0 - after_birth / ex_post_seconds
  } else {
    0.0 }}

/// `length` samples (one delay line's worth) of the pre-charge
/// waveshape of `kind` for a loop at `f0`, scaled so its peak is
/// `peak`. The first sample is the one the loop reads next.
///
/// The ebow's is a sine at `f0` plus `EBOW_SECOND_HARMONIC` of a sine
/// at `2 f0`. The bow's (a sawtooth through the loop's own loss) and
/// the wind's (a sine at the partial nearest the Strouhal frequency)
/// arrive with their drives in slice 8 of
/// 'TODO/resonance/02_plan.org'; until then they take the ebow's.
pub fn precharge_waveshape(
  kind: EnergyKind,
  f0: f32,
  sample_rate: f32,
  length: usize,
  peak: f32,
) -> Vec<f32> {
  let unscaled: Vec<f32> = match kind {
    EnergyKind::Ebow | EnergyKind::Bow | EnergyKind::Wind =>
      ebow_waveshape(f0, sample_rate, length), };
  let unscaled_peak: f32 = unscaled.iter().fold(0.0, |largest, value| largest.max(value.abs()));
  if unscaled_peak <= 0.0 {
    return unscaled; }
  unscaled.iter().map(|value| value * peak / unscaled_peak).collect() }

/// What the drive of `energy.kind` adds into a loop this sample,
/// given the energy factor, the loop's `f0` and round-trip gain, and
/// its arriving wave `arriving`. 0 when the factor is 0, so a voice
/// outside its window, or under a pedal at the heel, renders exactly
/// as an undriven one.
///
/// The bow and wind drives arrive in slice 8 of
/// 'TODO/resonance/02_plan.org'; until then those kinds drive as the
/// ebow does.
pub fn drive(
  energy: &EnergyParams,
  energy_factor: f32,
  f0: f32,
  round_trip_gain: f32,
  arriving: f32,
) -> f32 {
  if energy_factor <= 0.0 {
    return 0.0; }
  match energy.kind {
    EnergyKind::Ebow | EnergyKind::Bow | EnergyKind::Wind =>
      ebow_drive(energy, energy_factor, f0, round_trip_gain, arriving), }}

/// `k * A * tanh(v / A)`. See the module comment.
pub fn ebow_drive(
  energy: &EnergyParams,
  energy_factor: f32,
  f0: f32,
  round_trip_gain: f32,
  arriving: f32,
) -> f32 {
  let limit: f32 = energy.drive_limit;
  ebow_drive_gain(energy, energy_factor, f0, round_trip_gain) * limit * (arriving / limit).tanh() }

/// The ebow's `k = max(0, exp(rate / f0) - g)`, with `rate` moved by
/// the energy factor from the string's own decay rate to
/// `growth_rate_per_second_max`. See the module comment.
pub fn ebow_drive_gain(
  energy: &EnergyParams,
  energy_factor: f32,
  f0: f32,
  round_trip_gain: f32,
) -> f32 {
  let factor: f32 = energy_factor.clamp(0.0, 1.0);
  let natural_decay_rate: f32 = -f0 * round_trip_gain.ln();
  let rate: f32 =
    energy.growth_rate_per_second_max * factor - natural_decay_rate * (1.0 - factor);
  ((rate / f0).exp() - round_trip_gain).max(0.0) }

/// A pedal position as the energy reads it: NaN (never reported) is
/// 0, and the rest is clamped to [0, 1].
pub fn reported_position(
  pedal_position: f32,
) -> f32 {
  if pedal_position.is_nan() { 0.0 } else { pedal_position.clamp(0.0, 1.0) }}

/// `sin(2 pi f0 t) + EBOW_SECOND_HARMONIC sin(4 pi f0 t)` at the
/// first `length` samples.
fn ebow_waveshape(
  f0: f32,
  sample_rate: f32,
  length: usize,
) -> Vec<f32> {
  (0 .. length)
    .map(|index| {
      let phase: f64 = 2.0 * PI * f0 as f64 * index as f64 / sample_rate as f64;
      (phase.sin() + EBOW_SECOND_HARMONIC as f64 * (2.0 * phase).sin()) as f32 })
    .collect() }
