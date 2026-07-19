//! Chord storage (TODO/chord-storage-v2): the per-grid chord layer -- nine slots of
//! stored voice snapshots, the arm switch, and the registry of live (recalled) chord
//! voices.
//!
//! A stored chord is a bag of VOICE snapshots, not a pitch set (2_discussion "the
//! model"): each voice carries its own timbre, its settled gain components, and its
//! factored-pulse factor, so two slots (or a slot and the piano layer) may hold the
//! same pitch. Recalled voices are the CHORD layer -- they coexist with fingered and
//! sustained voices at the same pitch and are never swallowed by them (Jeff: chords
//! are "complicated multi-voice scenes", not piano keys). Their reasons are per
//! voice, so nothing here touches `RingStore`'s pitch sets.
//!
//! Everything in this module is pure state logic (no locks, no I/O): the grid thread
//! (keys.rs) decides-then-acts around it, and the synth (synth.rs) owns the voices.

use std::collections::HashMap;

use crate::types::{Timbre, VoiceState};

/// The number of chord slots per grid: the block's 10 cells minus the arm button.
pub const SLOTS: usize = 9;

/// One stored voice: everything needed to re-sound it exactly as saved, except the
/// envelope (a recall is a strike -- attack + pluck run from the top) and anything
/// that is grid state rather than voice state (2_discussion "what not to store").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StoredVoice {
  /// Absolute EDO step -- the pitch, never the cell, so a recall ignores scrolling.
  /// For a voice saved mid-glide this is the glide's TARGET (the held map and the
  /// registry are re-filed on drag, so the capture reads the target for free).
  pub pitch: i32,
  /// The full timbre slot, mutations included -- NOT re-resolved from the rig.
  pub timbre: Timbre,
  /// The settled gain components at save time, stored separately (not baked into
  /// `timbre.gain`) so a recalled voice renders through the exact same chain as a
  /// live one.
  pub fader_gain: f32,
  pub pedal_gain: f32,
  /// Oscillator phase RELATIVE to the chord's timesetter, in [0,1). The timesetter
  /// itself stores 0 and every recall starts it there, reproducing the save-instant
  /// waveform configuration (Jeff's phase reconsideration in 2_discussion).
  pub osc_phase: f32,
  /// The factored pulse as a FACTOR against the base tempo at save time (rate /
  /// base); recall multiplies by the base at recall, so a retapped tempo carries
  /// chords along. 0 = no pulse, and a recall cannot start one.
  pub pulse_factor: f32,
  /// Pulse phase relative to the chord's pulse reference, in [0,1); 0 for unpulsed
  /// voices.
  pub pulse_phase: f32,
}

/// One slot's stored chord. Never empty: saving silence leaves the slot unchanged
/// (2_discussion: "no need for empty, only overwrite").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoredChord {
  pub voices: Vec<StoredVoice>,
}

/// One LIVE chord voice (a recalled `StoredVoice`), keyed in the voice map by
/// `VoiceSource::SurfaceChord { grid, seq }`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveChordVoice {
  /// The slot this voice's recall came from (for the slot's solid LED and its
  /// toggle-off).
  pub slot: usize,
  /// The voice's CURRENT pitch -- re-filed when a pitch drag moves it.
  pub pitch: i32,
  /// Whether this voice is in the edit selection. Chord voices carry their own edit
  /// flag (never `RingStore`'s edited set): editing one adds no sustain reason
  /// (Jeff's round-2 correction -- the edited ⊆ sustained invariant is the PIANO
  /// layer's).
  pub edited: bool,
}

/// One grid's chord layer, alongside its accrete bank and edit machine in
/// [`super::ring::GridRing`] (same single lock).
#[derive(Debug, Default)]
pub struct ChordLayer {
  pub slots: [Option<StoredChord>; SLOTS],
  /// Write-arm: while true the next slot press SAVES (then disarms). Sticky across
  /// note presses; a second arm press disarms.
  pub armed: bool,
  /// Which slots are toggled ON (lit solid; their recall's voices ring).
  pub active: [bool; SLOTS],
  /// The live chord voices, by their key's `seq`.
  pub live: HashMap<u64, LiveChordVoice>,
  next_seq: u64,
}

/// What a press inside the chord block hits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockCell {
  Arm,
  Slot(usize),
}

/// Map a cell to its role in the 5x2 block: top row = slots 0..=4, bottom row = ARM
/// (bottom-left) then slots 5..=8. None outside the rect (including the `NO_RECT`
/// sentinel, which contains no cell).
pub fn block_cell(rect: [i32; 4], cell: (i32, i32)) -> Option<BlockCell> {
  let [x0, y0, x1, y1] = rect;
  let (x, y) = cell;
  if x < x0 || x > x1 || y < y0 || y > y1 {
    return None;
  }
  let dx = (x - x0) as usize;
  if y == y0 {
    Some(BlockCell::Slot(dx))
  } else if dx == 0 {
    Some(BlockCell::Arm)
  } else {
    Some(BlockCell::Slot(4 + dx))
  }
}

