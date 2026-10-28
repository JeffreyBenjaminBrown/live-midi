//! The strings engine's block render over the shared voice map.
//!
//! The renderer keeps one string voice per `VoiceState.id` in the
//! map (keyed by id, so a sustain, which moves a `VoiceState` to
//! another key without changing its id, does not restart it). Per
//! block it first adopts changed parameters, then syncs with the
//! map: an id it has not seen spawns a string voice tuned to `freq`
//! and struck by the exciter; an id that left the map drops its
//! string voice. Per sample it steps every voice's control state
//! (`voices::step_voice_controls`, as the sawwave engine does, and
//! drops from the map a voice that has finished), retunes a voice
//! whose `freq` moved, reads every loop's arriving wave, runs the
//! junction, writes every loop back, sums each voice's loops times
//! its gain coefficient and the sink's amplitude, runs the sum
//! through the body, clamps it to +-0.95 and writes it to every
//! channel of the frame. After the block it reaps silent strings.
//!
//! A string is silent when its voice has been in the map for over
//! `REAP_AFTER_SECONDS`, its excitation has streamed in, and its
//! loops' energy is below `SILENT_STRING_ENERGY`. Its string voice
//! is dropped (the map entry stays until the runtime removes it) and
//! its id is remembered as reaped, so it is not respawned while it
//! stays in the map: a hundred silent drones then cost no delay
//! lines.
//!
//! Nothing allocates per sample: the scratch buffers are sized at
//! block rate, and the map is filtered in place.

use crate::strings::body::BodyBank;
use crate::strings::exciter::{dry_table, knock_table};
use crate::strings::junction::Junction;
use crate::strings::params::StringsParams;
use crate::strings::voice::StringVoice;
use crate::types::{VoiceId, VoiceMap, VoiceState, VoiceStep};
use crate::voices::{step_voice_controls, SLIDE_SLEW_SECS};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A string voice younger than this, in seconds, is never reaped.
pub const REAP_AFTER_SECONDS: f32 = 1.0;

/// A string voice whose loops' energy (sum of squares of every delay
/// line) is below this is silent.
pub const SILENT_STRING_ENERGY: f32 = 1e-10;

/// The output's clamp, as the sawwave engine's.
pub const OUTPUT_CLAMP: f32 = 0.95;

pub struct StringsRenderer {
  params: StringsParams,
  sample_rate: f32,
  junction: Junction,
  body: BodyBank,
  /// The knock every voice born under `params` streams in.
  knock_table: Arc<[f32]>,
  /// Ordered by id, so the junction sums the loops in the same order
  /// on every run.
  string_voices: BTreeMap<VoiceId, StringVoice>,
  /// Ids reaped as silent and still in the map.
  reaped_voice_ids: Vec<VoiceId>,
  /// Scratch: the ids in the map this block, sorted.
  voice_ids_in_map: Vec<VoiceId>,
  /// Scratch: every loop's arriving wave this sample, voice by voice
  /// in id order, each voice's loops in order.
  arriving_waves: Vec<f32>,
  /// Engine time: samples rendered since construction.
  sample_clock: u64,
  /// The pedal-slide pitch smoother's time constant handed to
  /// `step_voice_controls`, as `BlockRenderer::slide_slew_secs`.
  pub slide_slew_secs: f32, }

impl StringsRenderer {
  pub fn new(
    params: StringsParams,
    sample_rate: f32,
  ) -> StringsRenderer {
    StringsRenderer {
      params,
      sample_rate,
      junction: Junction::new(&params.bridge, sample_rate),
      body: BodyBank::new(&params.body, sample_rate),
      knock_table: Arc::from(knock_table(&params.exciter, &params.body, sample_rate)),
      string_voices: BTreeMap::new(),
      reaped_voice_ids: Vec::new(),
      voice_ids_in_map: Vec::new(),
      arriving_waves: Vec::new(),
      sample_clock: 0,
      slide_slew_secs: SLIDE_SLEW_SECS, }}

