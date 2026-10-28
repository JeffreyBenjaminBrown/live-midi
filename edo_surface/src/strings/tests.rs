//! Tests of one loop, the exciter, the curves and validation. Each
//! renders a loop as a plain Karplus-Strong string (it writes back
//! what arrives), which is the loop alone, before any junction.

use crate::strings::body::{BodyMode, BodyModes};
use crate::strings::exciter::{excitation_table, excitation_table_length, period_samples};
use crate::strings::first_order::{
  allpass_with_phase_delay, first_order_magnitude, first_order_phase_delay_samples,
};
use crate::strings::loop_dsp::{design_loop, Loop, LoopDesign};
use crate::strings::params::{
  dispersion_applies, inharmonicity_b, inharmonicity_perceptual_threshold, round_trip_gain,
  t60_seconds, BodyParams, ExciterKind, StringParams, StringsParams,
};
use std::f64::consts::PI;

const SAMPLE_RATE: f32 = 48_000.0;

/// A plucked loop at each test pitch, run for a second with the
/// default string parameters (so dispersion applies at the lower
/// three and not at 3000 Hz), has its fundamental within 2 cents of
/// `f0`. The last pitch sits just below the loss filter's reference,
/// where the requested high-frequency loss is steepest.
#[test]
fn a_plucked_loop_sounds_within_two_cents_of_its_pitch() {
  let params: StringsParams = plucked_defaults();
  let just_below_the_loss_reference: f32 = 0.99 * params.string.high_frequency_hz;
  for f0 in [55.0_f32, 440.0, 1760.0, 3000.0, just_below_the_loss_reference] {
    let mut string_loop: Loop = Loop::new(&params.string, f0, SAMPLE_RATE);
    string_loop.add_table(&excitation_table(&params.exciter, &params.body, f0, SAMPLE_RATE));
    let rendered: Vec<f32> = render_plain_karplus_strong(&mut string_loop, seconds(1.2));
    let measured_hz: f64 = partial_frequency_near(&rendered[seconds(0.2) ..], f0 as f64);
    let error_cents: f64 = cents_between(measured_hz, f0 as f64);
    println!("pitch at {f0} Hz: measured {measured_hz:.4} Hz, error {error_cents:+.4} cents, \
              dispersion sections {}", string_loop.design().dispersion_section_count);
    assert!(error_cents.abs() < 2.0, "{f0} Hz sounds at {measured_hz} Hz ({error_cents} cents)"); }}

/// A loop filled with one period of its fundamental decays 60 dB in
/// `T60(f0)` within 10 percent, measured as the slope of its RMS
/// envelope between -10 and -40 dB.
#[test]
fn a_loop_decays_with_the_t60_curve() {
  let params: StringsParams = StringsParams::default();
  for f0 in [110.0_f32, 880.0] {
    let expected_t60: f64 = t60_seconds(&params.string, f0) as f64;
    let mut string_loop: Loop = Loop::new(&params.string, f0, SAMPLE_RATE);
    string_loop.add_table(&one_period_of_sine(f0));
    let rendered: Vec<f32> =
      render_plain_karplus_strong(&mut string_loop, seconds(expected_t60 as f32 * 0.8 + 0.5));
    let measured_t60: f64 = t60_from_rms_envelope(&rendered);
    let error_fraction: f64 = measured_t60 / expected_t60 - 1.0;
    println!("T60 at {f0} Hz: expected {expected_t60:.4} s, measured {measured_t60:.4} s, \
              error {:+.3} percent", 100.0 * error_fraction);
    assert!(error_fraction.abs() < 0.1,
            "{f0} Hz decays in {measured_t60} s, expected {expected_t60} s"); }}