/// The block-relative cell of slot `i` (inverse of `block_cell`), for the painter.
pub fn slot_cell(rect: [i32; 4], slot: usize) -> (i32, i32) {
  let [x0, y0, ..] = rect;
  if slot < 5 {
    (x0 + slot as i32, y0)
  } else {
    (x0 + (slot - 4) as i32, y0 + 1)
  }
}

/// The arm button's cell: the block's bottom-left.
pub fn arm_cell(rect: [i32; 4]) -> (i32, i32) {
  (rect[0], rect[1] + 1)
}

impl ChordLayer {
  pub fn new() -> Self {
    ChordLayer::default()
  }

  /// The live chord voices' current pitches (for reflection, handles, capture).
  pub fn live_pitches(&self) -> impl Iterator<Item = i32> + '_ {
    self.live.values().map(|v| v.pitch)
  }

  /// Store `chord` into `slot` and disarm. An EMPTY capture only disarms -- the slot
  /// keeps its contents ("saving emptiness ... leaves the slot unchanged"). Ringing
  /// voices from this very slot are untouched either way: overwriting an active slot
  /// leaves what you just captured sounding, lit solid; the next off/on plays the
  /// new contents.
  pub fn save(&mut self, slot: usize, chord: StoredChord) {
    self.armed = false;
    if !chord.voices.is_empty() {
      self.slots[slot] = Some(chord);
    }
  }

  /// Toggle-ON bookkeeping: allocate a seq per stored voice and register each as a
  /// live chord voice (not edited). Returns `(seq, voice)` pairs for the caller to
  /// spawn, or an empty vec when the slot is empty (an empty slot is inert -- it
  /// stays dim and does not toggle). No-op if the slot is already active.
  pub fn begin_recall(&mut self, slot: usize) -> Vec<(u64, StoredVoice)> {
    if self.active[slot] {
      return Vec::new();
    }
    let Some(chord) = self.slots[slot].clone() else {
      return Vec::new();
    };
    self.active[slot] = true;
    chord
      .voices
      .into_iter()
      .map(|v| {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.live.insert(seq, LiveChordVoice { slot, pitch: v.pitch, edited: false });
        (seq, v)
      })
      .collect()
  }

  /// Toggle-OFF bookkeeping: unregister this slot's live voices and return their
  /// seqs for the caller to end (release ramp).
  pub fn end_recall(&mut self, slot: usize) -> Vec<u64> {
    self.active[slot] = false;
    let seqs: Vec<u64> =
      self.live.iter().filter(|(_, v)| v.slot == slot).map(|(s, _)| *s).collect();
    for s in &seqs {
      self.live.remove(s);
    }
    seqs
  }

  /// The widened clear ("end all sustain and chord"): unregister EVERY live chord
  /// voice and untoggle every slot; returns all seqs for the caller to end.
  pub fn end_all(&mut self) -> Vec<u64> {
    self.active = [false; SLOTS];
    let seqs: Vec<u64> = self.live.keys().copied().collect();
    self.live.clear();
    seqs
  }
}

/// Build a [`StoredChord`] from the captured voices' live states (2_discussion "the
/// timesetter"). `parts` is every captured voice as (current pitch, its `VoiceState`
/// snapshot); `base_hz` is the base tempo at save time (pulse rates store as factors
/// against it; a non-positive base stores every pulse as 0 -- unreachable, since the
/// runtime always seeds a base).
///
/// Timesetter = the voice furthest through its cycle (largest oscillator phase);
/// ties -> lowest pitch -> first captured. The pulse reference is the timesetter if
/// it pulses, else the pulsed voice chosen by the same rule over pulse phases.
/// Envelope state is deliberately not read: a recall re-strikes. The pedal component
/// reads `grid_gain_target` (the settled aim), not the mid-slew `grid_gain`.
pub fn snapshot(parts: &[(i32, VoiceState)], base_hz: f32) -> StoredChord {
  if parts.is_empty() {
    return StoredChord::default();
  }
  let best_by = |phase_of: &dyn Fn(&VoiceState) -> f32, only_pulsed: bool| -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, (pitch, state)) in parts.iter().enumerate() {
      if only_pulsed && state.factored_pulse_freq <= 0.0 {
        continue;
      }
      best = match best {
        None => Some(i),
        Some(b) => {
          let (bp, bs) = (&parts[b].0, &parts[b].1);
          let better = phase_of(state) > phase_of(bs)
            || (phase_of(state) == phase_of(bs) && pitch < bp);
          if better { Some(i) } else { Some(b) }
        }
      };
    }
    best
  };
  let timesetter = best_by(&|s: &VoiceState| s.phase, false).expect("parts is non-empty");
  let osc_zero = parts[timesetter].1.phase;
  let pulse_ref = if parts[timesetter].1.factored_pulse_freq > 0.0 {
    Some(timesetter)
  } else {
    best_by(&|s: &VoiceState| s.factored_pulse_phase, true)
  };
  let pulse_zero = pulse_ref.map(|i| parts[i].1.factored_pulse_phase).unwrap_or(0.0);

  let voices = parts
    .iter()
    .map(|(pitch, state)| {
      let pulsed = state.factored_pulse_freq > 0.0 && base_hz > 0.0;
      StoredVoice {
        pitch: *pitch,
        timbre: state.timbre,
        fader_gain: state.fader_gain,
        pedal_gain: state.grid_gain_target,
        osc_phase: (state.phase - osc_zero).rem_euclid(1.0),
        pulse_factor: if pulsed { state.factored_pulse_freq / base_hz } else { 0.0 },
        pulse_phase: if pulsed {
          (state.factored_pulse_phase - pulse_zero).rem_euclid(1.0)
        } else {
          0.0
        },
      }
    })
    .collect();
  StoredChord { voices }
}

