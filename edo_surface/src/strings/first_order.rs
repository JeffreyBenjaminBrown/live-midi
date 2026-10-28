//! First-order IIR filters, `H(z) = (b0 + b1 z^-1) / (1 + a1
//! z^-1)`: their coefficients, their state, and their magnitude and
//! phase delay at a frequency. Every filter inside a loop is one of
//! these, and so is the exciter's lowpass.

use std::f64::consts::PI;

/// The largest magnitude `allpass_with_phase_delay` lets its exact
/// coefficient have (the Thiran coefficients of delays in [0.5, 1.5)
/// lie within [-0.2, 0.34]); beyond it the pole is near enough to the
/// unit circle to ring, or past it.
pub const LARGEST_EXACT_ALLPASS_COEFFICIENT: f64 = 0.5;

/// `H(z) = (b0 + b1 z^-1) / (1 + a1 z^-1)`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FirstOrderCoefficients {
  pub b0: f32,
  pub b1: f32,
  pub a1: f32, }

#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FirstOrderState {
  previous_input: f32,
  previous_output: f32, }

impl FirstOrderState {
  pub fn tick(
    &mut self,
    coefficients: &FirstOrderCoefficients,
    input: f32,
  ) -> f32 {
    let output: f32 = coefficients.b0 * input + coefficients.b1 * self.previous_input
      - coefficients.a1 * self.previous_output;
    self.previous_input = input;
    self.previous_output = output;
    output }}

/// `input` through the filter of `coefficients`, from rest.
pub fn filtered_from_rest(
  coefficients: &FirstOrderCoefficients,
  input: &[f32],
) -> Vec<f32> {
  let mut state: FirstOrderState = FirstOrderState::default();
  input.iter().map(|sample| state.tick(coefficients, *sample)).collect() }

/// The first-order allpass `(a + z^-1) / (1 + a z^-1)` whose phase
/// delay at `w` is exactly `delay_samples`:
/// `a = sin((1 - d) w / 2) / sin((1 + d) w / 2)`. As `w` falls this
/// tends to the Thiran coefficient `(1 - d) / (1 + d)`, which is exact
/// only at DC and would leave a 3 kHz note at 48 kHz measurably flat.
/// Near Nyquist no stable first-order allpass has the asked delay
/// (the exact `a` passes `+-1`); where the exact `a` leaves
/// `[-LARGEST_EXACT_ALLPASS_COEFFICIENT, LARGEST_EXACT_ALLPASS_COEFFICIENT]`
/// the Thiran coefficient, always stable for `d > 0`, is used instead.
pub fn allpass_with_phase_delay(
  delay_samples: f64,
  w: f64,
) -> FirstOrderCoefficients {
  let exact: f64 =
    ((1.0 - delay_samples) * w / 2.0).sin() / ((1.0 + delay_samples) * w / 2.0).sin();
  let a: f64 = if exact.abs() <= LARGEST_EXACT_ALLPASS_COEFFICIENT {
    exact
  } else { (1.0 - delay_samples) / (1.0 + delay_samples) };
  FirstOrderCoefficients { b0: a as f32, b1: 1.0, a1: a as f32 }}

/// `y = (1 - p) x + p y1` with `p = exp(-2 pi cutoff / fs)`: a
/// one-pole lowpass with unit gain at DC.
pub fn one_pole_lowpass(
  cutoff_hz: f32,
  sample_rate: f32,
) -> FirstOrderCoefficients {
  let pole: f64 = (-2.0 * PI * cutoff_hz as f64 / sample_rate as f64).exp();
  FirstOrderCoefficients { b0: (1.0 - pole) as f32, b1: 0.0, a1: -pole as f32 }}

/// `y = k (x - x1) + R y1` with `R = exp(-2 pi cutoff / fs)` and `k =
/// (1 + R) / 2`: a one-pole highpass with gain 0 at DC and exactly 1
/// at Nyquist.
pub fn one_pole_highpass(
  cutoff_hz: f32,
  sample_rate: f32,
) -> FirstOrderCoefficients {
  let pole: f64 = (-2.0 * PI * cutoff_hz as f64 / sample_rate as f64).exp();
  let gain: f64 = (1.0 + pole) / 2.0;
  FirstOrderCoefficients { b0: gain as f32, b1: -gain as f32, a1: -pole as f32 }}

/// `|H(e^jw)|` of a first-order filter.
pub fn first_order_magnitude(
  coefficients: &FirstOrderCoefficients,
  w: f64,
) -> f64 {
  let (numerator, denominator): ((f64, f64), (f64, f64)) =
    first_order_numerator_and_denominator(coefficients, w);
  numerator.0.hypot(numerator.1) / denominator.0.hypot(denominator.1) }

/// The phase delay `-arg(H(e^jw)) / w` of a first-order filter, in
/// samples. Negative for a filter that leads (a highpass at low `w`).
/// The numerator's and denominator's arguments are each taken
/// continuously from `w = 0` (the denominator's real part stays
/// positive for a stable pole), so no unwrapping is needed below
/// Nyquist.
pub fn first_order_phase_delay_samples(
  coefficients: &FirstOrderCoefficients,
  w: f64,
) -> f64 {
  let (numerator, denominator): ((f64, f64), (f64, f64)) =
    first_order_numerator_and_denominator(coefficients, w);
  let phase: f64 = numerator.1.atan2(numerator.0) - denominator.1.atan2(denominator.0);
  -phase / w }

/// `b0 + b1 e^-jw` and `1 + a1 e^-jw`, each as (real, imaginary).
fn first_order_numerator_and_denominator(
  coefficients: &FirstOrderCoefficients,
  w: f64,
) -> ((f64, f64), (f64, f64)) {
  let (b0, b1, a1): (f64, f64, f64) =
    (coefficients.b0 as f64, coefficients.b1 as f64, coefficients.a1 as f64);
  ( (b0 + b1 * w.cos(), -b1 * w.sin()),
    (1.0 + a1 * w.cos(), -a1 * w.sin()) ) }
