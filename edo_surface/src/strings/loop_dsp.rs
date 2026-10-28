//! One delay-line loop of a string: the extended Karplus-Strong
//! string. Per sample the wave leaves the delay line and passes, in
//! order, the tuning allpass, the dispersion chain, the loss filter
//! and the DC blocker; what comes out is the wave arriving at the
//! bridge (`Loop::read_arriving`), and whatever the caller writes
//! back (`Loop::write`) re-enters the delay line. With the caller
//! writing back exactly what arrived, the loop is a plain
//! Karplus-Strong string; a junction coupling several loops writes
//! back the arriving wave minus the bridge's velocity instead.
//!
//! The round trip is `P = sample_rate / f0` samples: `N` whole
//! samples of delay, plus the tuning allpass's fractional delay,
//! plus the phase delays at `f0` of the dispersion chain, the loss
//! filter and the DC blocker (the last is negative: a highpass leads).
//! The tuning allpass takes whatever `P` leaves after `N` and the
//! three filters, so the fundamental sounds exactly at `f0`.
//!
//! Every filter is first order, `H(z) = (b0 + b1 z^-1) / (1 + a1
//! z^-1)`, and every magnitude and phase delay the design needs is
//! computed from that transfer function at `w0 = 2 pi f0 /
//! sample_rate`.

use crate::strings::params::{
  amplitude_after_t60_decay, dispersion_applies, inharmonicity_b, round_trip_gain, t60_seconds,
  StringParams, MAX_DISPERSION_SECTIONS,
};
use crate::strings::first_order::{
  allpass_with_phase_delay, first_order_magnitude, first_order_phase_delay_samples,
  one_pole_highpass, FirstOrderCoefficients, FirstOrderState,
};
use std::f64::consts::PI;

/// The DC blocker's cutoff is `f0` over this. Its loss and phase lead
/// at `f0` are then small and the same in every register, and the
/// loss filter and the tuning both absorb them exactly. What they
/// cannot absorb is that the lead shrinks for the upper partials, so
/// they sit flat of exact harmonics by about `1200 log2(1 + 1 / (2 pi
/// x))` cents with `x` this, 1.4 cents at 200; a larger value flattens
/// them less but lets an in-loop DC offset linger longer (its decay
/// rate is about `pi f0 / x` nepers per second). A blocker with a
/// fixed pole, such as the classic 0.995, cuts off near 38 Hz at 48
/// kHz; inside a 55 Hz loop it would remove 18 percent of the
/// amplitude per round trip, far more than any musical T60 allows.
pub const DC_BLOCKER_CUTOFF_BELOW_F0: f32 = 200.0;

/// The darkest loss filter: the pole the loss design falls back to
/// when a one-pole lowpass cannot reach the requested high-frequency
/// decay (it approaches -1 as the request grows).
pub const DARKEST_LOSS_POLE: f32 = -0.99;

/// The loss filter's gain at DC never reaches 1, so the loop never
/// gains energy at any frequency.
pub const MAXIMUM_LOSS_DC_GAIN: f32 = 0.999_999;

/// The fewest whole samples of delay a loop has. A note whose period
/// is shorter than this plus the tuning allpass's minimum (half a
/// sample) plus its filters' delays cannot be tuned: its delay
/// clamps here and it sounds flat. (At 48 kHz that is above about
/// 20 kHz, so only by an absurd pitch or a tiny sample rate.)
pub const MINIMUM_DELAY_SAMPLES: usize = 1;

/// The tuning allpass's fractional delay lives in [0.5, 1.5): at the
/// low end it borrows a whole sample of the delay line, which keeps
/// its pole away from -1 (where it would ring).
pub const MINIMUM_TUNING_DELAY_SAMPLES: f64 = 0.5;

pub struct Loop {
  sample_rate: f32,
  f0: f32,
  design: LoopDesign,
  /// A ring. The most recent `design.delay_samples` writes, ending
  /// just before `write_index`, are the wave in flight; older slots
  /// are spent.
  delay_line: Vec<f32>,
  write_index: usize,
  tuning_allpass_state: FirstOrderState,
  /// The first `design.dispersion_section_count` are in use.
  dispersion_states: [FirstOrderState; MAX_DISPERSION_SECTIONS as usize],
  loss_state: FirstOrderState,
  dc_blocker_state: FirstOrderState, }