#[cfg(test)]
mod tests {
  use super::*;

  const RECT: [i32; 4] = [5, 0, 9, 1];

  fn state(phase: f32, pulse_freq: f32, pulse_phase: f32) -> VoiceState {
    VoiceState {
      id: 0,
      freq: 100.0,
      phase,
      env: 0.5,
      target_env: 1.0,
      ramp_per_sample: 0.0,
      pending_attack: None,
      sustain_env: 1.0,
      decay_per_sample: 1.0,
      freq_target: 0.0,
      glide_per_sample: 1.0,
      factored_pulse_freq: pulse_freq,
      factored_pulse_phase: pulse_phase,
      fader_gain: 1.0,
      grid_gain: 1.0,
      grid_gain_target: 1.0,
      timbre: Timbre::default(),
      am_phase: 0.0,
      fm_phase: 0.0,
      rel_am_phase: 0.0,
      rel_fm_phase: 0.0,
    }
  }

  #[test]
  fn block_cells_map_slots_and_the_arm_corner() {
    // Top row: slots 0..=4.
    assert_eq!(block_cell(RECT, (5, 0)), Some(BlockCell::Slot(0)));
    assert_eq!(block_cell(RECT, (9, 0)), Some(BlockCell::Slot(4)));
    // Bottom row: arm at the left, then slots 5..=8.
    assert_eq!(block_cell(RECT, (5, 1)), Some(BlockCell::Arm));
    assert_eq!(block_cell(RECT, (6, 1)), Some(BlockCell::Slot(5)));
    assert_eq!(block_cell(RECT, (9, 1)), Some(BlockCell::Slot(8)));
    // Outside.
    assert_eq!(block_cell(RECT, (4, 0)), None);
    assert_eq!(block_cell(RECT, (5, 2)), None);
    // NO_RECT contains nothing.
    assert_eq!(block_cell([-1, -1, -1, -1], (0, 0)), None);
    // slot_cell is the inverse.
    for slot in 0..SLOTS {
      assert_eq!(block_cell(RECT, slot_cell(RECT, slot)), Some(BlockCell::Slot(slot)));
    }
    assert_eq!(arm_cell(RECT), (5, 1));
  }

  #[test]
  fn snapshot_picks_the_furthest_phase_as_timesetter_and_stores_relative_phases() {
    // Voice at phase 0.75 is furthest -> timesetter (stores 0); the 0.25 voice
    // stores 0.5 relative (0.25 - 0.75 mod 1).
    let parts = vec![(10, state(0.25, 0.0, 0.0)), (20, state(0.75, 0.0, 0.0))];
    let chord = snapshot(&parts, 1.0);
    assert_eq!(chord.voices[1].osc_phase, 0.0, "the timesetter starts at 0");
    assert_eq!(chord.voices[0].osc_phase, 0.5, "relative phase preserved");
  }

  #[test]
  fn snapshot_breaks_phase_ties_toward_the_lowest_pitch() {
    let parts = vec![(20, state(0.5, 0.0, 0.0)), (10, state(0.5, 0.0, 0.0))];
    let chord = snapshot(&parts, 1.0);
    assert_eq!(chord.voices[1].osc_phase, 0.0, "pitch 10 wins the tie");
    assert_eq!(chord.voices[0].osc_phase, 0.0, "same phase -> same relative offset");
  }

