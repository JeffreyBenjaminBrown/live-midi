//! A string voice: the strings engine's audio state for one
//! `VoiceState`, kept by the renderer under the voice's id. It holds
//! `string.unison_count` loops, the k-th detuned by `(k - (count - 1)
//! / 2) * string.unison_detune_cents`, each its own junction member,
//! and the two excitation tables it streams into them from its birth.
//!
//! Per sample the renderer calls `push_arriving_waves` (every loop's
//! wave arriving at the bridge), runs the junction on all the loops
//! of the engine, then calls `write_back`, which writes each loop's
//! `v_i - v_b + excitation / unison_count + drive_i`. The excitation
//! is one sample of the dry table plus one of the knock table while
//! either lasts, so a table of any length streams in over time.

use crate::strings::loop_dsp::Loop;
use crate::strings::params::{StringParams, StringsParams};
use std::sync::Arc;

/// A voice retunes its loops when its `VoiceState.freq` has moved
/// from the pitch they were tuned to by more than this fraction
/// (0.01 percent, about 0.17 cents).
pub const RETUNE_RELATIVE_THRESHOLD: f32 = 1e-4;

pub struct StringVoice {
  /// The voice's pitch, Hz: the loops are this, each detuned by its
  /// unison offset.
  f0: f32,
  /// Engine time, in samples, of the block in which the voice's id
  /// first appeared.
  birth: u64,
  loops: Vec<Loop>,
  /// One period of the note's own excitation; empty for a voice
  /// that rings only by sympathy.
  dry_table: Vec<f32>,
  /// The body's knock, shared by every voice born under the same
  /// parameters; empty for a voice that rings only by sympathy.
  knock_table: Arc<[f32]>,
  /// How far into both tables the voice has streamed.
  excitation_position: usize,
  /// This sample's gain coefficient (`VoiceStep.gain` times its
  /// factored pulse), set by the renderer before the voice's output
  /// is summed.
  gain: f32, }

impl StringVoice {
  /// A voice at `f0` Hz born at engine sample `birth`, with
  /// `string.unison_count` silent loops and the two tables to stream
  /// in. Pass empty tables for a voice that rings only by sympathy.
  pub fn new(
    params: &StringsParams,
    f0: f32,
    sample_rate: f32,
    birth: u64,
    dry_table: Vec<f32>,
    knock_table: Arc<[f32]>,
  ) -> StringVoice {
    let unison_count: usize = params.string.unison_count.max(1) as usize;
    let loops: Vec<Loop> = (0 .. unison_count)
      .map(|unison_index| {
        let offset_cents: f32 =
          unison_offset_cents(unison_index, unison_count, params.string.unison_detune_cents);
        Loop::new(&params.string, detuned(f0, offset_cents), sample_rate) })
      .collect();
    StringVoice {
      f0,
      birth,
      loops,
      dry_table,
      knock_table,
      excitation_position: 0,
      gain: 0.0, }}

  /// Retunes the loops to `f0` if it has moved from the voice's
  /// pitch by more than `RETUNE_RELATIVE_THRESHOLD`.
  pub fn retune_if_moved(
    &mut self,
    string: &StringParams,
    f0: f32,
  ) {
    if (f0 / self.f0 - 1.0).abs() > RETUNE_RELATIVE_THRESHOLD {
      self.retune(string, f0); }}

  /// Retunes every loop to `f0`, its unison offset recomputed from
  /// `string.unison_detune_cents`, and every coefficient from
  /// `string`. The wave in flight is kept. The loop count stays what
  /// it was at birth: a changed `unison_count` applies to notes born
  /// afterward.
  pub fn retune(
    &mut self,
    string: &StringParams,
    f0: f32,
  ) {
    let unison_count: usize = self.loops.len();
    for (unison_index, string_loop) in self.loops.iter_mut().enumerate() {
      let offset_cents: f32 =
        unison_offset_cents(unison_index, unison_count, string.unison_detune_cents);
      string_loop.retune(string, detuned(f0, offset_cents)); }
    self.f0 = f0; }

  /// Appends each loop's wave arriving at the bridge this sample to
  /// `arriving_waves`, in loop order, and returns their sum. Call it
  /// once per sample, before `write_back`.
  pub fn push_arriving_waves(
    &mut self,
    arriving_waves: &mut Vec<f32>,
  ) -> f32 {
    let mut sum: f32 = 0.0;
    for string_loop in self.loops.iter_mut() {
      let arriving: f32 = string_loop.read_arriving();
      arriving_waves.push(arriving);
      sum += arriving; }
    sum }

  /// Writes each loop's `arriving - bridge_velocity + excitation /
  /// unison_count + drive` into its delay line and advances the
  /// excitation one sample. `arriving_waves` are this voice's waves
  /// from `push_arriving_waves`, in the same order.
  pub fn write_back(
    &mut self,
    arriving_waves: &[f32],
    bridge_velocity: f32,
  ) {
    let excitation_per_loop: f32 = self.next_excitation_sample() / self.loops.len() as f32;
    for (loop_index, (string_loop, arriving)) in self.loops
      .iter_mut()
      .zip(arriving_waves.iter())
      .enumerate()
    {
      let drive: f32 = drive_into_loop(loop_index, *arriving);
      string_loop.write(arriving - bridge_velocity + excitation_per_loop + drive); }}

  /// The sum of every loop's stored energy (`Loop::energy`).
  pub fn energy(
    &self,
  ) -> f32 {
    self.loops.iter().map(|string_loop| string_loop.energy()).sum() }

  /// Whether both tables have streamed in entirely.
  pub fn excitation_finished(
    &self,
  ) -> bool {
    self.excitation_position >= self.dry_table.len().max(self.knock_table.len()) }

  pub fn set_gain(
    &mut self,
    gain: f32,
  ) {
    self.gain = gain; }

  pub fn gain(
    &self,
  ) -> f32 {
    self.gain }

  pub fn birth(
    &self,
  ) -> u64 {
    self.birth }

  pub fn f0(
    &self,
  ) -> f32 {
    self.f0 }

  pub fn loop_count(
    &self,
  ) -> usize {
    self.loops.len() }

  pub fn loops(
    &self,
  ) -> &[Loop] {
    &self.loops }

  /// This sample of the dry table plus this sample of the knock (0
  /// past either's end), and the read position moved on.
  fn next_excitation_sample(
    &mut self,
  ) -> f32 {
    if self.excitation_finished() {
      return 0.0; }
    let position: usize = self.excitation_position;
    self.excitation_position += 1;
    self.dry_table.get(position).copied().unwrap_or(0.0)
      + self.knock_table.get(position).copied().unwrap_or(0.0) }}

/// What the energy source adds into loop `loop_index` this sample,
/// given the wave arriving from it. No energy source drives a string
/// yet (there is no energy pedal), so it adds nothing; the drive
/// will be computed here from the energy window and the arriving
/// wave.
fn drive_into_loop(
  _loop_index: usize,
  _arriving: f32,
) -> f32 {
  0.0 }

/// Cents by which the `unison_index`-th of `unison_count` loops is
/// detuned: `(k - (count - 1) / 2) * detune_cents`, symmetric about
/// the voice's pitch.
fn unison_offset_cents(
  unison_index: usize,
  unison_count: usize,
  detune_cents: f32,
) -> f32 {
  (unison_index as f32 - (unison_count as f32 - 1.0) / 2.0) * detune_cents }

/// `f0` moved by `offset_cents`.
fn detuned(
  f0: f32,
  offset_cents: f32,
) -> f32 {
  f0 * (offset_cents / 1200.0).exp2() }
