//! Tests of the strings engine. The first ones render a loop as a
//! plain Karplus-Strong string (it writes back what arrives), which
//! is the loop alone, before any junction; then come the exciter,
//! the body, the curves and validation; then the junction, the
//! renderer over a voice map, and sympathy between strings; then the
//! energy pedal's pre-charge and drive.

use crate::strings::body::{BodyBank, BodyMode, BodyModes};
use crate::strings::energy::{
  drive, ebow_drive_gain, pedal_power, precharge_amplitude, PedalHistory,
};
use crate::strings::exciter::{dry_table, knock_table, period_samples};
use crate::strings::first_order::{
  allpass_with_phase_delay, first_order_magnitude, first_order_phase_delay_samples,
};
use crate::strings::junction::Junction;
use crate::strings::loop_dsp::{design_loop, Loop, LoopDesign};
use crate::strings::noise::DeterministicNoise;
use crate::strings::params::{
  dispersion_applies, inharmonicity_b, inharmonicity_perceptual_threshold, round_trip_gain,
  t60_seconds, BodyParams, ExciterKind, StringParams, StringsParams,
};
use crate::strings::renderer::{couple_through_junction, StringsRenderer};
use crate::strings::scripted_voices::{release_voice_state, struck_voice_state, RELEASE_SECS};
use crate::strings::voice::StringVoice;
use crate::types::{VoiceId, VoiceMap, VoiceSource, VoiceState};
use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::sync::Arc;

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
    string_loop.add_table(&dry_table(&params.exciter, f0, SAMPLE_RATE));
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

/// Both exciter kinds give the same dry table on every call, one
/// period long and peaking at `pick_energy`; the knock is the same on
/// every call too, and its energy is `(knock_mix * pick_energy)^2`.
#[test]
fn both_exciter_kinds_are_deterministic_and_normalized() {
  let defaults: StringsParams = StringsParams::default();
  for kind in [ExciterKind::Pluck, ExciterKind::Strike] {
    for f0 in [55.0_f32, 440.0, 3000.0] {
      let mut params: StringsParams = defaults;
      params.exciter.kind = kind;
      let first: Vec<f32> = dry_table(&params.exciter, f0, SAMPLE_RATE);
      let second: Vec<f32> = dry_table(&params.exciter, f0, SAMPLE_RATE);
      assert_eq!(first, second, "{kind:?} at {f0} Hz differs between calls");
      let peak: f32 = first.iter().fold(0.0, |peak, sample| peak.max(sample.abs()));
      assert!((peak - params.exciter.pick_energy).abs() < 1e-6,
              "{kind:?} at {f0} Hz peaks at {peak}");
      assert_eq!(first.len(), period_samples(f0, SAMPLE_RATE)); }}
  for knock_mix in [0.0_f32, 0.3, 1.0] {
    let mut params: StringsParams = defaults;
    params.exciter.knock_mix = knock_mix;
    let knock: Vec<f32> = knock_table(&params.exciter, &params.body, SAMPLE_RATE);
    assert_eq!(knock, knock_table(&params.exciter, &params.body, SAMPLE_RATE));
    let energy: f32 = knock.iter().map(|sample| sample * sample).sum();
    let expected: f32 = (knock_mix * params.exciter.pick_energy).powi(2);
    assert!((energy - expected).abs() <= 1e-4 * expected.max(1e-6),
            "knock_mix {knock_mix}: knock energy {energy}, expected {expected}"); }}

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
  string_loop.add_table(&dry_table(&params.exciter, 220.0, SAMPLE_RATE));
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

