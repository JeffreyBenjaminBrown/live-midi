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
//! `A0 / unison_count`: the waveshape of the voice's energy kind
//! (`precharge_waveshape`). `A0` is the same sum for every kind.
//!
//! The drive, ebow kind. During the ex-post leg the voice computes
//! one drive sample `k * A * tanh(v / A)` and adds it to every one of
//! its loops, with `v` the mean of the voice's loops' arriving waves,
//! `A = energy.drive_limit * pedal(t) ^ e`, `g` the mean of the loops'
//! round-trip gains at their `f0`s, and `k = max(0, exp(rate / f0) -
//! g)`: the gain that, added to `g`, makes the amplitude of the
//! loops' common (in-phase) motion grow by `rate` nepers per second.
//! One drive over the whole course, as one magnet under all of a
//! note's unison strings, keeps the unisons in phase while they grow;
//! a drive per loop, each from its own wave, would let their slight
//! detune pull them apart, and the heard sum would fall while the
//! loops kept their energy. (The drive adds to the wave after the
//! loss, so the gain per round trip is `g (1 + k)` rather than `g +
//! k`; the two differ by `(1 - g) k`, a fraction of a percent of the
//! growth for any musical decay.) The limit `A` rises with the pedal,
//! so a light pedal blooms to a low ceiling and a full one to
//! `energy.drive_limit`. The pedal moves `rate` from `-lambda`, the
//! string's own decay rate (`lambda = -f0 ln g`, at which `k` is 0
//! and the string decays as if undriven), to
//! `energy.growth_rate_per_second_max`: `rate =
//! growth_rate_per_second_max * f - lambda * (1 - f)` with `f` the
//! energy factor. So a pedal at the heel adds nothing, a light
//! pedal lengthens the sustain, and a full one at the window's peak
//! grows every register at the same rate. The tanh bounds what the
//! drive adds to `k A`, so no pedal makes a string run away.
//!
//! The drive, bow kind. A bow moving at `v_bow = energy.bow_velocity
//! * f`, `f` the energy factor, rubs the string, whose speed under it
//! is `v`, the mean of the voice's loops' arriving waves. With `delta
//! = v_bow - v`, the voice's one drive sample is `delta * bow`, where
//! `bow = clamp((|delta / f * energy.bow_slipperiness| + 0.75) ^ -4,
//! 0.0025, 0.245)` is STK's friction table ('BowTable') between the
//! bounds of Elements' ('resonator.cc'). Where the table sits at its
//! upper bound (`|delta / f * bow_slipperiness|` below about 0.67)
//! the bow grips: the drive pulls the string toward the bow's speed,
//! which damps it. Beyond, the table falls as the inverse fourth power and the
//! bow slips; there the drive shrinks as `delta` grows, a negative
//! resistance that feeds the string's motion until the grip limits
//! it. The bow's speed sets the level the string settles at, so
//! `energy.drive_limit` does not apply to the bow. The drive sample
//! passes a one-pole highpass at `f0 / 10` (`DriveState`), which keeps
//! the friction from driving the loop's near-DC motion.
//!
//! The table reads `delta / f`, not `delta`: the bow's force falls
//! with its speed, as a player's does to stay inside the Schelleng
//! window, so the bow's motion keeps its shape and only its size
//! follows `f`. A bow slowed with its force held (a table reading
//! `delta`) grips: it would not speak under a light pedal, and it
//! would stop every bowed string as the window closed. So whether
//! the bow speaks depends on `bow_velocity * bow_slipperiness` and
//! not on the pedal: below about 0.7 it grips throughout and damps the
//! string (a non-start); from about 0.8 to 2 it speaks, at about the
//! same level in every register; above, the low strings lock onto
//! an upper partial (a squeak), and from about 2.5 they slip
//! throughout and the bow adds little (it skates).
//!
//! The drive, wind kind. White noise in [-1, 1) from the voice's own
//! seeded generator (`WindSource`, its seed from the voice's id, so
//! no two notes blow the same noise) passes the second-order bandpass
//! of `body::Resonator` (peak gain about 1) at the Strouhal frequency
//! `f_s = energy.wind_speed_hz_max * pedal(t) ^ e`, clamped to [20
//! Hz, 0.45 sample_rate], of bandwidth `f_s / energy.wind_q`, and is
//! scaled by `tri(t) * energy.wind_gain`; the voice adds that one
//! sample to every loop. The string's partial nearest `f_s` gathers
//! the noise and blooms, the more the longer it rings; as the pedal
//! rises `f_s` climbs the partials, as wind over an aeolian harp's
//! string jumps from harmonic to harmonic. The bandpass is
//! redesigned, its state kept, when `f_s` has moved by more than 1
//! percent. The noise does not depend on the string's motion, so it
//! sounds a silent string too, and its level is `wind_gain`'s, not
//! `energy.drive_limit`'s.

