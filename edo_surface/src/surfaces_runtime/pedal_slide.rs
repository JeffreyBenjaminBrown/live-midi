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
  #[allow(dead_code)] // the fades land in slice 4; the shape and its tests are here now
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

/// WHEN home swaps ends. The rebuild lands this in two steps (`6_plan.org`'s slices):
/// slice 1 swaps roles only on ARRIVAL at an endpoint -- enough to prove "the pedal
/// lives forever, the far side now holds the old home" without the re-filing
/// complexity -- and slice 3 moves the swap to the pedal's midpoint (with the
/// hysteresis band) as `1_vision` specifies. One knob so the two differ in exactly one
/// place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipPolicy {
  /// Roles swap when the pedal REACHES the far end.
  AtEndpoints,
  /// Roles swap at the pedal's midpoint, with the hysteresis band (`1_vision`).
  AtMidpoint,
}

/// What one voice is doing under the pedal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
  /// A pitch slide: `seg` holds fractional EDO-step pitches; amplitude is untouched.
  Pitch,
}

/// One voice the pedal manages: its identity, where it is FILED, and its map.
///
/// `voice` and `filed` are the pairing's copy of facts that also live in the voice map
/// and the ring. They are changed in exactly one place -- [`PedalSlideState::confirm_refile`],
/// called by the owner AFTER it has moved the real ones -- so they cannot drift.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pairing {
  pub voice: VoiceSource,
  /// The integer pitch this voice is currently filed and painted at (the ring's sets,
  /// its drone key if it is a drone, the cell the dances mark).
  pub filed: i32,
  pub kind: Kind,
  pub seg: Segments,
}

impl Pairing {
  /// The pitch this voice should sound right now.
  pub fn current_pitch(&self) -> StepPitch {
    StepPitch(self.seg.cur)
  }
}

/// One drive command the wiring applies to a voice each step: the pitch it should be
/// sounding now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drive {
  pub voice: VoiceSource,
  pub pitch: StepPitch,
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

