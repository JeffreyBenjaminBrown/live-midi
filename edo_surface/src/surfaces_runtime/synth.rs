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
//! renderer) without touching any sawwave file, exactly as the looper's sink does.
//! Unlike the looper's sink, we key our voices with our own honest `VoiceSource`
//! variants (`SurfaceFinger` / `SurfaceDrone` / `SurfaceRetired`) rather than reusing
//! `Accreted` as an opaque handle -- the render engine never reads the key either way,
//! but our own map-walking code (fader gains, pedal gains, factored-pulse retune,
//! the bulk clears) gets to match on what a voice actually is instead of re-deriving
//! it from a chord offset.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::pitch::freq_for_pitch;
use crate::types::{Timbre, VoiceId, VoiceMap, VoiceSource, VoiceState};
use crate::voices::pluck_envelope;

/// Aim every voice belonging to `grid` -- fingered and sustained (drone) alike -- at
/// fader gain `gain`, in place. Drives the *live* volume control: a volume strip sets
/// the loudness of whatever grid it `controls` (its own, in the current rigs), and
/// moving it must change notes that are already sounding (a fader, not a radio) -- so
/// we walk the shared map and ASSIGN `fader_gain` on that grid's voices. Plain
/// assignment, not a ratio: the volume chain is stored components multiplied at
/// render time (`voices::accumulate_voices`), so the timbre slot's amplitude lives in
/// `timbre.gain` untouched and the fader is free to land anywhere, including 0 --
/// a fader parked at the bottom recovers exactly like the pedal does. Future note-ons
/// pick up the new fader from the shared per-grid state (via `SurfaceSink::note_on`);
/// this only touches the ones already in flight. Sustained (accreted) voices keep
/// following the fader of the grid they came from -- a drone you can only kill with
/// `clear` should at least obey a volume fader. Retired voices (release tails) are
/// left alone, like `set_grid_pedal_gain` leaves them.
pub fn set_grid_fader_gain(voices: &Arc<Mutex<VoiceMap>>, grid: usize, gain: f32) {
  if !gain.is_finite() {
    return;
  }
  let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
  for (src, state) in voices.iter_mut() {
    let voice_grid = match src {
      VoiceSource::SurfaceFinger { grid, .. } => *grid,
      VoiceSource::SurfaceDrone { grid, .. } => *grid,
      _ => continue,
    };
    if voice_grid == grid {
      state.fader_gain = gain;
    }
  }
}

/// Multiply the factored-pulse RATE of one grid's edit-mode voices, in place.
///
/// Selection is by PITCH, because that is how edit mode selects -- a factored-pulse
/// edit applies to every note in edit mode on this grid at once (Jeff's asymmetry: a
/// pitch DRAG moves only the nearest, `2_discussion` 2d).
///
/// But a voice's key does not carry its pitch uniformly: a *fingered* voice
/// (`SurfaceFinger`) is keyed by the cell it was struck on, not its pitch, so `held`
/// -- this grid's live cell -> pitch map -- is how a fingered voice's pitch is known.
///
/// The phase is deliberately KEPT. `factored_pulse_phase` free-runs and only
/// `factored_pulse_freq` is read per sample, so changing the rate mid-note is a slope change
/// with no amplitude step -- i.e. no click. `note_on_legato` already relied on exactly
/// that ("re-aims the factored pulse's rate at this onset, but its phase continues"),
/// which is why this is safe rather than merely plausible.
///
/// Multiplying, not setting: "slower ones continue to be slower than faster ones"
/// (`1_vision`), so notes that already differ keep their spread.
///
/// A voice with no factored pulse (`factored_pulse_freq == 0`) stays un-pulsed:
/// multiplying zero cannot start one, and a note struck while cycling was off is
/// meant to stay silent of it.
pub fn scale_factored_pulse_rate(
  voices: &Arc<Mutex<VoiceMap>>,
  grid: usize,
  ratio: f32,
  edited: &HashSet<i32>,
  held: &HashMap<(i32, i32), i32>,
) {
  if !ratio.is_finite() || ratio <= 0.0 || edited.is_empty() {
    return;
  }
  // The fingered voices whose pitch is being edited, by their cell keys.
  let cells: HashSet<VoiceSource> = held
    .iter()
    .filter(|(_, pitch)| edited.contains(pitch))
    .map(|(cell, _)| voice_key(grid, *cell))
    .collect();
  let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
  for (src, state) in voices.iter_mut() {
    if state.factored_pulse_freq == 0.0 {
      continue;
    }
    let wanted = cells.contains(src)
      || matches!(src, VoiceSource::SurfaceDrone { grid: g, pitch }
                  if *g == grid && edited.contains(pitch));
    if wanted {
      state.factored_pulse_freq *= ratio;
    }
  }
}

