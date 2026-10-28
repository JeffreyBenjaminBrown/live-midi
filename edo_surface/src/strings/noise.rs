//! A deterministic random source: the 32-bit linear congruential
//! generator of Numerical Recipes (multiplier 1664525, increment
//! 1013904223). Every seed is valid, the same seed gives the same
//! sequence on every platform, and its upper bits are white enough
//! for an excitation burst or a jitter. Not for anything statistical.

pub struct DeterministicNoise {
  state: u32, }

impl DeterministicNoise {
  pub fn new(
    seed: u32,
  ) -> DeterministicNoise {
    DeterministicNoise { state: seed }}

  /// Uniform in [-1, 1).
  pub fn next_bipolar(
    &mut self,
  ) -> f32 {
    2.0 * self.next_unit() - 1.0 }

  /// Uniform in [0, 1), from the generator's upper 24 bits (the
  /// lower bits of an LCG have short periods).
  pub fn next_unit(
    &mut self,
  ) -> f32 {
    self.state = self.state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    let upper_24_bits: u32 = self.state >> 8;
    upper_24_bits as f32 / 16_777_216.0 }}
