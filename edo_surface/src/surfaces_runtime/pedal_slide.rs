//! The pedal-slide engine, per grid -- pure logic, no I/O (see
//! `TODO/pedal-slide/1_vision.org`, `2_discussion.org`, and the rebuild plan
//! `6_plan.org`). One [`PedalSlideState`] per grid, owned by that grid's thread.
//!
//! *The heart is the ANCHORED-SEGMENTS map* ([`Segments`]). At every instant the
//! pedal-to-value map is two segments JOINED AT THE PEDAL'S CURRENT POSITION: one from
//! `(0, s0)` up to `(f_now, cur)`, one on to `(1, s1)`. Moving the pedal follows the
//! segment in the direction of travel toward that side's endpoint; the segment BEHIND
//! is re-derived from wherever you now are. That single rule delivers everything the
//! discussion's formalization asks for:
//! - *continuity*: `cur` only ever moves along a segment, never jumps (a mid-flight
//!   retarget pins the anchor at the current point and swaps the FAR endpoint, so the
//!   value the pedal is at does not move -- only the goalpost ahead does);
//! - *hysteresis*: reverse mid-sweep and the anchor re-pins at the reversal point, so
//!   the path back is a fresh straight segment to home, not a retrace;
//! - *endpoint exactness*: at `f = 0` the map is `s0`, at `f = 1` it is `s1`, always;
//! - *kink-gone-at-endpoint*: reach an endpoint and `cur == s_that_end`, so the two
//!   segments fuse into one line for the whole return trip.
//!
//! This module and its `Segments` math are LIFTED from the reverted first
//! implementation (`TODO/pedal-slide/5_diffs.org`), which the post-mortem found
//! correct and honestly tested. What is rebuilt around it is the OWNERSHIP, which is
//! what actually failed:
//!
//! *One owner, no queues.* The engine is touched by exactly one thread -- the grid
//! thread that owns the ring, the sink, and the held map. The expression-pedal thread
//! only PUBLISHES a normalized fraction into an atomic; the grid thread reads it each
//! repaint and does everything else. The reverted build split a sliding voice's state
//! across two threads and handed its IDENTITY (the pitch-keyed drone key) from one
//! filing to the other through a queue while both sides kept acting; every drive after
//! the hand-off missed, and a `reconcile` racing the stale ring cancelled the pairing
//! outright (`6_plan.org` "diagnosis verification"). None of that is expressible here.
//!
//! *Identity moves only on confirmation.* [`PedalSlideState::on_pedal`] never mutates
//! a pairing's key or filed pitch. It PROPOSES [`Refile`]s; the owner applies each one
//! to the ring / sink / held map and calls [`PedalSlideState::confirm_refile`] for the
//! ones it actually landed. A refile the owner declines (its destination pitch is
//! occupied) simply leaves that pairing filed where it was -- the engine and the world
//! cannot drift apart, because the engine only learns of a move that happened.
//!
//! *Pitch lives in EDO-step space* ([`StepPitch`]). A pitch segment interpolates
//! fractional EDO steps linearly, which IS log-linear in frequency (equal steps = equal
//! cents), so the segment math needs no logarithms of its own and the one real units
//! hazard -- steps vs Hz, both `f32` -- is a type error rather than a silent
//! detune. `Segments` itself stays units-free: it carries pitch here and amplitude for
//! the fades, and the newtype appears only at the boundary where the confusion lives.

use std::collections::HashSet;

use crate::types::VoiceSource;

/// The pedal's normalized midpoint and the hysteresis band around it. Home/target
/// flips only on crossing `0.5 +/- BAND` in the direction away from the current home,
/// so a pedal resting near the middle (sensor noise, foot tremor) cannot flicker the
/// LED roles and the retarget side many times a second (`2_discussion` "the midpoint
/// flip wants hysteresis").
pub const MIDPOINT: f32 = 0.5;
pub const HYSTERESIS_BAND: f32 = 0.05;

const EPS: f32 = 1e-6;

/// A pitch in FRACTIONAL EDO steps -- what a slide is at mid-flight, where no cell
/// holds it. The instrument's other pitches are integer steps (`i32`) and its
/// frequencies are Hz (`f32`); this newtype exists so the third quantity, which shares
/// `f32` with Hz, cannot be passed where Hz is wanted. Conversions are explicit and
/// live here: [`StepPitch::to_hz`] for the sink, [`StepPitch::filed`] for the
/// integer-keyed sets.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct StepPitch(pub f32);

impl StepPitch {
  /// `fund * 2^(steps / edo)` -- log-linear in frequency, which is exactly what a
  /// linear walk through step space means.
  pub fn to_hz(self, fund: f64, edo: i32) -> f32 {
    (fund * 2.0_f64.powf(self.0 as f64 / edo as f64)) as f32
  }

  /// The integer step this pitch is FILED under (the ring's sets, the drone keys, the
  /// LED cells are all integer-keyed).
  pub fn filed(self) -> i32 {
    self.0.round() as i32
  }
}

impl From<i32> for StepPitch {
  fn from(steps: i32) -> Self {
    StepPitch(steps as f32)
  }
}

/// The shape of one segment, rescaled to its span. Pitch segments are linear (equal
/// EDO steps per equal pedal travel = log-linear in Hz); amplitude fades are quartic
/// (the vision's `x^4`), so a swell eases out of silence and into full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
  Linear,
  Quartic,
}

impl Shape {
  /// Map a normalized position `t` in `[0, 1]` along a segment to its normalized
  /// value in `[0, 1]`.
  fn at(self, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match self {
      Shape::Linear => t,
      Shape::Quartic => t * t * t * t,
    }
  }
}

/// Two segments joined at the pedal's current position -- the anchored-segments map.
/// `s0` is the value at pedal fraction 0, `s1` at fraction 1. `(anchor_f, anchor_v)`
/// is the joint the two segments meet at, and `cur` is the value at the pedal's
/// current fraction (== `anchor_v` immediately after a re-pin, then walking toward
/// the moving side's endpoint as the pedal travels).
///
/// Units-free on purpose: `cur` is fractional EDO steps for a pitch pairing and an
/// amplitude in `[0, 1]` for a fade. The caller knows which; the map does not care.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segments {
  pub s0: f32,
  pub s1: f32,
  pub anchor_f: f32,
  pub anchor_v: f32,
  pub cur: f32,
  pub shape: Shape,
}

impl Segments {
  /// A fresh map anchored at `(f, v)` -- the pedal is at `f` and the value is `v`
  /// there. `s0`/`s1` are the endpoint values.
  pub fn new(s0: f32, s1: f32, f: f32, v: f32, shape: Shape) -> Self {
    Segments { s0, s1, anchor_f: f, anchor_v: v, cur: v, shape }
  }

  /// The value the map holds at pedal fraction `f`, moving `up` (toward `s1`) or down
  /// (toward `s0`). Endpoints are exact; between the anchor and the moving-side
  /// endpoint the segment's shape (rescaled to that span) applies.
  fn eval(&self, f: f32, up: bool) -> f32 {
    if f <= 0.0 {
      return self.s0;
    }
    if f >= 1.0 {
      return self.s1;
    }
    let (target, span) = if up {
      (self.s1, 1.0 - self.anchor_f)
    } else {
      (self.s0, self.anchor_f)
    };
    if span <= EPS {
      return target;
    }
    let t = if up { (f - self.anchor_f) / span } else { (self.anchor_f - f) / span };
    self.anchor_v + (target - self.anchor_v) * self.shape.at(t)
  }

  /// The value at `f` on a map whose joint is FIXED, choosing the segment by which
  /// SIDE of the joint `f` falls on rather than by the direction of travel. That is
  /// what a pinned map means: leaving the joint and coming back retraces the same
  /// curve, instead of re-deriving a fresh segment from wherever you turned around.
  fn eval_pinned(&self, f: f32) -> f32 {
    self.eval(f.clamp(0.0, 1.0), f >= self.anchor_f)
  }

  /// Re-pin the anchor at the current position (a reversal, a retarget, or an
  /// endpoint): the joint moves to `(f, cur)` so the segment behind re-derives from
  /// here.
  fn repin(&mut self, f: f32) {
    self.anchor_f = f;
    self.anchor_v = self.cur;
  }

  /// The endpoint value on the low (`s0`) or high (`s1`) side.
  fn side(&self, low: bool) -> f32 {
    if low {
      self.s0
    } else {
      self.s1
    }
  }
}

