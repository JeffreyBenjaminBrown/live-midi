//! The howl path: the optional active loop beside the passive
//! junction. Per sample it takes the sum of every junction member's
//! arriving wave (the sum the junction takes), saturates it as
//! `h = howl.level * tanh(howl.gain * sum)`, delays it by
//! `howl.delay_ms` (the speaker's distance), and the renderer adds
//! the delayed `h` into every loop's write and every modal voice's
//! input. Whatever the gain, `|h| <= howl.level`, so the path is
//! bounded; above the threshold at which its small-signal gain beats
//! a note's loss, the note sustains on it indefinitely, and the
//! partials that several notes share win the saturation.
//!
//! A gain of 0 turns the path off: it computes nothing, writes
//! nothing and adds exactly 0.
//!
//! How loud a sustained note settles: saturated, `h` is near a
//! square wave of amplitude `level` at the note's pitch, whose
//! fundamental, `4 / pi * level`, a loop of round-trip gain `g`
//! accumulates to an amplitude of about `4 / pi * level / (1 - g)`.
//! With `1 - g` between about 0.002 and 0.013 over the default decay
//! curve, a `level` of a few thousandths settles near a struck
//! note's level.

use crate::strings::params::HowlParams;

/// The longest `howl.delay_ms` validation allows; the ring holds this
/// much, so a changed delay allocates nothing.
pub const MAX_HOWL_DELAY_MS: f32 = 20.0;

pub struct HowlPath {
  gain: f32,
  level: f32,
  /// The delay, in samples, at least 1.
  delay_samples: usize,
  /// A ring of the saturated sums: slot `write_index` takes this
  /// sample's, and the one `delay_samples` behind it is the one added
  /// now. Room for `MAX_HOWL_DELAY_MS`, plus the slot written.
  ring: Vec<f32>,
  write_index: usize,
  sample_rate: f32, }

impl HowlPath {
  /// The path of `howl` at rest (its ring silent).
  pub fn new(
    howl: &HowlParams,
    sample_rate: f32,
  ) -> HowlPath {
    let ring_length: usize = delay_samples(MAX_HOWL_DELAY_MS, sample_rate) + 1;
    HowlPath {
      gain: howl.gain,
      level: howl.level,
      delay_samples: delay_samples(howl.delay_ms, sample_rate).min(ring_length - 1),
      ring: vec![0.0; ring_length],
      write_index: 0,
      sample_rate, }}

  /// Adopts changed howl parameters: the gain and level apply at
  /// once, the delay reads further back or nearer in the same ring. A
  /// path that was off starts with a silent ring, so it does not
  /// replay what it held when it was last on.
  pub fn reconfigure(
    &mut self,
    howl: &HowlParams,
  ) {
    if !self.is_on() {
      self.ring.iter_mut().for_each(|slot| *slot = 0.0); }
    self.gain = howl.gain;
    self.level = howl.level;
    self.delay_samples =
      delay_samples(howl.delay_ms, self.sample_rate).min(self.ring.len() - 1); }

  /// Whether the path runs: its gain is above 0.
  pub fn is_on(
    &self,
  ) -> bool {
    self.gain > 0.0 }

  /// What the path adds into every member this sample: the saturated
  /// sum of `delay_samples` ago. `sum_of_arriving_waves` is this
  /// sample's sum over every member, which it stores for later. 0
  /// with nothing computed when the path is off. Call it once per
  /// sample.
  pub fn feedback(
    &mut self,
    sum_of_arriving_waves: f32,
  ) -> f32 {
    if !self.is_on() {
      return 0.0; }
    let length: usize = self.ring.len();
    let delayed: f32 = self.ring[(self.write_index + length - self.delay_samples) % length];
    self.ring[self.write_index] = self.level * (self.gain * sum_of_arriving_waves).tanh();
    self.write_index = (self.write_index + 1) % length;
    delayed }}

/// `delay_ms` at `sample_rate`, rounded to whole samples, at least 1.
fn delay_samples(
  delay_ms: f32,
  sample_rate: f32,
) -> usize {
  ((delay_ms / 1000.0 * sample_rate).round() as usize).max(1) }
