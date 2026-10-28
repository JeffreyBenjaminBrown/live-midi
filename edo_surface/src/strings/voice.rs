//! The strings engine's audio state for one `VoiceState`, kept by the
//! renderer under the voice's id: a `VoiceKind`, which is either a
//! string voice of loops (`StringVoice`, below) or, for a note born
//! above the crossover, a modal voice (`modal::ModalVoice`). The
//! renderer drives both through `VoiceKind` alike: per sample it
//! reads every junction member's arriving wave, runs the junction,
//! and writes every member back.
//!
//! A string voice holds
//! `string.unison_count` loops, the k-th detuned by `(k - (count - 1)
//! / 2) * string.unison_detune_cents`, each its own junction member,
//! and the two excitation tables it streams into them from its birth.
//!
//! Per sample the renderer calls `push_arriving_waves` (every loop's
//! wave arriving at the bridge), runs the junction on all the loops
//! of the engine, then calls `write_back`, which writes each loop's
//! `v_i - v_b + excitation / unison_count + drive + howl`. The excitation
//! is one sample of the dry table plus one of the knock table while
//! either lasts, so a table of any length streams in over time. The
//! drive is the energy pedal's (`energy::drive`), one sample for the
//! whole voice, taken from the mean of its loops' arriving waves and
//! added to every loop, so the unisons are driven together; it reads
//! the energy factor and the pedal's power the renderer hands the
//! voice each sample, and the voice's own drive state
//! (`energy::DriveState`: the wind's noise, the bow's highpass). The howl is the howl path's sample
//! (`howl::HowlPath`), the same for every loop. A voice keeps the
//! energy parameters it was born under, so a change to them applies
//! to notes born afterward.

use crate::strings::energy::{drive, precharge_waveshape, DriveState};
use crate::strings::loop_dsp::Loop;
use crate::strings::modal::ModalVoice;
use crate::strings::params::{EnergyParams, StringParams, StringsParams};
use crate::types::VoiceId;
use std::sync::Arc;

/// A voice retunes its loops when its `VoiceState.freq` has moved
/// from the pitch they were tuned to by more than this fraction
/// (0.01 percent, about 0.17 cents).
pub const RETUNE_RELATIVE_THRESHOLD: f32 = 1e-4;

/// A voice of either kind, behind one interface.
pub enum VoiceKind {
  Loops(StringVoice),
  Modal(ModalVoice), }

/// A note's two excitation tables, streamed side by side from its
/// birth, one sample of each per sample while either lasts.
pub struct ExcitationStream {
  /// One period of the note's own excitation; empty for a voice
  /// that rings only by sympathy.
  dry_table: Vec<f32>,
  /// The body's knock, shared by every voice born under the same
  /// parameters; empty for a voice that rings only by sympathy.
  knock_table: Arc<[f32]>,
  /// How far into both tables the voice has streamed.
  position: usize, }

pub struct StringVoice {
  /// The voice's pitch, Hz: the loops are this, each detuned by its
  /// unison offset.
  f0: f32,
  /// Engine time, in samples, of the block in which the voice's id
  /// first appeared.
  birth: u64,
  loops: Vec<Loop>,
  excitation: ExcitationStream,
  /// This sample's gain coefficient (`VoiceStep.gain` times its
  /// factored pulse), set by the renderer before the voice's output
  /// is summed.
  gain: f32,
  /// The energy parameters at birth: the voice's window and drive.
  energy: EnergyParams,
  /// The voice's own state of the drive: the wind's noise and
  /// bandpass, the bow's highpass.
  drive_state: DriveState,
  /// This sample's energy factor `tri(t) * pedal(t) ^ e`
  /// (`energy::ex_post_energy_factor`), set by the renderer; 0 outside
  /// the window.
  energy_factor: f32,
  /// This sample's `pedal(t) ^ e` (`energy::pedal_power`), set by the
  /// renderer: the drive's limit is `energy.drive_limit` times this.
  pedal_power: f32, }