/// The pedal's return to VOLUME duty when slide mode is turned off (`2_discussion`'s
/// amendment). A plain catch-up pickup would confuse -- you would push the pedal and
/// nothing would happen until it crossed the frozen volume -- so instead the
/// pedal-to-volume map becomes the SAME anchored construction as a fade: two quartic
/// segments joined at `(F_now, V_frozen)`, one down to `(0, 0)` and one up to `(1, 1)`.
/// Moving the pedal EITHER way therefore changes the volume at once. The moment it
/// reaches an endpoint the map reverts to the ordinary taper (`PedalVolumeCurve`),
/// whose endpoints are exactly 0 and 1 -- so the hand-off back is continuous too.
///
/// Lifted from the reverted build, where it was self-contained and the post-mortem
/// judged it likely correct; re-verified here rather than rewritten.
#[derive(Clone, Copy, Debug)]
pub struct VolumeReturn {
  seg: Segments,
  cur_f: f32,
  moving_up: bool,
  reverted: bool,
}

impl VolumeReturn {
  /// Begin the return with the pedal at fraction `f` and the volume frozen at
  /// `v_frozen`.
  pub fn new(f: f32, v_frozen: f32) -> Self {
    let f = f.clamp(0.0, 1.0);
    VolumeReturn {
      seg: Segments::new(0.0, 1.0, f, v_frozen.clamp(0.0, 1.0), Shape::Quartic),
      cur_f: f,
      moving_up: true,
      reverted: false,
    }
  }

  /// Apply a pedal reading. `Some(gain)` while the kinked return map is in charge,
  /// `None` once the pedal has reached an endpoint and the caller should resume the
  /// ordinary taper.
  pub fn on_pedal(&mut self, f: f32) -> Option<f32> {
    if self.reverted {
      return None;
    }
    let f = f.clamp(0.0, 1.0);
    let f_old = self.cur_f;
    if (f - f_old).abs() > EPS {
      let up = f > f_old;
      if up != self.moving_up {
        self.seg.repin(f_old);
      }
      self.moving_up = up;
      self.seg.cur = self.seg.eval(f, up);
    }
    self.cur_f = f;
    if f <= 0.0 || f >= 1.0 {
      self.reverted = true;
      return None;
    }
    Some(self.seg.cur.clamp(0.0, 1.0))
  }

  #[cfg(test)]
  pub fn reverted(&self) -> bool {
    self.reverted
  }
}

/// Which side of the pedal is "home" -- the end the voice is currently filed at. Home
/// is `Low` (pedal fraction 0) or `High` (fraction 1); it governs (a) which endpoint a
/// new pick replaces (always the FAR one) and (b) the LED roles and the filed pitch.
/// The flip never moves any value -- the map does not mention "home".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Home {
  Low,
  High,
}

impl Home {
  fn is_low(self) -> bool {
    self == Home::Low
  }

  fn flipped(self) -> Home {
    match self {
      Home::Low => Home::High,
      Home::High => Home::Low,
    }
  }
}

/// WHEN home swaps ends.
///
/// `AtMidpoint` is the shipping behaviour and what `1_vision` specifies: "'home'
/// switches from one side of the pedal to the other every time I reach it -- but
/// actually before then. It has to switch when I cross the midpoint of the pedal's
/// range." `AtEndpoints` was the rebuild's slice-1 stepping stone (prove the pedal
/// lives forever before adding mid-travel re-filing); it survives because it is the
/// cleanest way to test the arrival semantics in isolation, with no band to reason
/// about, and because keeping both makes the flip point one knob rather than a
/// condition smeared through the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipPolicy {
  /// Roles swap when the pedal REACHES the far end.
  #[allow(dead_code)] // constructed only by the tests, via `set_flip_policy`
  AtEndpoints,
  /// Roles swap at the pedal's midpoint, with the hysteresis band (`1_vision`).
  AtMidpoint,
}

/// The WRONG-WAY GATE: what happens when a slide is picked with the pedal parked away
/// from an endpoint, and the foot then travels toward home instead of toward the
/// target (Jeff's design, 2026-07-20).
///
/// The problem it solves: with the note at `H` and the pedal at `F`, the map cannot be
/// a single straight line over the whole travel AND leave the pitch alone at the moment
/// of the pick AND land exactly on the target -- the slope is forced to `delta/(1-F)`,
/// and something has to give on the near side of `F`. Letting the line simply continue
/// would strand the note among pitches you cannot name and cannot get back from.
///
/// So the near side does nothing to the PITCH -- it holds at `H`, which is therefore
/// never lost -- and says so with the VOLUME instead: full at `F`, fading quartically
/// to silence at the home end. Going the wrong way is audibly going nowhere, and
/// returning to `F` brings the note back, continuously, exactly at `H`.
///
/// `pin` is the one kink in this instrument that OUTLIVES the pedal leaving it: every
/// other re-pins on reversal, this one must not, or `H` would drift away from the
/// position that recovers it. Both pins dissolve at an endpoint:
/// - reaching the FAR end (the target) drops the gate entirely -- the map becomes the
///   plain line between the two pitches and the pedal stops touching volume for good;
/// - reaching the HOME end (silence) moves both pins to the opposite end, so the way
///   back sweeps pitch `H -> T` and volume `0 -> full` across the whole travel, the two
///   arriving together.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WrongWayGate {
  /// Where the pitch map's joint is frozen: the pitch holds at `H` on the home side of
  /// this, and slides toward the target beyond it.
  pub pitch_pin: f32,
  /// Where the volume is full: it fades quartically from here to silence at the home
  /// end, and is full everywhere beyond.
  pub gain_pin: f32,
  /// Which end was home when the gate opened. Fixed for its lifetime -- the midpoint
  /// flip re-labels roles for painting, and must not turn this map inside out.
  pub home_low: bool,
}

impl WrongWayGate {
  fn home_end(&self) -> f32 {
    if self.home_low {
      0.0
    } else {
      1.0
    }
  }

  fn far_end(&self) -> f32 {
    1.0 - self.home_end()
  }

  /// The volume factor at pedal position `f`: 1 from the pin outward, and a quartic
  /// falling to exactly 0 at the home end. Quartic like the fades, so silence is
  /// approached gently and most of the travel still sounds.
  fn gain(&self, f: f32) -> f32 {
    let span = (self.gain_pin - self.home_end()).abs();
    if span <= EPS {
      return 1.0;
    }
    let t = ((f - self.home_end()).abs() / span).clamp(0.0, 1.0);
    t * t * t * t
  }
}

/// What one voice is doing under the pedal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
  /// A pitch slide: `seg` holds fractional EDO-step pitches; amplitude is untouched.
  Pitch,
  /// An amplitude swell into a FIXED pitch, from silence: `seg` holds amplitude in
  /// `[0, 1]` and the pitch stays at `filed`. The vision's "just fade in", and -- per
  /// Jeff's chosen answer to `2_discussion`'s open question -- what a pick does when
  /// nothing is in edit mode, making the pedal a swell into any pitch.
  Fade,
}

/// Which end of a fade's travel is FULL volume. A swell's silent end is where it
/// started; a fade-out's silent end is where it is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeDir {
  In,
  Out,
}

/// WHY a voice is in the managed set -- and therefore what takes it back out.
///
/// This is `6_plan.org`'s "a slide-managed set, explicit and per grid", made explicit.
/// The reverted build had no such concept: it asked the EDIT SELECTION who the pedal
/// managed, which is right for the single-note case the vision opens with and wrong the
/// moment chords arrive -- a recalled chord's voices are never in the edit set, so the
/// matcher could not see them, nothing paired, and every note became a fade. That is
/// bug 1, and it was a membership bug rather than a matching one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tenure {
  /// Joined by being in the edit selection (the ordinary pick). Deselecting it -- an
  /// editmode clear, a sustain clear -- cancels the slide, per `2_discussion`: "a slide
  /// needs its selection".
  WhileEdited,
  /// Joined by a chord recall or by being swelled into existence. The edit selection
  /// never had a say over these, so it does not get one now; they live as long as their
  /// voice does.
  WhileSounding,
}

/// One voice the pedal manages: its identity, where it is FILED, and its map.
///
/// `voice` and `filed` are the pairing's copy of facts that also live in the voice map
/// and the ring. They are changed in exactly one place -- [`PedalSlideState::confirm_refile`],
/// called by the owner AFTER it has moved the real ones -- so they cannot drift.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pairing {
  pub voice: VoiceSource,
  /// Why this voice is managed, and so what removes it (see [`Tenure`]).
  pub tenure: Tenure,
  /// The integer pitch this voice is currently filed and painted at (the ring's sets,
  /// its drone key if it is a drone, the cell the dances mark).
  pub filed: i32,
  /// The pedal position this pairing last saw, so the gate's volume can be read off the
  /// pairing alone (the segment carries the pitch, not the fraction).
  pub at_f: f32,
  pub kind: Kind,
  pub seg: Segments,
  /// Live only while this slide was picked away from an endpoint and has not reached
  /// one since (see [`WrongWayGate`]). While it is set, the pitch map is PINNED rather
  /// than re-anchored on reversal, and the pedal drives this voice's volume as well as
  /// its pitch.
  pub gate: Option<WrongWayGate>,
}

