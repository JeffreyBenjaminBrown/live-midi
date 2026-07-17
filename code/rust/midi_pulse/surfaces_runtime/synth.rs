//! The surfaces note sink: a *(grid, cell)*-keyed voice driver over the (unchanged)
//! sawwave engine.
//!
//! The looper's sink keys voices by *pitch* (ref-counting several sources onto one
//! shared voice). Surfaces needs the opposite: the same pitch fingered on both grids
//! must be *two independent voices* (each in that grid's own waveform), and releasing
//! one must never gate the other -- and even two cells on one grid that collide to the
//! same absolute pitch stay independent (one voice per finger). So we key by
//! `(grid, cell)`, with no ref-counting: each press is its own voice.
//!
//! We reuse the engine's pure pieces (`freq_for_pitch`, `VoiceState`, the block
//! renderer) without touching any sawwave file, exactly as the looper's sink does --
//! and, like it, we reuse the `Accreted` `VoiceSource` variant purely as an opaque
//! per-voice handle (`render_block` never reads the key), here packing the grid index
//! and the cell into it.

use std::sync::{Arc, Mutex};

use crate::pitch::freq_for_pitch;
use crate::types::{Timbre, VoiceId, VoiceMap, VoiceSource, VoiceState};
use crate::voices::pluck_envelope;

/// The `chord` offset that marks a voice as *sustained* (accreted) rather than
/// fingered: a sustained voice from grid `g` is keyed `Accreted { chord: SUSTAIN_BASE
/// + g, .. }`. Far above any real grid index, so finger and sustain keys never
/// collide in the shared map.
pub const SUSTAIN_BASE: usize = 0x100;

/// The `chord` offset for *retired* voices: drones cut by a retrigger of their own
/// pitch (`cut_sustained`). A retired voice only rings out its release ramp; moving
/// it off the sustain key frees that key for the replacing note immediately, so the
/// pitch can re-drone before the old tail has died. Far above `SUSTAIN_BASE + grid`.
const RETIRED_BASE: usize = 0x10000;

/// Multiply the per-voice gain of every voice belonging to `grid` by `ratio`, in
/// place. Drives the *live* volume control: a volume strip sets the loudness of
/// whatever grid it `controls` (its own, in the current rigs), and moving it must
/// change notes that are already sounding (a fader, not a radio) -- so we walk the
/// shared map and rescale that grid's voices by new_fader / old_fader. Ratio (not
/// assignment) because a voice's gain also carries its timbre slot's `amplitude`,
/// which must survive fader moves. Future note-ons pick up the new fader from the
/// shared per-grid state; this only touches the ones already in flight. Sustained
/// (accreted) voices keep following the fader of the grid they came from -- a drone
/// you can only kill with `clear` should at least obey a volume pedal.
pub fn rescale_grid_gain(voices: &Arc<Mutex<VoiceMap>>, grid: usize, ratio: f32) {
  if !ratio.is_finite() {
    return; // an old fader gain of 0 never happens (the log fader bottoms at -30 dB)
  }
  let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
  for (src, state) in voices.iter_mut() {
    if let VoiceSource::Accreted { chord, .. } = src {
      if *chord == grid || *chord == SUSTAIN_BASE + grid {
        state.timbre.gain *= ratio;
      }
    }
  }
}

/// Pack a grid index + cell into an opaque `VoiceMap` key. Distinct grids get
/// distinct `chord` values, so the same cell on two grids never collides; distinct
/// cells on one grid pack to distinct `pitch` fields. Cells are 0..grid_w/h (<= 16),
/// so `y * 256 + x` is collision-free for any real grid.
fn voice_key(grid: usize, cell: (i32, i32)) -> VoiceSource {
  VoiceSource::Accreted { chord: grid, pitch: cell.1 * 256 + cell.0 }
}

