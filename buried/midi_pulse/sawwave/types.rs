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
pub enum WindowId {
  Edo,
  ControlsTop,    // y=14, x=0..3 — wipe / accrete / emit-is-toggle / set-accretion-target
  ControlsBottom, // y=15, x=0..15 — silence + chord 1..15
}

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

// === Timbre =============================================================

// The tone-colour of a voice: waveform + per-voice gain + slow AM + slow FM.
// Captured per note (the live timbre at note-on; a per-note snapshot for loop
// notes). `Default` is the pre-timbre instrument -- a triangle at unity gain with
// no modulation -- so existing rigs sound identical until a timbre is set.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waveform { Sine, Triangle, Square, Saw }

impl Default for Waveform {
  fn default() -> Self { Waveform::Triangle }
}

// *Absolute* amplitude modulation (tremolo). `depth` in [0,1] is the modulation
// depth: the carrier is multiplied by a unipolar wave in [1-depth, 1], so depth 0 =
// no AM and depth 1 = dips to silence. `freq` is the LFO rate in Hz -- fixed,
// disregarding the voice's pitch (hence "absolute"; cf. `RelAm`). `shape` in [0,1]
// morphs the LFO wave within the rig-level `AmShapeFamily`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Am { pub depth: f32, pub freq: f32, pub shape: f32 }

impl Default for Am {
  fn default() -> Self { Am { depth: 0.0, freq: 1.0, shape: 0.0 } }
}

// *Absolute* frequency modulation (vibrato), driven by a sine. `depth_cents` is the
// peak pitch deviation in cents; `freq` is the LFO rate in Hz -- fixed, disregarding
// the voice's pitch (cf. `RelFm`). depth 0 = no FM. Exponential (a pitch offset), so
// it can never push the carrier below 0 Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fm { pub depth_cents: f32, pub freq: f32 }

impl Default for Fm {
  fn default() -> Self { Fm { depth_cents: 0.0, freq: 1.0 } }
}

// *Relative* amplitude modulation: a second, independent AM whose sine LFO runs at
// `freq * <voice pitch>` Hz (freq is a unitless multiple of the voice's frequency; it
// tracks glides/retunes). `depth` in [0,1] as in `Am`. freq 0 = off, as is depth 0.
// A voice can carry this *and* the absolute `Am` at once (typically abs slow, rel
// fast -- at audio rates this is ring-mod territory; give the sink `oversample`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelAm { pub depth: f32, pub freq: f32 }

impl Default for RelAm {
  fn default() -> Self { RelAm { depth: 0.0, freq: 1.0 } }
}

// *Relative* frequency modulation: a second, independent FM, *linear* and scaled by
// the voice's pitch. The sine LFO runs at `freq * <voice pitch>` Hz (freq unitless,
// like `RelAm`), and `depth` is the peak deviation as a multiple of the voice's
// frequency: depth 1 sweeps the carrier all the way from 2*f down to 0*f, and depth
// > 1 pushes it *below zero* -- true through-zero FM (the oscillator core runs
// backward; see `voices::osc`). freq 0 = off, as is depth 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelFm { pub depth: f32, pub freq: f32 }

impl Default for RelFm {
  fn default() -> Self { RelFm { depth: 0.0, freq: 1.0 } }
}

// The instrument-wide choice of AM-shape morph family ([D 3c]: rig-level, one
// per instrument). The `shape` value (0..1) sweeps within the chosen family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmShapeFamily { SinToSquare, TriToSquare }