use crate::strings::body::Resonator;
use crate::strings::first_order::{one_pole_highpass, FirstOrderCoefficients, FirstOrderState};
use crate::strings::noise::DeterministicNoise;
use crate::strings::params::{EnergyKind, EnergyParams};
use crate::types::VoiceId;
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

/// The bow's friction table is `(|delta / f * bow_slipperiness| +
/// BOW_TABLE_OFFSET) ^ -4`, clamped to [`BOW_TABLE_MINIMUM`,
/// `BOW_TABLE_MAXIMUM`] (STK's 'BowTable' with Elements' bounds).
pub const BOW_TABLE_OFFSET: f32 = 0.75;

pub const BOW_TABLE_MINIMUM: f32 = 0.0025;

pub const BOW_TABLE_MAXIMUM: f32 = 0.245;

/// The bow's drive passes a one-pole highpass at `f0` over this
/// before it enters the loops: the friction's negative resistance
/// acts at every frequency, and below about the loop's DC blocker
/// (`loop_dsp::DC_BLOCKER_CUTOFF_BELOW_F0`) the loop is too weakly
/// damped to hold it, so an unfiltered bow sets the string's
/// near-DC motion wandering by several hundredths at a few Hz.
pub const BOW_DRIVE_HIGHPASS_BELOW_F0: f32 = 10.0;

/// The wind's Strouhal frequency is clamped to at least this, Hz.
pub const LOWEST_STROUHAL_HZ: f32 = 20.0;

/// The wind's Strouhal frequency is clamped to at most this fraction
/// of the sample rate, where a bandpass is still a resonance.
pub const HIGHEST_STROUHAL_FRACTION_OF_SAMPLE_RATE: f32 = 0.45;

/// The wind's bandpass is redesigned when the Strouhal frequency
/// has moved by more than this fraction (1 percent, 17 cents) from
/// the one it was designed at; finer moves are inaudible in a band
/// `1 / wind_q` wide.
pub const WIND_REDESIGN_RELATIVE_THRESHOLD: f32 = 0.01;

/// Mixed with the voice's id into the seed of the voice's wind
/// noise.
pub const WIND_NOISE_SEED_BASE: u32 = 0x57D1_0001;

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

/// A voice's own drive state, which `drive` reads and advances: the
/// wind kind's noise and bandpass, and the bow kind's highpass. Every
/// voice holds one; each kind uses its own part.
pub struct DriveState {
  wind: WindSource,
  /// At `f0 / BOW_DRIVE_HIGHPASS_BELOW_F0`, `f0` the voice's pitch at
  /// birth.
  bow_highpass: FirstOrderCoefficients,
  bow_highpass_state: FirstOrderState, }

/// A voice's wind: its own white noise and the bandpass at the
/// Strouhal frequency it passes through.
pub struct WindSource {
  noise: DeterministicNoise,
  bandpass: Resonator,
  /// Hz: the Strouhal frequency the bandpass was last designed at.
  designed_center_hz: f32,
  sample_rate: f32, }

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

/// The pedal's power `pedal ^ e` at `pedal_position` (NaN when it
/// never reported, which counts as 0) under `energy`: the factor
/// that scales the drive's limit.
pub fn pedal_power(
  energy: &EnergyParams,
  pedal_position: f32,
) -> f32 {
  let pedal: f32 = reported_position(pedal_position);
  if pedal <= 0.0 { 0.0 } else { pedal.powf(energy.pedal_energy_exponent) }}

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
/// waveshape of `energy.kind` for a loop at `f0` whose loss filter
/// is `loss`, scaled so its peak is `peak`. The first sample is the
/// one the loop reads next. `pedal_power_at_birth` is `pedal ^ e` at
/// the voice's birth, which picks the wind's partial.
///
/// The ebow's is a sine at `f0` plus `EBOW_SECOND_HARMONIC` of a sine
/// at `2 f0`: nearly a sine, as a string under a magnet settles. The
/// bow's is a sawtooth at `f0` passed once through `loss` from rest:
/// the bowed string's locked spectrum, its upper partials as dulled
/// as one round trip dulls them. The wind's is a sine at the partial
/// `k f0` nearest the Strouhal frequency at birth
/// (`wind_precharge_partial_number`), the one the wind blooms.
pub fn precharge_waveshape(
  energy: &EnergyParams,
  pedal_power_at_birth: f32,
  loss: &FirstOrderCoefficients,
  f0: f32,
  sample_rate: f32,
  length: usize,
  peak: f32,
) -> Vec<f32> {
  let unscaled: Vec<f32> = match energy.kind {
    EnergyKind::Ebow => ebow_waveshape(f0, sample_rate, length),
    EnergyKind::Bow => bow_waveshape(loss, f0, sample_rate, length),
    EnergyKind::Wind => {
      let partial_number: u32 =
        wind_precharge_partial_number(energy, pedal_power_at_birth, f0, sample_rate);
      sine_waveshape(partial_number as f32 * f0, sample_rate, length) }};
  let unscaled_peak: f32 = unscaled.iter().fold(0.0, |largest, value| largest.max(value.abs()));
  if unscaled_peak <= 0.0 {
    return unscaled; }
  unscaled.iter().map(|value| value * peak / unscaled_peak).collect() }