impl VoiceKind {
  /// How many junction members the voice is: its loops, or 1 for a
  /// modal voice.
  pub fn member_count(
    &self,
  ) -> usize {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.loop_count(),
      VoiceKind::Modal(_) => 1, }}

  /// Appends each member's arriving wave this sample to
  /// `arriving_waves`, in member order, and returns their sum. Call
  /// it once per sample, before `write_back`.
  pub fn push_arriving_waves(
    &mut self,
    arriving_waves: &mut Vec<f32>,
  ) -> f32 {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.push_arriving_waves(arriving_waves),
      VoiceKind::Modal(modal_voice) => {
        let arriving: f32 = modal_voice.read_arriving();
        arriving_waves.push(arriving);
        arriving }}}

  /// Writes every member back, from this voice's `arriving_waves`
  /// (from `push_arriving_waves`), the bridge velocity and the howl
  /// path's sample, and advances the excitation.
  pub fn write_back(
    &mut self,
    arriving_waves: &[f32],
    bridge_velocity: f32,
    howl_feedback: f32,
  ) {
    match self {
      VoiceKind::Loops(string_voice) =>
        string_voice.write_back(arriving_waves, bridge_velocity, howl_feedback),
      VoiceKind::Modal(modal_voice) =>
        modal_voice.write_back(arriving_waves[0], bridge_velocity, howl_feedback), }}

  /// Adds the pre-charge of peak `amplitude`, its waveshape that of
  /// the voice's energy kind under the pedal's power
  /// `pedal_power_at_birth` (`energy::precharge_waveshape`). Call it at
  /// birth, with the `string` parameters the voice was born under.
  pub fn precharge(
    &mut self,
    amplitude: f32,
    pedal_power_at_birth: f32,
    string: &StringParams,
    sample_rate: f32,
  ) {
    match self {
      VoiceKind::Loops(string_voice) =>
        string_voice.precharge(amplitude, pedal_power_at_birth, sample_rate),
      VoiceKind::Modal(modal_voice) =>
        modal_voice.precharge(amplitude, pedal_power_at_birth, string, sample_rate), }}

  /// Retunes the voice to `f0` if it has moved by more than
  /// `RETUNE_RELATIVE_THRESHOLD`.
  pub fn retune_if_moved(
    &mut self,
    params: &StringsParams,
    f0: f32,
  ) {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.retune_if_moved(&params.string, f0),
      VoiceKind::Modal(modal_voice) =>
        modal_voice.retune_if_moved(&params.string, &params.modal, f0), }}

  /// Retunes the voice to `f0` under `params`: a string voice's
  /// loops from the string table, a modal voice's modes from the
  /// string and modal tables.
  pub fn retune(
    &mut self,
    params: &StringsParams,
    f0: f32,
  ) {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.retune(&params.string, f0),
      VoiceKind::Modal(modal_voice) => modal_voice.retune(&params.string, &params.modal, f0), }}

  /// The stored energy: a string voice's loops', or what loops would
  /// hold of a modal voice's motion.
  pub fn energy(
    &self,
  ) -> f32 {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.energy(),
      VoiceKind::Modal(modal_voice) => modal_voice.energy(), }}

  pub fn excitation_finished(
    &self,
  ) -> bool {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.excitation_finished(),
      VoiceKind::Modal(modal_voice) => modal_voice.excitation_finished(), }}

  pub fn set_gain(
    &mut self,
    gain: f32,
  ) {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.set_gain(gain),
      VoiceKind::Modal(modal_voice) => modal_voice.set_gain(gain), }}

  pub fn gain(
    &self,
  ) -> f32 {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.gain(),
      VoiceKind::Modal(modal_voice) => modal_voice.gain(), }}

  pub fn set_energy_factor(
    &mut self,
    energy_factor: f32,
  ) {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.set_energy_factor(energy_factor),
      VoiceKind::Modal(modal_voice) => modal_voice.set_energy_factor(energy_factor), }}

  pub fn set_pedal_power(
    &mut self,
    pedal_power: f32,
  ) {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.set_pedal_power(pedal_power),
      VoiceKind::Modal(modal_voice) => modal_voice.set_pedal_power(pedal_power), }}

  /// The energy parameters the voice was born under.
  pub fn energy_params(
    &self,
  ) -> &EnergyParams {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.energy_params(),
      VoiceKind::Modal(modal_voice) => modal_voice.energy_params(), }}

  pub fn birth(
    &self,
  ) -> u64 {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.birth(),
      VoiceKind::Modal(modal_voice) => modal_voice.birth(), }}

  pub fn f0(
    &self,
  ) -> f32 {
    match self {
      VoiceKind::Loops(string_voice) => string_voice.f0(),
      VoiceKind::Modal(modal_voice) => modal_voice.f0(), }}

  /// The string voice, if the voice is of loops.
  pub fn as_loops(
    &self,
  ) -> Option<&StringVoice> {
    match self {
      VoiceKind::Loops(string_voice) => Some(string_voice),
      VoiceKind::Modal(_) => None, }}

  /// The modal voice, if the voice is modal.
  pub fn as_modal(
    &self,
  ) -> Option<&ModalVoice> {
    match self {
      VoiceKind::Loops(_) => None,
      VoiceKind::Modal(modal_voice) => Some(modal_voice), }}}

