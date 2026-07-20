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
  /// Oscillator phase at the start of the BASE CYCLE, in [0,1). The base is the
  /// chord's longest-period (lowest-frequency) voice; a recall replays the ensemble
  /// from the base-cycle boundary NEAREST the save instant (just passed or just
  /// ahead) -- the base begins at exactly 0, and every other voice begins wherever
  /// it was, in its own cycle AND relative to the base cycle, at that instant
  /// (queues/branch-2.org correction: "longest phase -> longest period";
  /// "everything sounds the same relative to the base cycle").
  pub osc_phase: f32,
  /// The factored pulse as a FACTOR against the base tempo at save time (rate /
  /// base); recall multiplies by the base at recall, so a retapped tempo carries
  /// chords along. 0 = no pulse, and a recall cannot start one.
  pub pulse_factor: f32,
  /// Pulse phase at the start of the base cycle (the SAME time anchor as
  /// `osc_phase` -- one coherent instant), in [0,1); 0 for unpulsed voices.
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

  /// The LOCAL end-sustain's chord half (queues/branch-2.org: "local end sustain
  /// should also end a chord voice there -- for those controls, the origin doesn't
  /// matter"): unregister every live chord voice at `pitch`, untoggle any slot this
  /// leaves voiceless (a solid LED over silence would lie), and return the seqs for
  /// the caller to end. Voices at other pitches -- same slot or not -- are spared.
  pub fn end_at_pitch(&mut self, pitch: i32) -> Vec<u64> {
    let seqs: Vec<u64> =
      self.live.iter().filter(|(_, v)| v.pitch == pitch).map(|(s, _)| *s).collect();
    for s in &seqs {
      self.live.remove(s);
    }
    for slot in 0..SLOTS {
      if self.active[slot] && !self.live.values().any(|v| v.slot == slot) {
        self.active[slot] = false;
      }
    }
    seqs
  }
}