/// What the drive of `energy.kind` adds into every loop of a voice
/// this sample, given the voice's `state`, the energy factor `tri(t) *
/// pedal(t) ^ e`, the pedal's power `pedal(t) ^ e`, the voice's `f0`,
/// the mean of its loops' round-trip gains, and the mean of their
/// arriving waves. 0 when the factor or the power is 0, with the
/// state left as it was, so a voice outside its window, or under a
/// pedal at the heel, renders exactly as an undriven one. See the
/// module comment for each kind.
pub fn drive(
  energy: &EnergyParams,
  state: &mut DriveState,
  energy_factor: f32,
  pedal_power: f32,
  f0: f32,
  round_trip_gain: f32,
  arriving: f32,
) -> f32 {
  if energy_factor <= 0.0 || pedal_power <= 0.0 {
    return 0.0; }
  match energy.kind {
    EnergyKind::Ebow =>
      ebow_drive(energy, energy_factor, pedal_power, f0, round_trip_gain, arriving),
    EnergyKind::Bow => {
      let friction: f32 = bow_drive(energy, energy_factor, arriving);
      state.bow_highpass_state.tick(&state.bow_highpass, friction) }
    EnergyKind::Wind => wind_drive(energy, &mut state.wind, energy_factor, pedal_power), }}