  /// Renders `data` (interleaved, `channels` per frame) from
  /// `voices`, as the module comment describes. `params` are the
  /// live parameters; when they differ from the ones last used they
  /// are adopted first. A changed `sample_rate` starts the engine
  /// over (every voice in the map is struck again).
  #[allow(clippy::too_many_arguments)]
  pub fn render_block(
    &mut self,
    voices: &mut VoiceMap,
    data: &mut [f32],
    channels: usize,
    sample_rate: f32,
    amplitude: f32,
    params: &StringsParams,
  ) {
    if sample_rate != self.sample_rate {
      let slide_slew_secs: f32 = self.slide_slew_secs;
      *self = StringsRenderer::new(*params, sample_rate);
      self.slide_slew_secs = slide_slew_secs;
    } else if *params != self.params {
      self.adopt_params(params); }
    self.sync_with_voice_map(voices);
    for frame in data.chunks_mut(channels.max(1)) {
      let sample: f32 = self.render_sample(voices, amplitude);
      for out in frame.iter_mut() {
        *out = sample; }}
    self.reap_silent_strings(); }

  /// How many string voices the engine holds.
  pub fn voice_count(
    &self,
  ) -> usize {
    self.string_voices.len() }

  /// The parameters the engine last rendered with.
  pub fn params(
    &self,
  ) -> &StringsParams {
    &self.params }

  /// The string voice of `id`, if the engine holds one.
  pub fn string_voice(
    &self,
    id: VoiceId,
  ) -> Option<&StringVoice> {
    self.string_voices.get(&id) }

  /// Applies changed parameters. Loss, dispersion and unison detune
  /// changes retune every sounding loop, bridge changes reconfigure
  /// the junction, and body changes rebuild the bank (its resonators
  /// start from rest). Exciter and body changes rebuild the knock,
  /// which notes born afterward stream; a sounding note finishes the
  /// tables it was born with.
  fn adopt_params(
    &mut self,
    params: &StringsParams,
  ) {
    let previous: StringsParams = self.params;
    self.params = *params;
    if params.string != previous.string {
      for string_voice in self.string_voices.values_mut() {
        let f0: f32 = string_voice.f0();
        string_voice.retune(&params.string, f0); }}
    if params.bridge != previous.bridge {
      self.junction.reconfigure(&params.bridge, self.sample_rate); }
    if params.body != previous.body {
      self.body = BodyBank::new(&params.body, self.sample_rate); }
    if params.exciter != previous.exciter || params.body != previous.body {
      self.knock_table =
        Arc::from(knock_table(&params.exciter, &params.body, self.sample_rate)); }}

  /// Drops the string voices and reaped ids whose voice left the map,
  /// spawns a struck string voice for every id new to the engine,
  /// and sizes the scratch for this block's loops.
  fn sync_with_voice_map(
    &mut self,
    voices: &VoiceMap,
  ) {
    self.voice_ids_in_map.clear();
    self.voice_ids_in_map.extend(voices.values().map(|voice| voice.id));
    self.voice_ids_in_map.sort_unstable();
    { // Forget what left the map.
      let ids_in_map: &[VoiceId] = &self.voice_ids_in_map;
      self.string_voices.retain(|id, _| ids_in_map.binary_search(id).is_ok());
      self.reaped_voice_ids.retain(|id| ids_in_map.binary_search(id).is_ok()); }
    for voice in voices.values() {
      let known: bool = self.string_voices.contains_key(&voice.id)
        || self.reaped_voice_ids.contains(&voice.id);
      if !known && voice.freq.is_finite() && voice.freq > 0.0 {
        let string_voice: StringVoice = self.spawn_struck(voice);
        self.string_voices.insert(voice.id, string_voice); }}
    let loop_count: usize =
      self.string_voices.values().map(|string_voice| string_voice.loop_count()).sum();
    self.arriving_waves.reserve(loop_count.saturating_sub(self.arriving_waves.len())); }