/// The key of a *sustained* voice: per source grid and absolute pitch (the accrete
/// set is keyed the same way), disjoint from every finger key via `SUSTAIN_BASE`.
fn sustain_key(grid: usize, pitch: i32) -> VoiceSource {
  VoiceSource::Accreted { chord: SUSTAIN_BASE + grid, pitch }
}

/// One grid's voice driver into a *shared* `VoiceMap` (both grids + the audio stream
/// share the map; the render sums all voices). Each grid thread owns its own
/// `SurfaceSink` and only ever touches keys carrying its own grid index, so the two
/// grids never interfere even though they write the same map.
pub struct SurfaceSink {
  grid: usize,
  voices: Arc<Mutex<VoiceMap>>,
  next_id: VoiceId,
  fund: f64,
  edo: i32,
  sample_rate: f32,
  attack_secs: f32,
  release_secs: f32,
  /// The pluck envelope for every struck note (see `voices::pluck_envelope`).
  sustain_env: f32,
  decay_per_sample: f32,
}

impl SurfaceSink {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    grid: usize,
    voices: Arc<Mutex<VoiceMap>>,
    fund: f64,
    edo: i32,
    sample_rate: f32,
    attack_secs: f32,
    release_secs: f32,
    sustain_level: f32,
    decay_secs: f32,
  ) -> Self {
    let (sustain_env, decay_per_sample) = pluck_envelope(sustain_level, decay_secs, 1.0, sample_rate);
    SurfaceSink {
      grid,
      voices,
      next_id: 0,
      fund,
      edo,
      sample_rate,
      attack_secs,
      release_secs,
      sustain_env,
      decay_per_sample,
    }
  }

  /// Start `cell` sounding `pitch` (an absolute EDO step) with `timbre`. Spawns a
  /// fresh voice (its AM/FM LFOs retriggered at phase 0); overwrites any existing
  /// voice for the same cell (a retrigger, though the grid thread debounces
  /// repeats). `pulse_hz` is the polyrhythm pulse at this note's onset (None = no
  /// pulse), fixed for the voice's life.
  pub fn note_on(&mut self, cell: (i32, i32), pitch: i32, timbre: Timbre, pulse_hz: Option<f32>) {
    let id = self.next_id;
    self.next_id += 1;
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    voices.insert(
      voice_key(self.grid, cell),
      VoiceState {
        id,
        freq: freq_for_pitch(pitch, self.fund, self.edo),
        freq_target: 0.0,
        glide_per_sample: 1.0,
        tempo_am_freq: pulse_hz.unwrap_or(0.0),
        tempo_am_phase: 0.0,
        phase: 0.0,
        env: 0.0,
        target_env: 1.0,
        ramp_per_sample: 1.0 / (self.attack_secs * self.sample_rate),
        sustain_env: self.sustain_env,
        decay_per_sample: self.decay_per_sample,
        timbre,
        am_phase: 0.0,
        fm_phase: 0.0,
      },
    );
  }

  /// Adopt hot-reloaded tuning + pluck parameters (the 'r' reload). Future notes
  /// use them; sounding voices keep their struck frequency and envelope.
  pub fn retune(&mut self, fund: f64, edo: i32, sustain_level: f32, decay_secs: f32) {
    self.fund = fund;
    self.edo = edo;
    let (sustain_env, decay_per_sample) =
      pluck_envelope(sustain_level, decay_secs, 1.0, self.sample_rate);
    self.sustain_env = sustain_env;
    self.decay_per_sample = decay_per_sample;
  }

  /// `note_on`, but the voice STARTS at `from_pitch` and glides into `pitch` over
  /// `glide_secs` (the slide feature). The frequency walks multiplicatively, so the
  /// glide is pitch-linear; the engine snaps it onto the target and ends the glide
  /// (`VoiceState::glide_per_sample`). Same pitch = a plain `note_on`.
  #[allow(clippy::too_many_arguments)]
  pub fn note_on_gliding(
    &mut self,
    cell: (i32, i32),
    pitch: i32,
    from_pitch: i32,
    timbre: Timbre,
    glide_secs: f32,
    pulse_hz: Option<f32>,
  ) {
    if from_pitch == pitch {
      return self.note_on(cell, pitch, timbre, pulse_hz);
    }
    self.note_on(cell, pitch, timbre, pulse_hz);
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = voices.get_mut(&voice_key(self.grid, cell)) {
      let from = freq_for_pitch(from_pitch, self.fund, self.edo);
      let to = state.freq;
      let samples = (glide_secs * self.sample_rate).max(1.0);
      state.freq = from;
      state.freq_target = to;
      state.glide_per_sample = (to / from).powf(1.0 / samples);
    }
  }

  /// The mono+slide note-on: STEAL the voice still sounding at `from_cell` (the
  /// note mono is cutting) and glide it into `pitch` under the new cell's key. The
  /// envelope simply continues -- no attack and no pluck re-trigger (misc.org
  /// "slide when mono is on should not re-trigger the attack") -- and the voice
  /// keeps its timbre and gain (it IS the same voice). `pulse_hz` re-aims the
  /// polyrhythm pulse's rate at this onset, but the pulse phase continues (no
  /// amplitude step). Returns false if nothing sounds at `from_cell` (the caller
  /// falls back to a fresh strike).
  pub fn note_on_legato(
    &mut self,
    from_cell: (i32, i32),
    cell: (i32, i32),
    pitch: i32,
    glide_secs: f32,
    pulse_hz: Option<f32>,
  ) -> bool {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut state) = voices.remove(&voice_key(self.grid, from_cell)) else {
      return false;
    };
    let to = freq_for_pitch(pitch, self.fund, self.edo);
    let from = state.freq; // wherever it is NOW, mid-glide included
    if to == from {
      state.glide_per_sample = 1.0;
    } else {
      let samples = (glide_secs * self.sample_rate).max(1.0);
      state.freq_target = to;
      state.glide_per_sample = (to / from).powf(1.0 / samples);
    }
    state.tempo_am_freq = pulse_hz.unwrap_or(0.0);
    voices.insert(voice_key(self.grid, cell), state);
    true
  }

  /// Glide the voice at `cell` to `pitch` over `glide_secs`, leaving everything else
  /// about it alone -- envelope, timbre, gain, pulse rate and phase all continue. The
  /// per-voice pitch edit: the note keeps sounding and simply *moves*.
  ///
  /// Unlike `note_on_legato` this neither spawns nor re-keys a voice, so the note
  /// stays addressable at the cell it was struck on while its PITCH moves away from
  /// that cell's nominal pitch.
  ///
  /// Safe to call repeatedly, including mid-glide and in the opposite direction: the
  /// source is re-read from the voice's LIVE `freq`, so each call simply re-aims from
  /// wherever it currently is. That matters -- `glide_per_sample` encodes direction
  /// (the integrator's crossing test compares it against 1.0), so re-aiming backwards
  /// by patching `freq_target` alone would make the voice sail past its target and
  /// never stop. Dragging an edit-mode note back and forth does exactly that.
  ///
  /// Returns false if no voice is at `cell`.
  pub fn glide_voice_to(&mut self, cell: (i32, i32), pitch: i32, glide_secs: f32) -> bool {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = voices.get_mut(&voice_key(self.grid, cell)) else {
      return false;
    };
    let to = freq_for_pitch(pitch, self.fund, self.edo);
    let from = state.freq; // live, mid-glide included
    if to == from {
      state.glide_per_sample = 1.0;
    } else {
      let samples = (glide_secs * self.sample_rate).max(1.0);
      state.freq_target = to;
      state.glide_per_sample = (to / from).powf(1.0 / samples);
    }
    true
  }

  /// Release `cell`: its voice rings out (ramps to zero over `release_secs`).
  pub fn note_off(&mut self, cell: (i32, i32)) {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = voices.get_mut(&voice_key(self.grid, cell)) {
      state.target_env = 0.0;
      state.ramp_per_sample = state.env / (self.release_secs * self.sample_rate);
    }
  }

  /// The accrete path's note-off: instead of releasing, move `cell`'s voice under the
  /// sustain key for `pitch` (its struck pitch), so it keeps ringing -- seamlessly,
  /// since it is the same `VoiceState` continuing. If a sustained voice already rings
  /// at that key (the pitch was sustained twice from this grid), release the finger
  /// voice normally instead: overwriting a mid-ring voice with a phase-mismatched one
  /// would click, and one drone at the pitch is enough.
  pub fn sustain_note(&mut self, cell: (i32, i32), pitch: i32) {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut state) = voices.remove(&voice_key(self.grid, cell)) else {
      return;
    };
    let key = sustain_key(self.grid, pitch);
    if voices.contains_key(&key) {
      state.target_env = 0.0;
      state.ramp_per_sample = state.env / (self.release_secs * self.sample_rate);
      voices.insert(voice_key(self.grid, cell), state);
    } else {
      voices.insert(key, state);
    }
  }

  /// This bank's accrete 'clear': ramp THIS grid's sustained voices to silence over
  /// the sink's release time. The other grid's drones and all fingered voices are
  /// untouched (accrete banks are per-monome).
  pub fn release_sustained(&mut self) {
    release_sustained_voices(&self.voices, self.grid, self.release_secs, self.sample_rate);
  }

  /// A manual retrigger of a sustaining pitch: cut THIS grid's drone at `pitch` so
  /// the incoming strike replaces it instead of doubling it (misc.org "retriggering
  /// a sustaining note replaces it"). The dying voice is re-keyed under a unique
  /// retired slot -- freeing the sustain key at once -- and rings out its release
  /// ramp there (the render reaps it at silence). The accrete SET is not this
  /// sink's to touch: the pitch keeps its sustained membership, so releasing the
  /// replacing note re-drones it. No-op when no drone rings at `pitch` (a pitch
  /// merely *in the set* while still fingered has no drone voice yet).
  pub fn cut_sustained(&mut self, pitch: i32) {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut state) = voices.remove(&sustain_key(self.grid, pitch)) else {
      return;
    };
    state.target_env = 0.0;
    state.ramp_per_sample = state.env / (self.release_secs * self.sample_rate);
    // next_id doubles as the retired-key uniquifier: two cuts never collide.
    let uniq = self.next_id as i32;
    self.next_id += 1;
    voices.insert(VoiceSource::Accreted { chord: RETIRED_BASE + self.grid, pitch: uniq }, state);
  }
}