/// With `mix` 1 the body's output has the RMS of a white-noise input
/// (another seed than the one `norm` was measured with), and with
/// `mix` 0 it is exactly its input.
#[test]
fn the_body_bank_keeps_white_noise_rms_and_mix_zero_is_dry() {
  let body: BodyParams = StringsParams::default().body;
  let mut wet_bank: BodyBank = BodyBank::new(&BodyParams { mix: 1.0, ..body }, SAMPLE_RATE);
  let mut noise: DeterministicNoise = DeterministicNoise::new(12345);
  let settle: usize = seconds(0.3);
  let (input_energy, output_energy): (f64, f64) = (0 .. settle + seconds(2.0))
    .fold((0.0, 0.0), |(input_energy, output_energy), index| {
      let input: f32 = noise.next_bipolar();
      let output: f32 = wet_bank.process(input);
      if index < settle { (input_energy, output_energy) } else {
        ( input_energy + (input as f64).powi(2), output_energy + (output as f64).powi(2) ) }});
  let rms_ratio: f64 = (output_energy / input_energy).sqrt();
  println!("body: norm {:.4}, white-noise RMS out over in {rms_ratio:.4}", wet_bank.norm());
  assert!((rms_ratio - 1.0).abs() < 0.05, "the body changes white noise's RMS by {rms_ratio}");
  let mut dry_bank: BodyBank = BodyBank::new(&BodyParams { mix: 0.0, ..body }, SAMPLE_RATE);
  for index in 0 .. 1000 {
    let input: f32 = (index as f32 * 0.37).sin();
    assert_eq!(dry_bank.process(input), input); }}

/// 32 loops (16 voices of two unisons) at pitches that include
/// unisons, octaves and fifths, struck and then left alone with no
/// drive, store an energy (the sum of squares of every delay line)
/// that never rises from one 128-sample block to the next once the
/// excitation has streamed in, for a light, the default and a heavy
/// bridge.
///
/// The loops carry no dispersion chain here. A chain of first-order
/// allpasses holds tens of samples of the wave in its few states
/// (its group delay at a low note is about 15 samples a section), so
/// as a strike's pulses pass through it the delay lines' sum of
/// squares dips and recovers, rising by up to 110 percent from one
/// block to the next in a lone loop with no junction at all. With the chain, the test
/// asserts the weaker fact that the energy averaged over successive
/// quarter seconds never rises.
#[test]
fn the_junction_never_raises_the_stored_energy() {
  let pitches: [f32; 16] = [
    110.0, 110.0, 220.0, 220.0, 165.0, 330.0, 440.0, 55.0,
    82.5, 247.5, 660.0, 880.0, 123.75, 371.25, 1320.0, 2640.0 ];
  let block: usize = 128;
  let blocks_per_quarter_second: usize = seconds(0.25) / block;
  for dispersion_sections in [0_u32, StringsParams::default().string.dispersion_sections] {
    for heaviness in [1.0_f32, 50.0, 500.0] {
      let mut params: StringsParams = StringsParams::default();
      params.bridge.heaviness = heaviness;
      params.string.dispersion_sections = dispersion_sections;
      let block_energies: Vec<f32> = block_energies_after_the_excitation(
        &params, &pitches, block, seconds(3.0) / block);
      let largest_block_rise: f32 = largest_relative_rise(&block_energies);
      let quarter_second_means: Vec<f32> = block_energies
        .chunks_exact(blocks_per_quarter_second)
        .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
        .collect();
      let largest_quarter_second_rise: f32 = largest_relative_rise(&quarter_second_means);
      println!("junction r = {heaviness}, {dispersion_sections} dispersion sections: energy \
                from {:.4} to {:.4e}, largest rise block to block {largest_block_rise:+.3e}, \
                quarter second to quarter second {largest_quarter_second_rise:+.3e}",
               block_energies[0], block_energies[block_energies.len() - 1]);
      assert!(block_energies.iter().all(|energy| energy.is_finite()));
      if dispersion_sections == 0 {
        assert!(largest_block_rise <= 0.0,
                "r = {heaviness}: the energy rose by {largest_block_rise} in a block");
      } else {
        assert!(largest_quarter_second_rise <= 0.0,
                "r = {heaviness}: the energy rose by {largest_quarter_second_rise} in a \
                 quarter second"); }
      assert!(block_energies[block_energies.len() - 1] < block_energies[0]); }}}