impl Pairing {
  /// The pitch this voice should sound right now. A fade holds its pitch still and
  /// moves its amplitude; a pitch slide is the other way round.
  pub fn current_pitch(&self) -> StepPitch {
    match self.kind {
      Kind::Pitch => StepPitch(self.seg.cur),
      Kind::Fade => StepPitch(self.filed as f32),
    }
  }

  /// The amplitude factor in `[0, 1]` the pedal is imposing on this voice now, or
  /// `None` when it is imposing none. A swell's whole business is amplitude; a pitch
  /// slide leaves it alone (the grid volume is frozen) EXCEPT while its wrong-way gate
  /// is open, where the fade is what says "this direction is going nowhere".
  pub fn current_amp(&self) -> Option<f32> {
    match self.kind {
      Kind::Fade => Some(self.seg.cur.clamp(0.0, 1.0)),
      Kind::Pitch => self.gate.map(|g| g.gain(self.at_f)),
    }
  }
}

/// One drive command the wiring applies to a voice each step: the pitch it should be
/// sounding now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drive {
  pub voice: VoiceSource,
  pub pitch: StepPitch,
  /// A fade's target amplitude; `None` for a pitch slide.
  pub amp: Option<f32>,
}

/// A PROPOSED re-file: home swapped ends, so this voice is now painted (and, if it is
/// a drone, keyed) at the other endpoint's pitch. The owner applies it against the
/// ring, the sink, and the held map, then calls [`PedalSlideState::confirm_refile`];
/// an owner that declines (something already occupies `to`) simply does not confirm,
/// and the pairing stays filed where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refile {
  pub voice_old: VoiceSource,
  pub voice_new: VoiceSource,
  pub from: i32,
  pub to: i32,
}

/// What the wiring must do after a pick. A pick that lands on an existing voice needs
/// nothing; one with nothing to pair asks for a fresh voice to be spawned SILENT, which
/// only the sink can do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PickOutcome {
  /// Pitches needing a fresh voice, born silent, for the pedal to swell in. A single
  /// pick produces at most one; a chord recall can produce several.
  pub spawn_fade_ins: Vec<i32>,
}

/// One grid's pedal-slide engine.
#[derive(Debug)]
pub struct PedalSlideState {
  /// Is slide mode on for this grid? While on, the EX-P pedal drives this engine
  /// instead of volume.
  mode: bool,
  /// The pedal's last normalized fraction `[0, 1]`.
  cur_f: f32,
  /// Has this pedal EVER reported a position? Until it has, the instrument has no idea
  /// where the foot is resting -- an EX-P only sends CCs under a foot, so a session
  /// where the pedal has not been touched knows nothing about it.
  ///
  /// Guessing here is not harmless: it decides which side is home, and therefore which
  /// endpoint holds each voice's current pitch. Guess wrong and the first CC to arrive
  /// finds the map already stretched between the wrong ends, and the pitch LEAPS to
  /// wherever that CC falls -- audible as a jump on the very first pedal move, which is
  /// exactly the discontinuity Jeff heard. So the engine stays agnostic: no home until
  /// the pedal speaks, and the side it speaks from becomes home
  /// (see [`PedalSlideState::adopt_first_reading`]).
  pedal_seen: bool,
  /// The pedal's last travel direction (true = rising toward `s1`).
  moving_up: bool,
  /// Which side is home right now.
  home: Home,
  /// When home swaps ends (see [`FlipPolicy`]).
  flip: FlipPolicy,
  /// The voices the pedal manages -- the SLIDE-MANAGED SET (`6_plan.org`). Distinct
  /// from the edit selection on purpose: the reverted build conflated the two, which
  /// is why a recalled chord's voices were invisible to the matcher (bug 1).
  pairings: Vec<Pairing>,
  /// The chord slot currently supplying the target set, for its LED. `1_vision`: "at
  /// most one chord can be selected from storage at a time".
  target_slot: Option<usize>,
}

impl Default for PedalSlideState {
  fn default() -> Self {
    PedalSlideState::new()
  }
}

impl PedalSlideState {
  pub fn new() -> Self {
    PedalSlideState {
      mode: false,
      cur_f: 0.0,
      pedal_seen: false,
      moving_up: true,
      home: Home::Low,
      flip: FlipPolicy::AtMidpoint,
      pairings: Vec::new(),
      target_slot: None,
    }
  }

  /// Which chord slot is supplying the targets (lit solid), if any.
  pub fn target_slot(&self) -> Option<usize> {
    self.target_slot
  }

  pub fn set_target_slot(&mut self, slot: Option<usize>) {
    self.target_slot = slot;
  }

  pub fn mode(&self) -> bool {
    self.mode
  }

  pub fn home(&self) -> Home {
    self.home
  }

  /// Override when home swaps ends. Used by the tests to exercise the arrival
  /// semantics in isolation (`FlipPolicy::AtEndpoints`), where no band is in play.
  #[cfg(test)]
  pub fn set_flip_policy(&mut self, flip: FlipPolicy) {
    self.flip = flip;
  }

  /// Nothing managed -- the resting state of a grid in slide mode that has not been
  /// given a target yet. The repaint checks this before doing any per-step work.
  pub fn is_empty(&self) -> bool {
    self.pairings.is_empty()
  }

  // Read by the TESTS rather than by the runtime, which goes through `drives()` and
  // `led_roles()`; they are the engine's observable state and the harness asserts on
  // them directly.
  #[allow(dead_code)]
  pub fn pairings(&self) -> &[Pairing] {
    &self.pairings
  }

  /// The current target pitches (integer EDO steps) -- the FAR goalpost of each
  /// pairing, i.e. where the pedal is heading.
  #[allow(dead_code)]
  pub fn targets(&self) -> Vec<i32> {
    self
      .pairings
      .iter()
      .filter(|p| p.kind == Kind::Pitch)
      .map(|p| StepPitch(p.seg.side(!self.home.is_low())).filed())
      .collect()
  }

  /// Enter slide mode. `f` is the pedal's position if it has ever reported one, and
  /// `None` if it has not -- in which case home stays UNDECIDED (provisionally `Low`,
  /// but nothing is committed) until the first CC arrives and settles it.
  ///
  /// Starts with no pairings either way, so the pedal is inert -- and no pitch is
  /// yanked -- until the first pick.
  ///
  /// Nothing is recorded about the grid volume: it simply stops being driven, and the
  /// pedal thread reads the frozen value straight off the shared per-grid gain when it
  /// takes volume duty back (`pedal_volume.rs`). One fact, one home.
  pub fn enter(&mut self, f: Option<f32>) {
    self.mode = true;
    self.pairings.clear();
    self.target_slot = None;
    if let Some(f) = f {
      self.pedal_seen = true;
      self.cur_f = f.clamp(0.0, 1.0);
    }
    self.home = if self.pedal_seen && self.cur_f > MIDPOINT { Home::High } else { Home::Low };
  }

  /// Whether the pedal has ever reported a position. The on-screen readout answers the
  /// same question from the published atomic's NaN sentinel (it has no engine to ask),
  /// so this exists for the tests, which assert on the engine's own view of it.
  #[allow(dead_code)]
  pub fn pedal_seen(&self) -> bool {
    self.pedal_seen
  }

  /// The pedal's first-ever reading: this is where the foot has been all along, so THIS
  /// side is home. Adopt the position and re-anchor every pairing to it WITHOUT moving
  /// a single pitch.
  ///
  /// Nothing has moved yet (no reading means no travel), so each voice still sounds its
  /// base pitch and `cur == anchor_v`. Re-anchoring is therefore lossless: if the
  /// provisional home was the wrong side, the two endpoints simply swap, so the side
  /// the foot is actually on holds where each voice IS and the far side holds its
  /// target. Then the anchor moves to the real position.
  fn adopt_first_reading(&mut self, f: f32) {
    let actual = if f > MIDPOINT { Home::High } else { Home::Low };
    if actual != self.home {
      // The provisional home was the other side. Swap the goalposts rather than moving
      // the voice: the pitch a slide is AT must not depend on where the foot turned out
      // to be resting.
      for p in &mut self.pairings {
        std::mem::swap(&mut p.seg.s0, &mut p.seg.s1);
      }
      self.home = actual;
    }
    for p in &mut self.pairings {
      p.seg.anchor_f = f;
      p.seg.anchor_v = p.seg.cur;
    }
    self.cur_f = f;
    self.pedal_seen = true;
  }

  /// Leave slide mode mid-anything: the managed set empties and every voice it held is
  /// returned for the owner to freeze exactly where it is -- smoothness above all; they
  /// are sustained voices, and a later drag can tidy an off-grid pitch.
  pub fn exit(&mut self) -> Vec<VoiceSource> {
    self.mode = false;
    self.target_slot = None;
    self.pairings.drain(..).map(|p| p.voice).collect()
  }