/// The coefficients and delay length of one loop at one pitch.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LoopDesign {
  /// `N`, whole samples of delay.
  pub delay_samples: usize,
  /// The tuning allpass's fractional delay at `f0`, in [0.5, 1.5)
  /// except when the delay clamps at `MINIMUM_DELAY_SAMPLES`.
  pub tuning_delay_samples: f32,
  pub tuning_allpass: FirstOrderCoefficients,
  /// 0 when the chain is skipped.
  pub dispersion_section_count: usize,
  /// One section's coefficients; every section is the same.
  pub dispersion_allpass: FirstOrderCoefficients,
  pub loss: FirstOrderCoefficients,
  pub dc_blocker: FirstOrderCoefficients,
  /// The loop's total gain per round trip at `f0` (loss filter times
  /// DC blocker), which is `params::round_trip_gain` unless the loss
  /// filter's DC gain had to clamp at `MAXIMUM_LOSS_DC_GAIN`.
  pub round_trip_gain: f32, }

impl Loop {
  /// A silent loop tuned to `f0` Hz. Its delay line has room for
  /// twice its delay, so retuning it down by up to an octave
  /// allocates nothing.
  pub fn new(
    params: &StringParams,
    f0: f32,
    sample_rate: f32,
  ) -> Loop {
    let design: LoopDesign = design_loop(params, f0, sample_rate);
    let capacity: usize = 2 * design.delay_samples + 4;
    Loop {
      sample_rate,
      f0,
      design,
      delay_line: vec![0.0; capacity],
      write_index: 0,
      tuning_allpass_state: FirstOrderState::default(),
      dispersion_states: [FirstOrderState::default(); MAX_DISPERSION_SECTIONS as usize],
      loss_state: FirstOrderState::default(),
      dc_blocker_state: FirstOrderState::default(), }}

  /// Moves the loop to `f0` with `params` (which may also have
  /// changed): recomputes every coefficient and the delay length,
  /// and keeps the wave in flight and the filter states. A longer
  /// delay reaches further back into the ring; a delay longer than
  /// the ring grows it (allocating) with the wave kept in order.
  pub fn retune(
    &mut self,
    params: &StringParams,
    f0: f32,
  ) {
    let design: LoopDesign = design_loop(params, f0, self.sample_rate);
    if design.delay_samples > self.delay_line.len() {
      self.grow_delay_line(2 * design.delay_samples + 4); }
    { // Sections that join the chain start from rest.
      let first_new_section: usize = self.design.dispersion_section_count;
      for state in self.dispersion_states
        .iter_mut()
        .take(design.dispersion_section_count)
        .skip(first_new_section)
      { *state = FirstOrderState::default(); }}
    self.f0 = f0;
    self.design = design; }

  /// The wave arriving at the bridge this sample: the delay line's
  /// oldest sample in flight, through the tuning allpass, the
  /// dispersion chain, the loss filter and the DC blocker. Call it
  /// once per sample, before `write`.
  pub fn read_arriving(
    &mut self,
  ) -> f32 {
    let delayed: f32 = self.delay_line[self.read_index()];
    let tuned: f32 = self.tuning_allpass_state.tick(&self.design.tuning_allpass, delayed);
    let dispersed: f32 = {
      let mut wave: f32 = tuned;
      let section_count: usize = self.design.dispersion_section_count;
      for state in self.dispersion_states.iter_mut().take(section_count) {
        wave = state.tick(&self.design.dispersion_allpass, wave); }
      wave };
    let damped: f32 = self.loss_state.tick(&self.design.loss, dispersed);
    self.dc_blocker_state.tick(&self.design.dc_blocker, damped) }

  /// Pushes `v` into the delay line and advances one sample.
  pub fn write(
    &mut self,
    v: f32,
  ) {
    self.delay_line[self.write_index] = v;
    self.write_index = (self.write_index + 1) % self.delay_line.len(); }

  /// Adds `table` into the wave in flight, its first sample into the
  /// next one `read_arriving` takes. A table longer than the delay
  /// wraps around and adds onto itself, which is what streaming it
  /// in would give a lossless loop.
  pub fn add_table(
    &mut self,
    table: &[f32],
  ) {
    let delay_samples: usize = self.design.delay_samples;
    for (offset, value) in table.iter().enumerate() {
      let index: usize = self.in_flight_index(offset % delay_samples);
      self.delay_line[index] += value; }}