/// From 20 Hz to 20 kHz the loop's gain per round trip at `f0` is
/// the T60 curve's (to the f32 rounding of its coefficients) up to
/// 8 kHz, and nowhere does the loop gain at any frequency reach 1.
#[test]
fn the_loop_design_meets_the_t60_curve_and_never_gains() {
  let string: StringParams = StringsParams::default().string;
  let mut f0: f32 = 20.0;
  while f0 < 20_000.0 {
    let design: LoopDesign = design_loop(&string, f0, SAMPLE_RATE);
    if f0 < 8000.0 {
      let expected: f32 = round_trip_gain(&string, f0);
      assert!((design.round_trip_gain / expected - 1.0).abs() < 1e-5,
              "{f0} Hz: round trip gain {} against {expected}", design.round_trip_gain); }
    for step in 0 ..= 512 {
      let w: f64 = PI * step as f64 / 512.0;
      let loop_gain: f64 =
        first_order_magnitude(&design.loss, w) * first_order_magnitude(&design.dc_blocker, w);
      assert!(loop_gain < 1.0, "{f0} Hz: the loop gains {loop_gain} at w = {w}"); }
    f0 *= 1.06; }}

/// Below the perceptual threshold of `B` the loop carries no
/// dispersion chain and its 4th partial sits at 4 times its
/// fundamental; well above it the chain is there and stretches the
/// 4th partial sharp by at least half of what `f_n = n f0 sqrt(1 + B
/// n^2)` predicts.
#[test]
fn dispersion_is_skipped_below_the_threshold_and_stretches_above() {
  let f0: f32 = 220.0;
  let threshold: f32 = inharmonicity_perceptual_threshold(f0);
  let below: StringParams = string_with_constant_b(threshold / 100.0);
  let above: StringParams = string_with_constant_b(threshold * 100.0);
  assert!(!dispersion_applies(&below, f0));
  assert!(dispersion_applies(&above, f0));
  assert_eq!(Loop::new(&below, f0, SAMPLE_RATE).design().dispersion_section_count, 0);
  assert_eq!(Loop::new(&above, f0, SAMPLE_RATE).design().dispersion_section_count,
             above.dispersion_sections as usize);
  let stretch_below: f64 = fourth_partial_stretch_cents(&below, f0);
  let stretch_above: f64 = fourth_partial_stretch_cents(&above, f0);
  let predicted_above: f64 = {
    let b: f64 = inharmonicity_b(&above, f0) as f64;
    600.0 * ((1.0 + 16.0 * b) / (1.0 + b)).log2() };
  println!("4th partial at {f0} Hz: B {:.3e} (below) stretched {stretch_below:+.3} cents; \
            B {:.3e} (above) stretched {stretch_above:+.3} cents, predicted {predicted_above:+.3}",
           inharmonicity_b(&below, f0), inharmonicity_b(&above, f0));
  assert!(stretch_below.abs() < 2.0, "no chain, yet the 4th partial is {stretch_below} cents off");
  assert!(stretch_above - stretch_below > 0.5 * predicted_above,
          "the chain stretches the 4th partial {stretch_above} cents, predicted {predicted_above}");
  { // Zero sections skip the chain whatever B is.
    let no_sections: StringParams = StringParams { dispersion_sections: 0, ..above };
    assert!(!dispersion_applies(&no_sections, f0));
    assert_eq!(Loop::new(&no_sections, f0, SAMPLE_RATE).design().dispersion_section_count, 0); }}

/// Both exciter kinds give the same table on every call, peak at
/// `pick_energy`, and one period plus the knock long.
#[test]
fn both_exciter_kinds_are_deterministic_and_normalized() {
  let defaults: StringsParams = StringsParams::default();
  for kind in [ExciterKind::Pluck, ExciterKind::Strike] {
    for f0 in [55.0_f32, 440.0, 3000.0] {
      let mut params: StringsParams = defaults;
      params.exciter.kind = kind;
      let first: Vec<f32> = excitation_table(&params.exciter, &params.body, f0, SAMPLE_RATE);
      let second: Vec<f32> = excitation_table(&params.exciter, &params.body, f0, SAMPLE_RATE);
      assert_eq!(first, second, "{kind:?} at {f0} Hz differs between calls");
      let peak: f32 = first.iter().fold(0.0, |peak, sample| peak.max(sample.abs()));
      assert!((peak - params.exciter.pick_energy).abs() < 1e-6,
              "{kind:?} at {f0} Hz peaks at {peak}");
      let knock_samples: usize =
        (params.exciter.knock_ms / 1000.0 * SAMPLE_RATE).round() as usize;
      assert_eq!(first.len(), period_samples(f0, SAMPLE_RATE) + knock_samples - 1);
      assert_eq!(first.len(), excitation_table_length(&params.exciter, f0, SAMPLE_RATE)); }}}