  /// Reconcile the managed set against the grid's live edit selection (`edited`,
  /// integer EDO steps): a pairing whose voice left the selection -- an editmode-clear
  /// deselecting it, or a sustain-clear ending it (which cascades the edit reason away)
  /// -- is cancelled, and its voice returned for freezing. Compares against the
  /// pairing's OWN `filed` pitch, which the owner keeps in step with the ring, so this
  /// can no longer misread a note mid-hand-off (the reverted build's bug 2, whose
  /// sharpest edge was exactly this call reading stale ring state).
  pub fn reconcile(
    &mut self,
    edited: &HashSet<i32>,
    alive: impl Fn(&VoiceSource) -> bool,
  ) -> Vec<VoiceSource> {
    if !self.mode {
      return Vec::new();
    }
    let mut dropped = Vec::new();
    self.pairings.retain(|p| {
      // A voice that has ended (a sustain clear, a release) leaves the managed set
      // whatever kind it was -- there is nothing left to drive.
      if !alive(&p.voice) {
        return false;
      }
      // Only a voice that JOINED through the edit selection can be evicted by losing
      // it. Chord voices and swells joined another way and answer to their voice alone.
      match p.tenure {
        Tenure::WhileSounding => true,
        Tenure::WhileEdited if edited.contains(&p.filed) => true,
        Tenure::WhileEdited => {
          dropped.push(p.voice);
          false
        }
      }
    });
    dropped
  }

  /// Apply one pedal reading: advance every pairing along its anchored map, then
  /// decide whether home swaps ends. PROPOSES the resulting re-files -- it does not
  /// apply them (see the module docs); the owner applies each and confirms it.
  ///
  /// Also tracks the fraction while mode is OFF, so `enter` knows which side the pedal
  /// is on without waiting for it to move.
  pub fn on_pedal(&mut self, f: f32) -> Vec<Refile> {
    let f = f.clamp(0.0, 1.0);
    if !self.pedal_seen {
      // The first word from the pedal tells us where the foot is, which is a fact to
      // ADOPT, not a movement to follow. Following it would sweep every map from a
      // position that was only ever a guess.
      self.adopt_first_reading(f);
      return Vec::new();
    }
    if !self.mode {
      self.cur_f = f;
      return Vec::new();
    }
    let f_old = self.cur_f;
    if (f - f_old).abs() > EPS {
      let up = f > f_old;
      for p in &mut self.pairings {
        // A reversal re-pins the anchor at the reversal point, so the way back is a
        // fresh segment to home (the hysteresis). A GATED pairing is the one exception
        // -- its joint is what makes the home pitch recoverable, so it must not drift
        // to wherever the foot happened to turn around.
        if up != self.moving_up && p.gate.is_none() {
          p.seg.repin(f_old);
        }
        p.seg.cur = if p.gate.is_some() { p.seg.eval_pinned(f) } else { p.seg.eval(f, up) };
        p.at_f = f;
      }
      self.moving_up = up;
    } else {
      for p in &mut self.pairings {
        p.at_f = f;
      }
    }
    self.cur_f = f;
    self.dissolve_gates_at_endpoints(f);
    self.propose_flip(f)
  }

  /// "As soon as the pedal reaches either endpoint, the kink should vanish."
  ///
  /// Reaching the FAR end means the slide arrived: drop the gate, re-pin the map at
  /// the endpoint so the return trip is one clean line between the two pitches, and
  /// hand volume back for good -- from here the pedal is pure pitch.
  ///
  /// Reaching the HOME end means the wrong way was followed all the way to silence.
  /// Nothing is lost there, so both pins move to the opposite end: the way back now
  /// sweeps pitch `H -> T` and volume `0 -> full` across the whole travel, arriving
  /// together. Both hand-offs are continuous, because at the endpoint the old map and
  /// the new one agree on both pitch and volume.
  fn dissolve_gates_at_endpoints(&mut self, f: f32) {
    for p in &mut self.pairings {
      let Some(g) = p.gate else { continue };
      if (f - g.far_end()).abs() <= EPS {
        p.seg.repin(f);
        p.gate = None;
      } else if (f - g.home_end()).abs() <= EPS {
        p.gate = Some(WrongWayGate { pitch_pin: g.home_end(), gain_pin: g.far_end(), ..g });
        p.seg.anchor_f = g.home_end();
        p.seg.anchor_v = p.seg.cur;
      }
    }
  }

  /// Has the pedal crossed the point where home swaps ends? If so, flip the LABEL now
  /// (it is a global role marker and moves no value) and propose one re-file per
  /// pairing whose painted pitch therefore moves.
  fn propose_flip(&mut self, f: f32) -> Vec<Refile> {
    let crossed = match (self.flip, self.home) {
      // Arrival at the far end is a swap: the pedal never dies -- reaching the target
      // makes it home, the far side now holds the pitch you came from, and sliding back
      // returns there. Under `AtMidpoint` the swap has usually already happened by
      // then, so these arms only fire for a sweep that turned around inside the band.
      (FlipPolicy::AtEndpoints, Home::Low) => f >= 1.0,
      (FlipPolicy::AtEndpoints, Home::High) => f <= 0.0,
      // The vision's midpoint, with a band so a foot resting near the middle (sensor
      // noise, tremor) cannot flicker the roles -- and with it the LED colours and the
      // end a new pick would replace -- many times a second.
      (FlipPolicy::AtMidpoint, Home::Low) => f > MIDPOINT + HYSTERESIS_BAND,
      (FlipPolicy::AtMidpoint, Home::High) => f < MIDPOINT - HYSTERESIS_BAND,
    };
    if !crossed || self.pairings.is_empty() {
      return Vec::new();
    }
    let new_home = self.home.flipped();
    let mut refiles = Vec::new();
    for p in &self.pairings {
      // A fade holds one pitch and moves only its amplitude, so it is never re-filed.
      if p.kind != Kind::Pitch {
        continue;
      }
      let to = StepPitch(p.seg.side(new_home.is_low())).filed();
      if to == p.filed {
        continue;
      }
      refiles.push(Refile {
        voice_old: p.voice,
        voice_new: rekeyed(p.voice, to),
        from: p.filed,
        to,
      });
    }
    self.home = new_home;
    refiles
  }

  /// The owner applied `refile` to the ring / sink / held map: adopt the new identity.
  /// Unconfirmed proposals leave the pairing exactly as it was.
  pub fn confirm_refile(&mut self, refile: &Refile) {
    if let Some(p) = self.pairings.iter_mut().find(|p| p.voice == refile.voice_old) {
      p.voice = refile.voice_new;
      p.filed = refile.to;
    }
  }

  /// The owner re-keyed a managed voice WITHOUT its pitch moving: the same note, now
  /// held differently. Two gestures do this and both can happen mid-slide --
  ///
  /// - a finger lifting off a note that is being slid (`SurfaceFinger` -> `SurfaceDrone`,
  ///   via `SurfaceSink::sustain_note`), and
  /// - a finger landing on a sliding drone to retrigger it (`SurfaceDrone` ->
  ///   `SurfaceFinger`, via `cut_sustained` + `note_on_continuing`)
  ///
  /// -- and each is the reverted build's disease in a second guise: the pairing would
  /// go on addressing a key the voice map no longer has, and the note would silently
  /// freeze mid-glide. Same discipline as [`confirm_refile`]: the owner moves the voice
  /// first and tells the engine after.
  pub fn rekey_voice(&mut self, old: VoiceSource, new: VoiceSource) {
    if old == new {
      return;
    }
    if let Some(p) = self.pairings.iter_mut().find(|p| p.voice == old) {
      p.voice = new;
    }
  }

  /// A pick of a FREE pitch while slide mode is on. `candidates` is the grid's
  /// pairable voices as `(key, current filed pitch)` -- its edit-mode voices. Per the
  /// vision's home-proximity rule the pick becomes the new target of the NEAREST voice
  /// (ties to the lower voice), replacing that pairing's FAR goalpost with a kink at
  /// the current fraction -- so a pick can never yank the pitch you are nearest to.
  ///
  /// With NOTHING pairable, the pick instead swells a NEW voice into being from
  /// silence -- Jeff's chosen answer in `2_discussion` ("let's go with your favorite"),
  /// which makes the pedal a swell into any pitch and is the one-note case of the chord
  /// fade logic. The caller spawns the voice and reports it back through
  /// [`register_fade`].
  pub fn pick(&mut self, pitch: i32, candidates: &[(VoiceSource, i32)]) -> PickOutcome {
    let nearest = candidates
      .iter()
      .min_by_key(|(_, base)| ((*base - pitch).abs(), *base))
      .copied();
    match nearest {
      Some((voice, base)) => {
        let pairing = self.pitch_pairing(voice, base, pitch, Tenure::WhileEdited);
        if let Some(slot) = self.pairings.iter_mut().find(|p| p.voice == pairing.voice) {
          *slot = pairing;
        } else {
          self.pairings.push(pairing);
        }
        PickOutcome::default()
      }
      // Already swelling this very pitch? Re-picking it is a no-op rather than a
      // second, colliding voice at the same drone key.
      None if self.pairings.iter().any(|p| p.kind == Kind::Fade && p.filed == pitch) => {
        PickOutcome::default()
      }
      None => PickOutcome { spawn_fade_ins: vec![pitch] },
    }
  }