  /// The sum of squares of the wave in flight (the delay line's
  /// active `delay_samples`, not its spent slots).
  pub fn energy(
    &self,
  ) -> f32 {
    (0 .. self.design.delay_samples)
      .map(|offset| {
        let value: f32 = self.delay_line[self.in_flight_index(offset)];
        value * value })
      .sum() }

  pub fn f0(
    &self,
  ) -> f32 {
    self.f0 }

  /// The loop's gain per round trip at `f0`.
  pub fn round_trip_gain(
    &self,
  ) -> f32 {
    self.design.round_trip_gain }

  pub fn design(
    &self,
  ) -> &LoopDesign {
    &self.design }

  fn read_index(
    &self,
  ) -> usize {
    self.in_flight_index(0) }

  /// The ring index of the sample in flight `offset` samples after
  /// the oldest (offset 0 is the next one read).
  fn in_flight_index(
    &self,
    offset: usize,
  ) -> usize {
    let length: usize = self.delay_line.len();
    (self.write_index + length - self.design.delay_samples + offset) % length }

  /// Replaces the ring by a longer one holding the same samples in
  /// the same order, the newest just before the write index.
  fn grow_delay_line(
    &mut self,
    capacity: usize,
  ) {
    let old_length: usize = self.delay_line.len();
    let mut grown: Vec<f32> = vec![0.0; capacity];
    for age_rank in 0 .. old_length {
      // age_rank 0 is the oldest sample, the one at the write index.
      grown[capacity - old_length + age_rank] =
        self.delay_line[(self.write_index + age_rank) % old_length]; }
    self.delay_line = grown;
    self.write_index = 0; }}

/// The coefficients and delay length that put a loop's fundamental
/// at `f0` with the decay of `params`. See the module comment.
pub fn design_loop(
  params: &StringParams,
  f0: f32,
  sample_rate: f32,
) -> LoopDesign {
  let w0: f64 = 2.0 * PI * f0 as f64 / sample_rate as f64;
  let dc_blocker: FirstOrderCoefficients = design_dc_blocker(f0, sample_rate);
  let loss: FirstOrderCoefficients = design_loss(params, f0, sample_rate, &dc_blocker);
  let delay_without_dispersion: f64 = {
    let period_samples: f64 = sample_rate as f64 / f0 as f64;
    period_samples
      - first_order_phase_delay_samples(&loss, w0)
      - first_order_phase_delay_samples(&dc_blocker, w0) };
  let (dispersion_section_count, dispersion_allpass): (usize, FirstOrderCoefficients) = {
    // The chain is dropped when its delay would not leave the tuning
    // allpass its minimum: a pitch in tune matters more than a stretch.
    let (section_count, allpass): (usize, FirstOrderCoefficients) =
      design_dispersion(params, f0);
    let chain_delay: f64 = section_count as f64 * first_order_phase_delay_samples(&allpass, w0);
    let leaves_the_tuning_its_minimum: bool = delay_without_dispersion - chain_delay
      >= MINIMUM_DELAY_SAMPLES as f64 + MINIMUM_TUNING_DELAY_SAMPLES;
    if leaves_the_tuning_its_minimum { (section_count, allpass) } else { (0, allpass) }};
  let (delay_samples, tuning_delay_samples): (usize, f64) = {
    let delay_for_line_and_tuning: f64 = delay_without_dispersion
      - dispersion_section_count as f64
        * first_order_phase_delay_samples(&dispersion_allpass, w0);
    split_delay_into_whole_and_fraction(delay_for_line_and_tuning) };
  LoopDesign {
    delay_samples,
    tuning_delay_samples: tuning_delay_samples as f32,
    tuning_allpass: allpass_with_phase_delay(tuning_delay_samples, w0),
    dispersion_section_count,
    dispersion_allpass,
    loss,
    dc_blocker,
    round_trip_gain: (first_order_magnitude(&loss, w0) * first_order_magnitude(&dc_blocker, w0))
      as f32, }}