/// The sympathy between strings weakens with the bridge's
/// heaviness: a struck 220 Hz voice and a silent one at the same
/// pitch share one junction, and half a second later the silent one
/// holds an energy above 1e-6 and above a tenth of the struck one's
/// under a light bridge (r = 20), and under a heavy one (r = 5000)
/// below a fiftieth of the struck one's and a tenth of what it held
/// under the light bridge.
///
/// Two loops at one pitch resonate exactly, so even a heavy bridge
/// hands the silent string about `2 / (N + r)` of the struck one's
/// amplitude every round trip, coherently: after 110 round trips at
/// r = 5000 its energy is about half a percent of the struck one's
/// (measured 4.6e-3 against 0.88), far above an absolute 1e-6, which
/// would take a bridge some 70 times heavier.
#[test]
fn a_silent_string_rings_by_sympathy_more_under_a_lighter_bridge() {
  let mut sympathetic_energy_by_heaviness: Vec<(f32, f32, f32)> = Vec::new();
  for heaviness in [20.0_f32, 5000.0] {
    let mut params: StringsParams = StringsParams::default();
    params.bridge.heaviness = heaviness;
    let mut string_voices: BTreeMap<VoiceId, StringVoice> = struck_string_voices(&params, &[220.0]);
    string_voices.insert(
      1, StringVoice::new(&params, 220.0, SAMPLE_RATE, 0, Vec::new(), Arc::from(Vec::new())));
    let mut junction: Junction = Junction::new(&params.bridge, SAMPLE_RATE);
    let mut arriving_waves: Vec<f32> = Vec::with_capacity(4);
    for _ in 0 .. seconds(0.5) {
      couple_through_junction(&mut string_voices, &mut junction, &mut arriving_waves); }
    let struck_energy: f32 = string_voices[&0].energy();
    let silent_energy: f32 = string_voices[&1].energy();
    println!("sympathy at r = {heaviness}: struck string {struck_energy:.4e}, \
              silent string {silent_energy:.4e}");
    sympathetic_energy_by_heaviness.push((heaviness, struck_energy, silent_energy)); }
  let (_, light_struck_energy, light_silent_energy): (f32, f32, f32) =
    sympathetic_energy_by_heaviness[0];
  let (_, heavy_struck_energy, heavy_silent_energy): (f32, f32, f32) =
    sympathetic_energy_by_heaviness[1];
  assert!(light_silent_energy > 1e-6, "r = 20: the silent string holds {light_silent_energy}");
  assert!(light_silent_energy > 0.1 * light_struck_energy,
          "r = 20: the silent string holds {light_silent_energy}, the struck one \
           {light_struck_energy}");
  assert!(heavy_silent_energy < 0.02 * heavy_struck_energy,
          "r = 5000: the silent string holds {heavy_silent_energy}, the struck one \
           {heavy_struck_energy}");
  assert!(heavy_silent_energy < 0.1 * light_silent_energy,
          "r = 5000: the silent string holds {heavy_silent_energy}, against \
           {light_silent_energy} at r = 20"); }

/// Three voices rendered for 2 s give finite output, below the clamp
/// and audible.
#[test]
fn the_renderer_renders_three_voices_finite_below_the_clamp() {
  let params: StringsParams = StringsParams::default();
  let mut renderer: StringsRenderer = StringsRenderer::new(params, SAMPLE_RATE);
  let mut voices: VoiceMap = VoiceMap::new();
  for (index, freq) in [110.0_f32, 220.0, 330.0].iter().enumerate() {
    voices.insert(finger_key(index), struck_voice_state(index as VoiceId, *freq, SAMPLE_RATE)); }
  let rendered: Vec<f32> = render_seconds(&mut renderer, &mut voices, &params, 2.0);
  let peak: f32 = peak_of(&rendered);
  println!("three voices: peak {peak:.4}");
  assert!(rendered.iter().all(|sample| sample.is_finite()));
  assert!(peak < 0.95, "peak {peak} reaches the clamp");
  assert!(peak > 1e-3, "peak {peak} is silent");
  assert_eq!(renderer.voice_count(), 3); }