  /// The caller spawned the swell voice this engine asked for: adopt it. The fade runs
  /// from silence at the home end to full at the far end, quartic-shaped and anchored
  /// where the pedal is -- so a pick made mid-travel pins a kink exactly as a pitch
  /// retarget does, and no amplitude ever jumps.
  pub fn register_fade(&mut self, voice: VoiceSource, pitch: i32, dir: FadeDir) {
    self.upsert(self.fade_pairing(voice, pitch, dir, self.fade_start(voice, dir)));
  }

  /// Where a fade begins: a voice already in the managed set fades from the amplitude
  /// it is actually at (so a chord recalled twice, or a leftover caught mid-swell, does
  /// not jump), and a fresh one from the obvious end.
  fn fade_start(&self, voice: VoiceSource, dir: FadeDir) -> f32 {
    let existing = self.pairings.iter().find(|p| p.voice == voice).and_then(|p| p.current_amp());
    existing.unwrap_or(match dir {
      FadeDir::In => 0.0,
      FadeDir::Out => 1.0,
    })
  }

  fn upsert(&mut self, pairing: Pairing) {
    if let Some(slot) = self.pairings.iter_mut().find(|p| p.voice == pairing.voice) {
      *slot = pairing;
    } else {
      self.pairings.push(pairing);
    }
  }

  fn fade_pairing(&self, voice: VoiceSource, pitch: i32, dir: FadeDir, start: f32) -> Pairing {
    let end = match dir {
      FadeDir::In => 1.0,
      FadeDir::Out => 0.0,
    };
    let far_is_high = self.home.is_low();
    let (s0, s1) = if far_is_high { (start, end) } else { (end, start) };
    let seg = Segments::new(s0, s1, self.cur_f, start, Shape::Quartic);
    // A swell is already all amplitude; it needs no wrong-way signal on top.
    Pairing {
      voice, tenure: Tenure::WhileSounding, filed: pitch, at_f: self.cur_f,
      kind: Kind::Fade, seg, gate: None,
    }
  }

  /// A whole SET of targets arriving at once -- a chord recalled while pedal slide is
  /// on (`1_vision`: "its voices all join the target set"). This is bug 1's fix.
  ///
  /// `candidates` is every voice the pedal may move: the previous chord's sounding
  /// voices AND this grid's edit-mode voices. The reverted build passed only the latter,
  /// so a recalled chord found nothing to pair with, every old voice became a fade-out
  /// and every new pitch a fade-in -- a crossfade, which is exactly what Jeff heard
  /// instead of a glide.
  ///
  /// The pairing rule is `2_discussion`'s, which Jeff confirmed: walk the candidates in
  /// ASCENDING pitch, each taking its nearest remaining target, ties to the LOWER
  /// target. Deterministic, cheap, and it matches how the instrument breaks every other
  /// tie. Then:
  /// - a candidate that already had a target KEEPS it if it is still in the set (so a
  ///   pitch already satisfied stays put rather than being re-shuffled);
  /// - spare targets swell NEW voices in from silence;
  /// - spare candidates fade OUT.
  ///
  /// Voices that are not candidates keep whatever they were doing.
  pub fn match_targets(
    &mut self,
    targets: &[i32],
    candidates: &[(VoiceSource, i32)],
  ) -> PickOutcome {
    let mut ordered: Vec<(VoiceSource, i32)> = candidates.to_vec();
    ordered.sort_by_key(|(_, pitch)| *pitch);
    let mut remaining: Vec<i32> = targets.to_vec();
    remaining.sort_unstable();
    remaining.dedup();

    // A candidate already aimed at one of these targets keeps it -- claimed first, so
    // the ascending pass cannot hand it to somebody else.
    let mut kept: Vec<(VoiceSource, i32)> = Vec::new();
    for (voice, _) in &ordered {
      let Some(prev) = self.pairings.iter().find(|p| p.voice == *voice && p.kind == Kind::Pitch)
      else {
        continue;
      };
      let aimed = StepPitch(prev.seg.side(!self.home.is_low())).filed();
      if let Some(i) = remaining.iter().position(|t| *t == aimed) {
        remaining.remove(i);
        kept.push((*voice, aimed));
      }
    }

    let mut fresh: Vec<Pairing> = Vec::new();
    for (voice, base) in &ordered {
      if let Some((_, aimed)) = kept.iter().find(|(v, _)| v == voice) {
        fresh.push(self.pitch_pairing(*voice, *base, *aimed, Tenure::WhileSounding));
        continue;
      }
      if remaining.is_empty() {
        // More voices than targets: this one fades out from wherever it is.
        fresh.push(self.fade_pairing(*voice, *base, FadeDir::Out, self.fade_start(*voice, FadeDir::Out)));
        continue;
      }
      let (i, _) = remaining
        .iter()
        .enumerate()
        .min_by_key(|(_, t)| ((*t - base).abs(), **t))
        .expect("remaining is non-empty");
      let target = remaining.remove(i);
      fresh.push(self.pitch_pairing(*voice, *base, target, Tenure::WhileSounding));
    }

    // Candidates leave their old pairings behind wholesale; anything else the pedal was
    // managing (a swell from an earlier pick, say) carries on untouched.
    let involved: Vec<VoiceSource> = ordered.iter().map(|(v, _)| *v).collect();
    self.pairings.retain(|p| !involved.contains(&p.voice));
    self.pairings.extend(fresh);
    PickOutcome { spawn_fade_ins: remaining }
  }

  /// Build (or retarget) a pitch pairing for `voice`, currently filed at `base`, aiming
  /// at `target`. A pre-existing pairing keeps its map and only retargets its FAR
  /// endpoint (kink at the current fraction).
  fn pitch_pairing(&self, voice: VoiceSource, base: i32, target: i32, tenure: Tenure) -> Pairing {
    let far_is_high = self.home.is_low();
    if let Some(prev) = self.pairings.iter().find(|p| p.voice == voice) {
      let mut seg = prev.seg;
      if (seg.side(!far_is_high) - target as f32).abs() > EPS {
        // Retarget the far side: pin the kink at the current fraction, swap the
        // goalpost. The near side -- the pitch this voice is filed at -- is untouched.
        seg.repin(self.cur_f);
        if far_is_high {
          seg.s1 = target as f32;
        } else {
          seg.s0 = target as f32;
        }
      }
      // A RETARGET keeps whatever gate it had: the pin marks where this voice's home
      // is recoverable from, and re-aiming the far goalpost does not move that.
      return Pairing {
        voice, tenure, filed: prev.filed, at_f: self.cur_f, kind: Kind::Pitch, seg,
        gate: prev.gate,
      };
    }
    // A fresh pairing: the home endpoint is the voice's current pitch, the far endpoint
    // the target, anchored where the pedal is.
    let (s0, s1) =
      if far_is_high { (base as f32, target as f32) } else { (target as f32, base as f32) };
    let seg = Segments::new(s0, s1, self.cur_f, base as f32, Shape::Linear);
    // Picked AT an endpoint, the map is already the plain line from one end to the
    // other and there is no wrong way to go -- no gate. Picked anywhere else, the near
    // side of the pedal cannot slide (see `WrongWayGate`), so it fades instead.
    let at_end = self.cur_f <= EPS || self.cur_f >= 1.0 - EPS;
    let gate = (!at_end).then_some(WrongWayGate {
      pitch_pin: self.cur_f,
      gain_pin: self.cur_f,
      home_low: far_is_high,
    });
    Pairing { voice, tenure, filed: base, at_f: self.cur_f, kind: Kind::Pitch, seg, gate }
  }

  /// The per-voice drive commands to apply this instant.
  pub fn drives(&self) -> Vec<Drive> {
    self
      .pairings
      .iter()
      .map(|p| Drive { voice: p.voice, pitch: p.current_pitch(), amp: p.current_amp() })
      .collect()
  }