  /// A string voice for `voice`, born now and struck: its own dry
  /// table at `voice.freq` and the shared knock.
  fn spawn_struck(
    &self,
    voice: &VoiceState,
  ) -> StringVoice {
    StringVoice::new(
      &self.params,
      voice.freq,
      self.sample_rate,
      self.sample_clock,
      dry_table(&self.params.exciter, voice.freq, self.sample_rate),
      Arc::clone(&self.knock_table)) }

  /// One output sample: control steps, the junction, the voices'
  /// sum, the body and the clamp.
  fn render_sample(
    &mut self,
    voices: &mut VoiceMap,
    amplitude: f32,
  ) -> f32 {
    self.step_controls_of_every_voice(voices);
    let strings_sum: f32 = couple_through_junction(
      &mut self.string_voices, &mut self.junction, &mut self.arriving_waves);
    self.sample_clock += 1;
    self.body.process(strings_sum * amplitude).clamp(-OUTPUT_CLAMP, OUTPUT_CLAMP) }

  /// Steps every voice's control state one sample. A finished voice
  /// leaves the map and drops its string voice; a living one hands
  /// its string voice this sample's gain and retunes it if its
  /// `freq` moved.
  fn step_controls_of_every_voice(
    &mut self,
    voices: &mut VoiceMap,
  ) {
    let sample_rate: f32 = self.sample_rate;
    let slide_slew_secs: f32 = self.slide_slew_secs;
    let string_voices: &mut BTreeMap<VoiceId, StringVoice> = &mut self.string_voices;
    let params: &StringsParams = &self.params;
    voices.retain(|_, voice| {
      let step: VoiceStep = step_voice_controls(voice, 1.0, sample_rate, slide_slew_secs);
      if !step.alive {
        string_voices.remove(&voice.id);
        return false; }
      if let Some(string_voice) = string_voices.get_mut(&voice.id) {
        string_voice.set_gain(step.gain * step.factored_pulse);
        string_voice.retune_if_moved(&params.string, voice.freq); }
      true }); }

  /// Drops the string voices that are silent (see the module
  /// comment) and remembers their ids as reaped.
  fn reap_silent_strings(
    &mut self,
  ) {
    let sample_clock: u64 = self.sample_clock;
    let youngest_reapable_age: u64 = (REAP_AFTER_SECONDS * self.sample_rate) as u64;
    let reaped_voice_ids: &mut Vec<VoiceId> = &mut self.reaped_voice_ids;
    self.string_voices.retain(|id, string_voice| {
      // An energy window, once the energy pedal exists, will keep a
      // string from being reaped while it is open; none is open yet.
      let silent: bool = sample_clock - string_voice.birth() > youngest_reapable_age
        && string_voice.excitation_finished()
        && string_voice.energy() < SILENT_STRING_ENERGY;
      if silent {
        reaped_voice_ids.push(*id); }
      !silent }); }}

/// One sample of the junction over every loop of `string_voices`:
/// reads each loop's arriving wave into `arriving_waves` (cleared
/// first; its capacity should hold every loop), takes the bridge
/// velocity of their sum, writes every loop back, and returns the
/// sum over voices of each voice's arriving waves times its gain.
pub fn couple_through_junction(
  string_voices: &mut BTreeMap<VoiceId, StringVoice>,
  junction: &mut Junction,
  arriving_waves: &mut Vec<f32>,
) -> f32 {
  arriving_waves.clear();
  let sum_of_arriving_waves: f32 = string_voices
    .values_mut()
    .map(|string_voice| string_voice.push_arriving_waves(arriving_waves))
    .sum();
  let bridge_velocity: f32 =
    junction.bridge_velocity(sum_of_arriving_waves, arriving_waves.len());
  let mut voices_sum: f32 = 0.0;
  let mut first_loop: usize = 0;
  for string_voice in string_voices.values_mut() {
    let voice_waves: &[f32] = &arriving_waves[first_loop .. first_loop + string_voice.loop_count()];
    string_voice.write_back(voice_waves, bridge_velocity);
    voices_sum += voice_waves.iter().sum::<f32>() * string_voice.gain();
    first_loop += voice_waves.len(); }
  voices_sum }
