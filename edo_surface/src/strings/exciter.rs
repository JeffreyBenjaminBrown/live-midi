//! The excitation table: what a note's loops are filled with at
//! key-down. One period of a raw source (white noise for a pluck,
//! a few raised-cosine pulses for a strike), through a one-pole
//! lowpass at `exciter.brightness` times `f0`, through the position
//! comb `1 - z^-round(position P)`, plus the same convolved with the
//! body's knock and scaled by `exciter.knock_mix`, and normalized so
//! the table's peak is `exciter.pick_energy`.
//!
//! The same parameters and pitch give the same table every time: the
//! noise has a fixed seed and nothing else is random.

use crate::strings::body::BodyModes;
use crate::strings::first_order::{one_pole_lowpass, FirstOrderCoefficients, FirstOrderState};
use crate::strings::noise::DeterministicNoise;
use crate::strings::params::{BodyParams, ExciterKind, ExciterParams};
use std::f64::consts::PI;

/// The seed of every pluck's noise.
pub const PLUCK_NOISE_SEED: u32 = 0x5EED_0001;

/// The excitation lowpass's cutoff never exceeds this fraction of the
/// sample rate, however bright the setting or high the note.
pub const HIGHEST_EXCITATION_CUTOFF_FRACTION_OF_SAMPLE_RATE: f32 = 0.45;

/// The knock window `exp(-t / (knock_ms / KNOCK_WINDOW_TIME_CONSTANTS))`
/// falls 60 dB (a factor `e^-6.9`) over `knock_ms`.
pub const KNOCK_WINDOW_TIME_CONSTANTS: f32 = 6.9;

/// The table for a note at `f0` Hz. It is
/// `excitation_table_length` samples long: one period (the dry
/// excitation) plus the knock's tail.
pub fn excitation_table(
  exciter: &ExciterParams,
  body: &BodyParams,
  f0: f32,
  sample_rate: f32,
) -> Vec<f32> {
  let period: usize = period_samples(f0, sample_rate);
  let source: Vec<f32> = match exciter.kind {
    ExciterKind::Pluck => white_noise(period),
    ExciterKind::Strike => strike_pulses(exciter, period), };
  let cutoff_hz: f32 = (exciter.brightness * f0)
    .min(HIGHEST_EXCITATION_CUTOFF_FRACTION_OF_SAMPLE_RATE * sample_rate);
  let smoothed: Vec<f32> = lowpassed(&source, cutoff_hz, sample_rate);
  let combed: Vec<f32> = position_comb(&smoothed, position_comb_delay(exciter, period));
  let knock: Vec<f32> = knock_table(exciter, body, sample_rate);
  let knocked: Vec<f32> = add_convolution_with_knock(&combed, &knock, exciter.knock_mix);
  normalized_to_peak(knocked, exciter.pick_energy) }

/// One period plus the knock, less the sample they share: the length
/// of a full linear convolution of the two.
pub fn excitation_table_length(
  exciter: &ExciterParams,
  f0: f32,
  sample_rate: f32,
) -> usize {
  period_samples(f0, sample_rate) + knock_length_samples(exciter, sample_rate) - 1 }

/// `sample_rate / f0` rounded, at least 2.
pub fn period_samples(
  f0: f32,
  sample_rate: f32,
) -> usize {
  ((sample_rate / f0).round() as usize).max(2) }

/// `period` samples of uniform white noise in [-1, 1) from
/// `PLUCK_NOISE_SEED`.
fn white_noise(
  period: usize,
) -> Vec<f32> {
  let mut noise: DeterministicNoise = DeterministicNoise::new(PLUCK_NOISE_SEED);
  (0 .. period).map(|_| noise.next_bipolar()).collect() }

/// `exciter.strike_pulses` raised-cosine (`sin^2`) pulses of unit
/// height, each `hammer_contact_fraction * period` samples wide
/// (at least one) and starting one width after the previous one
/// ends, in a table one period long. Validation keeps the train
/// inside the period; a pulse that would overrun it is cut.
fn strike_pulses(
  exciter: &ExciterParams,
  period: usize,
) -> Vec<f32> {
  let width: usize = ((exciter.hammer_contact_fraction * period as f32).round() as usize).max(1);
  let mut table: Vec<f32> = vec![0.0; period];
  for pulse_index in 0 .. exciter.strike_pulses as usize {
    let start: usize = 2 * pulse_index * width;
    for offset in 0 .. width {
      if let Some(sample) = table.get_mut(start + offset) {
        let phase: f64 = PI * (offset as f64 + 0.5) / width as f64;
        *sample = (phase.sin() * phase.sin()) as f32; }}}
  table }