  /// The LED roles: the HOME pitches (where each voice is filed -- lit + danced through
  /// the ordinary sustained path) and the TARGET pitches (the far goalpost, which
  /// flashes and does not dance).
  pub fn led_roles(&self) -> (HashSet<i32>, HashSet<i32>) {
    let mut home = HashSet::new();
    let mut target = HashSet::new();
    for p in &self.pairings {
      home.insert(p.filed);
      match p.kind {
        Kind::Pitch => {
          target.insert(StepPitch(p.seg.side(!self.home.is_low())).filed());
        }
        // A swelling voice flashes at its own pitch while it is still coming in --
        // that cell IS the destination. Once the pedal has carried it home (full
        // volume, roles swapped) it stops flashing and is simply a sustained note,
        // lit and square-dancing through the ordinary path.
        Kind::Fade => {
          if p.seg.side(!self.home.is_low()) > p.seg.side(self.home.is_low()) {
            target.insert(p.filed);
          }
        }
      }
    }
    (home, target)
  }
}

/// A voice's key after its filed pitch moves to `to`. A pitch-keyed DRONE re-keys; a
/// cell-keyed FINGERED voice keeps its key (its identity is the cell under the finger,
/// not the pitch).
fn rekeyed(voice: VoiceSource, to: i32) -> VoiceSource {
  match voice {
    VoiceSource::SurfaceDrone { grid, .. } => VoiceSource::SurfaceDrone { grid, pitch: to },
    other => other,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn finger(cell: (i32, i32)) -> VoiceSource {
    VoiceSource::SurfaceFinger { grid: 0, cell }
  }

  fn drone(pitch: i32) -> VoiceSource {
    VoiceSource::SurfaceDrone { grid: 0, pitch }
  }

  // ---- the anchored-segments map (lifted with the corpse's tests: this math was
  // never the bug, and its 22 assertions were honest) ----

  /// A plain pitch slide from home to target: endpoints exact, monotone between.
  #[test]
  fn a_pitch_segment_lands_home_at_0_and_target_at_1() {
    let mut s = Segments::new(10.0, 20.0, 0.0, 10.0, Shape::Linear);
    assert_eq!(s.eval(0.0, true), 10.0, "exact home at 0");
    assert_eq!(s.eval(1.0, true), 20.0, "exact target at 1");
    s.cur = s.eval(0.5, true);
    assert!((s.cur - 15.0).abs() < 1e-4, "halfway is halfway (linear)");
  }

  /// Continuity under a mid-flight retarget: pinning the anchor at the current point
  /// and swapping the far endpoint must not move the value the pedal is at.
  #[test]
  fn a_mid_flight_retarget_does_not_move_the_current_pitch() {
    let mut s = Segments::new(10.0, 20.0, 0.0, 10.0, Shape::Linear);
    s.cur = s.eval(0.6, true);
    assert!((s.cur - 16.0).abs() < 1e-4);
    s.repin(0.6);
    s.s1 = 30.0;
    assert!((s.eval(0.6, true) - 16.0).abs() < 1e-4, "no discontinuity at the retarget");
    assert!((s.eval(1.0, true) - 30.0).abs() < 1e-4, "the new goalpost is reached exactly");
    let mid = s.eval(0.8, true); // t = (0.8-0.6)/(1-0.6) = 0.5 -> 16 + (30-16)*0.5
    assert!((mid - 23.0).abs() < 1e-4);
  }

  /// Hysteresis: sweep up along a kinked map, reverse, and the path down is a fresh
  /// straight segment to home -- not a retrace of the way up.
  #[test]
  fn reversing_mid_flight_takes_a_fresh_segment_home() {
    let mut s = Segments::new(0.0, 12.0, 0.0, 0.0, Shape::Linear);
    s.cur = s.eval(0.5, true);
    s.repin(0.5);
    s.s1 = 100.0;
    s.cur = s.eval(0.75, true); // 6 + (100-6)*0.5 = 53
    assert!((s.cur - 53.0).abs() < 1e-3);
    s.repin(0.75);
    let down = s.eval(0.5, false); // t = (0.75-0.5)/0.75 = 1/3
    assert!((down - 53.0 * (2.0 / 3.0)).abs() < 1e-3, "fresh straight segment home, got {down}");
    assert!((down - 6.0).abs() > 1.0, "definitely not a retrace of the way up");
    assert!((s.eval(0.0, false) - 0.0).abs() < 1e-4, "still lands home exactly");
  }

  /// Kink gone at the endpoint: reach 1, and the whole return trip is one straight
  /// line from the target back to home.
  #[test]
  fn the_kink_dies_at_the_endpoint() {
    let mut s = Segments::new(0.0, 10.0, 0.0, 0.0, Shape::Linear);
    s.cur = s.eval(0.4, true);
    s.repin(0.4);
    s.s1 = 40.0; // kink at 0.4
    s.cur = s.eval(1.0, true);
    assert!((s.cur - 40.0).abs() < 1e-4);
    s.repin(1.0);
    for &(f, want) in &[(0.75_f32, 30.0_f32), (0.5, 20.0), (0.25, 10.0), (0.0, 0.0)] {
      let got = s.eval(f, false);
      assert!((got - want).abs() < 1e-3, "at f={f} want {want} got {got} -- one clean line");
    }
  }

  /// The quartic fade shape (wired in slice 4; the shape is proven now).
  #[test]
  fn a_quartic_segment_is_exact_at_the_ends_and_eased_between() {
    let s = Segments::new(0.0, 1.0, 0.0, 0.0, Shape::Quartic);
    assert_eq!(s.eval(0.0, true), 0.0, "silent at home");
    assert_eq!(s.eval(1.0, true), 1.0, "full at the far end");
    assert!((s.eval(0.5, true) - 0.0625).abs() < 1e-4, "0.5^4 -- eased out of silence");
  }

  #[test]
  fn a_quartic_segment_pins_a_kink_without_a_jump() {
    let mut s = Segments::new(0.0, 1.0, 0.0, 0.0, Shape::Quartic);
    s.cur = s.eval(0.6, true);
    assert!((s.cur - 0.1296).abs() < 1e-4);
    s.repin(0.6);
    assert!((s.eval(0.6, true) - 0.1296).abs() < 1e-4, "continuous across the kink");
    assert!((s.eval(1.0, true) - 1.0).abs() < 1e-4, "still exact at the top");
  }

  // ---- the units newtype ----

  #[test]
  fn a_step_pitch_converts_log_linearly_to_hz_and_rounds_to_its_filed_step() {
    let edo = 46;
    let fund = 80.0;
    let octave_up = StepPitch(edo as f32).to_hz(fund, edo);
    assert!((octave_up - 160.0).abs() < 1e-3, "edo steps up is exactly an octave");
    // Equal step distances are equal RATIOS -- the log-linearity a pitch slide needs.
    let a = StepPitch(10.0).to_hz(fund, edo);
    let b = StepPitch(20.0).to_hz(fund, edo);
    let c = StepPitch(30.0).to_hz(fund, edo);
    assert!(((b / a) - (c / b)).abs() < 1e-5, "equal steps = equal ratios");
    assert_eq!(StepPitch(13.4).filed(), 13);
    assert_eq!(StepPitch(13.6).filed(), 14);
  }

  // ---- the managed set, picks, and drives ----

  #[test]
  fn entering_takes_the_pedals_current_side_as_home() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.9));
    assert_eq!(st.home(), Home::High, "the foot is down, so home is the toe end");
    st.enter(Some(0.1));
    assert_eq!(st.home(), Home::Low);
  }

  #[test]
  fn a_pick_targets_the_nearest_candidate_and_a_re_pick_retargets_the_same_voice() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    let voices = [(drone(10), 10), (drone(50), 50)];
    st.pick(46, &voices);
    assert_eq!(st.pairings().len(), 1, "one voice pairs -- the nearer one");
    assert_eq!(st.pairings()[0].voice, drone(50), "50 is nearer to 46 than 10 is");
    assert_eq!(st.targets(), vec![46]);
    // Re-picking retargets the SAME voice rather than pairing a second one (the
    // vision's slide-to-one-then-another workflow).
    st.pick(48, &voices);
    assert_eq!(st.pairings().len(), 1);
    assert_eq!(st.targets(), vec![48]);
  }

  /// With nothing to pair, the pick asks for a fresh voice to swell in from silence
  /// (`2_discussion`: the pedal as a swell into any pitch).
  #[test]
  fn a_pick_with_no_candidates_asks_for_a_swell_voice() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    assert_eq!(st.pick(25, &[]).spawn_fade_ins, vec![25]);
    assert!(st.is_empty(), "nothing is managed until the caller reports the voice back");

    st.register_fade(drone(25), 25, FadeDir::In);
    assert_eq!(st.pairings().len(), 1);
    // Re-picking the same pitch must not spawn a second voice on the same drone key.
    assert_eq!(st.pick(25, &[]).spawn_fade_ins, Vec::<i32>::new(), "already swelling this pitch");
  }

  /// The swell: silent at the home end, full at the far end, quartic-eased between, and
  /// driving AMPLITUDE while leaving the pitch alone.
  #[test]
  fn a_swell_voice_rises_from_silence_to_full_across_the_pedal() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(25, &[]);
    st.register_fade(drone(25), 25, FadeDir::In);

    let amp = |st: &PedalSlideState| st.drives()[0].amp.unwrap();
    assert_eq!(amp(&st), 0.0, "silent at the heel");
    assert!((st.drives()[0].pitch.0 - 25.0).abs() < 1e-4, "at its own fixed pitch");
    st.on_pedal(0.5);
    assert!((amp(&st) - 0.0625).abs() < 1e-3, "quartic: eased out of silence");
    st.on_pedal(1.0);
    assert!((amp(&st) - 1.0).abs() < 1e-4, "full at the toe");
    assert!((st.drives()[0].pitch.0 - 25.0).abs() < 1e-4, "and its pitch never moved");
  }

  /// A swell picked MID-TRAVEL pins a kink like a pitch retarget, so no amplitude jumps
  /// -- the cure `2_discussion` found for the "discontinuity at F" the vision caught
  /// itself on.
  #[test]
  fn a_swell_started_mid_travel_does_not_jump() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.on_pedal(0.6);
    st.pick(25, &[]);
    st.register_fade(drone(25), 25, FadeDir::In);
    assert_eq!(st.drives()[0].amp, Some(0.0), "born silent, right where the pedal is");
    st.on_pedal(1.0);
    assert!((st.drives()[0].amp.unwrap() - 1.0).abs() < 1e-4, "and still exact at the toe");
  }

  /// A fade is not an edit-mode voice, so the edit selection has no say over it -- but
  /// it dies with its voice, like everything else in the managed set.
  #[test]
  fn reconcile_leaves_fades_alone_but_drops_voices_that_ended() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.register_fade(drone(25), 25, FadeDir::In);
    assert!(st.reconcile(&HashSet::new(), |_| true).is_empty(), "no selection, still swelling");
    assert!(!st.is_empty());
    st.reconcile(&HashSet::new(), |_| false);
    assert!(st.is_empty(), "its voice ended, so it left the managed set");
  }

  /// A swelling voice FLASHES at its own pitch while it is still coming in, then stops
  /// once the pedal has carried it home -- from there it is an ordinary sustained note.
  #[test]
  fn a_swelling_voice_flashes_until_it_has_arrived() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.register_fade(drone(25), 25, FadeDir::In);
    let (_, target) = st.led_roles();
    assert!(target.contains(&25), "flashing while it swells in");
    st.on_pedal(1.0); // arrive: home flips to the full-volume end
    let (home, target) = st.led_roles();
    assert!(!target.contains(&25), "arrived: it stops flashing");
    assert!(home.contains(&25), "and is just a lit, sustained note now");
  }

  #[test]
  fn a_full_flight_drives_the_voice_from_home_to_target_and_back() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(22, &[(drone(10), 10)]);
    assert!((st.drives()[0].pitch.0 - 10.0).abs() < 1e-4, "at the heel the voice is home");
    st.on_pedal(0.5);
    assert!((st.drives()[0].pitch.0 - 16.0).abs() < 1e-3, "halfway is halfway");
    st.on_pedal(1.0);
    assert!((st.drives()[0].pitch.0 - 22.0).abs() < 1e-4, "the toe reaches the target exactly");
    // ARRIVAL IS A ROLE SWAP, not a completion: the far side now holds the old home,
    // so coming back reaches it exactly (the reverted build's bug 3).
    assert_eq!(st.home(), Home::High, "arriving made the target home");
    assert_eq!(st.targets(), vec![10], "and the old home became the target");
    st.on_pedal(0.0);
    assert!((st.drives()[0].pitch.0 - 10.0).abs() < 1e-4, "back to the old home, exactly");
    assert_eq!(st.home(), Home::Low, "and the roles swap again -- the pedal lives forever");
  }

  #[test]
  fn a_retarget_only_moves_the_far_goalpost_and_never_jumps_the_pitch() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(30, &[(drone(10), 10)]); // home low 10, far high 30
    st.on_pedal(0.5);
    let before = st.drives()[0].pitch.0;
    assert!((before - 20.0).abs() < 1e-3);
    st.pick(40, &[(drone(10), 10)]);
    let p = &st.pairings()[0];
    assert_eq!(p.seg.s0, 10.0, "the home (low) goalpost is untouched");
    assert_eq!(p.seg.s1, 40.0, "only the far (high) goalpost moved");
    assert!((st.drives()[0].pitch.0 - before).abs() < 1e-3, "the current pitch did not jump");
    st.on_pedal(1.0);
    assert!((st.drives()[0].pitch.0 - 40.0).abs() < 1e-4, "and the NEW target is exact");
  }

  // ---- identity: proposal, confirmation, and refusal ----

  #[test]
  fn arrival_proposes_a_refile_that_only_takes_effect_once_confirmed() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(30, &[(drone(10), 10)]);
    let refiles = st.on_pedal(1.0);
    assert_eq!(refiles.len(), 1);
    let r = refiles[0];
    assert_eq!((r.from, r.to), (10, 30));
    assert_eq!(r.voice_old, drone(10));
    assert_eq!(r.voice_new, drone(30), "a pitch-keyed drone re-keys with its filing");
    // Until the owner confirms, the pairing still names the OLD identity -- so a
    // decline leaves engine and world agreeing rather than drifting.
    assert_eq!(st.pairings()[0].voice, drone(10));
    assert_eq!(st.pairings()[0].filed, 10);
    st.confirm_refile(&r);
    assert_eq!(st.pairings()[0].voice, drone(30));
    assert_eq!(st.pairings()[0].filed, 30);
  }

  #[test]
  fn a_fingered_voice_keeps_its_cell_key_across_a_refile() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(30, &[(finger((5, 5)), 10)]);
    let refiles = st.on_pedal(1.0);
    assert_eq!(refiles[0].voice_new, finger((5, 5)), "a finger's identity is its cell");
    assert_eq!(refiles[0].to, 30, "but its FILED pitch still moves");
    st.confirm_refile(&refiles[0]);
    assert_eq!(st.pairings()[0].filed, 30);
  }

  #[test]
  fn an_unconfirmed_refile_leaves_the_voice_drivable_at_its_old_key() {
    // The owner declined (something already sounds at the target pitch). The flight
    // must continue under the key the voice map really holds -- the reverted build
    // instead drove a key that did not exist yet and silently lost every step.
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(30, &[(drone(10), 10)]);
    st.on_pedal(1.0); // proposed, deliberately NOT confirmed
    assert_eq!(st.drives()[0].voice, drone(10), "still addressed by the key that exists");
    st.on_pedal(0.5);
    assert_eq!(st.drives()[0].voice, drone(10));
    assert!(st.drives()[0].pitch.0 < 30.0, "and it is still being driven");
  }

  // ---- reconcile ----

  #[test]
  fn reconcile_drops_a_pairing_whose_note_left_the_edit_selection() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(30, &[(drone(10), 10)]);
    let edited: HashSet<i32> = [10].into_iter().collect();
    assert!(st.reconcile(&edited, |_| true).is_empty(), "still selected: nothing dropped");
    let dropped = st.reconcile(&HashSet::new(), |_| true);
    assert_eq!(dropped, vec![drone(10)], "deselected: the pairing is cancelled");
    assert!(st.is_empty());
  }

  #[test]
  fn reconcile_follows_a_confirmed_refile_rather_than_the_pitch_it_started_at() {
    // The bug-2 shape, pinned: after the note re-files to its target, the edit set
    // names the TARGET pitch. Reconcile must read the pairing's own filed pitch, not
    // re-derive one from a stale notion of home.
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(30, &[(drone(10), 10)]);
    let refiles = st.on_pedal(1.0);
    st.confirm_refile(&refiles[0]);
    let edited: HashSet<i32> = [30].into_iter().collect();
    assert!(st.reconcile(&edited, |_| true).is_empty(), "the note is selected under its NEW pitch");
    assert!(!st.is_empty(), "the pairing survives -- it was never really deselected");
  }

  // ---- LED roles + exit ----

  #[test]
  fn led_roles_split_home_and_target_and_swap_on_arrival() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(30, &[(drone(10), 10)]);
    let (home, target) = st.led_roles();
    assert!(home.contains(&10) && target.contains(&30));
    let refiles = st.on_pedal(1.0);
    st.confirm_refile(&refiles[0]);
    let (home, target) = st.led_roles();
    assert!(home.contains(&30), "the arrived-at pitch is now the lit, danced home");
    assert!(target.contains(&10), "and the pitch we came from now flashes as the target");
  }

  #[test]
  fn exit_hands_back_every_driven_voice_for_freezing() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    st.pick(20, &[(drone(10), 10)]);
    assert_eq!(st.exit(), vec![drone(10)], "the owner freezes exactly these");
    assert!(!st.mode() && st.is_empty());
  }

  // ---- the wrong-way gate (Jeff's design, 2026-07-20) ----

  /// Picked with the pedal parked mid-travel: the pitch holds at H on the near side of
  /// the pick and slides H -> T beyond it, and the VOLUME is what says which way is
  /// which -- full from the pick outward, fading quartically to silence at the home
  /// end. So the home pitch can never be lost, and going the wrong way is audibly
  /// going nowhere.
  #[test]
  fn going_the_wrong_way_holds_the_pitch_and_fades_the_volume_out() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.4));
    st.pick(27, &[(drone(0), 0)]);

    // Toward the target: pitch slides, volume stays full.
    for &(f, want) in &[(0.4_f32, 0.0_f32), (0.7, 13.5), (1.0, 27.0)] {
      st.on_pedal(f);
      let d = st.drives()[0];
      assert!((d.pitch.0 - want).abs() < 1e-3, "at f={f} wanted {want}, got {}", d.pitch.0);
      assert!(d.amp.is_none_or(|a| (a - 1.0).abs() < 1e-4), "full volume the right way");
    }
  }

  #[test]
  fn the_wrong_way_is_silent_at_the_end_and_recovers_exactly_at_the_pick() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.4));
    st.pick(27, &[(drone(0), 0)]);

    // The wrong way: pitch pinned at H, volume falling quartically.
    let mut last = 1.0_f32;
    for f in [0.3_f32, 0.2, 0.1] {
      st.on_pedal(f);
      let d = st.drives()[0];
      assert!((d.pitch.0 - 0.0).abs() < 1e-4, "the pitch holds at H, got {}", d.pitch.0);
      let amp = d.amp.expect("the wrong way drives volume");
      assert!(amp < last, "and the volume keeps falling: {amp} !< {last}");
      last = amp;
    }
    // Turning back BEFORE the end (the end itself dissolves the pin -- see
    // `reaching_the_wrong_end_makes_the_whole_travel_useful_again`), the pitch is still
    // pinned at H and the volume climbs back the way it fell.
    st.on_pedal(0.2);
    assert!((st.drives()[0].pitch.0 - 0.0).abs() < 1e-4, "still H on the way back");
    assert!(st.drives()[0].amp.unwrap() > last, "and the volume is coming back");
    st.on_pedal(0.4);
    assert_eq!(st.drives()[0].amp, Some(1.0), "full again exactly at the pick point");
    assert!((st.drives()[0].pitch.0 - 0.0).abs() < 1e-4, "and exactly at H there");

    // All the way to the home end IS silence.
    st.on_pedal(0.0);
    assert_eq!(st.drives()[0].amp, Some(0.0), "silent at the home end");
  }

  /// The pin is the one kink that outlives the pedal leaving it. Every other re-pins on
  /// reversal; if this one did, H would drift to wherever the foot turned around and
  /// could never be recovered.
  #[test]
  fn the_wrong_way_pin_does_not_follow_the_foot() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.4));
    st.pick(27, &[(drone(0), 0)]);
    // Wander around inside the wrong-way region, reversing twice.
    for f in [0.25_f32, 0.1, 0.3, 0.15] {
      st.on_pedal(f);
      assert!((st.drives()[0].pitch.0 - 0.0).abs() < 1e-4, "still pinned at H at f={f}");
    }
    // Back through the pick point: full volume, and the slide starts from there again
    // with its ORIGINAL gearing (27 steps over the remaining 0.6 of travel).
    st.on_pedal(0.4);
    assert_eq!(st.drives()[0].amp, Some(1.0), "full volume exactly at the pick point");
    st.on_pedal(0.7);
    assert!(
      (st.drives()[0].pitch.0 - 13.5).abs() < 1e-3,
      "the gearing is unchanged by the wandering, got {}",
      st.drives()[0].pitch.0,
    );
  }

  /// Reaching the FAR end drops the gate for good: the map becomes the plain line
  /// between the two pitches and the pedal stops touching volume.
  #[test]
  fn arriving_dissolves_the_gate_and_hands_volume_back() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.4));
    st.pick(27, &[(drone(0), 0)]);
    st.on_pedal(1.0);
    assert_eq!(st.drives()[0].amp, None, "the pedal no longer controls volume");
    // ...and the whole travel is now one clean line, exact at both ends.
    st.on_pedal(0.5);
    assert!((st.drives()[0].pitch.0 - 13.5).abs() < 1e-3, "got {}", st.drives()[0].pitch.0);
    st.on_pedal(0.0);
    assert!((st.drives()[0].pitch.0 - 0.0).abs() < 1e-4, "back to H exactly");
    assert_eq!(st.drives()[0].amp, None, "still no volume duty");
  }

  /// Reaching the WRONG end moves both pins to the opposite end, so the way back sweeps
  /// pitch H -> T and volume 0 -> full across the whole travel, arriving together.
  #[test]
  fn reaching_the_wrong_end_makes_the_whole_travel_useful_again() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.4));
    st.pick(27, &[(drone(0), 0)]);
    st.on_pedal(0.0);
    assert_eq!(st.drives()[0].amp, Some(0.0), "silent at the wrong end");

    st.on_pedal(0.5);
    let d = st.drives()[0];
    assert!((d.pitch.0 - 13.5).abs() < 1e-3, "pitch now spans the FULL travel, got {}", d.pitch.0);
    let amp = d.amp.expect("volume is still coming back in");
    assert!((amp - 0.0625).abs() < 1e-3, "quartic across the whole travel, got {amp}");

    st.on_pedal(1.0);
    let d = st.drives()[0];
    assert!((d.pitch.0 - 27.0).abs() < 1e-4, "reaches T exactly");
    assert_eq!(d.amp, None, "and at T the volume is full and handed back for good");
  }

  /// Picked AT an endpoint there is no wrong way to go, so no gate is opened and the
  /// pedal never touches volume -- the ordinary case must stay ordinary.
  #[test]
  fn a_pick_at_an_endpoint_opens_no_gate() {
    for park in [0.0_f32, 1.0] {
      let mut st = PedalSlideState::new();
      st.enter(Some(park));
      st.pick(27, &[(drone(0), 0)]);
      assert!(st.pairings()[0].gate.is_none(), "parked at {park}: no gate");
      assert_eq!(st.drives()[0].amp, None, "and no volume duty");
    }
  }

  // ---- the anchored-kink volume return (2_discussion's amendment) ----

  /// The amendment's whole point: leaving slide mode must not leave a dead zone. Push
  /// or pull and the volume responds AT ONCE, in either direction, from wherever it
  /// froze.
  #[test]
  fn the_volume_return_is_continuous_at_exit_and_immediate_both_ways() {
    let mut vr = VolumeReturn::new(0.5, 0.5);
    assert_eq!(vr.on_pedal(0.5), Some(0.5), "continuous at the exit point -- no jump");
    let up = vr.on_pedal(0.75).unwrap();
    assert!(up > 0.5, "raising the pedal raises the volume at once, got {up}");
    // Back down THROUGH the frozen point keeps lowering it -- the reversal re-pins, so
    // the way down is a fresh segment rather than a retrace with a dead patch.
    let mid = vr.on_pedal(0.5).unwrap();
    assert!(mid < up, "coming back down lowers it immediately, got {mid}");
    let low = vr.on_pedal(0.25).unwrap();
    assert!(low < mid, "and keeps lowering past the old freeze point, got {low}");
  }

  /// Reaching either end hands back to the ordinary taper, continuously: the segment is
  /// exactly 0 at the heel and 1 at the toe, which is where the taper starts and ends.
  #[test]
  fn the_volume_return_reverts_to_the_ordinary_taper_at_an_endpoint() {
    let mut vr = VolumeReturn::new(0.4, 0.6);
    assert!(vr.on_pedal(0.3).is_some(), "still on the kinked map mid-travel");
    assert_eq!(vr.on_pedal(0.0), None, "reaching the heel reverts");
    assert!(vr.reverted());
    assert_eq!(vr.on_pedal(0.5), None, "and it stays reverted");

    let mut vr = VolumeReturn::new(0.4, 0.6);
    assert_eq!(vr.on_pedal(1.0), None, "the toe end reverts too");
    assert!(vr.reverted());
  }

  #[test]
  fn the_pedal_is_inert_until_the_first_pick() {
    let mut st = PedalSlideState::new();
    st.enter(Some(0.0));
    assert!(st.on_pedal(1.0).is_empty(), "no pairings: no re-files");
    assert!(st.drives().is_empty(), "and nothing to drive -- no pitch is yanked");
  }
}
