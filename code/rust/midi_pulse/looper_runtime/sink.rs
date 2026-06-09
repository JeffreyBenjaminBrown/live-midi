//! The looper's note sink: a pitch-keyed voice driver over the (unchanged)
//! sawwave engine.
//!
//! The sawwave voice map is keyed by `VoiceSource` and `render_block` ignores the
//! key entirely, so the looper owns its *own* `VoiceMap` and reuses the engine's
//! pure pieces (`render_block_with_amplitude`, `freq_for_pitch`, `VoiceState`)
//! without touching any sawwave file.
//!
//! Live edo-grid play and each loop slot are distinct *sources*. A pitch wanted
//! by several sources at once is a single voice, released only when the last
//! source lets go -- the `OutputGate` ref-count pattern (record.rs), here
//! pitch-keyed for the audio synth instead of MIDI-note-keyed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::pitch::freq_for_pitch;
use crate::types::{Timbre, VoiceId, VoiceMap, VoiceSource, VoiceState};

/// Who is asking for a pitch to sound. `Live` carries the edo cell so that two
/// distinct held cells whose steps collide to the same absolute pitch are two
/// independent ref-holders (releasing one must not silence the other).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteSource {
  /// Live playing on the edo grid, keyed by the held cell.
  Live(i32, i32),
  /// Playback of loop slot `n`.
  Slot(usize),
}

pub struct SawNoteSink {
  voices: Arc<Mutex<VoiceMap>>,
  /// pitch (absolute EDO step) -> the sources currently holding it down.
  refs: HashMap<i32, HashSet<NoteSource>>,
  next_id: VoiceId,
  fund: f64,
  edo: i32,
  sample_rate: f32,
  attack_secs: f32,
  release_secs: f32,
}

impl SawNoteSink {
  pub fn new(
    voices: Arc<Mutex<VoiceMap>>,
    fund: f64,
    edo: i32,
    sample_rate: f32,
    attack_secs: f32,
    release_secs: f32,
  ) -> Self {
    SawNoteSink {
      voices,
      refs: HashMap::new(),
      next_id: 0,
      fund,
      edo,
      sample_rate,
      attack_secs,
      release_secs,
    }
  }

  /// `source` wants `pitch` (an absolute EDO step) sounding with `timbre`. Spawns a
  /// voice (stamped with `timbre`, its AM/FM LFOs retriggered at phase 0) only if
  /// no source held this pitch yet. If another source already holds the pitch this
  /// only ref-counts -- the first holder's timbre wins for the shared voice.
  pub fn note_on(&mut self, pitch: i32, source: NoteSource, timbre: Timbre) {
    let sources = self.refs.entry(pitch).or_default();
    let was_empty = sources.is_empty();
    sources.insert(source);
    if !was_empty {
      return; // already sounding for another source; ref-count only.
    }
    let id = self.next_id;
    self.next_id += 1;
    // Recover from poisoning: a panicked thread must not permanently break audio.
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    voices.insert(
      voice_key(pitch),
      VoiceState {
        id,
        freq: freq_for_pitch(pitch, self.fund, self.edo),
        phase: 0.0,
        env: 0.0,
        target_env: 1.0,
        ramp_per_sample: 1.0 / (self.attack_secs * self.sample_rate),
        timbre,
        am_phase: 0.0,
        fm_phase: 0.0,
      },
    );
  }

  /// `source` releases `pitch`. The voice rings out (ramps to zero) only once the
  /// last source has let go.
  pub fn note_off(&mut self, pitch: i32, source: NoteSource) {
    let Some(sources) = self.refs.get_mut(&pitch) else {
      return;
    };
    sources.remove(&source);
    if !sources.is_empty() {
      return; // still held by another source.
    }
    self.refs.remove(&pitch);
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = voices.get_mut(&voice_key(pitch)) {
      state.target_env = 0.0;
      // Ramp the remaining envelope down over release_secs (matches the engine's
      // own release in ramp_chord_accretion_to_zero). If env is already 0 the
      // voice is dropped on the next render block.
      state.ramp_per_sample = state.env / (self.release_secs * self.sample_rate);
    }
  }

  /// How many voices are currently in the shared map (live + playback).
  pub fn voice_count(&self) -> usize {
    self.voices.lock().unwrap_or_else(|e| e.into_inner()).len()
  }

  /// The pitches `source` is currently holding down (its individual notes that
  /// are sounding right now). Used by the edo grid to light a sounding loop's
  /// notes one at a time, in time with playback.
  pub fn pitches_held_by(&self, source: NoteSource) -> Vec<i32> {
    self
      .refs
      .iter()
      .filter_map(|(pitch, sources)| sources.contains(&source).then_some(*pitch))
      .collect()
  }

  /// Release everything a source holds (e.g. when a loop stops sounding).
  pub fn release_source(&mut self, source: NoteSource) {
    let held: Vec<i32> = self
      .refs
      .iter()
      .filter_map(|(pitch, sources)| sources.contains(&source).then_some(*pitch))
      .collect();
    for pitch in held {
      self.note_off(pitch, source);
    }
  }
}