/// `k * A * tanh(v / A)` with `A = energy.drive_limit * pedal_power`.
/// See the module comment. `pedal_power` must be above 0.
pub fn ebow_drive(
  energy: &EnergyParams,
  energy_factor: f32,
  pedal_power: f32,
  f0: f32,
  round_trip_gain: f32,
  arriving: f32,
) -> f32 {
  let limit: f32 = energy.drive_limit * pedal_power;
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

/// The bow's `delta * bow_friction(delta / f)`, `delta` the bow's
/// speed `energy.bow_velocity * f` less the string's `arriving` wave
/// and `f` the energy factor. See the module comment. `energy_factor`
/// must be above 0.
pub fn bow_drive(
  energy: &EnergyParams,
  energy_factor: f32,
  arriving: f32,
) -> f32 {
  let bow_speed: f32 = energy.bow_velocity * energy_factor;
  let speed_difference: f32 = bow_speed - arriving;
  speed_difference
    * bow_friction(energy.bow_slipperiness, speed_difference / energy_factor) }

/// The friction table `clamp((|scaled_speed_difference *
/// bow_slipperiness| + BOW_TABLE_OFFSET) ^ -4, BOW_TABLE_MINIMUM,
/// BOW_TABLE_MAXIMUM)`.
/// `scaled_speed_difference` is the bow's speed less the string's,
/// over the energy factor: the difference as it would be at the
/// window's peak under a full pedal.
pub fn bow_friction(
  bow_slipperiness: f32,
  scaled_speed_difference: f32,
) -> f32 {
  ((scaled_speed_difference * bow_slipperiness).abs() + BOW_TABLE_OFFSET)
    .powi(-4)
    .clamp(BOW_TABLE_MINIMUM, BOW_TABLE_MAXIMUM) }

/// `tri(t) * energy.wind_gain` times this sample of the voice's
/// wind at the Strouhal frequency of `pedal_power`. The triangle is
/// the energy factor over the pedal's power. `pedal_power` must be
/// above 0.
pub fn wind_drive(
  energy: &EnergyParams,
  wind: &mut WindSource,
  energy_factor: f32,
  pedal_power: f32,
) -> f32 {
  let triangle: f32 = energy_factor / pedal_power;
  triangle * energy.wind_gain * wind.next_sample(energy, pedal_power) }

impl DriveState {
  /// The drive state of the voice `voice_id` born at `f0` under
  /// `energy`, at rest.
  pub fn new(
    energy: &EnergyParams,
    voice_id: VoiceId,
    f0: f32,
    sample_rate: f32,
  ) -> DriveState {
    DriveState {
      wind: WindSource::new(energy, voice_id, sample_rate),
      bow_highpass: one_pole_highpass(f0 / BOW_DRIVE_HIGHPASS_BELOW_F0, sample_rate),
      bow_highpass_state: FirstOrderState::default(), }}}

impl WindSource {
  /// The wind of the voice `voice_id` under `energy`, its noise
  /// seeded from the id (so two voices never blow the same noise) and
  /// its bandpass at rest, designed at the full pedal's Strouhal
  /// frequency.
  pub fn new(
    energy: &EnergyParams,
    voice_id: VoiceId,
    sample_rate: f32,
  ) -> WindSource {
    let center_hz: f32 = strouhal_frequency_hz(energy, 1.0, sample_rate);
    WindSource {
      noise: DeterministicNoise::new(wind_noise_seed(voice_id)),
      bandpass: Resonator::new(center_hz, center_hz / energy.wind_q, sample_rate),
      designed_center_hz: center_hz,
      sample_rate, }}

  /// One sample of white noise in [-1, 1) through the bandpass at the
  /// Strouhal frequency of `pedal_power` (bandwidth that over
  /// `energy.wind_q`, peak gain about 1), redesigned first if that
  /// frequency has moved by more than `WIND_REDESIGN_RELATIVE_THRESHOLD`.
  pub fn next_sample(
    &mut self,
    energy: &EnergyParams,
    pedal_power: f32,
  ) -> f32 {
    let center_hz: f32 = strouhal_frequency_hz(energy, pedal_power, self.sample_rate);
    if (center_hz / self.designed_center_hz - 1.0).abs() > WIND_REDESIGN_RELATIVE_THRESHOLD {
      self.bandpass.redesign(center_hz, center_hz / energy.wind_q, self.sample_rate);
      self.designed_center_hz = center_hz; }
    self.bandpass.tick(self.noise.next_bipolar()) }

  /// Hz: the Strouhal frequency the bandpass is designed at.
  pub fn designed_center_hz(
    &self,
  ) -> f32 {
    self.designed_center_hz }}

/// The `k >= 1` whose partial `k f0` is nearest the Strouhal
/// frequency of `pedal_power_at_birth`, kept below Nyquist.
pub fn wind_precharge_partial_number(
  energy: &EnergyParams,
  pedal_power_at_birth: f32,
  f0: f32,
  sample_rate: f32,
) -> u32 {
  let strouhal_hz: f32 = strouhal_frequency_hz(energy, pedal_power_at_birth, sample_rate);
  let nearest: u32 = (strouhal_hz / f0).round().max(1.0) as u32;
  let below_nyquist: u32 = ((0.5 * sample_rate / f0).ceil() as u32).saturating_sub(1).max(1);
  nearest.min(below_nyquist) }

/// The Strouhal frequency `f_s = energy.wind_speed_hz_max *
/// pedal_power`, clamped to [`LOWEST_STROUHAL_HZ`,
/// `HIGHEST_STROUHAL_FRACTION_OF_SAMPLE_RATE * sample_rate`].
pub fn strouhal_frequency_hz(
  energy: &EnergyParams,
  pedal_power: f32,
  sample_rate: f32,
) -> f32 {
  (energy.wind_speed_hz_max * pedal_power)
    .clamp(LOWEST_STROUHAL_HZ, HIGHEST_STROUHAL_FRACTION_OF_SAMPLE_RATE * sample_rate) }

/// The seed of the wind noise of the voice `voice_id`: its id mixed
/// (Knuth's multiplicative hash) into `WIND_NOISE_SEED_BASE`, so
/// neighboring ids start far apart in the generator's sequence.
pub fn wind_noise_seed(
  voice_id: VoiceId,
) -> u32 {
  let folded_id: u32 = (voice_id ^ (voice_id >> 32)) as u32;
  WIND_NOISE_SEED_BASE ^ folded_id.wrapping_mul(0x9E37_79B9) }

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

/// The rising sawtooth `2 frac(f0 t) - 1` at the first `length`
/// samples, through `loss` once, from rest.
fn bow_waveshape(
  loss: &FirstOrderCoefficients,
  f0: f32,
  sample_rate: f32,
  length: usize,
) -> Vec<f32> {
  let mut loss_state: FirstOrderState = FirstOrderState::default();
  (0 .. length)
    .map(|index| {
      let cycles: f64 = f0 as f64 * index as f64 / sample_rate as f64;
      let sawtooth: f32 = (2.0 * cycles.fract() - 1.0) as f32;
      loss_state.tick(loss, sawtooth) })
    .collect() }

/// `sin(2 pi frequency t)` at the first `length` samples.
fn sine_waveshape(
  frequency_hz: f32,
  sample_rate: f32,
  length: usize,
) -> Vec<f32> {
  (0 .. length)
    .map(|index| {
      (2.0 * PI * frequency_hz as f64 * index as f64 / sample_rate as f64).sin() as f32 })
    .collect() }
