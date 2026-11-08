//! Turning live events and playback actions into voices of the sawwave
//! engine's shared voice map.
//!
//! A voice takes its rig's envelope and its timbre slot at its start. Its
//! gain has two parts: the rig's master amplitude (`fader_gain`, fixed) and
//! its volume in dB (`grid_gain`, which the engine slews toward
//! `grid_gain_target`, so a volume change mid-note glides instead of
//! clicking). A timbre change mid-note crossfades, as in the surfaces runtime.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::rig::load_named_rig;
use crate::types::{TimbreXfade, VoiceMap, VoiceSource, VoiceState};
use crate::voices::pluck_envelope;

use super::live_grid::LiveEvent;
use super::playback::PlaybackAction;
use super::rig_layout::{editor_rig, RigSound};
use super::track::Settings;

/// How long a timbre change mid-note crossfades.
const TIMBRE_XFADE_SECS: f32 = 0.03;

pub fn db_to_gain(db: f32) -> f32 { 10f32.powf(db / 20.0) }

/// The rigs' sounds, loaded on first use. A rig that fails to load is
/// remembered as failed (with its error) so it is reported once, not per note.
#[derive(Default)]
pub struct RigSounds {
  loaded: HashMap<String, Result<RigSound, String>>,
}

impl RigSounds {
  pub fn get(
    &mut self,
    rig: &str,
  ) -> Result<&RigSound, &String> {
    self.loaded.entry(rig.to_string())
      .or_insert_with(|| load_rig_sound(rig))
      .as_ref() }

  /// Forget every loaded rig, so edited rig files are read again.
  pub fn reload(&mut self) { self.loaded.clear(); }

  /// The rigs that failed to load, with why.
  pub fn failures(&self) -> Vec<String> {
    self.loaded.iter()
      .filter_map(|(name, result)| result.as_ref().err().map(|e| format!("{name}: {e}")))
      .collect() }
}

fn load_rig_sound(rig: &str) -> Result<RigSound, String> {
  let loaded = load_named_rig(rig)?;
  let editor = editor_rig(rig, &loaded)?;
  Ok(editor.grids[0].sound.clone()) }

pub struct Sound {
  pub voices: Arc<Mutex<VoiceMap>>,
  pub sample_rate: f32,
  pub rigs: RigSounds,
  next_id: u64,
  /// Each started voice's rig's release time, used when it ends.
  releases: HashMap<VoiceSource, f32>,
}

impl Sound {
  pub fn new(
    voices: Arc<Mutex<VoiceMap>>,
    sample_rate: f32,
  ) -> Sound {
    Sound {
      voices, sample_rate, rigs: RigSounds::default(), next_id: 0, releases: HashMap::new() } }

  /// Sound a live event. `settings` are the grid's settings after the event.
  pub fn live(
    &mut self,
    event: &LiveEvent,
    settings: &Settings,
    hz: f64,
  ) {
    match event {
      LiveEvent::NoteOn { grid, cell, .. } =>
        self.start(VoiceSource::EditorFinger { grid: *grid, cell: *cell }, hz, settings),
      LiveEvent::NoteOff { grid, cell } =>
        self.end(VoiceSource::EditorFinger { grid: *grid, cell: *cell }),
      LiveEvent::Parameter { grid, .. } => {
        let grid: usize = *grid;
        self.retone(|source| matches!(source,
          VoiceSource::EditorFinger { grid: g, .. } if *g == grid), settings); } } }

  pub fn playback(
    &mut self,
    action: &PlaybackAction,
  ) {
    match action {
      PlaybackAction::NoteStart { track, seq, hz, settings } =>
        self.start(VoiceSource::EditorTrackNote { track: *track, seq: *seq }, *hz, settings),
      PlaybackAction::NoteEnd { track, seq } =>
        self.end(VoiceSource::EditorTrackNote { track: *track, seq: *seq }),
      PlaybackAction::SettingsChanged { track, settings } => {
        let track: u64 = *track;
        self.retone(|source| matches!(source,
          VoiceSource::EditorTrackNote { track: t, .. } if *t == track), settings); } } }

