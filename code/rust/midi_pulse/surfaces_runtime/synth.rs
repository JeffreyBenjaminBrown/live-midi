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
  /// voice for the same cell (a retrigger, though the grid thread debounces repeats).
  pub fn note_on(&mut self, cell: (i32, i32), pitch: i32, timbre: Timbre) {
    let id = self.next_id;
    self.next_id += 1;
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    voices.insert(
      voice_key(self.grid, cell),
      VoiceState {
        id,
        freq: freq_for_pitch(pitch, self.fund, self.edo),
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

  /// The accrete 'clear': ramp EVERY sustained voice (both grids') to silence over
  /// this sink's release time. Fingered voices are untouched.
  pub fn release_all_sustained(&mut self) {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    for (src, state) in voices.iter_mut() {
      if let VoiceSource::Accreted { chord, .. } = src {
        if *chord >= SUSTAIN_BASE {
          state.target_env = 0.0;
          state.ramp_per_sample = state.env / (self.release_secs * self.sample_rate);
        }
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
    a.note_on((0, 0), 30, Timbre::default());
    b.note_on((0, 0), 30, Timbre::default());
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
    a.note_on((0, 8), 8, Timbre::default());
    a.note_on((1, 0), 8, Timbre::default());
    assert_eq!(count(&voices), 2);
    a.note_off((0, 8));
    assert_eq!(target_env(&voices, 0, (1, 0)), Some(1.0), "the other cell still holds");
  }

  #[test]
  fn note_on_stamps_the_grids_waveform() {
    let voices = shared();
    let mut a = sink(0, &voices);
    let t = Timbre { waveform: Waveform::Saw, ..Timbre::default() };
    a.note_on((3, 4), 20, t);
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
  fn sustain_note_keeps_the_voice_ringing_under_the_sustain_key() {
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on((3, 4), 20, Timbre::default());
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
    a.note_on((0, 8), 8, Timbre::default());
    a.note_on((1, 0), 8, Timbre::default());
    a.sustain_note((0, 8), 8);
    a.sustain_note((1, 0), 8);
    assert_eq!(target_env(&voices, 0, (1, 0)), Some(0.0), "second finger voice ramps out");
    let v = voices.lock().unwrap();
    assert_eq!(v.get(&sustain_key(0, 8)).map(|s| s.target_env), Some(1.0), "one drone rings");
  }

  #[test]
  fn release_all_sustained_silences_both_grids_drones_only() {
    let voices = shared();
    let mut a = sink(0, &voices);
    let mut b = sink(1, &voices);
    a.note_on((3, 4), 20, Timbre::default());
    a.sustain_note((3, 4), 20);
    b.note_on((5, 5), 33, Timbre::default());
    b.sustain_note((5, 5), 33);
    a.note_on((6, 6), 40, Timbre::default()); // a still-fingered note
    a.release_all_sustained();
    let v = voices.lock().unwrap();
    assert_eq!(v.get(&sustain_key(0, 20)).map(|s| s.target_env), Some(0.0), "grid a drone released");
    assert_eq!(v.get(&sustain_key(1, 33)).map(|s| s.target_env), Some(0.0), "grid b drone released");
    assert_eq!(v.get(&voice_key(0, (6, 6))).map(|s| s.target_env), Some(1.0), "fingered note untouched");
  }

  #[test]
  fn the_volume_fader_reaches_sustained_voices_from_its_grid() {
    let voices = shared();
    let mut a = sink(0, &voices);
    let mut b = sink(1, &voices);
    // Grid 0's drone carries a timbre-slot amplitude of 0.8 in its gain; the fader
    // rescale is a ratio, so that factor must survive.
    a.note_on((3, 4), 20, Timbre { gain: 0.8, ..Timbre::default() });
    a.sustain_note((3, 4), 20);
    b.note_on((3, 4), 20, Timbre::default());
    b.sustain_note((3, 4), 20);
    rescale_grid_gain(&voices, 0, 0.25);
    let v = voices.lock().unwrap();
    let g0 = v.get(&sustain_key(0, 20)).map(|s| s.timbre.gain).unwrap();
    assert!((g0 - 0.2).abs() < 1e-6, "grid 0's drone rescaled by the ratio: {g0}");
    assert_eq!(v.get(&sustain_key(1, 20)).map(|s| s.timbre.gain), Some(1.0), "grid 1's drone untouched");
  }
}