/// The body's centers rise from about `lowest_hz` to about
/// `highest_hz` in order, each bandwidth is its center over `q`, the
/// gains fall as `1 / sqrt(center / lowest_hz)`, the same seed gives
/// the same bank and another seed another, and the impulse response
/// is finite and rings.
#[test]
fn the_body_design_is_log_spaced_seeded_and_rings() {
  let body: BodyParams = StringsParams::default().body;
  let modes: BodyModes = BodyModes::design(&body, SAMPLE_RATE);
  assert_eq!(modes.modes.len(), body.mode_count as usize);
  assert_eq!(modes, BodyModes::design(&body, SAMPLE_RATE));
  assert_ne!(modes, BodyModes::design(&BodyParams { seed: body.seed + 1, ..body }, SAMPLE_RATE));
  let spacing_octaves: f32 =
    (body.highest_hz / body.lowest_hz).log2() / (body.mode_count - 1) as f32;
  let jitter_ratio: f32 = (spacing_octaves * 0.5).exp2();
  let first: &BodyMode = modes.modes.first().expect("a mode");
  let last: &BodyMode = modes.modes.last().expect("a mode");
  assert!(first.center_hz > body.lowest_hz / jitter_ratio
            && first.center_hz < body.lowest_hz * jitter_ratio);
  assert!(last.center_hz > body.highest_hz / jitter_ratio
            && last.center_hz < body.highest_hz * jitter_ratio);
  for pair in modes.modes.windows(2) {
    assert!(pair[1].center_hz > pair[0].center_hz, "centers out of order: {pair:?}"); }
  for mode in &modes.modes {
    assert!((mode.bandwidth_hz - mode.center_hz / body.q).abs() < 1e-3);
    assert!((mode.gain - (body.lowest_hz / mode.center_hz).sqrt()).abs() < 1e-5); }
  let response: Vec<f32> = modes.impulse_response(SAMPLE_RATE, seconds(0.1));
  assert!(response.iter().all(|sample| sample.is_finite()));
  let late_energy: f32 = response[seconds(0.05) ..].iter().map(|sample| sample * sample).sum();
  assert!(late_energy > 0.0); }

/// The default parameters pass validation, and a bad value fails it
/// with an error that names its key.
#[test]
fn validation_accepts_the_defaults_and_names_a_bad_key() {
  assert_eq!(StringsParams::default().validate(), Ok(( )));
  let cases: Vec<(&str, fn(&mut StringsParams))> = vec![
    ("bridge.heaviness", |p| p.bridge.heaviness = 0.0),
    ("energy.pedal_energy_exponent", |p| p.energy.pedal_energy_exponent = 0.0),
    ("string.unison_count", |p| p.string.unison_count = 4),
    ("string.unison_count", |p| p.string.unison_count = 0),
    ("exciter.position", |p| p.exciter.position = 0.6),
    ("exciter.position", |p| p.exciter.position = 0.0),
    ("body.mix", |p| p.body.mix = 1.5),
    ("howl.delay_ms", |p| p.howl.delay_ms = 0.1),
    ("howl.delay_ms", |p| p.howl.delay_ms = 25.0),
    ("string.t60_seconds_at_55hz", |p| p.string.t60_seconds_at_55hz = f32::NAN),
    ("string.dispersion_sections", |p| p.string.dispersion_sections = 17),
    ("body.highest_hz", |p| p.body.highest_hz = p.body.lowest_hz),
    ("exciter.hammer_contact_fraction", |p| p.exciter.hammer_contact_fraction = 0.4),
    ("energy.bow_force", |p| p.energy.bow_force = 0.5), ];
  for (key, spoil) in cases {
    let mut params: StringsParams = StringsParams::default();
    spoil(&mut params);
    let error: String = params.validate().expect_err(key);
    assert!(error.starts_with(key), "the error for {key} reads: {error}"); }}