/// SET the factored-pulse RATE of one grid's edit-mode voices, in place -- the
/// `=1` switch's counterpart to `scale_factored_pulse_rate`'s multiply
/// (`hooks::factored_pulse_press`, driven off `PolyrhythmState::press`'s on/off
/// report). Same selection mechanics: `edited` is the grid's edit-mode
/// PITCHES, and `held` -- this grid's live cell -> pitch map -- is how a
/// *fingered* voice's pitch is found (it is keyed by cell, not pitch).
///
/// Unlike the multiplier this SETS rather than multiplies, and `hz == 0.0` is a
/// legal target: cycling turning OFF must actually stop an edit-mode voice's
/// pulse, not merely decline to touch it (`scale_factored_pulse_rate`'s "zero
/// stays zero" rule is deliberately not repeated here).
///
/// Setting a stopped voice (`factored_pulse_freq == 0.0`) to a nonzero `hz`
/// starts its pulse mid-note. `factored_pulse_phase` free-runs regardless of
/// `factored_pulse_freq` (see the engine's render), so this is just a slope
/// change from wherever the phase already sits -- the same reasoning that
/// makes `scale_factored_pulse_rate` safe on a running pulse.
///
/// Setting to 0.0 stops it, but the render treats `factored_pulse_freq == 0.0`
/// as amplitude multiplier 1.0 (no pulse), not "wherever the triangle wave
/// currently sits" -- so this step, unlike a rate change, CAN click. Jeff's ask
/// (=1 off actually silences an edit-mode voice's pulse) accepts that trade.
pub fn set_factored_pulse_rate_at(
  voices: &Arc<Mutex<VoiceMap>>,
  grid: usize,
  edited: &HashSet<i32>,
  held: &HashMap<(i32, i32), i32>,
  hz: f32,
) {
  if !hz.is_finite() || hz < 0.0 || edited.is_empty() {
    return;
  }
  // The fingered voices whose pitch is being edited, by their cell keys (same
  // lookup as `scale_factored_pulse_rate`).
  let cells: HashSet<VoiceSource> = held
    .iter()
    .filter(|(_, pitch)| edited.contains(pitch))
    .map(|(cell, _)| voice_key(grid, *cell))
    .collect();
  let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
  for (src, state) in voices.iter_mut() {
    let wanted = cells.contains(src)
      || matches!(src, VoiceSource::SurfaceDrone { grid: g, pitch }
                  if *g == grid && edited.contains(pitch));
    if wanted {
      state.factored_pulse_freq = hz;
    }
  }
}

/// A fingered voice's key: per grid and the cell it was struck on.
fn voice_key(grid: usize, cell: (i32, i32)) -> VoiceSource {
  VoiceSource::SurfaceFinger { grid, cell }
}