/// A released voice's output (the body off, so what is heard is the
/// voice alone) falls below -60 dB of its peak within
/// `RELEASE_SECS` plus 50 ms, and the voice then leaves both the map
/// and the engine.
#[test]
fn a_released_voice_falls_silent_within_its_release() {
  let mut params: StringsParams = StringsParams::default();
  params.body.mix = 0.0;
  let mut renderer: StringsRenderer = StringsRenderer::new(params, SAMPLE_RATE);
  let mut voices: VoiceMap = VoiceMap::new();
  voices.insert(finger_key(0), struck_voice_state(0, 220.0, SAMPLE_RATE));
  let held: Vec<f32> = render_seconds(&mut renderer, &mut voices, &params, 0.5);
  let peak: f32 = peak_of(&held);
  release_voice_state(voices.get_mut(&finger_key(0)).expect("the voice"), SAMPLE_RATE);
  let released: Vec<f32> =
    render_seconds(&mut renderer, &mut voices, &params, RELEASE_SECS + 0.05 + 0.5);
  let after_release: f32 = peak_of(&released[seconds(RELEASE_SECS + 0.05) ..]);
  println!("release: peak {peak:.4}, after release + 50 ms {after_release:.3e}");
  assert!(after_release < 1e-3 * peak, "{after_release} remains of a peak of {peak}");
  assert!(voices.is_empty(), "the released voice stays in the map");
  assert_eq!(renderer.voice_count(), 0); }

/// A voice removed from the map drops its string voice; a voice
/// moved to another key under the same id (as a sustain moves it)
/// keeps its string voice, born when it was.
#[test]
fn the_renderer_follows_the_map_by_voice_id() {
  let params: StringsParams = StringsParams::default();
  let mut renderer: StringsRenderer = StringsRenderer::new(params, SAMPLE_RATE);
  let mut voices: VoiceMap = VoiceMap::new();
  voices.insert(finger_key(0), struck_voice_state(0, 220.0, SAMPLE_RATE));
  voices.insert(finger_key(1), struck_voice_state(1, 330.0, SAMPLE_RATE));
  render_seconds(&mut renderer, &mut voices, &params, 0.1);
  assert_eq!(renderer.voice_count(), 2);
  let birth: u64 = renderer.string_voice(1).expect("voice 1").birth();
  voices.remove(&finger_key(0));
  { // The sustain's move: same state, same id, a drone key.
    let state: VoiceState = voices.remove(&finger_key(1)).expect("voice 1");
    voices.insert(VoiceSource::SurfaceDrone { grid: 0, pitch: 7 }, state); }
  render_seconds(&mut renderer, &mut voices, &params, 0.1);
  assert_eq!(renderer.voice_count(), 1);
  assert!(renderer.string_voice(0).is_none());
  assert_eq!(renderer.string_voice(1).expect("voice 1").birth(), birth); }

/// A held voice whose string has decayed to silence keeps its string
/// voice, so it can still ring in sympathy.
#[test]
fn a_silent_held_string_keeps_its_string_voice() {
  let mut params: StringsParams = StringsParams::default();
  params.string.t60_seconds_at_55hz = 0.05;
  params.string.t60_seconds_at_1760hz = 0.05;
  let mut renderer: StringsRenderer = StringsRenderer::new(params, SAMPLE_RATE);
  let mut voices: VoiceMap = VoiceMap::new();
  voices.insert(finger_key(0), struck_voice_state(0, 220.0, SAMPLE_RATE));
  let rendered: Vec<f32> = render_seconds(&mut renderer, &mut voices, &params, 2.0);
  assert!(peak_of(&rendered[seconds(1.5) ..]) < 1e-6, "the string is not yet silent");
  assert_eq!(voices.len(), 1);
  assert_eq!(renderer.voice_count(), 1, "the silent string voice was dropped"); }

/// Changed parameters are adopted at the next block: the renderer's
/// parameters move, and a sounding loop is retuned to the new decay.
#[test]
fn the_renderer_adopts_changed_params_at_the_next_block() {
  let params: StringsParams = StringsParams::default();
  let mut renderer: StringsRenderer = StringsRenderer::new(params, SAMPLE_RATE);
  let mut voices: VoiceMap = VoiceMap::new();
  voices.insert(finger_key(0), struck_voice_state(0, 220.0, SAMPLE_RATE));
  render_seconds(&mut renderer, &mut voices, &params, 0.1);
  let gain_before: f32 = renderer.string_voice(0).expect("voice 0").loops()[0].round_trip_gain();
  let mut changed: StringsParams = params;
  changed.string.t60_seconds_at_55hz = 2.0;
  render_seconds(&mut renderer, &mut voices, &changed, 0.01);
  assert_eq!(*renderer.params(), changed);
  let gain_after: f32 = renderer.string_voice(0).expect("voice 0").loops()[0].round_trip_gain();
  assert!(gain_after < gain_before, "the loop kept its gain {gain_before} (now {gain_after})"); }