/// The two curves pass through their anchors and are straight lines
/// on log-log axes, so they keep going (and stay positive) beyond.
#[test]
fn the_curves_pass_through_their_anchors_and_extrapolate_log_log() {
  let string: StringParams = StringsParams::default().string;
  assert!(relative_error(t60_seconds(&string, 55.0), string.t60_seconds_at_55hz) < 1e-5);
  assert!(relative_error(t60_seconds(&string, 1760.0), string.t60_seconds_at_1760hz) < 1e-5);
  assert!(relative_error(inharmonicity_b(&string, 55.0), string.inharmonicity_b_at_55hz) < 1e-5);
  assert!(
    relative_error(inharmonicity_b(&string, 1760.0), string.inharmonicity_b_at_1760hz) < 1e-5);
  { // One octave changes the value by the same factor everywhere.
    let factor_per_octave: f32 = t60_seconds(&string, 110.0) / t60_seconds(&string, 55.0);
    let factor_past_the_top: f32 = t60_seconds(&string, 7040.0) / t60_seconds(&string, 3520.0);
    let factor_past_the_bottom: f32 = t60_seconds(&string, 27.5) / t60_seconds(&string, 13.75);
    assert!(relative_error(factor_past_the_top, factor_per_octave) < 1e-4);
    assert!(relative_error(factor_past_the_bottom, factor_per_octave) < 1e-4);
    assert!(t60_seconds(&string, 20_000.0) > 0.0); }}

/// The tuning allpass delays `f0` by exactly the fraction asked, at
/// a high pitch too, where the plain Thiran coefficient would not.
#[test]
fn the_tuning_allpass_has_the_asked_phase_delay_at_f0() {
  for (f0, delay) in [(55.0_f64, 0.5_f64), (440.0, 1.2), (3000.0, 0.5), (3000.0, 1.49)] {
    let w0: f64 = 2.0 * PI * f0 / SAMPLE_RATE as f64;
    let measured: f64 = first_order_phase_delay_samples(&allpass_with_phase_delay(delay, w0), w0);
    assert!((measured - delay).abs() < 1e-5, "{f0} Hz: asked {delay}, got {measured}"); }}

/// A loop's stored energy falls while it rings alone, and a retune
/// keeps the wave in flight (the energy carries over) and moves the
/// pitch.
#[test]
fn a_retuned_loop_keeps_its_wave_and_moves_its_pitch() {
  let params: StringsParams = plucked_defaults();
  let mut string_loop: Loop = Loop::new(&params.string, 220.0, SAMPLE_RATE);
  string_loop.add_table(&excitation_table(&params.exciter, &params.body, 220.0, SAMPLE_RATE));
  let energy_at_start: f32 = string_loop.energy();
  render_plain_karplus_strong(&mut string_loop, seconds(0.2));
  let energy_before_retune: f32 = string_loop.energy();
  assert!(energy_before_retune < energy_at_start && energy_before_retune > 0.0);
  string_loop.retune(&params.string, 110.0);
  assert_eq!(string_loop.f0(), 110.0);
  assert!(string_loop.energy() > 0.0);
  let rendered: Vec<f32> = render_plain_karplus_strong(&mut string_loop, seconds(1.0));
  let measured_hz: f64 = partial_frequency_near(&rendered[seconds(0.3) ..], 110.0);
  assert!(cents_between(measured_hz, 110.0).abs() < 2.0, "retuned to 110 Hz, sounds {measured_hz}");
  { // Retuning far below the ring's capacity grows it.
    string_loop.retune(&params.string, 27.5);
    let rendered_low: Vec<f32> = render_plain_karplus_strong(&mut string_loop, seconds(1.5));
    let measured_low: f64 = partial_frequency_near(&rendered_low[seconds(0.5) ..], 27.5);
    assert!(cents_between(measured_low, 27.5).abs() < 2.0,
            "retuned to 27.5 Hz, sounds {measured_low}"); }}