/// The looper owns its own `VoiceMap`, so we reuse the `Accreted` key purely as an
/// opaque per-pitch handle (the `chord` field is unused). `render_block` never
/// reads the key, so this never collides with anything.
fn voice_key(pitch: i32) -> VoiceSource {
  VoiceSource::Accreted { chord: 0, pitch }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sink() -> SawNoteSink {
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    SawNoteSink::new(Arc::clone(&voices), 220.0, 58, 48000.0, 0.003, 0.05)
  }

  fn voice_count(s: &SawNoteSink) -> usize {
    s.voices.lock().unwrap().len()
  }

  fn target_env(s: &SawNoteSink, pitch: i32) -> Option<f32> {
    s.voices.lock().unwrap().get(&voice_key(pitch)).map(|v| v.target_env)
  }

  #[test]
  fn note_on_spawns_one_voice_at_full_target() {
    let mut s = sink();
    s.note_on(30, NoteSource::Live(0, 0), Timbre::default());
    assert_eq!(voice_count(&s), 1);
    assert_eq!(target_env(&s, 30), Some(1.0));
  }

  #[test]
  fn two_sources_on_same_pitch_share_one_voice() {
    let mut s = sink();
    s.note_on(30, NoteSource::Live(0, 0), Timbre::default());
    s.note_on(30, NoteSource::Slot(2), Timbre::default());
    assert_eq!(voice_count(&s), 1, "same pitch must be one ref-counted voice");
    // First source releases: still held by the slot, so not yet ramping down.
    s.note_off(30, NoteSource::Live(0, 0));
    assert_eq!(target_env(&s, 30), Some(1.0), "still held by Slot(2)");
    // Last source releases: now it rings out.
    s.note_off(30, NoteSource::Slot(2));
    assert_eq!(target_env(&s, 30), Some(0.0), "last source gone -> release");
  }

  #[test]
  fn distinct_pitches_get_distinct_voices() {
    let mut s = sink();
    s.note_on(30, NoteSource::Live(0, 0), Timbre::default());
    s.note_on(35, NoteSource::Live(0, 0), Timbre::default());
    assert_eq!(voice_count(&s), 2);
  }

  #[test]
  fn note_off_unknown_pitch_is_a_noop() {
    let mut s = sink();
    s.note_off(99, NoteSource::Live(0, 0)); // must not panic
    assert_eq!(voice_count(&s), 0);
  }

  #[test]
  fn release_source_releases_all_that_sources_pitches_only() {
    let mut s = sink();
    s.note_on(30, NoteSource::Slot(0), Timbre::default());
    s.note_on(35, NoteSource::Slot(0), Timbre::default());
    s.note_on(40, NoteSource::Live(0, 0), Timbre::default());
    s.release_source(NoteSource::Slot(0));
    assert_eq!(target_env(&s, 30), Some(0.0));
    assert_eq!(target_env(&s, 35), Some(0.0));
    assert_eq!(target_env(&s, 40), Some(1.0), "Live's pitch is untouched");
  }

  #[test]
  fn shared_pitch_survives_release_source_of_one_holder() {
    let mut s = sink();
    s.note_on(30, NoteSource::Live(0, 0), Timbre::default());
    s.note_on(30, NoteSource::Slot(1), Timbre::default());
    s.release_source(NoteSource::Slot(1));
    assert_eq!(target_env(&s, 30), Some(1.0), "Live still holds 30");
  }

  #[test]
  fn two_distinct_cells_on_one_pitch_dont_collapse() {
    // The 58-8-1 collision: cells (0,8) and (1,0) both sound step 8. Each is a
    // distinct Live source, so releasing one must not silence the other.
    let mut s = sink();
    s.note_on(8, NoteSource::Live(0, 8), Timbre::default());
    s.note_on(8, NoteSource::Live(1, 0), Timbre::default());
    assert_eq!(voice_count(&s), 1);
    s.note_off(8, NoteSource::Live(0, 8));
    assert_eq!(target_env(&s, 8), Some(1.0), "still held by the other cell");
    s.note_off(8, NoteSource::Live(1, 0));
    assert_eq!(target_env(&s, 8), Some(0.0), "last cell released");
  }

  #[test]
  fn note_on_stamps_the_timbre_on_the_spawned_voice() {
    use crate::types::Waveform;
    let mut s = sink();
    let t = Timbre { waveform: Waveform::Saw, gain: 0.5, ..Timbre::default() };
    s.note_on(30, NoteSource::Live(0, 0), t);
    let voices = s.voices.lock().unwrap();
    let v = voices.get(&voice_key(30)).unwrap();
    assert_eq!(v.timbre.waveform, Waveform::Saw);
    assert_eq!(v.timbre.gain, 0.5);
    assert_eq!(v.am_phase, 0.0, "LFO retriggered at note-on");
  }

  #[test]
  fn first_timbre_wins_on_same_pitch_collision() {
    use crate::types::Waveform;
    let mut s = sink();
    s.note_on(30, NoteSource::Live(0, 0), Timbre { waveform: Waveform::Saw, ..Timbre::default() });
    // Second source, different timbre, but the pitch already sounds -> ref-count
    // only, so the first holder's timbre stands for the shared voice.
    s.note_on(30, NoteSource::Slot(1), Timbre { waveform: Waveform::Sine, ..Timbre::default() });
    let voices = s.voices.lock().unwrap();
    assert_eq!(voices.get(&voice_key(30)).unwrap().timbre.waveform, Waveform::Saw);
  }
}