impl Default for AmShapeFamily {
  fn default() -> Self { AmShapeFamily::SinToSquare }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timbre {
  pub waveform: Waveform,
  pub gain:     f32,   // linear per-voice gain; 1.0 = unity
  pub am:       Am,    // absolute (Hz-rate) tremolo
  pub fm:       Fm,    // absolute (Hz-rate, cents-depth) vibrato
  pub rel_am:   RelAm, // pitch-relative AM, independent of `am`
  pub rel_fm:   RelFm, // pitch-relative linear FM, independent of `fm`
}

impl Default for Timbre {
  fn default() -> Self {
    Timbre {
      waveform: Waveform::Triangle,
      gain: 1.0,
      am: Am::default(),
      fm: Fm::default(),
      rel_am: RelAm::default(),
      rel_fm: RelFm::default(),
    }
  }
}

// Per-voice audio state. The envelope: ramp env linearly toward target_env by
// ramp_per_sample each sample (the attack; also the release once target_env is 0);
// when the attack peaks and `sustain_env < target_env`, target_env drops to
// sustain_env and env *decays* toward it exponentially (the pluck -- multiply the
// distance by decay_per_sample each sample), so fresh strikes ring out over held
// notes. sustain_env == target_env (with decay_per_sample 1.0) is the flat,
// pre-pluck envelope. A voice is removed once env=0 AND target_env=0.
#[derive(Debug, Clone, Copy)]
pub struct VoiceState {
  pub id:              VoiceId,
  pub freq:            f32,
  pub phase:           f32,
  pub env:             f32,
  pub target_env:      f32,
  pub ramp_per_sample: f32,
  // The pluck decay (see above): the level the envelope settles at after the
  // attack peak, and the per-full-rate-sample retention of the distance to it.
  pub sustain_env:     f32,
  pub decay_per_sample: f32,
  // Frequency glide (the slide feature): while glide_per_sample != 1.0, freq is
  // multiplied by it each full-rate sample until it crosses freq_target, then
  // snaps there and the glide ends. glide_per_sample == 1.0 = no glide, and
  // freq_target is IGNORED (so plain voices need not keep it in sync with freq).
  pub freq_target:     f32,
  pub glide_per_sample: f32,
  // The factored pulse: a unipolar-triangle amplitude multiplier in [0,1] (1 at
  // each cycle start, 0 at the half-cycle, back to 1) at factored_pulse_freq Hz --
  // the tempo applied at the note's onset. 0.0 = no factored pulse, and multiplying
  // it cannot start one. Usually the note's rate for life, but per-voice edit mode
  // retunes sounding notes in place
  // (`surfaces_runtime::synth::scale_factored_pulse_rate`): the phase free-runs and
  // only this field is read per sample, so changing it mid-note is a slope change
  // with no amplitude step -- no click.
  // Deliberately separate from the note's timbre AM.
  pub factored_pulse_freq:   f32,
  pub factored_pulse_phase:  f32,
  // Timbre, plus the per-voice AM/FM LFO phases advanced each sample in
  // render_block. LFO phases reset to 0 at note-on (per-voice retrigger).
  // `am_phase`/`fm_phase` belong to the absolute (Hz-rate) modulators,
  // `rel_am_phase`/`rel_fm_phase` to the pitch-relative ones.
  pub timbre:          Timbre,
  pub am_phase:        f32,
  pub fm_phase:        f32,
  pub rel_am_phase:    f32,
  pub rel_fm_phase:    f32,
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
//
// Nursed is currently unused for the y=14 control buttons (none of
// them are Nursed), but the variant stays for symmetry — chord
// buttons in Nursed mode are conceptually Nursed even though
// chord_press / chord_release handle them directly without going
// through the Button enum.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum Button {
  Toggle { state: bool, on: ButtonAction, off: ButtonAction },
  Nursed { state: bool, on: ButtonAction, off: ButtonAction },
  Fire   { fire: ButtonAction },
}

#[derive(Debug, Clone, Copy)]
pub enum ButtonAction {
  AccreteOn, AccreteOff,
  WipeFire,
  EmitIsToggleOn, EmitIsToggleOff,
  SetTargetFire,                    // commit 6 implements; placeholder no-op now
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

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct AudioParams {
  pub amplitude: f32,
  pub attack_secs: f32,
  pub release_secs: f32,
  // The pluck envelope (cpal_synth `sustain_level` / `decay_secs`): every voice --
  // fingered or accreted -- strikes to a peak, then decays toward the sustain.
  pub sustain_level: f32,
  pub decay_secs: f32,
}

pub struct AppState {
  pub voices:           Arc<Mutex<VoiceMap>>,

  // One slot per chord. Commit 5 grows this to N_CHORDS=16; for now
  // there's a single entry at index 0 and all logic implicitly
  // addresses chord 0 via accretion_target / emitting_chord.
  pub chords:           Vec<Chord>,
  pub accretion_target: ChordId,            // which slot accrete-on / wipe affect
  pub emitting_chord:   Option<ChordId>,    // None = nothing emits

  // Single source of truth for which chord buttons are *physically*
  // depressed right now, in press order.
  //   Nursed mode: this IS the emit stack; top = current emitter.
  //   Toggle mode: informational; consulted on Toggle→Nursed flip.
  pub pressed_chords:   Vec<ChordId>,

  // Modal: while true, the next chord-button press picks the
  // accretion target. Any other "chord-related" button press exits
  // the mode without changing target. EDO presses are unaffected.
  // The set-accretion-target LED flashes while this is true.
  pub target_select_mode: bool,

  pub accrete_on:       bool,
  pub emit_is_toggle:   bool,
  pub next_voice_id:    VoiceId,
  pub pitchled_reasons: PitchLedReasons,
  // y=14 control buttons only (wipe / accrete-on / emit-is-toggle /
  // set-accretion-target). Chord buttons at y=15 are handled by
  // chord_press / chord_release directly; they have no per-button
  // state to store (emitting_chord and pressed_chords cover it).
  pub control_buttons:  HashMap<MonomeKey, Button>,

  // Immutable rig:
  pub pitch_class:      PitchClass,
  pub fund:             f64,
  pub edo:              i32,
  pub sample_rate:      f32,
  pub audio:            AudioParams,
}