fn relative_error(
  measured: f32,
  expected: f32,
) -> f32 {
  (measured / expected - 1.0).abs() }

fn plucked_defaults(
) -> StringsParams {
  let mut params: StringsParams = StringsParams::default();
  params.exciter.kind = ExciterKind::Pluck;
  params }

fn string_with_constant_b(
  b: f32,
) -> StringParams {
  StringParams {
    inharmonicity_b_at_55hz: b,
    inharmonicity_b_at_1760hz: b,
    ..StringsParams::default().string }}

/// How far the 4th partial of a loop excited by a pulse sits above 4
/// times its measured fundamental, in cents.
fn fourth_partial_stretch_cents(
  string: &StringParams,
  f0: f32,
) -> f64 {
  let mut string_loop: Loop = Loop::new(string, f0, SAMPLE_RATE);
  string_loop.add_table(&[1.0]);
  let rendered: Vec<f32> = render_plain_karplus_strong(&mut string_loop, seconds(1.0));
  let fundamental_hz: f64 = partial_frequency_near(&rendered[seconds(0.1) ..], f0 as f64);
  let fourth_hz: f64 = partial_frequency_near(&rendered[seconds(0.1) ..], 4.0 * fundamental_hz);
  cents_between(fourth_hz, 4.0 * fundamental_hz) }

/// Runs the loop as a plain Karplus-Strong string: each sample's
/// arriving wave is both the output and what is written back.
fn render_plain_karplus_strong(
  string_loop: &mut Loop,
  sample_count: usize,
) -> Vec<f32> {
  (0 .. sample_count)
    .map(|_| {
      let arriving: f32 = string_loop.read_arriving();
      string_loop.write(arriving);
      arriving })
    .collect() }

fn one_period_of_sine(
  f0: f32,
) -> Vec<f32> {
  let period: usize = period_samples(f0, SAMPLE_RATE);
  (0 .. period)
    .map(|index| (0.5 * (2.0 * PI * index as f64 / period as f64).sin()) as f32)
    .collect() }

fn seconds(
  duration: f32,
) -> usize {
  (duration * SAMPLE_RATE).round() as usize }

fn cents_between(
  measured_hz: f64,
  reference_hz: f64,
) -> f64 {
  1200.0 * (measured_hz / reference_hz).log2() }

/// The frequency, within 25 cents of `guess_hz`, at which the
/// Hann-windowed spectrum of `signal` peaks: a coarse scan at a
/// quarter of the window's bin, then golden-section refinement. One
/// partial's peak does not move with its decay, and the other
/// partials are far enough away that their leakage does not shift it.
fn partial_frequency_near(
  signal: &[f32],
  guess_hz: f64,
) -> f64 {
  let windowed: Vec<f64> = hann_windowed(signal);
  let duration_seconds: f64 = signal.len() as f64 / SAMPLE_RATE as f64;
  let lowest: f64 = guess_hz * (-25.0_f64 / 1200.0).exp2();
  let highest: f64 = guess_hz * (25.0_f64 / 1200.0).exp2();
  let step: f64 = 0.25 / duration_seconds;
  let coarse_best: f64 = {
    let step_count: usize = ((highest - lowest) / step).ceil() as usize;
    (0 ..= step_count)
      .map(|index| lowest + index as f64 * step)
      .fold((lowest, 0.0_f64), |(best_hz, best_magnitude), hz| {
        let magnitude: f64 = spectrum_magnitude_at(&windowed, hz);
        if magnitude > best_magnitude { (hz, magnitude) } else { (best_hz, best_magnitude) }})
      .0 };
  golden_section_maximum(|hz| spectrum_magnitude_at(&windowed, hz),
                         coarse_best - step, coarse_best + step) }