  #[test]
  fn snapshot_stores_pulse_factors_against_the_base_and_relative_pulse_phases() {
    // Base 2 Hz: a 3 Hz pulse stores factor 1.5. The timesetter (phase 0.9) is
    // unpulsed, so the pulse reference falls to the pulsed voice with the largest
    // pulse phase (0.6): it stores pulse_phase 0, the other 0.2 - 0.6 = 0.6 mod 1.
    let parts = vec![
      (10, state(0.9, 0.0, 0.0)),   // timesetter, unpulsed
      (20, state(0.1, 3.0, 0.6)),   // pulse reference
      (30, state(0.2, 4.0, 0.2)),
    ];
    let chord = snapshot(&parts, 2.0);
    assert_eq!(chord.voices[0].pulse_factor, 0.0, "unpulsed stays unpulsed");
    assert_eq!(chord.voices[1].pulse_factor, 1.5);
    assert_eq!(chord.voices[2].pulse_factor, 2.0);
    assert_eq!(chord.voices[1].pulse_phase, 0.0, "the pulse reference starts at 0");
    assert!((chord.voices[2].pulse_phase - 0.6).abs() < 1e-6, "relative pulse phase");
  }

  #[test]
  fn snapshot_reads_the_settled_pedal_aim_not_the_mid_slew_value() {
    let mut s = state(0.0, 0.0, 0.0);
    s.grid_gain = 0.4; // mid-slew
    s.grid_gain_target = 0.7; // the settled aim
    s.fader_gain = 0.9;
    let chord = snapshot(&[(10, s)], 1.0);
    assert_eq!(chord.voices[0].pedal_gain, 0.7);
    assert_eq!(chord.voices[0].fader_gain, 0.9);
  }

  #[test]
  fn save_overwrites_only_with_a_non_empty_capture_and_always_disarms() {
    let mut layer = ChordLayer::new();
    layer.armed = true;
    let one = StoredChord { voices: vec![StoredVoice {
      pitch: 5, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
      osc_phase: 0.0, pulse_factor: 0.0, pulse_phase: 0.0,
    }] };
    layer.save(2, one.clone());
    assert!(!layer.armed, "saving disarms");
    assert_eq!(layer.slots[2], Some(one.clone()));
    // Saving silence: disarm, slot untouched.
    layer.armed = true;
    layer.save(2, StoredChord::default());
    assert!(!layer.armed, "saving silence still disarms");
    assert_eq!(layer.slots[2], Some(one), "and leaves the slot unchanged");
  }

  #[test]
  fn recall_toggles_register_and_unregister_live_voices() {
    let mut layer = ChordLayer::new();
    let chord = StoredChord { voices: vec![
      StoredVoice { pitch: 5, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
                    osc_phase: 0.0, pulse_factor: 0.0, pulse_phase: 0.0 },
      StoredVoice { pitch: 9, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
                    osc_phase: 0.5, pulse_factor: 0.0, pulse_phase: 0.0 },
    ] };
    layer.save(0, chord);
    let spawned = layer.begin_recall(0);
    assert_eq!(spawned.len(), 2);
    assert!(layer.active[0]);
    assert_eq!(layer.live.len(), 2);
    assert!(layer.live.values().all(|v| v.slot == 0 && !v.edited));
    // A second ON press while active is a no-op at this level (the caller toggles
    // OFF instead, but even called directly nothing doubles).
    assert!(layer.begin_recall(0).is_empty(), "already active: nothing re-spawns");
    assert_eq!(layer.live.len(), 2);
    // Toggle off: the same seqs come back and the registry empties.
    let mut ended = layer.end_recall(0);
    ended.sort_unstable();
    let mut want: Vec<u64> = spawned.iter().map(|(s, _)| *s).collect();
    want.sort_unstable();
    assert_eq!(ended, want);
    assert!(!layer.active[0]);
    assert!(layer.live.is_empty());
  }

  #[test]
  fn an_empty_slot_is_inert() {
    let mut layer = ChordLayer::new();
    assert!(layer.begin_recall(3).is_empty());
    assert!(!layer.active[3], "an empty slot never toggles on");
  }

  #[test]
  fn end_all_untoggles_everything_and_returns_every_live_seq() {
    let mut layer = ChordLayer::new();
    let v = StoredVoice { pitch: 5, timbre: Timbre::default(), fader_gain: 1.0,
                          pedal_gain: 1.0, osc_phase: 0.0, pulse_factor: 0.0, pulse_phase: 0.0 };
    layer.save(0, StoredChord { voices: vec![v] });
    layer.save(4, StoredChord { voices: vec![v, v] });
    layer.begin_recall(0);
    layer.begin_recall(4);
    let ended = layer.end_all();
    assert_eq!(ended.len(), 3, "every live voice, both slots");
    assert!(layer.live.is_empty());
    assert!(!layer.active[0] && !layer.active[4]);
    assert!(layer.slots[0].is_some() && layer.slots[4].is_some(), "contents survive");
  }
}