/// A constant full pedal over the whole ex-ante leg (300 ms here)
/// pre-charges a string with what the closed form of the plan's sum
/// gives: `precharge_gain * integral over s in [0, ex_ante] of (s /
/// ex_ante) exp(-(ex_ante - s) / tau) ds`, which is `precharge_gain *
/// (tau (1 - E) - tau^2 / ex_ante (1 - E (1 + ex_ante / tau)))` with
/// `E = exp(-ex_ante / tau)`, within 1 percent, at a sample rate whose
/// millisecond is a whole number of samples and at one whose is not.
#[test]
fn a_constant_full_pedal_precharges_as_the_closed_form_integral() {
  let mut params: StringsParams = StringsParams::default();
  params.energy.ex_ante_energizing_ms = 300.0;
  let f0: f32 = 220.0;
  let tau: f64 = t60_seconds(&params.string, f0) as f64 / 1000.0_f64.ln();
  let ex_ante_seconds: f64 = params.energy.ex_ante_energizing_ms as f64 / 1000.0;
  let expected: f64 = {
    let decayed_over_the_leg: f64 = (-ex_ante_seconds / tau).exp();
    params.energy.precharge_gain as f64
      * (tau * (1.0 - decayed_over_the_leg)
         - tau * tau / ex_ante_seconds
           * (1.0 - decayed_over_the_leg * (1.0 + ex_ante_seconds / tau))) };
  for sample_rate in [SAMPLE_RATE, 44_100.0] {
    let birth: u64 = sample_rate as u64;
    let history: PedalHistory = history_of_a_constant_pedal(1.0, birth, sample_rate);
    let measured: f32 = precharge_amplitude(&history, birth, &params.energy, tau as f32, sample_rate);
    println!("pre-charge of a full pedal over {} ms at {sample_rate} Hz: {measured:.6}, \
              closed form {expected:.6}", params.energy.ex_ante_energizing_ms);
    assert!(relative_error(measured, expected as f32) < 0.01,
            "pre-charge {measured}, closed form {expected}"); }}

/// A pedal that never reported (no history, or a history of NaN)
/// pre-charges nothing, and so does a pedal at the heel.
#[test]
fn an_unreported_or_heel_pedal_precharges_nothing() {
  let params: StringsParams = StringsParams::default();
  let birth: u64 = SAMPLE_RATE as u64;
  let tau: f32 = t60_seconds(&params.string, 220.0) / 1000.0_f32.ln();
  let empty: PedalHistory = PedalHistory::new(SAMPLE_RATE);
  assert_eq!(precharge_amplitude(&empty, birth, &params.energy, tau, SAMPLE_RATE), 0.0);
  for position in [f32::NAN, 0.0] {
    let history: PedalHistory = history_of_a_constant_pedal(position, birth, SAMPLE_RATE);
    assert_eq!(precharge_amplitude(&history, birth, &params.energy, tau, SAMPLE_RATE), 0.0,
               "a pedal at {position} pre-charged"); }}