fn hann_windowed(
  signal: &[f32],
) -> Vec<f64> {
  let length: f64 = signal.len() as f64;
  signal.iter()
    .enumerate()
    .map(|(index, sample)| {
      let window: f64 = 0.5 - 0.5 * (2.0 * PI * index as f64 / length).cos();
      window * *sample as f64 })
    .collect() }

/// `|sum x[n] e^(-j 2 pi f n / fs)|`, the phasor stepped by
/// multiplication.
fn spectrum_magnitude_at(
  windowed: &[f64],
  hz: f64,
) -> f64 {
  let step_angle: f64 = -2.0 * PI * hz / SAMPLE_RATE as f64;
  let (step_real, step_imaginary): (f64, f64) = (step_angle.cos(), step_angle.sin());
  let (mut phasor_real, mut phasor_imaginary): (f64, f64) = (1.0, 0.0);
  let (mut sum_real, mut sum_imaginary): (f64, f64) = (0.0, 0.0);
  for sample in windowed {
    sum_real += sample * phasor_real;
    sum_imaginary += sample * phasor_imaginary;
    let next_real: f64 = phasor_real * step_real - phasor_imaginary * step_imaginary;
    phasor_imaginary = phasor_real * step_imaginary + phasor_imaginary * step_real;
    phasor_real = next_real; }
  sum_real.hypot(sum_imaginary) }

fn golden_section_maximum(
  function: impl Fn(f64) -> f64,
  low: f64,
  high: f64,
) -> f64 {
  let ratio: f64 = (5.0_f64.sqrt() - 1.0) / 2.0;
  let (mut low, mut high): (f64, f64) = (low, high);
  for _ in 0 .. 60 {
    let left: f64 = high - ratio * (high - low);
    let right: f64 = low + ratio * (high - low);
    if function(left) > function(right) { high = right; } else { low = left; }}
  (low + high) / 2.0 }

/// Seconds to fall 60 dB, from a least-squares line through the RMS
/// envelope (10 ms windows, in dB below its loudest window) over the
/// windows from the first below -10 dB to the last above -40 dB.
fn t60_from_rms_envelope(
  signal: &[f32],
) -> f64 {
  let window_samples: usize = seconds(0.01);
  let levels_db: Vec<f64> = signal
    .chunks_exact(window_samples)
    .map(|window| {
      let mean_square: f64 =
        window.iter().map(|sample| (*sample as f64) * (*sample as f64)).sum::<f64>()
          / window_samples as f64;
      10.0 * mean_square.log10() })
    .collect();
  let loudest_db: f64 = levels_db.iter().cloned().fold(f64::MIN, f64::max);
  let first_below_minus_10: usize = levels_db.iter()
    .position(|level| *level < loudest_db - 10.0)
    .expect("the envelope falls 10 dB");
  let last_above_minus_40: usize = levels_db.iter()
    .rposition(|level| *level > loudest_db - 40.0)
    .expect("the envelope stays within 40 dB somewhere");
  assert!(last_above_minus_40 > first_below_minus_10 + 10, "too few windows to fit the decay");
  let points: Vec<(f64, f64)> = (first_below_minus_10 ..= last_above_minus_40)
    .map(|index| (index as f64 * window_samples as f64 / SAMPLE_RATE as f64, levels_db[index]))
    .collect();
  let count: f64 = points.len() as f64;
  let mean_time: f64 = points.iter().map(|(time, _)| time).sum::<f64>() / count;
  let mean_level: f64 = points.iter().map(|(_, level)| level).sum::<f64>() / count;
  let slope_db_per_second: f64 = points.iter()
    .map(|(time, level)| (time - mean_time) * (level - mean_level))
    .sum::<f64>()
    / points.iter().map(|(time, _)| (time - mean_time) * (time - mean_time)).sum::<f64>();
  -60.0 / slope_db_per_second }