/// Ramp one grid's sustained voices to silence -- that bank's accrete 'clear', in a
/// form the feet-accrete pedal hook can call without owning a sink.
pub fn release_sustained_voices(
  voices: &Arc<Mutex<VoiceMap>>,
  grid: usize,
  release_secs: f32,
  sample_rate: f32,
) {
  let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
  for (src, state) in voices.iter_mut() {
    if let VoiceSource::Accreted { chord, .. } = src {
      if *chord == SUSTAIN_BASE + grid {
        state.target_env = 0.0;
        state.ramp_per_sample = state.env / (release_secs * sample_rate);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::types::Waveform;
  use std::collections::HashMap;

  fn shared() -> Arc<Mutex<VoiceMap>> {
    Arc::new(Mutex::new(HashMap::new()))
  }

  fn sink(grid: usize, voices: &Arc<Mutex<VoiceMap>>) -> SurfaceSink {
    // sustain_level 1.0 = no pluck decay, so target_env assertions stay exact.
    SurfaceSink::new(grid, Arc::clone(voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5)
  }

  fn count(voices: &Arc<Mutex<VoiceMap>>) -> usize {
    voices.lock().unwrap().len()
  }

  fn target_env(voices: &Arc<Mutex<VoiceMap>>, grid: usize, cell: (i32, i32)) -> Option<f32> {
    voices.lock().unwrap().get(&voice_key(grid, cell)).map(|v| v.target_env)
  }

  #[test]
  fn same_pitch_on_two_grids_is_two_independent_voices() {
    // The core promise: fingering the same pitch on both grids yields TWO voices,
    // and releasing one leaves the other sounding.
    let voices = shared();
    let mut a = sink(0, &voices);
    let mut b = sink(1, &voices);
    a.note_on((0, 0), 30, Timbre::default(), None);
    b.note_on((0, 0), 30, Timbre::default(), None);
    assert_eq!(count(&voices), 2, "same pitch, two grids -> two voices");
    a.note_off((0, 0));
    assert_eq!(target_env(&voices, 0, (0, 0)), Some(0.0), "grid a's voice releases");
    assert_eq!(target_env(&voices, 1, (0, 0)), Some(1.0), "grid b's voice keeps sounding");
  }

  #[test]
  fn two_colliding_cells_on_one_grid_stay_independent() {
    // 58-8-1: cells (0,8) and (1,0) both sound step 8. Keyed by cell, they are two
    // voices; releasing one does not silence the other.
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on((0, 8), 8, Timbre::default(), None);
    a.note_on((1, 0), 8, Timbre::default(), None);
    assert_eq!(count(&voices), 2);
    a.note_off((0, 8));
    assert_eq!(target_env(&voices, 0, (1, 0)), Some(1.0), "the other cell still holds");
  }

  #[test]
  fn note_on_stamps_the_grids_waveform() {
    let voices = shared();
    let mut a = sink(0, &voices);
    let t = Timbre { waveform: Waveform::Saw, ..Timbre::default() };
    a.note_on((3, 4), 20, t, None);
    let v = voices.lock().unwrap();
    let state = v.get(&voice_key(0, (3, 4))).unwrap();
    assert_eq!(state.timbre.waveform, Waveform::Saw);
    assert_eq!(state.am_phase, 0.0, "LFO retriggered at note-on");
  }

  #[test]
  fn note_off_unknown_cell_is_a_noop() {
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_off((9, 9)); // must not panic
    assert_eq!(count(&voices), 0);
  }

  #[test]
  fn legato_steals_the_voice_instead_of_restriking() {
    // The mono+slide path: the cut note's voice is re-keyed under the new cell and
    // aimed at the new pitch. Envelope, timbre, and gain carry over untouched -- no
    // attack (env stays where it was), no pluck re-trigger.
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on((3, 4), 20, Timbre { gain: 0.7, ..Timbre::default() }, None);
    {
      // Pretend the attack finished a while ago (mid-note state).
      let mut v = voices.lock().unwrap();
      let s = v.get_mut(&voice_key(0, (3, 4))).unwrap();
      s.env = 0.9;
      s.phase = 0.37;
    }
    assert!(a.note_on_legato((3, 4), (5, 5), 30, 0.1, None), "the voice was there to steal");
    assert_eq!(count(&voices), 1, "moved, not duplicated");
    let v = voices.lock().unwrap();
    let s = v.get(&voice_key(0, (5, 5))).expect("re-keyed under the new cell");
    assert_eq!(s.env, 0.9, "the envelope continues -- no attack re-trigger");
    assert_eq!(s.phase, 0.37, "the oscillator continues");
    assert_eq!(s.timbre.gain, 0.7, "the stolen voice keeps its gain");
    assert_eq!(s.freq, freq_for_pitch(20, 80.0, 58), "the glide starts at the old pitch");
    assert_eq!(s.freq_target, freq_for_pitch(30, 80.0, 58), "aimed at the new pitch");
    assert!(s.glide_per_sample > 1.0, "gliding upward");
  }

  #[test]
  fn legato_to_the_same_pitch_moves_the_voice_without_a_glide() {
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on((0, 8), 8, Timbre::default(), None);
    assert!(a.note_on_legato((0, 8), (1, 0), 8, 0.1, None));
    let v = voices.lock().unwrap();
    let s = v.get(&voice_key(0, (1, 0))).unwrap();
    assert_eq!(s.glide_per_sample, 1.0, "same pitch: the glide stays inert");
    assert_eq!(s.freq, freq_for_pitch(8, 80.0, 58));
  }

  #[test]
  fn legato_with_no_voice_reports_false() {
    let voices = shared();
    let mut a = sink(0, &voices);
    assert!(!a.note_on_legato((9, 9), (5, 5), 30, 0.1, None), "nothing to steal");
    assert_eq!(count(&voices), 0, "and nothing appears");
  }

  #[test]
  fn note_on_gliding_starts_at_the_source_pitch_and_targets_the_new_one() {
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on_gliding((3, 4), 30, 20, Timbre::default(), 0.1, None);
    let v = voices.lock().unwrap();
    let state = v.get(&voice_key(0, (3, 4))).unwrap();
    let from = crate::pitch::freq_for_pitch(20, 80.0, 58);
    let to = crate::pitch::freq_for_pitch(30, 80.0, 58);
    assert_eq!(state.freq, from, "starts at the slide source's frequency");
    assert_eq!(state.freq_target, to, "targets the struck pitch");
    assert!(state.glide_per_sample > 1.0, "an upward slide walks the freq up");
  }

  #[test]
  fn note_on_gliding_from_the_same_pitch_is_a_plain_note() {
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on_gliding((3, 4), 30, 30, Timbre::default(), 0.1, None);
    let v = voices.lock().unwrap();
    let state = v.get(&voice_key(0, (3, 4))).unwrap();
    assert_eq!(state.glide_per_sample, 1.0, "no glide");
  }

  #[test]
  fn sustain_note_keeps_the_voice_ringing_under_the_sustain_key() {
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on((3, 4), 20, Timbre::default(), None);
    a.sustain_note((3, 4), 20);
    assert_eq!(count(&voices), 1, "the voice moved, not duplicated");
    let v = voices.lock().unwrap();
    let state = v.get(&sustain_key(0, 20)).expect("re-keyed under the sustain key");
    assert_eq!(state.target_env, 1.0, "still ringing (no release ramp)");
    assert!(v.get(&voice_key(0, (3, 4))).is_none(), "the finger key is gone");
  }

  #[test]
  fn sustaining_the_same_pitch_twice_releases_the_second_finger_voice() {
    // Cells (0,8) and (1,0) both sound step 8 in 58-8-1. Sustaining both: the first
    // becomes the drone; the second releases normally (no click, no double drone).
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on((0, 8), 8, Timbre::default(), None);
    a.note_on((1, 0), 8, Timbre::default(), None);
    a.sustain_note((0, 8), 8);
    a.sustain_note((1, 0), 8);
    assert_eq!(target_env(&voices, 0, (1, 0)), Some(0.0), "second finger voice ramps out");
    let v = voices.lock().unwrap();
    assert_eq!(v.get(&sustain_key(0, 8)).map(|s| s.target_env), Some(1.0), "one drone rings");
  }

  #[test]
  fn cut_sustained_ramps_the_drone_and_frees_the_sustain_key_for_a_replacement() {
    // Retrigger of a sustaining pitch (misc.org "retriggering a sustaining note
    // replaces it"): the old drone is cut -- moved off the sustain key, ramping
    // out -- and the very next strike + release can re-drone the pitch while the
    // old tail is still fading.
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on((3, 4), 20, Timbre::default(), None);
    a.sustain_note((3, 4), 20);
    a.cut_sustained(20);
    {
      let v = voices.lock().unwrap();
      assert!(v.get(&sustain_key(0, 20)).is_none(), "the sustain key frees immediately");
      assert_eq!(v.len(), 1, "the drone moved (retired), not vanished");
      assert_eq!(v.values().next().map(|s| s.target_env), Some(0.0), "and it is ramping out");
    }
    // The replacing note: strike the same pitch, release it under a live accrete.
    a.note_on((3, 4), 20, Timbre::default(), None);
    a.sustain_note((3, 4), 20);
    let v = voices.lock().unwrap();
    assert_eq!(v.get(&sustain_key(0, 20)).map(|s| s.target_env), Some(1.0), "the new drone rings");
    assert_eq!(v.len(), 2, "old tail still fading beside it");
  }

  #[test]
  fn cut_sustained_without_a_drone_is_a_noop() {
    // No drone at the pitch -- including the still-fingered case (the pitch may sit
    // in the accrete SET, but its voice is a finger voice until release).
    let voices = shared();
    let mut a = sink(0, &voices);
    a.cut_sustained(20); // empty map: must not panic
    a.note_on((3, 4), 20, Timbre::default(), None);
    a.cut_sustained(20);
    assert_eq!(target_env(&voices, 0, (3, 4)), Some(1.0), "the fingered voice is untouched");
    assert_eq!(count(&voices), 1);
  }

  #[test]
  fn cut_sustained_only_touches_its_own_grids_drone() {
    // Banks are per-monome: retriggering a pitch on grid a leaves grid b's drone
    // at the same pitch ringing.
    let voices = shared();
    let mut a = sink(0, &voices);
    let mut b = sink(1, &voices);
    a.note_on((3, 4), 20, Timbre::default(), None);
    a.sustain_note((3, 4), 20);
    b.note_on((3, 4), 20, Timbre::default(), None);
    b.sustain_note((3, 4), 20);
    a.cut_sustained(20);
    let v = voices.lock().unwrap();
    assert!(v.get(&sustain_key(0, 20)).is_none(), "grid a's drone was cut");
    assert_eq!(v.get(&sustain_key(1, 20)).map(|s| s.target_env), Some(1.0), "grid b's drone keeps ringing");
  }

  #[test]
  fn release_sustained_silences_only_its_own_grids_drones() {
    let voices = shared();
    let mut a = sink(0, &voices);
    let mut b = sink(1, &voices);
    a.note_on((3, 4), 20, Timbre::default(), None);
    a.sustain_note((3, 4), 20);
    b.note_on((5, 5), 33, Timbre::default(), None);
    b.sustain_note((5, 5), 33);
    a.note_on((6, 6), 40, Timbre::default(), None); // a still-fingered note
    a.release_sustained();
    let v = voices.lock().unwrap();
    assert_eq!(v.get(&sustain_key(0, 20)).map(|s| s.target_env), Some(0.0), "grid a drone released");
    assert_eq!(v.get(&sustain_key(1, 33)).map(|s| s.target_env), Some(1.0), "grid b drone keeps ringing (banks are per-monome)");
    assert_eq!(v.get(&voice_key(0, (6, 6))).map(|s| s.target_env), Some(1.0), "fingered note untouched");
  }

  #[test]
  fn the_volume_fader_reaches_sustained_voices_from_its_grid() {
    let voices = shared();
    let mut a = sink(0, &voices);
    let mut b = sink(1, &voices);
    // Grid 0's drone carries a timbre-slot amplitude of 0.8 in its gain; the fader
    // rescale is a ratio, so that factor must survive.
    a.note_on((3, 4), 20, Timbre { gain: 0.8, ..Timbre::default() }, None);
    a.sustain_note((3, 4), 20);
    b.note_on((3, 4), 20, Timbre::default(), None);
    b.sustain_note((3, 4), 20);
    rescale_grid_gain(&voices, 0, 0.25);
    let v = voices.lock().unwrap();
    let g0 = v.get(&sustain_key(0, 20)).map(|s| s.timbre.gain).unwrap();
    assert!((g0 - 0.2).abs() < 1e-6, "grid 0's drone rescaled by the ratio: {g0}");
    assert_eq!(v.get(&sustain_key(1, 20)).map(|s| s.timbre.gain), Some(1.0), "grid 1's drone untouched");
  }

  // ---- glide_voice_to: the per-voice pitch edit primitive ----

  #[test]
  fn glide_voice_to_aims_the_voice_at_the_new_pitch_without_re_keying_it() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), None);
    let before = v.lock().unwrap()[&voice_key(0, (3, 3))].freq;

    assert!(a.glide_voice_to((3, 3), 30, 0.1));
    let g = v.lock().unwrap();
    let st = &g[&voice_key(0, (3, 3))];
    assert_eq!(st.freq, before, "the voice does not JUMP; it starts gliding from where it was");
    assert_eq!(st.freq_target, freq_for_pitch(30, 80.0, 58));
    assert!(st.glide_per_sample > 1.0, "gliding up");
    assert_eq!(g.len(), 1, "no new voice; the note MOVES rather than restriking");
  }

  #[test]
  fn glide_voice_to_leaves_the_voice_addressable_at_its_original_cell() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), None);
    a.glide_voice_to((3, 3), 30, 0.1);
    // Still keyed by the cell it was struck on, even though its pitch has left that
    // cell's nominal pitch -- so a later release/edit still finds it.
    assert!(v.lock().unwrap().contains_key(&voice_key(0, (3, 3))));
    a.note_off((3, 3));
    let g = v.lock().unwrap();
    assert!(g[&voice_key(0, (3, 3))].target_env <= 0.0, "the moved note still releases");
  }

  /// The sharp edge: `glide_per_sample` encodes DIRECTION (the integrator's crossing
  /// test compares it against 1.0), so re-aiming must recompute it from the voice's
  /// live freq. Patching `freq_target` alone would leave a voice gliding up past a
  /// target now below it, and it would never stop. Dragging a note back and forth --
  /// exactly what edit mode does -- hits this.
  #[test]
  fn re_aiming_a_glide_backwards_mid_flight_reverses_its_direction() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), None);
    a.glide_voice_to((3, 3), 40, 0.1);
    assert!(v.lock().unwrap()[&voice_key(0, (3, 3))].glide_per_sample > 1.0, "up first");

    // Now aim BELOW the start, without letting the first glide finish.
    a.glide_voice_to((3, 3), 5, 0.1);
    let g = v.lock().unwrap();
    let st = &g[&voice_key(0, (3, 3))];
    assert!(st.glide_per_sample < 1.0, "re-aimed downward, not left sailing upward");
    assert_eq!(st.freq_target, freq_for_pitch(5, 80.0, 58));
  }

  #[test]
  fn gliding_a_voice_to_its_own_pitch_is_inert() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), None);
    assert!(a.glide_voice_to((3, 3), 20, 0.1));
    assert_eq!(
      v.lock().unwrap()[&voice_key(0, (3, 3))].glide_per_sample,
      1.0,
      "a no-op drag must not leave the integrator running",
    );
  }

  #[test]
  fn glide_voice_to_an_empty_cell_is_false() {
    let v = shared();
    let mut a = sink(0, &v);
    assert!(!a.glide_voice_to((9, 9), 30, 0.1));
  }

  /// The edit must not disturb anything else about the note -- it keeps sounding, with
  /// its own timbre, gain and pulse, and simply moves.
  #[test]
  fn glide_voice_to_preserves_the_envelope_timbre_and_pulse() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre { waveform: Waveform::Square, ..Timbre::default() }, Some(3.0));
    let (env, target, wave, pulse, phase) = {
      let g = v.lock().unwrap();
      let st = &g[&voice_key(0, (3, 3))];
      (st.env, st.target_env, st.timbre.waveform, st.tempo_am_freq, st.tempo_am_phase)
    };
    a.glide_voice_to((3, 3), 31, 0.1);
    let g = v.lock().unwrap();
    let st = &g[&voice_key(0, (3, 3))];
    assert_eq!(st.env, env, "no attack re-trigger");
    assert_eq!(st.target_env, target);
    assert_eq!(st.timbre.waveform, wave);
    assert_eq!(st.tempo_am_freq, pulse, "the pulse rate is untouched by a pitch edit");
    assert_eq!(st.tempo_am_phase, phase, "and its phase does not jump");
  }
}