/// Under a full pedal and a 1 s ex-post leg a struck note grows: its
/// RMS (20 ms windows, the body off) at 0.9 s is above its RMS at 0.1
/// s. Under a pedal at the heel it decays (0.9 s below 0.1 s), and
/// renders exactly as it does with no pedal at all.
///
/// The note is struck at a tenth of the drive's limit, so what is
/// measured is the drive's growth and not its limiter. Struck near
/// the limit, the tanh flattens the growth within a few tenths of a
/// second, and the level it then sustains follows `k`, which falls
/// with the triangle, so by 0.9 s the note is back near where it was
/// at 0.1 s.
#[test]
fn a_full_pedal_grows_a_note_over_its_window_and_a_heel_pedal_does_not() {
  let mut params: StringsParams = StringsParams::default();
  params.body.mix = 0.0;
  params.energy.ex_post_energizing_ms = 1000.0;
  params.exciter.pick_energy = params.energy.drive_limit / 10.0;
  let render_under_pedal = |pedal_position: f32| -> Vec<f32> {
    let mut renderer: StringsRenderer = StringsRenderer::new(params, SAMPLE_RATE);
    let mut voices: VoiceMap = VoiceMap::new();
    voices.insert(finger_key(0), struck_voice_state(0, 220.0, SAMPLE_RATE));
    render_seconds_under_pedal(&mut renderer, &mut voices, &params, 1.0, pedal_position) };
  let full: Vec<f32> = render_under_pedal(1.0);
  let heel: Vec<f32> = render_under_pedal(0.0);
  let unreported: Vec<f32> = render_under_pedal(f32::NAN);
  let (full_early, full_late): (f32, f32) = (rms_around(&full, 0.1), rms_around(&full, 0.9));
  let (heel_early, heel_late): (f32, f32) = (rms_around(&heel, 0.1), rms_around(&heel, 0.9));
  println!("full pedal: RMS {full_early:.4} at 0.1 s, {full_late:.4} at 0.9 s, peak {:.4}; \
            heel pedal: {heel_early:.4}, {heel_late:.4}", peak_of(&full));
  assert!(full.iter().all(|sample| sample.is_finite()));
  assert!(full_late > full_early, "a full pedal: {full_early} at 0.1 s, {full_late} at 0.9 s");
  assert!(heel_late < heel_early, "a heel pedal: {heel_early} at 0.1 s, {heel_late} at 0.9 s");
  assert_eq!(heel, unreported, "a heel pedal renders unlike no pedal"); }

/// Only the window gates the drive: a voice born under a full pedal
/// that stays full grows over its ex-post leg, and once the leg is
/// over its stored energy, averaged over successive 50 ms, never
/// rises again. (Block by block it may: the delay line holds a few
/// samples less than a period, so the sum of squares of a peaky wave
/// wobbles by a percent or two as the wave's shape drifts.)
#[test]
fn a_full_pedal_stops_feeding_a_note_when_its_window_closes() {
  let mut params: StringsParams = StringsParams::default();
  params.energy.ex_post_energizing_ms = 500.0;
  params.exciter.pick_energy = params.energy.drive_limit / 10.0;
  let block_seconds: f32 = 128.0 / SAMPLE_RATE;
  let blocks_per_50_ms: usize = seconds(0.05) / 128;
  let mut renderer: StringsRenderer = StringsRenderer::new(params, SAMPLE_RATE);
  let mut voices: VoiceMap = VoiceMap::new();
  render_seconds_under_pedal(&mut renderer, &mut voices, &params, 1.0, 1.0);
  voices.insert(finger_key(0), struck_voice_state(0, 220.0, SAMPLE_RATE));
  let ex_post_seconds: f32 = params.energy.ex_post_energizing_ms / 1000.0;
  let mut energies: Vec<(f32, f32)> = Vec::new();
  let mut since_birth: f32 = 0.0;
  while since_birth < ex_post_seconds + 1.5 {
    render_seconds_under_pedal(&mut renderer, &mut voices, &params, block_seconds, 1.0);
    since_birth += block_seconds;
    energies.push((since_birth, renderer.string_voice(0).expect("voice 0").energy())); }
  let energy_at = |seconds: f32| -> f32 {
    energies.iter().find(|(time, _)| *time >= seconds).expect("rendered that long").1 };
  let after_the_window: Vec<f32> = energies.iter()
    .filter(|(time, _)| *time > ex_post_seconds)
    .map(|(_, energy)| *energy)
    .collect::<Vec<f32>>()
    .chunks_exact(blocks_per_50_ms)
    .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
    .collect();
  let largest_rise: f32 = largest_relative_rise(&after_the_window);
  println!("window gate: energy {:.4} at 0.05 s, {:.4} at the window's end, {:.4} 1.5 s \
            later (50 ms mean); largest rise of the 50 ms means after the window \
            {largest_rise:+.3e}",
           energy_at(0.05), energy_at(ex_post_seconds), after_the_window[after_the_window.len() - 1]);
  assert!(energy_at(ex_post_seconds) > energy_at(0.05), "the note did not grow in its window");
  assert!(largest_rise <= 0.0, "the energy rose by {largest_rise} after the window"); }