/// `N` and the tuning allpass's delay, from what the period leaves
/// after the filters: `N = floor(delay - 0.5)`, so the fraction is in
/// [0.5, 1.5); clamped at `MINIMUM_DELAY_SAMPLES`.
fn split_delay_into_whole_and_fraction(
  delay_for_line_and_tuning: f64,
) -> (usize, f64) {
  let whole: f64 = (delay_for_line_and_tuning - MINIMUM_TUNING_DELAY_SAMPLES).floor();
  if whole < MINIMUM_DELAY_SAMPLES as f64 {
    (MINIMUM_DELAY_SAMPLES, MINIMUM_TUNING_DELAY_SAMPLES)
  } else {
    (whole as usize, delay_for_line_and_tuning - whole) }}

/// The normalized DC blocker (`first_order::one_pole_highpass`) with
/// its cutoff `f0 / DC_BLOCKER_CUTOFF_BELOW_F0`. Its gain is below 1
/// everywhere below Nyquist (the unnormalized `x - x1 + R y1` gains
/// `2 / (1 + R)` at the top, which a loop with little loss there
/// would amplify forever).
fn design_dc_blocker(
  f0: f32,
  sample_rate: f32,
) -> FirstOrderCoefficients {
  one_pole_highpass(f0 / DC_BLOCKER_CUTOFF_BELOW_F0, sample_rate) }

/// The one-pole loss `H(z) = g (1 + a) / (1 + a z^-1)`, `a` in
/// [`DARKEST_LOSS_POLE`, 0]. It is designed so that the loop
/// (this filter times the DC blocker) has gain `round_trip_gain(f0)`
/// at `f0` and `10 ^ (-3 / (f0 T60(f0) ratio))` at the reference
/// `high_frequency_hz`, where `ratio` is `high_frequency_t60_ratio`.
///
/// With `G0` and `Ghf` the gains this filter must have at `w0` and
/// `w_hf` (the loop's gains over the DC blocker's), the ratio of
/// `|H|^2 = g^2 (1 + a)^2 / (1 + 2 a cos w + a^2)` at the two gives
/// `(1 - R) a^2 + 2 (cos w0 - R cos w_hf) a + (1 - R) = 0` with
/// `R = (Ghf / G0)^2`. Its two roots multiply to 1, so the one in
/// (-1, 0) is `-2 (1 - R) / (b + sqrt(b^2 - 4 (1 - R)^2))` with `b`
/// the linear coefficient. Then `g` follows from `G0`.
///
/// The filter is flat (`a = 0`) when the reference is at or above
/// Nyquist, at or below `f0`, or asks for no extra loss. When the
/// request is beyond what one pole can reach (the discriminant is
/// negative), `a` is `DARKEST_LOSS_POLE`. Either way `a` is then
/// kept no darker than the pole at which `g` would reach
/// `MAXIMUM_LOSS_DC_GAIN`: a dark filter needs a DC gain above its
/// gain at `f0`, and for a note just below the reference (where
/// the request asks a steep drop between two near frequencies) that
/// DC gain would pass 1. There the loop keeps its decay at `f0` and
/// gives up the high-frequency loss it cannot have passively.
fn design_loss(
  params: &StringParams,
  f0: f32,
  sample_rate: f32,
  dc_blocker: &FirstOrderCoefficients,
) -> FirstOrderCoefficients {
  let w0: f64 = 2.0 * PI * f0 as f64 / sample_rate as f64;
  let w_high: f64 = 2.0 * PI * params.high_frequency_hz as f64 / sample_rate as f64;
  let gain_at_f0: f64 =
    round_trip_gain(params, f0) as f64 / first_order_magnitude(dc_blocker, w0);
  let pole: f64 = if w_high >= PI || w_high <= w0 {
    0.0
  } else {
    let loop_gain_at_high: f64 = amplitude_after_t60_decay(
      1.0 / f0 as f64,
      t60_seconds(params, f0) as f64 * params.high_frequency_t60_ratio as f64);
    let gain_at_high: f64 = loop_gain_at_high / first_order_magnitude(dc_blocker, w_high);
    let requested_pole: f64 = one_pole_loss_pole(gain_at_high / gain_at_f0, w0, w_high);
    // The darkest pole whose gain at w0, relative to its DC gain, is
    // gain_at_f0 / MAXIMUM_LOSS_DC_GAIN: its own gain at w0 over at
    // DC is the same equation, with DC in place of w0 and w0 in place
    // of w_high.
    let darkest_passive_pole: f64 =
      one_pole_loss_pole(gain_at_f0 / MAXIMUM_LOSS_DC_GAIN as f64, 0.0, w0);
    requested_pole.max(darkest_passive_pole) };
  let dc_gain: f64 = {
    // |H(w0)| = g (1 + a) / |1 + a e^-jw0| must be gain_at_f0.
    let shape_at_f0: f64 = (1.0 + pole) / (1.0 + 2.0 * pole * w0.cos() + pole * pole).sqrt();
    (gain_at_f0 / shape_at_f0).min(MAXIMUM_LOSS_DC_GAIN as f64) };
  FirstOrderCoefficients { b0: (dc_gain * (1.0 + pole)) as f32, b1: 0.0, a1: pole as f32 }}