/// One grid's pedal-slide engine.
#[derive(Debug)]
pub struct PedalSlideState {
  /// Is slide mode on for this grid? While on, the EX-P pedal drives this engine
  /// instead of volume.
  mode: bool,
  /// The pedal's last normalized fraction `[0, 1]`.
  cur_f: f32,
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
  /// The grid volume frozen when mode turned on, handed back when mode turns off.
  frozen_volume: f32,
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
      moving_up: true,
      home: Home::Low,
      flip: FlipPolicy::AtEndpoints,
      pairings: Vec::new(),
      frozen_volume: 1.0,
    }
  }

  pub fn mode(&self) -> bool {
    self.mode
  }

  pub fn home(&self) -> Home {
    self.home
  }

  pub fn fraction(&self) -> f32 {
    self.cur_f
  }

  pub fn frozen_volume(&self) -> f32 {
    self.frozen_volume
  }

  pub fn is_empty(&self) -> bool {
    self.pairings.is_empty()
  }

  pub fn pairings(&self) -> &[Pairing] {
    &self.pairings
  }

  /// The current target pitches (integer EDO steps) -- the FAR goalpost of each
  /// pairing, i.e. where the pedal is heading.
  pub fn targets(&self) -> Vec<i32> {
    self
      .pairings
      .iter()
      .map(|p| StepPitch(p.seg.side(!self.home.is_low())).filed())
      .collect()
  }

  /// Enter slide mode with the pedal at fraction `f`, freezing the grid volume at
  /// `frozen_volume`. Starts with no pairings, so the pedal is inert until the first
  /// pick (no pitch is yanked). Home follows the pedal's current side, so the first
  /// pick's far endpoint is the one away from the foot.
  pub fn enter(&mut self, f: f32, frozen_volume: f32) {
    self.mode = true;
    self.frozen_volume = frozen_volume;
    self.pairings.clear();
    self.cur_f = f.clamp(0.0, 1.0);
    self.home = if self.cur_f <= MIDPOINT { Home::Low } else { Home::High };
  }

  /// Leave slide mode mid-anything: the managed set empties (voices freeze exactly
  /// where they are -- smoothness above all; they are sustained voices, a later drag
  /// can tidy an off-grid pitch), and the frozen grid volume comes back for the
  /// pedal's return to volume duty. Returns the voices that were being driven, for
  /// the owner to freeze, and that volume.
  pub fn exit(&mut self) -> (Vec<VoiceSource>, f32) {
    self.mode = false;
    let dropped = self.pairings.drain(..).map(|p| p.voice).collect();
    (dropped, self.frozen_volume)
  }

  /// Reconcile the managed set against the grid's live edit selection (`edited`,
  /// integer EDO steps): a pairing whose voice left the selection -- an editmode-clear
  /// deselecting it, or a sustain-clear ending it (which cascades the edit reason away)
  /// -- is cancelled, and its voice returned for freezing. Compares against the
  /// pairing's OWN `filed` pitch, which the owner keeps in step with the ring, so this
  /// can no longer misread a note mid-hand-off (the reverted build's bug 2, whose
  /// sharpest edge was exactly this call reading stale ring state).
  pub fn reconcile(&mut self, edited: &HashSet<i32>) -> Vec<VoiceSource> {
    if !self.mode {
      return Vec::new();
    }
    let mut dropped = Vec::new();
    self.pairings.retain(|p| {
      if edited.contains(&p.filed) {
        true
      } else {
        dropped.push(p.voice);
        false
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
    if !self.mode {
      self.cur_f = f;
      return Vec::new();
    }
    let f_old = self.cur_f;
    if (f - f_old).abs() > EPS {
      let up = f > f_old;
      if up != self.moving_up && !self.pairings.is_empty() {
        // A reversal: re-pin every anchor at the reversal point, so the return path is
        // a fresh segment to home (the hysteresis). At an endpoint this is what fuses
        // the two segments into one clean line -- "the kink is gone".
        for p in &mut self.pairings {
          p.seg.repin(f_old);
        }
      }
      self.moving_up = up;
      for p in &mut self.pairings {
        p.seg.cur = p.seg.eval(f, up);
      }
    }
    self.cur_f = f;
    self.propose_flip(f)
  }

  /// Has the pedal crossed the point where home swaps ends? If so, flip the LABEL now
  /// (it is a global role marker and moves no value) and propose one re-file per
  /// pairing whose painted pitch therefore moves.
  fn propose_flip(&mut self, f: f32) -> Vec<Refile> {
    let crossed = match (self.flip, self.home) {
      // Slice 1: arrival at the far end is the swap. The pedal never dies -- reaching
      // the target makes it home, and the far side now holds the pitch you came from,
      // so sliding back returns there (`6_plan.org`: nothing is ever "completed" while
      // slide mode is on).
      (FlipPolicy::AtEndpoints, Home::Low) => f >= 1.0,
      (FlipPolicy::AtEndpoints, Home::High) => f <= 0.0,
      // Slice 3: the vision's midpoint, with a band so a resting foot cannot flicker
      // the roles.
      (FlipPolicy::AtMidpoint, Home::Low) => f > MIDPOINT + HYSTERESIS_BAND,
      (FlipPolicy::AtMidpoint, Home::High) => f < MIDPOINT - HYSTERESIS_BAND,
    };
    if !crossed || self.pairings.is_empty() {
      return Vec::new();
    }
    let new_home = self.home.flipped();
    let mut refiles = Vec::new();
    for p in &self.pairings {
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

  /// A pick of a FREE pitch while slide mode is on. `candidates` is the grid's
  /// pairable voices as `(key, current filed pitch)` -- its edit-mode voices. Per the
  /// vision's home-proximity rule the pick becomes the new target of the NEAREST voice
  /// (ties to the lower voice), replacing that pairing's FAR goalpost with a kink at
  /// the current fraction -- so a pick can never yank the pitch you are nearest to.
  ///
  /// Returns whether anything was picked (slice 1: with nothing pairable, nothing
  /// happens; the no-edit-voices swell is slice 4).
  pub fn pick(&mut self, pitch: i32, candidates: &[(VoiceSource, i32)]) -> bool {
    let Some(&(voice, base)) = candidates.iter().min_by_key(|(_, base)| ((*base - pitch).abs(), *base))
    else {
      return false;
    };
    let pairing = self.pitch_pairing(voice, base, pitch);
    if let Some(slot) = self.pairings.iter_mut().find(|p| p.voice == pairing.voice) {
      *slot = pairing;
    } else {
      self.pairings.push(pairing);
    }
    true
  }

  /// Build (or retarget) a pitch pairing for `voice`, currently filed at `base`, aiming
  /// at `target`. A pre-existing pairing keeps its map and only retargets its FAR
  /// endpoint (kink at the current fraction).
  fn pitch_pairing(&self, voice: VoiceSource, base: i32, target: i32) -> Pairing {
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
      return Pairing { voice, filed: prev.filed, kind: Kind::Pitch, seg };
    }
    // A fresh pairing: the home endpoint is the voice's current pitch, the far endpoint
    // the target, anchored where the pedal is.
    let (s0, s1) =
      if far_is_high { (base as f32, target as f32) } else { (target as f32, base as f32) };
    let seg = Segments::new(s0, s1, self.cur_f, base as f32, Shape::Linear);
    Pairing { voice, filed: base, kind: Kind::Pitch, seg }
  }

  /// The per-voice drive commands to apply this instant.
  pub fn drives(&self) -> Vec<Drive> {
    self.pairings.iter().map(|p| Drive { voice: p.voice, pitch: p.current_pitch() }).collect()
  }

  /// The LED roles: the HOME pitches (where each voice is filed -- lit + danced through
  /// the ordinary sustained path) and the TARGET pitches (the far goalpost, which
  /// flashes and does not dance).
  pub fn led_roles(&self) -> (HashSet<i32>, HashSet<i32>) {
    let mut home = HashSet::new();
    let mut target = HashSet::new();
    for p in &self.pairings {
      home.insert(p.filed);
      target.insert(StepPitch(p.seg.side(!self.home.is_low())).filed());
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
    st.enter(0.9, 1.0);
    assert_eq!(st.home(), Home::High, "the foot is down, so home is the toe end");
    st.enter(0.1, 1.0);
    assert_eq!(st.home(), Home::Low);
  }

  #[test]
  fn a_pick_targets_the_nearest_candidate_and_a_re_pick_retargets_the_same_voice() {
    let mut st = PedalSlideState::new();
    st.enter(0.0, 1.0);
    let voices = [(drone(10), 10), (drone(50), 50)];
    assert!(st.pick(46, &voices), "picked");
    assert_eq!(st.pairings().len(), 1, "one voice pairs -- the nearer one");
    assert_eq!(st.pairings()[0].voice, drone(50), "50 is nearer to 46 than 10 is");
    assert_eq!(st.targets(), vec![46]);
    // Re-picking retargets the SAME voice rather than pairing a second one (the
    // vision's slide-to-one-then-another workflow).
    assert!(st.pick(48, &voices));
    assert_eq!(st.pairings().len(), 1);
    assert_eq!(st.targets(), vec![48]);
  }

  #[test]
  fn a_pick_with_no_candidates_does_nothing_in_slice_1() {
    let mut st = PedalSlideState::new();
    st.enter(0.0, 1.0);
    assert!(!st.pick(25, &[]), "the swell into a fresh voice is slice 4");
    assert!(st.is_empty());
  }

  #[test]
  fn a_full_flight_drives_the_voice_from_home_to_target_and_back() {
    let mut st = PedalSlideState::new();
    st.enter(0.0, 1.0);
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
    st.enter(0.0, 1.0);
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
    st.enter(0.0, 1.0);
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
    st.enter(0.0, 1.0);
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
    st.enter(0.0, 1.0);
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
    st.enter(0.0, 1.0);
    st.pick(30, &[(drone(10), 10)]);
    let edited: HashSet<i32> = [10].into_iter().collect();
    assert!(st.reconcile(&edited).is_empty(), "still selected: nothing dropped");
    let dropped = st.reconcile(&HashSet::new());
    assert_eq!(dropped, vec![drone(10)], "deselected: the pairing is cancelled");
    assert!(st.is_empty());
  }

  #[test]
  fn reconcile_follows_a_confirmed_refile_rather_than_the_pitch_it_started_at() {
    // The bug-2 shape, pinned: after the note re-files to its target, the edit set
    // names the TARGET pitch. Reconcile must read the pairing's own filed pitch, not
    // re-derive one from a stale notion of home.
    let mut st = PedalSlideState::new();
    st.enter(0.0, 1.0);
    st.pick(30, &[(drone(10), 10)]);
    let refiles = st.on_pedal(1.0);
    st.confirm_refile(&refiles[0]);
    let edited: HashSet<i32> = [30].into_iter().collect();
    assert!(st.reconcile(&edited).is_empty(), "the note is selected under its NEW pitch");
    assert!(!st.is_empty(), "the pairing survives -- it was never really deselected");
  }

  // ---- LED roles + exit ----

  #[test]
  fn led_roles_split_home_and_target_and_swap_on_arrival() {
    let mut st = PedalSlideState::new();
    st.enter(0.0, 1.0);
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
  fn exit_hands_back_every_driven_voice_and_the_frozen_volume() {
    let mut st = PedalSlideState::new();
    st.enter(0.0, 0.42);
    st.pick(20, &[(drone(10), 10)]);
    let (dropped, vol) = st.exit();
    assert_eq!(dropped, vec![drone(10)], "the owner freezes exactly these");
    assert!((vol - 0.42).abs() < 1e-6, "the frozen volume comes back");
    assert!(!st.mode() && st.is_empty());
  }

  #[test]
  fn the_pedal_is_inert_until_the_first_pick() {
    let mut st = PedalSlideState::new();
    st.enter(0.0, 1.0);
    assert!(st.on_pedal(1.0).is_empty(), "no pairings: no re-files");
    assert!(st.drives().is_empty(), "and nothing to drive -- no pitch is yanked");
  }
}