/// The drive saturates at its limit times the pedal's power: a wave
/// far above the limit takes `k * drive_limit * pedal ^ e`, so a
/// light pedal blooms to a lower ceiling than a full one, while a
/// wave far below it takes `k * v` whatever the pedal.
#[test]
fn the_drive_saturates_at_its_limit_times_the_pedal_power() {
  let params: StringsParams = StringsParams::default();
  let f0: f32 = 220.0;
  let gain: f32 = round_trip_gain(&params.string, f0);
  for pedal_position in [1.0_f32, 0.5] {
    let power: f32 = pedal_power(&params.energy, pedal_position);
    let energy_factor: f32 = power;
    let k: f32 = ebow_drive_gain(&params.energy, energy_factor, f0, gain);
    let saturated: f32 = drive(&params.energy, energy_factor, power, f0, gain, 100.0);
    let small_wave: f32 = 1e-4;
    let linear: f32 = drive(&params.energy, energy_factor, power, f0, gain, small_wave);
    println!("pedal {pedal_position}: k {k:.3e}, saturated drive {saturated:.3e}, \
              k * limit * power {:.3e}", k * params.energy.drive_limit * power);
    assert!(k > 0.0);
    assert!(relative_error(saturated, k * params.energy.drive_limit * power) < 1e-4,
            "pedal {pedal_position}: saturated drive {saturated}");
    assert!(relative_error(linear, k * small_wave) < 1e-3,
            "pedal {pedal_position}: small-wave drive {linear}"); }}

/// A voice's unisons take one drive, from the mean of their waves,
/// so they grow together. The measure is the heard power over the
/// stored energy, relative to a one-loop voice's: 2 for two loops in
/// phase, falling as they drift apart. Under a full pedal the two
/// default unisons still sum near coherently at 0.6 s (about 1.7);
/// undriven they have drifted to about 0.5 by then, and a drive per
/// loop, each from its own wave, leaves them drifting just so.
#[test]
fn a_full_pedal_drives_the_unisons_of_a_voice_in_phase() {
  let mut params: StringsParams = StringsParams::default();
  params.body.mix = 0.0;
  params.energy.ex_post_energizing_ms = 1000.0;
  params.exciter.pick_energy = params.energy.drive_limit / 10.0;
  let heard_power_over_energy = |unison_count: u32, pedal_position: f32| -> f32 {
    let mut params: StringsParams = params;
    params.string.unison_count = unison_count;
    let mut renderer: StringsRenderer = StringsRenderer::new(params, SAMPLE_RATE);
    let mut voices: VoiceMap = VoiceMap::new();
    voices.insert(finger_key(0), struck_voice_state(0, 220.0, SAMPLE_RATE));
    let rendered: Vec<f32> =
      render_seconds_under_pedal(&mut renderer, &mut voices, &params, 0.6, pedal_position);
    let last_20_ms: &[f32] = &rendered[rendered.len() - seconds(0.02) ..];
    let heard_power: f32 =
      last_20_ms.iter().map(|sample| sample * sample).sum::<f32>() / last_20_ms.len() as f32;
    heard_power / renderer.string_voice(0).expect("voice 0").energy() };
  let coherence_under = |pedal_position: f32| -> f32 {
    heard_power_over_energy(2, pedal_position) / heard_power_over_energy(1, pedal_position) };
  let (driven, undriven): (f32, f32) = (coherence_under(1.0), coherence_under(0.0));
  println!("two unisons at 0.6 s, heard power over energy relative to one loop's: \
            {driven:.3} under a full pedal, {undriven:.3} under none");
  assert!(driven > 1.5, "the driven unisons drifted apart: {driven}");
  assert!(driven > undriven, "the drive did not hold the unisons together"); }