/// `input` through `first_order::one_pole_lowpass` at `cutoff_hz`,
/// from rest.
fn lowpassed(
  input: &[f32],
  cutoff_hz: f32,
  sample_rate: f32,
) -> Vec<f32> {
  let coefficients: FirstOrderCoefficients = one_pole_lowpass(cutoff_hz, sample_rate);
  let mut state: FirstOrderState = FirstOrderState::default();
  input.iter().map(|sample| state.tick(&coefficients, *sample)).collect() }

/// `round(position * period)`, at least one sample.
fn position_comb_delay(
  exciter: &ExciterParams,
  period: usize,
) -> usize {
  ((exciter.position * period as f32).round() as usize).max(1) }

/// `1 - z^-delay` applied circularly over the one-period table, as
/// the loop it fills (whose period the table is) would see it: every
/// harmonic that is a multiple of `period / delay` cancels, and so
/// does any DC.
fn position_comb(
  period_table: &[f32],
  delay: usize,
) -> Vec<f32> {
  let period: usize = period_table.len();
  (0 .. period)
    .map(|index| period_table[index] - period_table[(index + period - delay % period) % period])
    .collect() }

/// `dry + mix * (dry * knock)`, `*` the full linear convolution; the
/// dry part is zero-padded to the convolution's length.
fn add_convolution_with_knock(
  dry: &[f32],
  knock: &[f32],
  mix: f32,
) -> Vec<f32> {
  let mut result: Vec<f32> = vec![0.0; dry.len() + knock.len() - 1];
  result[.. dry.len()].copy_from_slice(dry);
  if mix > 0.0 {
    for (dry_index, dry_sample) in dry.iter().enumerate() {
      let scaled: f32 = mix * dry_sample;
      for (knock_index, knock_sample) in knock.iter().enumerate() {
        result[dry_index + knock_index] += scaled * knock_sample; }}}
  result }

/// The body's impulse response, `knock_ms` long, under the window
/// `exp(-t / (knock_ms / 6.9))`, scaled to unit energy (sum of
/// squares 1) so that convolving a white table with it keeps the
/// table's RMS, which makes `knock_mix` the knock's level relative
/// to the dry table.
pub fn knock_table(
  exciter: &ExciterParams,
  body: &BodyParams,
  sample_rate: f32,
) -> Vec<f32> {
  let length: usize = knock_length_samples(exciter, sample_rate);
  let response: Vec<f32> =
    BodyModes::design(body, sample_rate).impulse_response(sample_rate, length);
  let time_constant_samples: f32 =
    exciter.knock_ms / 1000.0 * sample_rate / KNOCK_WINDOW_TIME_CONSTANTS;
  let windowed: Vec<f32> = response.iter()
    .enumerate()
    .map(|(index, sample)| sample * (-(index as f32) / time_constant_samples).exp())
    .collect();
  let energy: f32 = windowed.iter().map(|sample| sample * sample).sum();
  if energy > 0.0 {
    let scale: f32 = 1.0 / energy.sqrt();
    windowed.iter().map(|sample| sample * scale).collect()
  } else { windowed }}

/// `knock_ms` in samples, rounded, at least one.
fn knock_length_samples(
  exciter: &ExciterParams,
  sample_rate: f32,
) -> usize {
  ((exciter.knock_ms / 1000.0 * sample_rate).round() as usize).max(1) }

/// `table` scaled so its largest magnitude is `peak` (unchanged if it
/// is all zeros).
fn normalized_to_peak(
  table: Vec<f32>,
  peak: f32,
) -> Vec<f32> {
  let largest: f32 = table.iter().fold(0.0, |largest, sample| largest.max(sample.abs()));
  if largest > 0.0 {
    let scale: f32 = peak / largest;
    table.iter().map(|sample| sample * scale).collect()
  } else { table }}
