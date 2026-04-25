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

// === Voices =============================================================

pub type VoiceId = u64;

// What gave rise to this voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VoiceSource {
  Fingered { xy: MonomeKey },
  Accreted { pitch: i32 },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PitchLedReason {
  PitchEquivalent { source_xy: MonomeKey },   // a fingered key at source_xy is held
  Chord           { pitch: i32 },             // pitch is in Chord AND emit_on
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

// === LED command =======================================================

// One LED command emitted by event handlers. Goes through the
// compositor's visibility filter (windows::set_led) on the way to
// the device.
pub type LedCmd = (WindowId, MonomeKey, bool);

// === The whole world ===================================================

pub struct AppState {
  pub voices:           Arc<Mutex<VoiceMap>>,
  pub pitch_accretion:  Chord,
  pub accrete_on:       bool,
  pub emit_on:          bool,
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