/// Renders struck `pitches` through one junction for `block_count`
/// blocks of `block` samples and returns the stored energy (every
/// voice's `energy`) at the end of each block after both tables have
/// streamed in.
fn block_energies_after_the_excitation(
  params: &StringsParams,
  pitches: &[f32],
  block: usize,
  block_count: usize,
) -> Vec<f32> {
  let mut string_voices: BTreeMap<VoiceId, StringVoice> = struck_string_voices(params, pitches);
  let mut junction: Junction = Junction::new(&params.bridge, SAMPLE_RATE);
  let mut arriving_waves: Vec<f32> = Vec::with_capacity(3 * pitches.len());
  let mut energies: Vec<f32> = Vec::with_capacity(block_count);
  for _ in 0 .. block_count {
    for _ in 0 .. block {
      couple_through_junction(&mut string_voices, &mut junction, &mut arriving_waves); }
    if string_voices.values().all(|voice| voice.excitation_finished()) {
      energies.push(string_voices.values().map(|voice| voice.energy()).sum()); }}
  energies }

/// The largest `next / previous - 1` over consecutive values; 0 when
/// none rises.
fn largest_relative_rise(
  values: &[f32],
) -> f32 {
  values.windows(2).fold(0.0, |largest, pair| largest.max(pair[1] / pair[0] - 1.0)) }

/// One string voice per pitch, struck with the dry table and the
/// knock of `params`, keyed by its index, all with gain 1.
fn struck_string_voices(
  params: &StringsParams,
  pitches: &[f32],
) -> BTreeMap<VoiceId, StringVoice> {
  let knock: Arc<[f32]> = Arc::from(knock_table(&params.exciter, &params.body, SAMPLE_RATE));
  pitches.iter()
    .enumerate()
    .map(|(index, f0)| {
      let mut voice: StringVoice = StringVoice::new(
        params, *f0, SAMPLE_RATE, 0, dry_table(&params.exciter, *f0, SAMPLE_RATE),
        Arc::clone(&knock));
      voice.set_gain(1.0);
      (index as VoiceId, voice) })
    .collect() }

/// Renders `duration` seconds in 128-frame mono blocks, as the audio
/// callback would, at amplitude 0.15, with no energy pedal.
fn render_seconds(
  renderer: &mut StringsRenderer,
  voices: &mut VoiceMap,
  params: &StringsParams,
  duration: f32,
) -> Vec<f32> {
  render_seconds_under_pedal(renderer, voices, params, duration, f32::NAN) }

/// `render_seconds` with the energy pedal held at `pedal_position`
/// (NaN: never reported).
fn render_seconds_under_pedal(
  renderer: &mut StringsRenderer,
  voices: &mut VoiceMap,
  params: &StringsParams,
  duration: f32,
  pedal_position: f32,
) -> Vec<f32> {
  let mut rendered: Vec<f32> = Vec::with_capacity(seconds(duration));
  let mut block: [f32; 128] = [0.0; 128];
  while rendered.len() < seconds(duration) {
    renderer.render_block(voices, &mut block, 1, SAMPLE_RATE, 0.15, params, pedal_position);
    rendered.extend_from_slice(&block); }
  rendered }

/// A pedal history at `sample_rate` recorded in 128-frame blocks
/// from engine time 0 through the block containing `through`, the
/// pedal at `position` throughout.
fn history_of_a_constant_pedal(
  position: f32,
  through: u64,
  sample_rate: f32,
) -> PedalHistory {
  let mut history: PedalHistory = PedalHistory::new(sample_rate);
  let block: u64 = 128;
  for block_start in (0 ..= through).step_by(block as usize) {
    history.record_block(block_start, block as usize, position); }
  history }

/// The RMS of `signal` over the 20 ms centered on `at_seconds`.
fn rms_around(
  signal: &[f32],
  at_seconds: f32,
) -> f32 {
  let window: &[f32] = &signal[seconds(at_seconds - 0.01) .. seconds(at_seconds + 0.01)];
  (window.iter().map(|sample| sample * sample).sum::<f32>() / window.len() as f32).sqrt() }

fn finger_key(
  index: usize,
) -> VoiceSource {
  VoiceSource::SurfaceFinger { grid: 0, cell: (index as i32, 0) }}

fn peak_of(
  signal: &[f32],
) -> f32 {
  signal.iter().fold(0.0, |peak, sample| peak.max(sample.abs())) }

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