/// Build a [`StoredChord`] from the captured voices' live states. `parts` is every
/// captured voice as (current pitch, its `VoiceState` snapshot); `base_hz` is the
/// base tempo at save time (pulse rates store as factors against it; a non-positive
/// base stores every pulse as 0 -- unreachable, since the runtime always seeds a
/// base); `fund`/`edo` are the tuning, for the pitch -> frequency map.
///
/// *The base cycle* (queues/branch-2.org correction, superseding 2_discussion's
/// largest-phase timesetter): the base is the LONGEST-PERIOD voice -- the lowest
/// pitch (ties: the voice closest to its cycle boundary in EITHER direction --
/// smallest `min(phase, 1 - phase)` -- then first captured; the caller feeds
/// `parts` pitch-sorted, so this is deterministic). A recall replays the ensemble
/// from the base-cycle boundary NEAREST the save instant -- the start just passed,
/// or the one just ahead (`dt` below is signed: positive rewinds, negative
/// advances; the tie rule's "either direction" is what makes the nearest boundary
/// the right anchor). Each voice's phase (and each pulse's phase -- the same
/// `dt`, one coherent instant) is stored as of that boundary. The base therefore
/// stores exactly 0, and every other voice stores where it was both in its own
/// cycle and relative to the base cycle -- so everything sounds the same relative
/// to the base cycle, as close as possible to how it sounded when saved.
///
/// Frequencies come from the stored PITCH (what a recall will sound), not the
/// possibly-mid-glide live `freq`. Envelope state is deliberately not read: a
/// recall re-strikes. The pedal component reads `grid_gain_target` (the settled
/// aim), not the mid-slew `grid_gain`.
pub fn snapshot(parts: &[(i32, VoiceState)], base_hz: f32, fund: f64, edo: i32) -> StoredChord {
  if parts.is_empty() {
    return StoredChord::default();
  }
  let freq = |p: i32| crate::pitch::freq_for_pitch(p, fund, edo);
  let boundary_dist = |ph: f32| ph.min(1.0 - ph);
  let base = parts
    .iter()
    .enumerate()
    .min_by(|(_, (pa, sa)), (_, (pb, sb))| {
      pa.cmp(pb).then(boundary_dist(sa.phase).total_cmp(&boundary_dist(sb.phase)))
    })
    .map(|(i, _)| i)
    .expect("parts is non-empty");
  let (base_pitch, base_state) = &parts[base];
  // The signed shift to the base's NEAREST cycle boundary, in seconds: rewind to
  // the start just passed (phase <= 0.5), or advance to the one just ahead.
  let ph = base_state.phase;
  let dt = (if ph <= 0.5 { ph } else { ph - 1.0 }) / freq(*base_pitch);

  let voices = parts
    .iter()
    .enumerate()
    .map(|(i, (pitch, state))| {
      let pulsed = state.factored_pulse_freq > 0.0 && base_hz > 0.0;
      StoredVoice {
        pitch: *pitch,
        timbre: state.timbre,
        fader_gain: state.fader_gain,
        pedal_gain: state.grid_gain_target,
        // Exactly 0 for the base itself (not `x - x` through floats, which could
        // round to a hair below 0 and wrap to ~1).
        osc_phase: if i == base {
          0.0
        } else {
          (state.phase - dt * freq(*pitch)).rem_euclid(1.0)
        },
        pulse_factor: if pulsed { state.factored_pulse_freq / base_hz } else { 0.0 },
        pulse_phase: if pulsed {
          (state.factored_pulse_phase - dt * state.factored_pulse_freq).rem_euclid(1.0)
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
      grid_gain_target: 1.0, slide_freq_target: 0.0,
      timbre: Timbre::default(),
      am_phase: 0.0,
      fm_phase: 0.0,
      rel_am_phase: 0.0,
      rel_fm_phase: 0.0,
      timbre_xfade: None,
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

  // Tuning for the snapshot tests: fund 100 Hz, 12-EDO, so pitch 0 = 100 Hz and
  // pitch 12 = 200 Hz -- an exact octave makes the rewind math legible.
  const FUND: f64 = 100.0;
  const EDO: i32 = 12;

  #[test]
  fn snapshot_rewinds_everything_to_the_start_of_the_longest_period_voices_cycle() {
    // The BASE is the lowest pitch (longest period), NOT the furthest phase
    // (queues/branch-2.org correction). Base: pitch 0 at phase 0.25 -> the save
    // instant rewinds by dt = 0.25 / 100 Hz. The octave voice (200 Hz, phase 0.9)
    // rewinds by dt * 200 = 0.5 of its own cycle: it stores 0.4 -- where it was,
    // in itself and relative to the base cycle, when the base cycle began.
    let parts = vec![(0, state(0.25, 0.0, 0.0)), (12, state(0.9, 0.0, 0.0))];
    let chord = snapshot(&parts, 1.0, FUND, EDO);
    assert_eq!(chord.voices[0].osc_phase, 0.0, "the base cycle starts at exactly 0");
    assert!((chord.voices[1].osc_phase - 0.4).abs() < 1e-6, "rewound in TIME, not rotated");
  }

  #[test]
  fn a_base_already_at_its_cycle_start_leaves_every_phase_as_saved() {
    // dt = 0: the save instant IS the base cycle's start, so every voice stores
    // exactly the phase it was saved at.
    let parts = vec![(0, state(0.0, 0.0, 0.0)), (7, state(0.33, 0.0, 0.0)), (12, state(0.8, 0.0, 0.0))];
    let chord = snapshot(&parts, 1.0, FUND, EDO);
    assert_eq!(chord.voices[0].osc_phase, 0.0);
    assert_eq!(chord.voices[1].osc_phase, 0.33);
    assert_eq!(chord.voices[2].osc_phase, 0.8);
  }

  #[test]
  fn snapshot_breaks_lowest_pitch_ties_toward_the_nearest_cycle_boundary() {
    // Two voices at the base pitch: phase 0.9 is 0.1 from its boundary (ahead),
    // phase 0.4 is 0.4 from its own (behind) -- 0.9 wins the tie. And its twin,
    // shifted by the same dt at the same frequency, keeps the saved 0.5 gap.
    let parts = vec![(0, state(0.4, 0.0, 0.0)), (0, state(0.9, 0.0, 0.0)), (12, state(0.1, 0.0, 0.0))];
    let chord = snapshot(&parts, 1.0, FUND, EDO);
    assert_eq!(chord.voices[1].osc_phase, 0.0, "the boundary-nearest co-pitch is the base");
    assert!((chord.voices[0].osc_phase - 0.5).abs() < 1e-6, "its twin keeps the 0.5 gap");
  }

  #[test]
  fn a_base_past_half_cycle_anchors_on_the_boundary_just_ahead() {
    // Base (pitch 0, 100 Hz) at phase 0.9: the nearest boundary is 0.1 of a cycle
    // AHEAD, so dt is a small ADVANCE (-0.1 cycles = -1 ms), not a 0.9-cycle
    // rewind. The octave voice (200 Hz, phase 0.15) advances by 0.2 of its own
    // cycle: it stores 0.35 -- the configuration at the upcoming base-cycle start.
    let parts = vec![(0, state(0.9, 0.0, 0.0)), (12, state(0.15, 0.0, 0.0))];
    let chord = snapshot(&parts, 1.0, FUND, EDO);
    assert_eq!(chord.voices[0].osc_phase, 0.0, "the base starts at 0 either way");
    assert!((chord.voices[1].osc_phase - 0.35).abs() < 1e-6, "advanced, not rewound");
  }

  #[test]
  fn snapshot_stores_pulse_factors_against_the_tempo_and_pulse_phases_at_the_same_anchor() {
    // Base tempo 2 Hz: a 3 Hz pulse stores factor 1.5, a 4 Hz one factor 2. The
    // pulses rewind by the SAME dt as the oscillators -- one coherent instant, no
    // separate pulse reference. Base voice: pitch 0 (100 Hz) at phase 0.5 -> dt =
    // 5 ms. The 3 Hz pulse rewinds 0.015 cycles (0.6 -> 0.585); the 4 Hz one 0.02
    // (0.2 -> 0.18).
    let parts = vec![
      (0, state(0.5, 0.0, 0.0)),    // the base, unpulsed -- no fallback needed
      (12, state(0.1, 3.0, 0.6)),
      (24, state(0.2, 4.0, 0.2)),
    ];
    let chord = snapshot(&parts, 2.0, FUND, EDO);
    assert_eq!(chord.voices[0].pulse_factor, 0.0, "unpulsed stays unpulsed");
    assert_eq!(chord.voices[1].pulse_factor, 1.5);
    assert_eq!(chord.voices[2].pulse_factor, 2.0);
    assert!((chord.voices[1].pulse_phase - 0.585).abs() < 1e-6, "rewound by the shared dt");
    assert!((chord.voices[2].pulse_phase - 0.18).abs() < 1e-6);
  }

  #[test]
  fn snapshot_reads_the_settled_pedal_aim_not_the_mid_slew_value() {
    let mut s = state(0.0, 0.0, 0.0);
    s.grid_gain = 0.4; // mid-slew
    s.grid_gain_target = 0.7; // the settled aim
    s.fader_gain = 0.9;
    let chord = snapshot(&[(10, s)], 1.0, FUND, EDO);
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
  fn end_at_pitch_is_origin_blind_and_untoggles_emptied_slots() {
    let mut layer = ChordLayer::new();
    let v = |pitch| StoredVoice {
      pitch, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
      osc_phase: 0.0, pulse_factor: 0.0, pulse_phase: 0.0,
    };
    layer.save(0, StoredChord { voices: vec![v(10), v(20)] });
    layer.save(1, StoredChord { voices: vec![v(10)] });
    layer.begin_recall(0);
    layer.begin_recall(1);
    let ended = layer.end_at_pitch(10);
    assert_eq!(ended.len(), 2, "voices at pitch 10 from BOTH slots end -- origin-blind");
    assert!(layer.active[0], "slot 0 keeps its pitch-20 voice and stays lit");
    assert!(!layer.active[1], "slot 1 was emptied -> untoggled (no solid LED over silence)");
    assert_eq!(layer.live.len(), 1);
    assert!(layer.live.values().all(|lv| lv.pitch == 20), "other pitches are spared");
    assert!(layer.slots[1].is_some(), "the stored chord itself survives");
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
