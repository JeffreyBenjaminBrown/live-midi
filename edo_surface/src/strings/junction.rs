//! The junction: the one bridge every sounding loop meets. Per
//! sample each loop hands it `v_i`, the wave arriving at the bridge,
//! and the junction answers the bridge velocity
//! `v_b = L((2 / (N + r)) sum_i v_i)`, with `N` the number of loops
//! in the engine, `r` the bridge heaviness and `L` a one-pole
//! lowpass with gain 1 at DC and cutoff `bridge.damping_hz`. Each
//! loop then writes `v_i - v_b` (plus its excitation and drive) back
//! into its delay line; the nut's inversion is folded into that sign.
//!
//! It is passive for any `r > 0`. At one frequency the write-back is
//! `(I - h c / N 1 1^T) v` with `h = L(e^jw)` and `c = 2 N / (N + r)`
//! in (0, 2): its eigenvalues are 1 (every direction with zero sum)
//! and `1 - h c` (the common direction), and `|1 - h c| <= 1` exactly
//! when `c |h|^2 <= 2 Re h`. For the one-pole `h = (1 - p) / (1 - p
//! e^-jw)` that reads `c (1 - p) <= 2 (1 - p cos w)`, true since
//! `c < 2` and `cos w <= 1`. So the bridge never adds energy; heavier
//! (larger `r`) is a smaller `c`, a weaker coupling.

use crate::strings::first_order::{one_pole_lowpass, FirstOrderCoefficients, FirstOrderState};
use crate::strings::params::BridgeParams;

pub struct Junction {
  heaviness: f32,
  damping_lowpass: FirstOrderCoefficients,
  damping_state: FirstOrderState, }

impl Junction {
  pub fn new(
    bridge: &BridgeParams,
    sample_rate: f32,
  ) -> Junction {
    Junction {
      heaviness: bridge.heaviness,
      damping_lowpass: damping_lowpass(bridge, sample_rate),
      damping_state: FirstOrderState::default(), }}

  /// Adopts changed bridge parameters, keeping the lowpass's state so
  /// the bridge velocity does not step.
  pub fn reconfigure(
    &mut self,
    bridge: &BridgeParams,
    sample_rate: f32,
  ) {
    self.heaviness = bridge.heaviness;
    self.damping_lowpass = damping_lowpass(bridge, sample_rate); }

  /// This sample's bridge velocity, from the sum of every loop's
  /// arriving wave and the number of loops summed. Call it once per
  /// sample, between the loops' reads and their writes.
  pub fn bridge_velocity(
    &mut self,
    sum_of_arriving_waves: f32,
    loop_count: usize,
  ) -> f32 {
    let undamped: f32 = 2.0 * sum_of_arriving_waves / (loop_count as f32 + self.heaviness);
    self.damping_state.tick(&self.damping_lowpass, undamped) }}

/// The one-pole lowpass at `damping_hz`, kept below Nyquist.
fn damping_lowpass(
  bridge: &BridgeParams,
  sample_rate: f32,
) -> FirstOrderCoefficients {
  one_pole_lowpass(bridge.damping_hz.min(0.49 * sample_rate), sample_rate) }