impl StringVoice {
  /// The voice `voice_id` at `f0` Hz born at engine sample `birth`,
  /// with `string.unison_count` silent loops and the two tables to
  /// stream in. Pass empty tables for a voice that rings only by
  /// sympathy.
  pub fn new(
    params: &StringsParams,
    voice_id: VoiceId,
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
      excitation: ExcitationStream::new(dry_table, knock_table),
      gain: 0.0,
      energy: params.energy,
      drive_state: DriveState::new(&params.energy, voice_id, f0, sample_rate),
      energy_factor: 0.0,
      pedal_power: 0.0, }}

  /// Adds the pre-charge into every loop: one delay line's worth of
  /// the waveshape of the voice's energy kind at the loop's own pitch
  /// and through its own loss filter, its peak `amplitude /
  /// unison_count` (`energy::precharge_waveshape`), under the pedal's
  /// power `pedal_power_at_birth`. Call it at birth, before the first
  /// sample.
  pub fn precharge(
    &mut self,
    amplitude: f32,
    pedal_power_at_birth: f32,
    sample_rate: f32,
  ) {
    let peak_per_loop: f32 = amplitude / self.loops.len() as f32;
    for string_loop in self.loops.iter_mut() {
      let waveshape: Vec<f32> = precharge_waveshape(
        &self.energy, pedal_power_at_birth, &string_loop.design().loss, string_loop.f0(),
        sample_rate, string_loop.design().delay_samples, peak_per_loop);
      string_loop.add_table(&waveshape); }}

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
  /// unison_count + drive + howl_feedback` into its delay line and
  /// advances the excitation one sample. `arriving_waves` are this
  /// voice's waves from `push_arriving_waves`, in the same order; the
  /// one drive sample comes from their mean (`drive_of_the_voice`).
  pub fn write_back(
    &mut self,
    arriving_waves: &[f32],
    bridge_velocity: f32,
    howl_feedback: f32,
  ) {
    let excitation_per_loop: f32 = self.excitation.next_sample() / self.loops.len() as f32;
    let drive: f32 = self.drive_of_the_voice(arriving_waves);
    for (string_loop, arriving) in self.loops.iter_mut().zip(arriving_waves.iter()) {
      string_loop.write(
        arriving - bridge_velocity + excitation_per_loop + drive + howl_feedback); }}

  /// The one drive sample every loop of the voice takes this sample:
  /// `energy::drive` at the voice's pitch, from the mean of its loops'
  /// round-trip gains and of their `arriving_waves`, and the voice's
  /// drive state. 0 outside the window, with nothing computed.
  pub fn drive_of_the_voice(
    &mut self,
    arriving_waves: &[f32],
  ) -> f32 {
    if self.energy_factor <= 0.0 || self.pedal_power <= 0.0 {
      return 0.0; }
    let loop_count: f32 = self.loops.len() as f32;
    let mean_arriving: f32 = arriving_waves.iter().sum::<f32>() / loop_count;
    let mean_round_trip_gain: f32 = self.loops.iter()
      .map(|string_loop| string_loop.round_trip_gain())
      .sum::<f32>() / loop_count;
    drive(&self.energy, &mut self.drive_state, self.energy_factor, self.pedal_power, self.f0,
          mean_round_trip_gain, mean_arriving) }

  /// The sum of every loop's stored energy (`Loop::energy`).
  pub fn energy(
    &self,
  ) -> f32 {
    self.loops.iter().map(|string_loop| string_loop.energy()).sum() }

  /// Whether both tables have streamed in entirely.
  pub fn excitation_finished(
    &self,
  ) -> bool {
    self.excitation.finished() }

  pub fn set_gain(
    &mut self,
    gain: f32,
  ) {
    self.gain = gain; }

  pub fn gain(
    &self,
  ) -> f32 {
    self.gain }

  pub fn set_energy_factor(
    &mut self,
    energy_factor: f32,
  ) {
    self.energy_factor = energy_factor; }

  pub fn set_pedal_power(
    &mut self,
    pedal_power: f32,
  ) {
    self.pedal_power = pedal_power; }

  /// The energy parameters the voice was born under.
  pub fn energy_params(
    &self,
  ) -> &EnergyParams {
    &self.energy }

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
    &self.loops }}

impl ExcitationStream {
  pub fn new(
    dry_table: Vec<f32>,
    knock_table: Arc<[f32]>,
  ) -> ExcitationStream {
    ExcitationStream { dry_table, knock_table, position: 0 }}

  /// This sample of the dry table plus this sample of the knock (0
  /// past either's end), and the read position moved on.
  pub fn next_sample(
    &mut self,
  ) -> f32 {
    if self.finished() {
      return 0.0; }
    let position: usize = self.position;
    self.position += 1;
    self.dry_table.get(position).copied().unwrap_or(0.0)
      + self.knock_table.get(position).copied().unwrap_or(0.0) }

  /// Whether both tables have streamed in entirely.
  pub fn finished(
    &self,
  ) -> bool {
    self.position >= self.dry_table.len().max(self.knock_table.len()) }

  /// Adds `table` into the dry table from its first sample on,
  /// lengthening it if `table` is longer. Call it before the first
  /// sample streams.
  pub fn add_into_dry_table(
    &mut self,
    table: &[f32],
  ) {
    if self.dry_table.len() < table.len() {
      self.dry_table.resize(table.len(), 0.0); }
    for (dry, value) in self.dry_table.iter_mut().zip(table.iter()) {
      *dry += value; }}}

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