  fn start(
    &mut self,
    source: VoiceSource,
    hz: f64,
    settings: &Settings,
  ) {
    let sample_rate: f32 = self.sample_rate;
    let Some(rig) = settings.rig.as_deref() else { return; };
    let Ok(sound) = self.rigs.get(rig) else { return; };
    let envelope = sound.envelope;
    let timbre = sound.timbres[settings.timbre.unwrap_or(0).min(sound.timbres.len() - 1)];
    let gain: f32 = db_to_gain(settings.volume_db.unwrap_or(0.0));
    let (sustain_env, decay_per_sample) =
      pluck_envelope(envelope.sustain_level, envelope.decay_secs, 1.0, sample_rate);
    let id: u64 = self.next_id;
    self.next_id += 1;
    self.releases.insert(source, envelope.release_secs);
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    voices.insert(source, VoiceState {
      id,
      freq: hz as f32,
      phase: 0.0,
      env: 0.0,
      target_env: 1.0,
      ramp_per_sample: 1.0 / (envelope.attack_secs * sample_rate).max(1.0),
      pending_attack: None,
      sustain_env,
      decay_per_sample,
      freq_target: 0.0,
      glide_per_sample: 1.0,
      factored_pulse_freq: 0.0,
      factored_pulse_phase: 0.0,
      fader_gain: envelope.amplitude,
      grid_gain: gain,
      grid_gain_target: gain,
      pedal_scale: 1.0,
      new_pedal_gain: 1.0,
      global_pedal_gain: 1.0,
      global_pedal_gain_target: 1.0,
      slide_freq_target: 0.0,
      timbre,
      am_phase: 0.0,
      fm_phase: 0.0,
      rel_am_phase: 0.0,
      rel_fm_phase: 0.0,
      timbre_xfade: None, }); }

  fn end(
    &mut self,
    source: VoiceSource,
  ) {
    let release_secs: f32 = self.releases.remove(&source).unwrap_or(0.05);
    self.release(source, release_secs); }

  fn release(
    &mut self,
    source: VoiceSource,
    release_secs: f32,
  ) {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = voices.get_mut(&source) {
      state.target_env = 0.0;
      state.ramp_per_sample = state.env / (release_secs * self.sample_rate).max(1.0);
      state.pending_attack = None; } }

  /// Give every still-sounding voice that `selects` the timbre and volume of
  /// `settings`.
  fn retone(
    &mut self,
    selects: impl Fn(&VoiceSource) -> bool,
    settings: &Settings,
  ) {
    let sample_rate: f32 = self.sample_rate;
    let Some(rig) = settings.rig.as_deref() else { return; };
    let Ok(sound) = self.rigs.get(rig) else { return; };
    let timbre = sound.timbres[settings.timbre.unwrap_or(0).min(sound.timbres.len() - 1)];
    let gain: f32 = db_to_gain(settings.volume_db.unwrap_or(0.0));
    let step: f32 = 1.0 / (TIMBRE_XFADE_SECS * sample_rate).max(1.0);
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    for (source, state) in voices.iter_mut() {
      if !selects(source) || state.target_env == 0.0 { continue; }
      state.grid_gain_target = gain;
      if state.timbre != timbre {
        state.timbre_xfade = Some(TimbreXfade { from: state.timbre, progress: 0.0, step });
        state.timbre = timbre; } } }

  /// Release every voice the editor started.
  pub fn silence(&mut self) {
    self.releases.clear();
    let sample_rate: f32 = self.sample_rate;
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    for state in voices.values_mut() {
      state.target_env = 0.0;
      state.ramp_per_sample = state.env / (0.05 * sample_rate).max(1.0); } }
}