/// The key of a *sustained* voice: per source grid and absolute pitch (the accrete
/// set is keyed the same way).
fn sustain_key(grid: usize, pitch: i32) -> VoiceSource {
  VoiceSource::SurfaceDrone { grid, pitch }
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
  /// The per-grid expression-pedal volumes (shared with the pedal thread, which
  /// writes them). A fresh note starts at ITS grid's current pedal volume --
  /// already settled, no slew-in. Unity when the rig has no pedals.
  pedal_gains: Arc<Mutex<Vec<f32>>>,
  /// The per-grid volume-fader gains -- the SAME shared `Arc` the volume strip's
  /// `set_volume` (keys.rs) writes into and `set_grid_fader_gain` walks live. A
  /// fresh note stamps its `fader_gain` from ITS grid's current entry (unity absent
  /// a fader or before its first move); `set_grid_fader_gain` re-aims voices already
  /// in flight. Two views of one vec, never two copies.
  fader_gains: Arc<Mutex<Vec<f32>>>,
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
    pedal_gains: Arc<Mutex<Vec<f32>>>,
    fader_gains: Arc<Mutex<Vec<f32>>>,
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
      pedal_gains,
      fader_gains,
    }
  }

  /// This grid's current expression-pedal volume (unity absent a pedal or before
  /// its first movement).
  fn pedal_gain(&self) -> f32 {
    let gains = self.pedal_gains.lock().unwrap_or_else(|e| e.into_inner());
    gains.get(self.grid).copied().unwrap_or(1.0)
  }

  /// This grid's current volume-fader gain (unity absent a fader or before its
  /// first move).
  fn fader_gain(&self) -> f32 {
    let gains = self.fader_gains.lock().unwrap_or_else(|e| e.into_inner());
    gains.get(self.grid).copied().unwrap_or(1.0)
  }

  /// Start `cell` sounding `pitch` (an absolute EDO step) with `timbre`. Spawns a
  /// fresh voice (its AM/FM LFOs retriggered at phase 0); overwrites any existing
  /// voice for the same cell (a retrigger, though the grid thread debounces
  /// repeats). `factored_pulse_hz` is the factored pulse at this note's onset (None =
  /// no factored pulse), fixed for the voice's life.
  pub fn note_on(&mut self, cell: (i32, i32), pitch: i32, timbre: Timbre, factored_pulse_hz: Option<f32>) {
    let id = self.next_id;
    self.next_id += 1;
    let pedal = self.pedal_gain();
    let fader = self.fader_gain();
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    voices.insert(
      voice_key(self.grid, cell),
      VoiceState {
        id,
        freq: freq_for_pitch(pitch, self.fund, self.edo),
        freq_target: 0.0,
        glide_per_sample: 1.0,
        factored_pulse_freq: factored_pulse_hz.unwrap_or(0.0),
        factored_pulse_phase: 0.0,
        phase: 0.0,
        env: 0.0,
        target_env: 1.0,
        ramp_per_sample: 1.0 / (self.attack_secs * self.sample_rate),
        pending_attack: None,
        sustain_env: self.sustain_env,
        decay_per_sample: self.decay_per_sample,
        // `timbre.gain` is the slot's amplitude alone (the caller no longer bakes the
        // fader into it -- see keys.rs); the fader and pedal are separate, stored
        // components multiplied in at render time.
        timbre,
        fader_gain: fader,
        grid_gain: pedal,
        grid_gain_target: pedal,
        am_phase: 0.0,
        fm_phase: 0.0,
        rel_am_phase: 0.0,
        rel_fm_phase: 0.0,
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
    factored_pulse_hz: Option<f32>,
  ) {
    if from_pitch == pitch {
      return self.note_on(cell, pitch, timbre, factored_pulse_hz);
    }
    self.note_on(cell, pitch, timbre, factored_pulse_hz);
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
  /// keeps its timbre and gain (it IS the same voice). `factored_pulse_hz` re-aims
  /// the factored pulse's rate at this onset, but its phase continues (no amplitude
  /// step). Returns false if nothing sounds at `from_cell` (the caller falls back to
  /// a fresh strike).
  pub fn note_on_legato(
    &mut self,
    from_cell: (i32, i32),
    cell: (i32, i32),
    pitch: i32,
    glide_secs: f32,
    factored_pulse_hz: Option<f32>,
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
    state.factored_pulse_freq = factored_pulse_hz.unwrap_or(0.0);
    voices.insert(voice_key(self.grid, cell), state);
    true
  }

  /// Glide the voice at `cell` to `pitch` over `glide_secs`, leaving everything else
  /// about it alone -- envelope, timbre, gain, and the factored pulse's rate and
  /// phase all continue. The per-voice pitch edit: the note keeps sounding and simply
  /// *moves*.
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
  // Superseded by `rehome_to_cell`: a drag always adopts the voice onto the drag
  // finger's cell, so an in-place glide is no longer called. Kept for its tests, which
  // document the glide integrator's direction-encoding (see the backward-re-aim one).
  #[allow(dead_code)]
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

  /// Hand a sounding voice to the finger now on `cell`: re-home it there as an ordinary
  /// FINGERED voice, gliding it to `to`, and RETRIGGER its pluck at the drag's onset. Its
  /// timbre, gain, and the factored pulse's rate and phase all continue; only the envelope
  /// re-fires, so each drag lands a rhythmic accent (Jeff). The re-attack happens at the
  /// OLD pitch -- the glide has not moved `freq` yet -- and the glide then carries it to `to`.
  ///
  /// This is what a drag does. Pressing a monomekey to drag a voice is a finger going
  /// down, and that finger must own the voice afterwards, or the voice is stranded:
  /// left at its old cell (which is not where the finger is) or, if the original
  /// finger had lifted, a bare drone that an exit will cut while the drag finger is
  /// still held. The source is either a fingered voice at `from_cell` or a drone keyed
  /// by `from_pitch`; either way it ends up fingered at `cell`.
  ///
  /// Unlike `note_on_legato`, this does NOT re-aim the factored pulse -- a drag must
  /// preserve it, not zero it. Returns false if no voice was found to move.
  pub fn rehome_to_cell(
    &mut self,
    from_cell: Option<(i32, i32)>,
    from_pitch: i32,
    cell: (i32, i32),
    to: i32,
    glide_secs: f32,
  ) -> bool {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    let src = match from_cell {
      Some(fc) => voice_key(self.grid, fc),
      None => sustain_key(self.grid, from_pitch),
    };
    let Some(mut state) = voices.remove(&src) else {
      return false;
    };
    // Retrigger the pluck at the drag's onset, at the OLD pitch (freq is untouched here),
    // with a dip-then-attack for a punchy accent: ramp the envelope DOWN to silence over
    // ~2 ms, and the engine then launches a fresh attack to the peak (see
    // `VoiceState::pending_attack`). The dip avoids the click a hard reset-to-silence
    // makes, and the full silence->peak attack gives real bite; the pluck decay then
    // rings it back down.
    const DIP_SECS: f32 = 0.002;
    state.target_env = 0.0;
    state.ramp_per_sample = state.env / (DIP_SECS * self.sample_rate).max(1.0);
    state.pending_attack = Some(1.0 / (self.attack_secs * self.sample_rate));
    let target = freq_for_pitch(to, self.fund, self.edo);
    let start = state.freq; // live, mid-glide included
    if target == start {
      state.glide_per_sample = 1.0;
    } else {
      let samples = (glide_secs * self.sample_rate).max(1.0);
      state.freq_target = target;
      state.glide_per_sample = (target / start).powf(1.0 / samples);
    }
    voices.insert(voice_key(self.grid, cell), state);
    true
  }

  /// Glide this grid's SUSTAINED (fingerless) drone at `from` to pitch `to`. The
  /// same edit as `glide_voice_to`, for a note whose finger has lifted: a drone is
  /// keyed by pitch rather than by cell, so it needs its own lookup. Re-keys the
  /// voice to `to`, since the key IS the pitch here -- otherwise the drone would
  /// still answer to the pitch it left.
  ///
  /// Returns false if no drone is at `from`.
  ///
  /// Superseded by `rehome_to_cell` (a drag adopts the drone onto the drag finger's
  /// cell), so no longer called; kept for its test.
  #[allow(dead_code)]
  pub fn glide_sustained_to(&mut self, from: i32, to: i32, glide_secs: f32) -> bool {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut state) = voices.remove(&sustain_key(self.grid, from)) else {
      return false;
    };
    let target = freq_for_pitch(to, self.fund, self.edo);
    let start = state.freq; // live, mid-glide included
    if target == start {
      state.glide_per_sample = 1.0;
    } else {
      let samples = (glide_secs * self.sample_rate).max(1.0);
      state.freq_target = target;
      state.glide_per_sample = (target / start).powf(1.0 / samples);
    }
    voices.insert(sustain_key(self.grid, to), state);
    true
  }

  /// Release `cell`: its voice rings out (ramps to zero over `release_secs`).
  pub fn note_off(&mut self, cell: (i32, i32)) {
    let mut voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = voices.get_mut(&voice_key(self.grid, cell)) {
      state.target_env = 0.0;
      state.ramp_per_sample = state.env / (self.release_secs * self.sample_rate);
      state.pending_attack = None; // a finger lifting mid-drag-dip must NOT re-strike
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

  /// This sink's release timing, for callers that end voices through the free
  /// functions (e.g. `end_edited_voices`) rather than a sink method.
  pub fn release_params(&self) -> (f32, f32) {
    (self.release_secs, self.sample_rate)
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
    let seq = self.next_id;
    self.next_id += 1;
    voices.insert(VoiceSource::SurfaceRetired { grid: self.grid, seq }, state);
  }
}

/// Aim every voice of `grid` -- fingered and sustained alike -- at expression-pedal
/// volume `gain` (absolute, 0..1; NOT a ratio, so a pedal parked at 0 recovers).
/// Each voice slews there per sample (`voices::GAIN_SLEW_SECS`), which is what
/// keeps a sweeping pedal free of zipper noise. Retired voices (release tails) are
/// left alone, like `set_grid_fader_gain` leaves them.
pub fn set_grid_pedal_gain(voices: &Arc<Mutex<VoiceMap>>, grid: usize, gain: f32) {
  if !gain.is_finite() {
    return;
  }
  let gain = gain.clamp(0.0, 1.0);
  let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
  for (src, state) in voices.iter_mut() {
    let voice_grid = match src {
      VoiceSource::SurfaceFinger { grid, .. } => *grid,
      VoiceSource::SurfaceDrone { grid, .. } => *grid,
      _ => continue,
    };
    if voice_grid == grid {
      state.grid_gain_target = gain;
    }
  }
}

/// End -- silence, by the ordinary release ramp, exactly how every voice ends --
/// each DRONE of `grid` at one of `pitches`. The bulk-clear controls' voice half:
/// the caller works out WHICH drones lost their last reason (editmode clear passes
/// edited-minus-sustained) and empties its own set; this touches only the voices.
/// Fingered voices are deliberately not here: a finger's voice is the finger's to
/// end (the rule the exit gesture lives by).
pub fn end_drones_at(
  voices: &Arc<Mutex<VoiceMap>>,
  grid: usize,
  pitches: &HashSet<i32>,
  release_secs: f32,
  sample_rate: f32,
) {
  if pitches.is_empty() {
    return;
  }
  let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
  for (src, state) in voices.iter_mut() {
    if matches!(src, VoiceSource::SurfaceDrone { grid: g, pitch }
         if *g == grid && pitches.contains(pitch))
    {
      state.target_env = 0.0;
      state.ramp_per_sample = state.env / (release_secs * sample_rate);
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
    let pedal_gains = Arc::new(Mutex::new(vec![1.0; 2]));
    let fader_gains = Arc::new(Mutex::new(vec![1.0; 2]));
    SurfaceSink::new(
      grid, Arc::clone(voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5, pedal_gains, fader_gains,
    )
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
  fn pedal_gain_targets_this_grids_voices_and_note_ons_start_at_it() {
    let voices = shared();
    let pedal_gains = Arc::new(Mutex::new(vec![1.0_f32; 2]));
    let fader_gains = Arc::new(Mutex::new(vec![1.0_f32; 2]));
    let mut a = SurfaceSink::new(
      0, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5,
      Arc::clone(&pedal_gains), Arc::clone(&fader_gains),
    );
    let mut b = sink(1, &voices);
    a.note_on((0, 0), 20, Timbre::default(), None);
    b.note_on((0, 0), 20, Timbre::default(), None);
    // The pedal moves: only grid 0's voices are re-aimed (absolute, not a ratio).
    set_grid_pedal_gain(&voices, 0, 0.25);
    {
      let v = voices.lock().unwrap();
      assert_eq!(v[&voice_key(0, (0, 0))].grid_gain_target, 0.25);
      assert_eq!(v[&voice_key(1, (0, 0))].grid_gain_target, 1.0, "the other grid is untouched");
    }
    // A fresh note starts already settled at its grid's pedal volume -- no slew-in.
    pedal_gains.lock().unwrap()[0] = 0.25;
    a.note_on((1, 1), 30, Timbre::default(), None);
    let v = voices.lock().unwrap();
    let s = &v[&voice_key(0, (1, 1))];
    assert_eq!((s.grid_gain, s.grid_gain_target), (0.25, 0.25));
  }

  #[test]
  fn end_drones_at_releases_named_drones_and_spares_every_finger() {
    // Grid 0 has: a drone at pitch 20 (to end), a fingered voice at pitch 30
    // (named but fingered), a fingered voice at pitch 40, and a drone at pitch 50.
    // Grid 1 has a drone at pitch 20 too (grids are independent).
    let voices = shared();
    let mut a = sink(0, &voices);
    let mut b = sink(1, &voices);
    a.note_on((0, 0), 20, Timbre::default(), None);
    a.sustain_note((0, 0), 20);
    a.note_on((1, 0), 30, Timbre::default(), None);
    a.note_on((2, 0), 40, Timbre::default(), None);
    a.note_on((3, 0), 50, Timbre::default(), None);
    a.sustain_note((3, 0), 50);
    b.note_on((0, 0), 20, Timbre::default(), None);
    b.sustain_note((0, 0), 20);

    let pitches: HashSet<i32> = [20, 30].into();
    end_drones_at(&voices, 0, &pitches, 0.05, 48000.0);

    let v = voices.lock().unwrap();
    let ended = |src: &VoiceSource| v[src].target_env == 0.0;
    assert!(ended(&sustain_key(0, 20)), "the named drone rings out");
    assert!(
      !ended(&voice_key(0, (1, 0))),
      "a finger at a named pitch keeps sounding -- a finger's voice is the finger's to end",
    );
    assert!(!ended(&voice_key(0, (2, 0))), "an unnamed finger keeps sounding");
    assert!(!ended(&sustain_key(0, 50)), "an unnamed drone keeps sounding");
    assert!(!ended(&sustain_key(1, 20)), "the OTHER grid's drone at 20 is untouched");
  }

  // The old `release_sustained_spares_the_keep_set` test is gone with the function it
  // covered (cleaning phase 6): the sustain-clear now runs `RingStore::remove_sustain`,
  // whose life-support behaviour (spare only fingered pitches, cascade edit away) is
  // pinned in ring.rs (`the_doubly_held_matrix`,
  // `remove_sustain_returns_the_fingerless_pitches_and_cascades_edit`), and the
  // drone-ending half is `end_drones_at` above.

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

  // `release_sustained_silences_only_its_own_grids_drones` is likewise gone with its
  // function; the per-grid drone isolation and "fingered note untouched" it asserted
  // are covered by `end_drones_at_releases_named_drones_and_spares_every_finger`,
  // which is the mechanism the clears now use.

  #[test]
  fn the_volume_fader_reaches_sustained_voices_from_its_grid() {
    let voices = shared();
    let mut a = sink(0, &voices);
    let mut b = sink(1, &voices);
    // Grid 0's drone carries a timbre-slot amplitude of 0.8; the fader is a separate,
    // stored component, so a fader move must leave it alone (no more ratio-into-gain).
    a.note_on((3, 4), 20, Timbre { gain: 0.8, ..Timbre::default() }, None);
    a.sustain_note((3, 4), 20);
    b.note_on((3, 4), 20, Timbre::default(), None);
    b.sustain_note((3, 4), 20);
    set_grid_fader_gain(&voices, 0, 0.25);
    let v = voices.lock().unwrap();
    assert_eq!(
      v.get(&sustain_key(0, 20)).map(|s| s.fader_gain), Some(0.25),
      "grid 0's drone fader assigned",
    );
    assert_eq!(
      v.get(&sustain_key(0, 20)).map(|s| s.timbre.gain), Some(0.8),
      "the slot amplitude survives the fader move -- it was never baked in",
    );
    assert_eq!(
      v.get(&sustain_key(1, 20)).map(|s| s.fader_gain), Some(1.0),
      "grid 1's drone untouched",
    );
  }

  #[test]
  fn the_volume_fader_can_reach_zero_and_recover() {
    // The old ratio-rescale math could never leave the fader at 0 (a ratio FROM zero
    // is undefined, hence the pedal's separate absolute field); plain assignment has
    // no such rule -- 0 is just another value.
    let voices = shared();
    let mut a = sink(0, &voices);
    a.note_on((3, 4), 20, Timbre { gain: 0.8, ..Timbre::default() }, None);
    set_grid_fader_gain(&voices, 0, 0.0);
    assert_eq!(voices.lock().unwrap()[&voice_key(0, (3, 4))].fader_gain, 0.0, "silenced");
    set_grid_fader_gain(&voices, 0, 0.5);
    let v = voices.lock().unwrap();
    assert_eq!(v[&voice_key(0, (3, 4))].fader_gain, 0.5, "recovers from zero");
    assert_eq!(v[&voice_key(0, (3, 4))].timbre.gain, 0.8, "slot amplitude untouched throughout");
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
  /// its own timbre, gain and factored pulse, and simply moves.
  #[test]
  fn glide_voice_to_preserves_the_envelope_timbre_and_factored_pulse() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre { waveform: Waveform::Square, ..Timbre::default() }, Some(3.0));
    let (env, target, wave, pulse, phase) = {
      let g = v.lock().unwrap();
      let st = &g[&voice_key(0, (3, 3))];
      (st.env, st.target_env, st.timbre.waveform, st.factored_pulse_freq, st.factored_pulse_phase)
    };
    a.glide_voice_to((3, 3), 31, 0.1);
    let g = v.lock().unwrap();
    let st = &g[&voice_key(0, (3, 3))];
    assert_eq!(st.env, env, "no attack re-trigger");
    assert_eq!(st.target_env, target);
    assert_eq!(st.timbre.waveform, wave);
    assert_eq!(st.factored_pulse_freq, pulse, "the factored-pulse rate is untouched by a pitch edit");
    assert_eq!(st.factored_pulse_phase, phase, "and its phase does not jump");
  }

  // ---- scale_factored_pulse_rate: retuning sounding notes' factored pulse (per-voice edit) ----

  /// The tests below strike notes at pitch 10 on (1,1) and pitch 20 on (2,2); a
  /// fingered voice is keyed by CELL, so its pitch is only knowable via this map.
  fn held() -> HashMap<(i32, i32), i32> {
    [((1, 1), 10), ((2, 2), 20)].into_iter().collect()
  }

  fn all_edited() -> HashSet<i32> {
    [10, 20].into_iter().collect()
  }

  #[test]
  fn scaling_the_factored_pulse_multiplies_only_the_wanted_pitches() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    a.note_on((2, 2), 20, Timbre::default(), Some(3.0));
    scale_factored_pulse_rate(&v, 0, 2.0, &[10].into_iter().collect(), &held());
    let g = v.lock().unwrap();
    assert_eq!(g[&voice_key(0, (1, 1))].factored_pulse_freq, 4.0, "the edited note doubles");
    assert_eq!(g[&voice_key(0, (2, 2))].factored_pulse_freq, 3.0, "the other note is untouched");
  }

  /// "x2 makes them all go twice as fast ... slower ones continue to be slower than
  /// faster ones" (1_vision) -- so it MULTIPLIES; it does not set a common rate.
  #[test]
  fn scaling_preserves_the_spread_between_notes() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(1.0));
    a.note_on((2, 2), 20, Timbre::default(), Some(4.0));
    scale_factored_pulse_rate(&v, 0, 2.0, &all_edited(), &held());
    let g = v.lock().unwrap();
    let slow = g[&voice_key(0, (1, 1))].factored_pulse_freq;
    let fast = g[&voice_key(0, (2, 2))].factored_pulse_freq;
    assert_eq!((slow, fast), (2.0, 8.0));
    assert_eq!(fast / slow, 4.0, "the ratio between them is preserved");
  }

  /// The phase must be kept, or the factored pulse steps and clicks. This is what
  /// makes retuning a live note safe at all.
  #[test]
  fn scaling_the_factored_pulse_keeps_the_phase() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    v.lock().unwrap().get_mut(&voice_key(0, (1, 1))).unwrap().factored_pulse_phase = 0.37;
    scale_factored_pulse_rate(&v, 0, 3.0, &all_edited(), &held());
    assert_eq!(
      v.lock().unwrap()[&voice_key(0, (1, 1))].factored_pulse_phase,
      0.37,
      "the factored pulse continues from where it was: a slope change, not a step",
    );
  }

  /// A note struck while cycling was off has no factored pulse, and multiplying zero
  /// cannot start one -- it is meant to stay un-pulsed.
  #[test]
  fn scaling_cannot_start_a_factored_pulse_on_an_unpulsed_note() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), None);
    scale_factored_pulse_rate(&v, 0, 4.0, &all_edited(), &held());
    assert_eq!(v.lock().unwrap()[&voice_key(0, (1, 1))].factored_pulse_freq, 0.0);
  }

  #[test]
  fn scaling_one_grids_factored_pulse_leaves_the_other_grid_alone() {
    let v = shared();
    let mut a = sink(0, &v);
    let mut b = sink(1, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    b.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    scale_factored_pulse_rate(&v, 0, 2.0, &all_edited(), &held());
    let g = v.lock().unwrap();
    assert_eq!(g[&voice_key(0, (1, 1))].factored_pulse_freq, 4.0);
    assert_eq!(g[&voice_key(1, (1, 1))].factored_pulse_freq, 2.0, "grid 1 untouched");
  }

  /// Sustained drones are edited too -- an edited note usually IS a drone, since
  /// accrete is how you free your hands to edit it.
  #[test]
  fn scaling_reaches_this_grids_sustained_drones() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    a.sustain_note((1, 1), 10);
    scale_factored_pulse_rate(&v, 0, 2.0, &[10].into_iter().collect(), &held());
    let g = v.lock().unwrap();
    assert!(
      g.values().any(|s| s.factored_pulse_freq == 4.0),
      "the drone's factored pulse was retuned",
    );
  }

  #[test]
  fn a_nonsense_ratio_is_ignored_rather_than_wrecking_the_voice() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    scale_factored_pulse_rate(&v, 0, 0.0, &all_edited(), &held());
    scale_factored_pulse_rate(&v, 0, f32::NAN, &all_edited(), &held());
    scale_factored_pulse_rate(&v, 0, -1.0, &all_edited(), &held());
    assert_eq!(v.lock().unwrap()[&voice_key(0, (1, 1))].factored_pulse_freq, 2.0);
  }

  // ---- set_factored_pulse_rate_at: =1's start/stop of an edit-mode voice's pulse ----

  /// The headline behavior: unlike the multiplier, SETTING reaches an un-pulsed
  /// voice (0.0 -> nonzero starts it) as well as a pulsing one (nonzero -> 0.0
  /// stops it).
  #[test]
  fn setting_the_factored_pulse_rate_can_start_or_stop_an_unpulsed_voice() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), None); // struck with cycling off: no pulse
    set_factored_pulse_rate_at(&v, 0, &[10].into_iter().collect(), &held(), 6.0);
    assert_eq!(
      v.lock().unwrap()[&voice_key(0, (1, 1))].factored_pulse_freq, 6.0,
      "=1 turning cycling ON starts the edit-mode voice's pulse",
    );
    set_factored_pulse_rate_at(&v, 0, &[10].into_iter().collect(), &held(), 0.0);
    assert_eq!(
      v.lock().unwrap()[&voice_key(0, (1, 1))].factored_pulse_freq, 0.0,
      "=1 turning cycling OFF stops it again",
    );
  }

  #[test]
  fn setting_the_factored_pulse_rate_touches_only_the_wanted_pitches() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    a.note_on((2, 2), 20, Timbre::default(), Some(3.0));
    set_factored_pulse_rate_at(&v, 0, &[10].into_iter().collect(), &held(), 9.0);
    let g = v.lock().unwrap();
    assert_eq!(g[&voice_key(0, (1, 1))].factored_pulse_freq, 9.0, "the edited note is SET");
    assert_eq!(g[&voice_key(0, (2, 2))].factored_pulse_freq, 3.0, "the other note is untouched");
  }

  #[test]
  fn setting_the_factored_pulse_rate_keeps_the_phase() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), None);
    v.lock().unwrap().get_mut(&voice_key(0, (1, 1))).unwrap().factored_pulse_phase = 0.61;
    set_factored_pulse_rate_at(&v, 0, &all_edited(), &held(), 5.0);
    assert_eq!(
      v.lock().unwrap()[&voice_key(0, (1, 1))].factored_pulse_phase, 0.61,
      "starting a pulse mid-note is a slope change: the phase does not jump",
    );
  }

  #[test]
  fn setting_the_factored_pulse_rate_reaches_this_grids_sustained_drones() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), None);
    a.sustain_note((1, 1), 10);
    set_factored_pulse_rate_at(&v, 0, &[10].into_iter().collect(), &held(), 4.0);
    let g = v.lock().unwrap();
    assert!(
      g.values().any(|s| s.factored_pulse_freq == 4.0),
      "the drone's factored pulse was set",
    );
  }

  #[test]
  fn setting_one_grids_factored_pulse_leaves_the_other_grid_alone() {
    let v = shared();
    let mut a = sink(0, &v);
    let mut b = sink(1, &v);
    a.note_on((1, 1), 10, Timbre::default(), None);
    b.note_on((1, 1), 10, Timbre::default(), None);
    set_factored_pulse_rate_at(&v, 0, &all_edited(), &held(), 4.0);
    let g = v.lock().unwrap();
    assert_eq!(g[&voice_key(0, (1, 1))].factored_pulse_freq, 4.0);
    assert_eq!(g[&voice_key(1, (1, 1))].factored_pulse_freq, 0.0, "grid 1 untouched");
  }

  #[test]
  fn setting_the_factored_pulse_rate_with_no_edited_pitches_is_a_noop() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    set_factored_pulse_rate_at(&v, 0, &HashSet::new(), &held(), 9.0);
    assert_eq!(v.lock().unwrap()[&voice_key(0, (1, 1))].factored_pulse_freq, 2.0);
  }

  #[test]
  fn a_nonsense_hz_is_ignored_rather_than_wrecking_the_voice() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 10, Timbre::default(), Some(2.0));
    set_factored_pulse_rate_at(&v, 0, &all_edited(), &held(), f32::NAN);
    set_factored_pulse_rate_at(&v, 0, &all_edited(), &held(), -1.0);
    assert_eq!(v.lock().unwrap()[&voice_key(0, (1, 1))].factored_pulse_freq, 2.0);
  }

  // ---- the drone's reasons are sustain and edit, never "a finger is down" ----

  /// Exiting edit mode used to skip the cut whenever ANY finger held that pitch. That
  /// conflates two different voices: the finger's (keyed by cell) and the drone's
  /// (keyed by pitch). A drone can exist while a finger is also down -- lift one of
  /// two fingers on the same pitch and the lifted one becomes a drone -- and skipping
  /// the cut then stranded it: audible forever, with nothing left able to cut it.
  #[test]
  fn a_drone_and_a_finger_can_sound_the_same_pitch_at_once() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 20, Timbre::default(), None);
    a.note_on((2, 2), 20, Timbre::default(), None); // a second cell, same pitch
    a.sustain_note((1, 1), 20); // the first finger lifts -> drone

    let g = v.lock().unwrap();
    assert!(g.contains_key(&sustain_key(0, 20)), "the lifted finger left a drone");
    assert!(g.contains_key(&voice_key(0, (2, 2))), "the other finger is still its own voice");
  }

  /// So cutting the drone must not depend on whether a finger is down: it is a
  /// different voice, and it survives untouched.
  #[test]
  fn cutting_the_drone_leaves_a_finger_on_the_same_pitch_alone() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 20, Timbre::default(), None);
    a.note_on((2, 2), 20, Timbre::default(), None);
    a.sustain_note((1, 1), 20);

    a.cut_sustained(20);
    let g = v.lock().unwrap();
    assert!(!g.contains_key(&sustain_key(0, 20)), "the drone is gone");
    let finger = &g[&voice_key(0, (2, 2))];
    assert!(finger.target_env > 0.0, "the finger's own voice is untouched");
  }

  /// And the case that makes "exit while still holding" work with no special case:
  /// a note with only a finger has no drone, so the cut is simply a no-op.
  #[test]
  fn cutting_a_pitch_with_no_drone_is_a_no_op() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((1, 1), 20, Timbre::default(), None);
    a.cut_sustained(20);
    let g = v.lock().unwrap();
    assert!(g[&voice_key(0, (1, 1))].target_env > 0.0, "the fingered note plays on");
  }

  // ---- a fingered voice is ended by its CELL, so scrolling cannot orphan it ----

  /// Jeff's worry: "we need to be sure that scrolling and then lifting the finger still
  /// ends the note." It does, and this is why. A fingered voice is keyed by the cell it
  /// was struck on, and `note_off` takes only that cell -- the register is not a
  /// parameter of any voice operation, so scrolling cannot reach a sounding voice.
  ///
  /// The pitch a cell was struck at is deliberately varied here to stand in for a
  /// scroll having changed what the cell means: the release must not care.
  #[test]
  fn note_off_ends_the_voice_at_a_cell_whatever_pitch_it_was_struck_at() {
    for pitch in [0, 20, -37, 500] {
      let v = shared();
      let mut a = sink(0, &v);
      a.note_on((3, 3), pitch, Timbre::default(), None);
      assert!(v.lock().unwrap()[&voice_key(0, (3, 3))].target_env > 0.0, "sounding");

      a.note_off((3, 3));
      assert!(
        v.lock().unwrap()[&voice_key(0, (3, 3))].target_env <= 0.0,
        "the cell's voice releases, struck-pitch {pitch} notwithstanding",
      );
    }
  }

  /// The stronger statement: releasing one cell touches only that cell's voice, even
  /// when another cell is sounding the very same pitch -- which is what a scroll can
  /// arrange, two fingers landing on one pitch from different cells.
  #[test]
  fn releasing_one_cell_leaves_another_cell_at_the_same_pitch_alone() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), None);
    a.note_on((4, 12), 20, Timbre::default(), None); // same pitch, other cell

    a.note_off((3, 3));
    assert!(v.lock().unwrap()[&voice_key(0, (3, 3))].target_env <= 0.0, "the lifted one releases");
    assert!(v.lock().unwrap()[&voice_key(0, (4, 12))].target_env > 0.0, "the other still sounds");
  }

  // ---- rehome_to_cell: a dragged voice is adopted by the drag finger ----

  /// Jeff's bug: "press it, put it into edit mode, and drag it, then take it out of
  /// edit mode, it dies -- even if the button I pressed to drag it is still depressed."
  ///
  /// The drag finger lifted from the original key, so the voice became a drone; the
  /// drag then moved that drone but never tied it to the finger now on the new key.
  /// Exit cut the drone. Re-homing the voice to the drag cell makes it a fingered voice
  /// there, which cut_sustained cannot touch.
  #[test]
  fn a_dragged_drone_becomes_fingered_and_survives_an_exit() {
    let v = shared();
    let mut a = sink(0, &v);
    // A voice was fingered, then its finger lifted while edited -> a drone at pitch 20.
    a.note_on((3, 3), 20, Timbre::default(), None);
    a.sustain_note((3, 3), 20);
    assert!(v.lock().unwrap().contains_key(&sustain_key(0, 20)), "it is a drone");

    // Drag it to pitch 35 by pressing cell (7,7): the finger on (7,7) adopts it.
    assert!(a.rehome_to_cell(None, 20, (7, 7), 35, 0.1));
    assert!(!v.lock().unwrap().contains_key(&sustain_key(0, 20)), "no longer a drone");
    assert!(v.lock().unwrap().contains_key(&voice_key(0, (7, 7))), "now fingered at the drag cell");

    // Exit edit mode tries to cut the drone at pitch 35 -- but there is none.
    a.cut_sustained(35);
    {
      let g = v.lock().unwrap();
      let st = &g[&voice_key(0, (7, 7))];
      // Alive: the drag's dip-then-attack leaves it at target_env 0 with an attack queued
      // for the moment it bottoms out, so aliveness is "sounding OR about to re-strike".
      assert!(
        st.target_env > 0.0 || st.pending_attack.is_some(),
        "the fingered voice survives the exit, because the drag finger holds it",
      );
    }

    // Lifting the drag cell ends it.
    a.note_off((7, 7));
    assert!(v.lock().unwrap()[&voice_key(0, (7, 7))].target_env <= 0.0, "and the lift ends it");
  }

  /// The other source: the original finger never lifted. Re-homing moves the voice off
  /// its old cell to the drag cell, so the drag finger owns it and the old cell is inert.
  #[test]
  fn a_dragged_fingered_voice_moves_to_the_drag_cell() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), None);

    assert!(a.rehome_to_cell(Some((3, 3)), 20, (7, 7), 35, 0.1));
    let g = v.lock().unwrap();
    assert!(!g.contains_key(&voice_key(0, (3, 3))), "gone from the old cell");
    assert!(g.contains_key(&voice_key(0, (7, 7))), "at the drag cell");
  }

  /// Re-homing preserves the factored pulse -- a drag must not silence a pulsing
  /// voice's pulse the way note_on_legato would.
  #[test]
  fn rehoming_keeps_the_factored_pulse() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), Some(3.0));
    let phase = {
      let g = v.lock().unwrap();
      g[&voice_key(0, (3, 3))].factored_pulse_phase
    };
    a.rehome_to_cell(Some((3, 3)), 20, (7, 7), 35, 0.1);
    let g = v.lock().unwrap();
    let st = &g[&voice_key(0, (7, 7))];
    assert_eq!(st.factored_pulse_freq, 3.0, "the pulse rate is kept");
    assert_eq!(st.factored_pulse_phase, phase, "and its phase does not jump");
  }

  /// A drag retriggers the pluck with a dip-then-attack, so each drag lands a punchy accent
  /// (Jeff). The drag first ramps the envelope DOWN toward silence and queues a fresh
  /// attack, which the engine launches once the dip bottoms out (see the sawwave test
  /// `a_pending_attack_dips_to_silence_then_re_strikes`).
  #[test]
  fn a_drag_sets_up_the_dip_then_attack_retrigger() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), None);
    // A voice mid-attack, well above silence.
    {
      let mut g = v.lock().unwrap();
      let st = g.get_mut(&voice_key(0, (3, 3))).unwrap();
      st.env = 0.5;
      st.target_env = 1.0;
      st.pending_attack = None;
    }
    a.rehome_to_cell(Some((3, 3)), 20, (7, 7), 35, 0.1);
    let g = v.lock().unwrap();
    let st = &g[&voice_key(0, (7, 7))];
    assert_eq!(st.target_env, 0.0, "the drag first ramps the envelope DOWN toward silence");
    assert!(st.pending_attack.is_some(), "with a fresh attack queued to launch at the bottom");
    assert!(st.env > 0.0, "the dip starts from the current level, not an instant jump -- no click");
  }

  #[test]
  fn rehoming_glides_rather_than_jumping() {
    let v = shared();
    let mut a = sink(0, &v);
    a.note_on((3, 3), 20, Timbre::default(), None);
    let before = v.lock().unwrap()[&voice_key(0, (3, 3))].freq;
    a.rehome_to_cell(Some((3, 3)), 20, (7, 7), 40, 0.1);
    let g = v.lock().unwrap();
    let st = &g[&voice_key(0, (7, 7))];
    assert_eq!(st.freq, before, "it starts gliding from where it was, not jumping");
    assert!(st.glide_per_sample > 1.0, "gliding up toward the new pitch");
  }

  #[test]
  fn rehoming_a_voice_that_is_not_there_is_false() {
    let v = shared();
    let mut a = sink(0, &v);
    assert!(!a.rehome_to_cell(Some((9, 9)), 20, (7, 7), 35, 0.1));
    assert!(!a.rehome_to_cell(None, 99, (7, 7), 35, 0.1));
  }
}
