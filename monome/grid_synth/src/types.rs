//! All the data shapes used by the app. No impls, no functions.
//! Look here when you need to know what a thing IS; look at the
//! domain modules (pitch.rs, voices.rs, leds.rs, ...) for what
//! happens to it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

// === Geometry ===========================================================

pub type MonomeKey = (i32, i32);
// Inclusive corners.
pub type Rect = (MonomeKey, MonomeKey);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowId { Edo, Accretion2x2, EmitToggle1x1 }

#[derive(Debug, Clone, Copy)]
pub struct Window {
  pub id:   WindowId,
  pub rect: Rect,
}

// === Pitch indexing =====================================================

// Per-grid pitch index. For each cell:
//   * its *absolute* pitch (in EDO units, octave preserved) — used
//     when storing a pitch in the accretion slot so emit can sound
//     it at the right frequency.
//   * its pitch *class* (absolute mod edo) — used to find the
//     enharmonic equivalents that should light together.
// Built once at startup; all lookups O(1).
pub struct PitchClass {
  pub key_to_pitch:       HashMap<MonomeKey, i32>,       // absolute (with octave)
  pub key_to_pitchclass:  HashMap<MonomeKey, i32>,       // absolute mod edo
  pub pitchclass_to_keys: HashMap<i32, Vec<MonomeKey>>,  // class -> all cells in that class
}

// === Chord identification ==============================================

// Index into AppState.chords. Each chord button gets one slot;
// commit 5 will set the layout (16 chord buttons, with chord 0
// reserved for "silence" — always empty). For now there's just
// one chord at index 0 and everything implicitly references it.
pub type ChordId = usize;

// === Voices =============================================================

pub type VoiceId = u64;

// What gave rise to this voice. The chord field on Accreted
// distinguishes accretion voices belonging to different chords —
// during emitter transitions both the old and new chord may have
// voices in the map at the same pitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VoiceSource {
  Fingered { xy: MonomeKey },
  Accreted { chord: ChordId, pitch: i32 },
}

// Per-voice audio state. The envelope is "ramp env toward target_env
// by ramp_per_sample each sample, clamping at target." A voice is
// removed once env=0 AND target_env=0.
#[derive(Debug, Clone, Copy)]
pub struct VoiceState {
  pub id:              VoiceId,
  pub freq:            f32,
  pub phase:           f32,
  pub env:             f32,
  pub target_env:      f32,
  pub ramp_per_sample: f32,
}

pub type VoiceMap = HashMap<VoiceSource, VoiceState>;

// === Accretion slot =====================================================

// Pitch (absolute, with octave) → set of finger-voice ids that
// "first introduced" this pitch. The set is fixed on first
// insertion; later press events for the same pitch don't grow it.
// Wipe clears the whole map.
pub type Chord = HashMap<i32, HashSet<VoiceId>>;

// === LED state for the EDO grid =========================================

// Why a cell on the EDO grid is currently lit. A cell stays lit as
// long as it has ≥1 reason and goes dark when its reason set empties.
// Only used by the EDO grid window — the control windows manage their
// own LEDs directly from button state.
//
// The chord field on Chord-variant reasons distinguishes "this cell is
// lit because chord N is emitting and contains its pitch" — needed
// during emitter switches when the old chord's reasons are removed
// and the new chord's are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PitchLedReason {
  PitchEquivalent { source_xy: MonomeKey },               // a fingered key at source_xy is held
  Chord           { chord: ChordId, pitch: i32 },         // pitch in chord N AND chord N is emitting
}

// Sparse: only cells with ≥1 reason appear here.
pub type PitchLedReasons = HashMap<MonomeKey, HashSet<PitchLedReason>>;

// === Buttons ============================================================

// What a control-window cell does. Stored in a button map on
// AppState; dispatched by ButtonAction (an enum, not a closure —
// closures fight Rust's borrow checker for &mut AppState).
#[derive(Debug, Clone, Copy)]
pub enum Button {
  Toggle { state: bool, on: ButtonAction, off: ButtonAction },
  Nursed { state: bool, on: ButtonAction, off: ButtonAction },
  Fire   { fire: ButtonAction },
}

#[derive(Debug, Clone, Copy)]
pub enum ButtonAction {
  AccreteOn, AccreteOff,
  EmitOn,    EmitOff,
  SilentFire,
  WipeFire,
  EmitIsToggleOn, EmitIsToggleOff,
}

// === Brightness =========================================================

// The monome renders only ~4 distinct brightness levels across the
// 0..15 OSC API — buckets of 4 aligned to multiples of 4 (verified
// by 12-brightness-test.sh on this device). This enum is the only
// way the rest of the code talks about brightness; leds::low_res_brightness
// maps it to the OSC integer.
// Dim and Mid are unused today — Dim lights the accretion target
// in commit 6, Mid is held in reserve for future state distinctions.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brightness {
  Off,    // bucket 0  (OSC level 0..=3)
  Dim,    // bucket 1  (OSC level 4..=7)
  Mid,    // bucket 2  (OSC level 8..=11)
  Bright, // bucket 3  (OSC level 12..=15)
}

// === LED command =======================================================

// One LED command emitted by event handlers. Goes through the
// compositor's visibility filter (windows::set_led) on the way to
// the device.
pub type LedCmd = (WindowId, MonomeKey, Brightness);

// === The whole world ===================================================

pub struct AppState {
  pub voices:           Arc<Mutex<VoiceMap>>,

  // One slot per chord. Commit 5 grows this to N_CHORDS=16; for now
  // there's a single entry at index 0 and all logic implicitly
  // addresses chord 0 via accretion_target / emitting_chord.
  pub chords:           Vec<Chord>,
  pub accretion_target: ChordId,            // which slot accrete-on / wipe affect
  pub emitting_chord:   Option<ChordId>,    // None = nothing emits

  pub accrete_on:       bool,
  pub emit_is_toggle:   bool,
  pub next_voice_id:    VoiceId,
  pub pitchled_reasons: PitchLedReasons,
  pub control_buttons:  HashMap<MonomeKey, Button>,

  // Immutable config:
  pub pitch_class:      PitchClass,
  pub fund:             f64,
  pub edo:              i32,
  pub sample_rate:      f32,
}