/// The pole `a` of `(1 + a) / (1 + a z^-1)` whose gain at `w_high`
/// over its gain at `w0` is `amplitude_ratio`. See `design_loss`.
fn one_pole_loss_pole(
  amplitude_ratio: f64,
  w0: f64,
  w_high: f64,
) -> f64 {
  let squared_ratio: f64 = amplitude_ratio * amplitude_ratio;
  let constant_coefficient: f64 = 1.0 - squared_ratio;
  let linear_coefficient: f64 = 2.0 * (w0.cos() - squared_ratio * w_high.cos());
  let discriminant: f64 =
    linear_coefficient * linear_coefficient - 4.0 * constant_coefficient * constant_coefficient;
  if squared_ratio >= 1.0 || linear_coefficient <= 0.0 {
    0.0
  } else if discriminant < 0.0 {
    DARKEST_LOSS_POLE as f64
  } else {
    let pole: f64 =
      -2.0 * constant_coefficient / (linear_coefficient + discriminant.sqrt());
    pole.clamp(DARKEST_LOSS_POLE as f64, 0.0) }}

/// The dispersion chain at `f0`: its section count (0 when
/// `dispersion_applies` is false) and the coefficients of its
/// identical first-order allpass sections `(a + z^-1) / (1 + a
/// z^-1)`. The coefficient is Rauhala and Välimäki's closed form
/// ("Dispersion modeling in waveguide piano synthesis using tunable
/// allpass filters", DAFx 2006) as Faust's `piano_dispersion_filter`
/// writes it: with `M` sections, `Bc = max(B, 1e-6)` and the
/// continuous piano key number `Ikey = 12 log2(f0 / 27.5) + 1`,
/// `kd = exp(k1 ln(Bc)^2 + k2 ln(Bc) + k3)`,
/// `Cd = exp((m1 ln M + m2) ln(Bc) + m3 ln M + m4)`,
/// `D = exp(Cd - Ikey kd)` and `a = (1 - D) / (1 + D)`, where
/// `k1 = -0.00179`, `k2 = -0.0233`, `k3 = -2.93`, `m1 = 0.0126`,
/// `m2 = 0.0606`, `m3 = -0.00825` and `m4 = 1.97` are their fit to
/// the piano's range.
fn design_dispersion(
  params: &StringParams,
  f0: f32,
) -> (usize, FirstOrderCoefficients) {
  if !dispersion_applies(params, f0) {
    // The count of 0 skips the chain; this section never runs.
    let unused_section: FirstOrderCoefficients =
      FirstOrderCoefficients { b0: 0.0, b1: 1.0, a1: 0.0 };
    return (0, unused_section); }
  let section_count: f64 = params.dispersion_sections as f64;
  let log_b: f64 = (inharmonicity_b(params, f0) as f64).max(1e-6).ln();
  let key_number: f64 = 12.0 * (f0 as f64 / 27.5).log2() + 1.0;
  let kd: f64 = (-0.00179 * log_b * log_b - 0.0233 * log_b - 2.93).exp();
  let cd: f64 =
    ((0.0126 * section_count.ln() + 0.0606) * log_b - 0.00825 * section_count.ln() + 1.97).exp();
  let d: f64 = (cd - key_number * kd).exp();
  let a: f64 = (1.0 - d) / (1.0 + d);
  ( params.dispersion_sections as usize,
    FirstOrderCoefficients { b0: a as f32, b1: 1.0, a1: a as f32 } ) }
