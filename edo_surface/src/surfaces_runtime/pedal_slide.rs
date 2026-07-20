//! The pedal-slide engine, per grid -- pure logic, no I/O (see
//! `TODO/pedal-slide/1_vision.org` and `2_discussion.org`). Each grid owns one
//! [`PedalSlideState`] behind its own lock; the grid thread (toggle, picks, painting)
//! and the expression-pedal thread (the pedal drive) both reach it.
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
//! The SAME structure carries amplitude fades ([`Shape::Quartic`]): the segment shape
//! is the quartic rescaled to its span, so a fading voice is silent at one end and at
//! full at the other, continuous and hysteretic by exactly the same construction. This
//! is the discussion's cure for the fade "discontinuity at F" the vision caught itself
//! on. For a linear (pitch) segment, re-anchoring at every step and evaluating the
//! whole segment from a fixed anchor give the same trajectory; for the quartic they do
//! not, and only the fixed-anchor-until-an-event reading is non-degenerate (an
//! incrementally re-anchored quartic has slope 0 at the anchor and would never move),
//! so the anchor is re-pinned only on a reversal, a retarget, or reaching an endpoint.
//!
//! *Pitch lives in EDO-step space.* A pitch segment interpolates fractional EDO steps
//! linearly, which IS log-linear in frequency (equal steps = equal cents), so the
//! wiring converts `cur` to Hz with `freq_for_pitch` and the segment math needs no
//! logarithms of its own.
//!
//! Phase A covered the toggle, the target set, the per-voice pairings, the home/target
//! hysteresis, the anchored map, and the fades. Phase B wires
//! [`PedalSlideState::match_targets`] to chord recall: a stored chord selected while
//! slide mode is on does not sound -- its pitches join the target set, existing slides
//! keep their targets, untargeted edit voices match ascending-nearest-ties-lower,
//! extra targets swell in,
//! and leftovers fade out. Fade voices keep the pitch-keyed `SurfaceDrone` key (they
//! ARE drones -- sustained, fingerless); `match_targets` reuses an existing fade rather
//! than spawning a second at one pitch, and the wiring skips a chord pitch a plain
//! finger/drone already holds, so the key never collides.

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
}

/// The pedal's return to VOLUME duty when slide mode is turned off (`2_discussion`
/// amendment): rather than a dead-zone catch-up (which would confuse -- pulling the
/// pedal down with no effect), the pedal-to-volume map becomes the SAME anchored-kink
/// construction as a slide fade -- two quartic segments joined at `(F_now, V_frozen)`,
/// one to `(0, 0)`, one to `(1, 1)` -- so moving the pedal EITHER way changes the
/// volume immediately. Once the pedal reaches an endpoint the map reverts to the
/// ordinary volume taper (`PedalVolumeCurve`), whose endpoints are exactly 0 and 1, so
/// the hand-off is continuous. Reuses [`Segments`] rather than a second machine.
#[derive(Clone, Copy, Debug)]
pub struct VolumeReturn {
  seg: Segments,
  cur_f: f32,
  moving_up: bool,
  reverted: bool,
}

impl VolumeReturn {
  /// Begin the return with the pedal at fraction `f` and the volume frozen at
  /// `v_frozen`. The endpoints match the taper's (0 at the heel, 1 at the toe).
  pub fn new(f: f32, v_frozen: f32) -> Self {
    let f = f.clamp(0.0, 1.0);
    VolumeReturn {
      seg: Segments::new(0.0, 1.0, f, v_frozen.clamp(0.0, 1.0), Shape::Quartic),
      cur_f: f,
      moving_up: true,
      reverted: false,
    }
  }

  /// Apply a pedal reading. Returns `Some(gain)` while the kinked return map is in
  /// effect, or `None` once the pedal has reached an endpoint and the caller should
  /// resume the ordinary taper (`PedalVolumeCurve::gain`). Continuous at the hand-off:
  /// the segment's endpoint value equals the taper's there (0 and 1).
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
    // Reaching either endpoint reverts to the ordinary taper (continuous: the segment
    // is exactly 0 at f=0 and 1 at f=1, as the taper is).
    if f <= 0.0 || f >= 1.0 {
      self.reverted = true;
      return None;
    }
    Some(self.seg.cur.clamp(0.0, 1.0))
  }

  #[allow(dead_code)] // asserted in the pure-module tests
  pub fn reverted(&self) -> bool {
    self.reverted
  }
}

/// Which side of the pedal is "home" -- the end the voice is currently nearest to.
/// Home is `Low` (nearest pedal fraction 0) or `High` (nearest 1); it flips with
/// hysteresis and governs (a) which endpoint a new pick replaces (always the FAR one)
/// and (b) the LED roles. The flip never moves any value -- the map does not mention
/// "home".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Home {
  Low,
  High,
}

/// What one edit-mode (or faded) voice is doing under the pedal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
  /// A pitch slide: `seg` holds fractional EDO-step pitches; amplitude stays full.
  Pitch,
  /// An amplitude swell into a fixed pitch (an extra target, or the one-pick swell):
  /// `seg` holds amplitude in `[0, 1]`; the pitch is fixed at `pitch`.
  FadeIn,
  /// An amplitude fade to silence (a leftover voice with no target): `seg` holds
  /// amplitude, `pitch` fixed. Produced by the batch matcher (phase B chord recall);
  /// unit-tested now.
  #[allow(dead_code)]
  FadeOut,
}

/// One voice paired to the pedal: its map, its kind, and (for fades) the fixed pitch
/// the swell/fade is applied at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pairing {
  pub voice: VoiceSource,
  pub kind: Kind,
  pub seg: Segments,
  /// The fixed pitch of a fade voice (ignored for `Pitch`, whose pitch is `seg.cur`).
  pub pitch: f32,
}

impl Pairing {
  /// The pitch (fractional EDO steps) this voice should sound right now.
  pub fn current_pitch(&self) -> f32 {
    match self.kind {
      Kind::Pitch => self.seg.cur,
      Kind::FadeIn | Kind::FadeOut => self.pitch,
    }
  }

  /// The amplitude in `[0, 1]` a fade voice should sound at right now, or `None` for
  /// a pitch pairing (whose amplitude the pedal does not drive -- volume is frozen).
  pub fn current_amp(&self) -> Option<f32> {
    match self.kind {
      Kind::Pitch => None,
      Kind::FadeIn | Kind::FadeOut => Some(self.seg.cur.clamp(0.0, 1.0)),
    }
  }
}

/// One drive command the wiring applies to a voice each pedal reading: the target
/// pitch (Hz-convertible EDO steps) and, for fades only, the target amplitude.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drive {
  pub voice: VoiceSource,
  pub pitch: f32,
  pub amp: Option<f32>,
}

/// A voice re-filed because home flipped across the midpoint: the note is now painted
/// at the OTHER end's pitch, so the ring's sets/dances/reflections follow
/// (`ring::RingStore::note_moved`), and a pitch-keyed DRONE voice re-keys from
/// `voice_old` to `voice_new` (a cell-keyed fingered voice keeps its key, so the two
/// are equal). Applied by the grid thread, which owns the ring and the sink. Pitches
/// are rounded to the nearest EDO step (the sets are integer-keyed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refile {
  pub voice_old: VoiceSource,
  pub voice_new: VoiceSource,
  pub from: i32,
  pub to: i32,
}

/// The result of a pick while slide mode is on: which pitches need a NEW fade-in voice
/// spawned from silence (the extra-targets / one-note-swell case), and which existing
/// voices became fade-outs (their pairing already updated in place). The wiring spawns
/// the fade-ins and calls [`PedalSlideState::register_fade_in`] with the created voice
/// keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PickOutcome {
  /// Pitches (integer EDO steps) that need a fresh fade-in voice spawned.
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
  /// The pedal's last travel direction (true = rising toward `s1`).
  moving_up: bool,
  /// Which side is home right now.
  home: Home,
  /// The per-voice pairings.
  pairings: Vec<Pairing>,
  /// Midpoint-flip re-files queued by `on_pedal` (the pedal thread) for the grid
  /// thread to apply against the ring and the sink (`take_refiles`).
  pending_refiles: Vec<Refile>,
  /// The grid volume frozen when mode turned on, handed back to the pedal for
  /// catch-up pickup when mode turns off.
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
      pairings: Vec::new(),
      pending_refiles: Vec::new(),
      frozen_volume: 1.0,
    }
  }

  pub fn mode(&self) -> bool {
    self.mode
  }

  pub fn home(&self) -> Home {
    self.home
  }

  pub fn frozen_volume(&self) -> f32 {
    self.frozen_volume
  }

  pub fn is_empty(&self) -> bool {
    self.pairings.is_empty()
  }

  /// The current target pitches (integer EDO steps) -- the FAR goalpost of each pitch
  /// pairing.
  pub fn targets(&self) -> Vec<i32> {
    let home_low = self.home == Home::Low;
    self
      .pairings
      .iter()
      .filter(|p| p.kind == Kind::Pitch)
      .map(|p| filed_side_pitch(&p.seg, !home_low).round() as i32)
      .collect()
  }

  pub fn pairings(&self) -> &[Pairing] {
    &self.pairings
  }

  /// The pitches (integer EDO steps) of the current FADE voices -- the swells the
  /// engine owns. The chord-recall wiring uses this to tell a chord pitch that
  /// coincides with an existing slide fade (which `match_targets` will REUSE) apart
  /// from one that coincides with a plain finger/drone (which it must skip), so it
  /// filters only the latter.
  pub fn fade_pitches(&self) -> Vec<i32> {
    self
      .pairings
      .iter()
      .filter(|p| matches!(p.kind, Kind::FadeIn | Kind::FadeOut))
      .map(|p| p.pitch.round() as i32)
      .collect()
  }

  /// Enter slide mode, freezing the grid volume at `frozen_volume`. Starts with no
  /// targets, so the pedal is inert until the first pick (no pitch is yanked).
  pub fn enter(&mut self, frozen_volume: f32) {
    self.mode = true;
    self.frozen_volume = frozen_volume;
    self.pairings.clear();
    // Home follows the pedal's current side, so the first pick's far endpoint is the
    // one away from it.
    self.home = if self.cur_f <= MIDPOINT { Home::Low } else { Home::High };
  }

  /// Leave slide mode mid-anything: voices freeze exactly where they are (targets and
  /// kinks dropped), and the frozen grid volume is handed back for the pedal's
  /// catch-up pickup. Returns that volume.
  pub fn exit(&mut self) -> f32 {
    self.mode = false;
    self.pairings.clear();
    self.frozen_volume
  }

  /// Reconcile the PITCH pairings against the grid's live edit selection (`edited`,
  /// integer EDO steps, home-side filed): a pairing whose voice left the selection --
  /// an editmode-clear deselecting it, or a sustain-clear ending it (which cascades the
  /// edit reason away) -- is cancelled, and its voice returned for freezing and its
  /// target unlit. Fade voices are not edit-mode voices, so they persist. Called by the
  /// grid thread each repaint, so it covers a clear from ANY surface (grid or pedal).
  pub fn reconcile(&mut self, edited: &std::collections::HashSet<i32>) -> Vec<VoiceSource> {
    if !self.mode {
      return Vec::new();
    }
    let home_low = self.home == Home::Low;
    let mut dropped = Vec::new();
    self.pairings.retain(|p| {
      if p.kind != Kind::Pitch {
        return true;
      }
      let filed = filed_side_pitch(&p.seg, home_low).round() as i32;
      if edited.contains(&filed) {
        true
      } else {
        dropped.push(p.voice);
        false
      }
    });
    dropped
  }

  /// Apply one pedal reading. Advances every pairing along the anchored map and updates
  /// the home hysteresis (queueing any midpoint-flip re-files for the grid thread to
  /// drain via `take_refiles`). Also tracks the fraction while mode is off (so the
  /// first pick after `enter` knows which side the pedal is on).
  pub fn on_pedal(&mut self, f: f32) {
    let f = f.clamp(0.0, 1.0);
    if !self.mode {
      self.cur_f = f;
      return;
    }
    let f_old = self.cur_f;
    if (f - f_old).abs() > EPS {
      let up = f > f_old;
      if up != self.moving_up && !self.pairings.is_empty() {
        // A reversal: re-pin every anchor at the reversal point, so the return path
        // is a fresh segment to home (the hysteresis).
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
    self.update_home(f);
  }

  /// Drain the midpoint-flip re-files (the grid thread applies them against the ring
  /// and the sink).
  pub fn take_refiles(&mut self) -> Vec<Refile> {
    std::mem::take(&mut self.pending_refiles)
  }

  /// Flip home if the pedal crossed the far edge of the hysteresis band, queueing the
  /// pitch pairings whose painted pitch therefore moved to the other side (and re-keying
  /// their drone voices in the pairing so the pedal keeps driving the right key).
  fn update_home(&mut self, f: f32) {
    let flip = match self.home {
      Home::Low => f > MIDPOINT + HYSTERESIS_BAND,
      Home::High => f < MIDPOINT - HYSTERESIS_BAND,
    };
    if !flip {
      return;
    }
    // Before the flip the note is filed at the OLD home end's pitch; after, at the new
    // home end's. The flip changes only which endpoint the note is "at" for painting
    // and re-filing -- not any sounding value.
    let (old_home_low, new_home_low) = match self.home {
      Home::Low => (true, false),
      Home::High => (false, true),
    };
    for p in &mut self.pairings {
      if p.kind != Kind::Pitch {
        continue;
      }
      let from = filed_side_pitch(&p.seg, old_home_low).round() as i32;
      let to = filed_side_pitch(&p.seg, new_home_low).round() as i32;
      if from == to {
        continue;
      }
      let voice_old = p.voice;
      let voice_new = match p.voice {
        VoiceSource::SurfaceDrone { grid, .. } => VoiceSource::SurfaceDrone { grid, pitch: to },
        other => other, // a fingered voice keeps its cell key
      };
      p.voice = voice_new;
      self.pending_refiles.push(Refile { voice_old, voice_new, from, to });
    }
    self.home = if new_home_low { Home::Low } else { Home::High };
  }

  /// A pick of a FREE pitch while slide mode is on (`keys.rs` play-path). `voices` is
  /// the grid's edit-mode voices as `(key, current filed pitch)`. Per the vision's
  /// home-proximity rule the pick becomes the new target of the NEAREST voice (ties to
  /// the lower voice), replacing that pairing's FAR goalpost with a kink at the current
  /// fraction -- never yanking the pitch it is currently nearest to. With NO edit-mode
  /// voices the pick swells a NEW voice in from silence (the one-note swell), reported
  /// in `spawn_fade_ins`.
  pub fn pick(&mut self, pitch: i32, voices: &[(VoiceSource, i32)]) -> PickOutcome {
    let nearest = voices
      .iter()
      .min_by_key(|(_, base)| ((*base - pitch).abs(), *base))
      .copied();
    match nearest {
      Some((voice, base)) => {
        let pairing = self.pitch_pairing(voice, base, pitch);
        self.upsert_pairing(pairing);
        PickOutcome::default()
      }
      // Nothing in edit mode: the pedal becomes a swell into this pitch.
      None => PickOutcome { spawn_fade_ins: vec![pitch] },
    }
  }

  /// A whole set of targets arriving at once -- the chord-storage case (phase B): a
  /// recalled chord's pitches all join the target set. Faithful to `1_vision`'s chord
  /// rule ("don't change the target of any voices that already have targets, target
  /// each as-yet untargeted voice that's in edit mode to the nearest available target,
  /// and just fade in the other voices"):
  ///
  /// - `chord_pitches` is deduped to a target SET (two chord voices at one pitch are
  ///   one slide destination).
  /// - EXISTING pitch pairings keep their targets untouched; their targets are "taken"
  ///   and their voices are not re-matched.
  /// - A target already equal to a voice's CURRENT filed pitch is *already satisfied*
  ///   (decision, `3_progress` phase B): no pairing, no fade -- the chord already has a
  ///   voice there. Such an untargeted edit voice is left alone (not faded out).
  /// - EXISTING fade voices are REUSED when the new chord re-requests their pitch
  ///   (flipped/kept as a swell IN, anchored at the current fraction so a mid-flight
  ///   re-selection does not jump), and CONVERTED to fade OUT when it does not (the
  ///   previous selection's un-consumed targets recede). This is what keeps a pitch
  ///   from ever carrying two fade voices, so the pitch-keyed `SurfaceDrone` key stays
  ///   collision-free without a dedicated fade key.
  /// - As-yet untargeted edit voices are matched ascending, each to its nearest
  ///   remaining AVAILABLE target, ties to the lower target; leftover voices (fewer
  ///   targets) fade OUT; leftover targets are returned in `spawn_fade_ins` for the
  ///   wiring to swell fresh voices IN.
  ///
  /// `edit_voices` is the grid's whole edit selection as `(key, current filed pitch)`;
  /// the caller has already dropped any chord pitch that coincides with a NON-edit
  /// sounding voice (a plain finger or drone), so this only ever spawns a fade into a
  /// pitch no live voice holds.
  pub fn match_targets(
    &mut self,
    chord_pitches: &[i32],
    edit_voices: &[(VoiceSource, i32)],
  ) -> PickOutcome {
    let home_low = self.home == Home::Low;
    // The target SET: deduped, ascending.
    let mut targets: Vec<i32> = chord_pitches.to_vec();
    targets.sort_unstable();
    targets.dedup();
    let target_set: HashSet<i32> = targets.iter().copied().collect();

    // Split the current pairings: pitch slides are kept verbatim (already-targeted
    // voices keep their targets); fades are reused or receded below.
    let existing = std::mem::take(&mut self.pairings);
    let mut kept_pitch: Vec<Pairing> = Vec::new();
    let mut fades: Vec<Pairing> = Vec::new();
    for p in existing {
      match p.kind {
        Kind::Pitch => kept_pitch.push(p),
        Kind::FadeIn | Kind::FadeOut => fades.push(p),
      }
    }

    // Targets a kept pairing already owns are unavailable; pitches a voice already
    // sits at are already satisfied.
    let taken: HashSet<i32> =
      kept_pitch.iter().map(|p| filed_side_pitch(&p.seg, !home_low).round() as i32).collect();
    let mut satisfied: HashSet<i32> =
      kept_pitch.iter().map(|p| filed_side_pitch(&p.seg, home_low).round() as i32).collect();
    for (_, base) in edit_voices {
      satisfied.insert(*base);
    }
    let mut available: Vec<i32> =
      targets.iter().copied().filter(|t| !taken.contains(t) && !satisfied.contains(t)).collect();

    // Reuse or recede the existing fades.
    let mut result_fades: Vec<Pairing> = Vec::new();
    for f in fades {
      let pitch = f.pitch.round() as i32;
      let cur_amp = f.current_amp().unwrap_or(0.0);
      if let Some(pos) = available.iter().position(|t| *t == pitch) {
        available.remove(pos);
        // Re-requested: keep swelling IN from wherever the fade currently sits.
        result_fades.push(self.fade_pairing(f.voice, f.pitch, Kind::FadeIn, cur_amp, 1.0));
      } else {
        // No longer wanted: fade OUT from wherever it currently sits.
        result_fades.push(self.fade_pairing(f.voice, f.pitch, Kind::FadeOut, cur_amp, 0.0));
      }
    }

    // The untargeted edit voices: those with no kept pitch pairing and not already
    // satisfied (already parked on a chord pitch -> left alone).
    let kept_voices: HashSet<VoiceSource> = kept_pitch.iter().map(|p| p.voice).collect();
    let mut matchable: Vec<(VoiceSource, i32)> = edit_voices
      .iter()
      .filter(|(v, base)| !kept_voices.contains(v) && !target_set.contains(base))
      .copied()
      .collect();
    matchable.sort_by_key(|(_, p)| *p);

    let mut new_pitch: Vec<Pairing> = Vec::new();
    for (voice, base) in &matchable {
      if available.is_empty() {
        // Fewer targets than voices: fade the leftover edit voice OUT from full.
        result_fades.push(self.fade_pairing(*voice, *base as f32, Kind::FadeOut, 1.0, 0.0));
        continue;
      }
      let (idx, _) = available
        .iter()
        .enumerate()
        .min_by_key(|(_, t)| ((*t - base).abs(), **t))
        .unwrap();
      let target = available.remove(idx);
      new_pitch.push(self.pitch_pairing(*voice, *base, target));
    }

    self.pairings = kept_pitch;
    self.pairings.extend(new_pitch);
    self.pairings.extend(result_fades);
    // Whatever targets remain need a fresh fade-in voice swelled in from silence.
    PickOutcome { spawn_fade_ins: available }
  }

  /// Replace (or insert) a pairing keyed by its voice.
  fn upsert_pairing(&mut self, pairing: Pairing) {
    if let Some(slot) = self.pairings.iter_mut().find(|p| p.voice == pairing.voice) {
      *slot = pairing;
    } else {
      self.pairings.push(pairing);
    }
  }

  /// Build (or retarget) a pitch pairing for `voice`, currently at `base`, aiming at
  /// `target`. A pre-existing pairing keeps its map and only retargets its FAR
  /// endpoint (kink at the current fraction) -- so a pick never yanks the pitch the
  /// voice is currently nearest to.
  fn pitch_pairing(&self, voice: VoiceSource, base: i32, target: i32) -> Pairing {
    let far_is_high = self.home == Home::Low;
    if let Some(prev) = self.pairings.iter().find(|p| p.voice == voice && p.kind == Kind::Pitch) {
      let mut seg = prev.seg;
      let cur_target = if far_is_high { seg.s1 } else { seg.s0 };
      if (cur_target - target as f32).abs() > EPS {
        // Retarget the far side: pin the kink at the current fraction, swap the
        // goalpost.
        seg.repin(self.cur_f);
        if far_is_high {
          seg.s1 = target as f32;
        } else {
          seg.s0 = target as f32;
        }
      }
      return Pairing { voice, kind: Kind::Pitch, seg, pitch: seg.cur };
    }
    // A fresh pairing: home endpoint = the voice's current pitch, far endpoint = the
    // target, anchored where the pedal is (below the anchor it reads home; above it
    // slides to the target).
    let (s0, s1) = if far_is_high { (base as f32, target as f32) } else { (target as f32, base as f32) };
    let seg = Segments::new(s0, s1, self.cur_f, base as f32, Shape::Linear);
    Pairing { voice, kind: Kind::Pitch, seg, pitch: base as f32 }
  }

  /// Build a fade pairing (in or out) at fixed `pitch`, amplitude walking from
  /// `start_amp` toward `end_amp` as the pedal sweeps toward the far side.
  fn fade_pairing(
    &self,
    voice: VoiceSource,
    pitch: f32,
    kind: Kind,
    start_amp: f32,
    end_amp: f32,
  ) -> Pairing {
    // The home side holds `start_amp`, the far side `end_amp`, quartic-shaped and
    // anchored at the current fraction (so a mid-flight fade pins a kink like a pitch
    // retarget does).
    let far_is_high = self.home == Home::Low;
    let (s0, s1) = if far_is_high { (start_amp, end_amp) } else { (end_amp, start_amp) };
    let seg = Segments::new(s0, s1, self.cur_f, start_amp, Shape::Quartic);
    Pairing { voice, kind, seg, pitch }
  }

  /// Register a fade-in voice the wiring just spawned for extra target `pitch`. The
  /// swell runs from silence (home) to full (far).
  pub fn register_fade_in(&mut self, voice: VoiceSource, pitch: i32) {
    let pairing = self.fade_pairing(voice, pitch as f32, Kind::FadeIn, 0.0, 1.0);
    self.pairings.push(pairing);
  }

  /// The per-voice drive commands to apply this instant.
  pub fn drives(&self) -> Vec<Drive> {
    self
      .pairings
      .iter()
      .map(|p| Drive { voice: p.voice, pitch: p.current_pitch(), amp: p.current_amp() })
      .collect()
  }

  /// The LED roles: the home-side pitches (lit + danced) and the target-side pitches
  /// (100 ms flash, no dance), as integer EDO steps. Only pitch pairings light a
  /// target; fades have no goalpost cell.
  pub fn led_roles(&self) -> (HashSet<i32>, HashSet<i32>) {
    let mut home = HashSet::new();
    let mut target = HashSet::new();
    let home_low = self.home == Home::Low;
    for p in &self.pairings {
      if p.kind != Kind::Pitch {
        continue;
      }
      home.insert(filed_side_pitch(&p.seg, home_low).round() as i32);
      target.insert(filed_side_pitch(&p.seg, !home_low).round() as i32);
    }
    (home, target)
  }
}

/// The pitch at one end of a segment: the low end (`s0`) or the high end (`s1`).
fn filed_side_pitch(seg: &Segments, low: bool) -> f32 {
  if low {
    seg.s0
  } else {
    seg.s1
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn finger(cell: (i32, i32)) -> VoiceSource {
    VoiceSource::SurfaceFinger { grid: 0, cell }
  }

  // ---- the anchored-segments map (the heart) ----

  /// A plain pitch slide from home to target: endpoints exact, monotone between.
  #[test]
  fn a_pitch_segment_lands_home_at_0_and_target_at_1() {
    // Home low (s0 = 10), target high (s1 = 20), anchored at the low end.
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
    // Sweep to f = 0.6: pitch is 10 + 10*0.6 = 16.
    s.cur = s.eval(0.6, true);
    assert!((s.cur - 16.0).abs() < 1e-4);
    // Retarget the far (high) endpoint to 30, kink pinned at f = 0.6.
    s.repin(0.6);
    s.s1 = 30.0;
    // The value at f = 0.6 is unchanged -- no jump.
    assert!((s.eval(0.6, true) - 16.0).abs() < 1e-4, "no discontinuity at the retarget");
    // Onward to f = 1 lands exactly on the NEW target.
    assert!((s.eval(1.0, true) - 30.0).abs() < 1e-4, "the new goalpost is reached exactly");
    // Halfway from the kink to the end: 16 + (30-16)*0.5 = 23.
    let mid = s.eval(0.8, true); // t = (0.8-0.6)/(1-0.6) = 0.5
    assert!((mid - 23.0).abs() < 1e-4);
  }

  /// Hysteresis: sweep up along a kinked map, reverse, and the path down is a fresh
  /// straight segment to home -- not a retrace of the way up.
  #[test]
  fn reversing_mid_flight_takes_a_fresh_segment_home() {
    let mut s = Segments::new(0.0, 12.0, 0.0, 0.0, Shape::Linear);
    // Up to f = 0.5: pitch 6. Retarget high to 100 (a big kink).
    s.cur = s.eval(0.5, true);
    s.repin(0.5);
    s.s1 = 100.0;
    // Up a touch to f = 0.75: 6 + (100-6)*0.5 = 53.
    s.cur = s.eval(0.75, true);
    assert!((s.cur - 53.0).abs() < 1e-3);
    // Reverse here: re-pin at (0.75, 53), now descending to s0 = 0.
    s.repin(0.75);
    // Back to f = 0.5: NOT 6 (the way up) -- a fresh line from (0.75,53) to (0,0).
    let down = s.eval(0.5, false); // t = (0.75-0.5)/0.75 = 1/3, 53*(1 - 1/3) = 35.33
    assert!((down - 53.0 * (2.0 / 3.0)).abs() < 1e-3, "fresh straight segment home, got {down}");
    assert!((down - 6.0).abs() > 1.0, "definitely not a retrace of the way up");
    // ...and it still lands home exactly at 0.
    assert!((s.eval(0.0, false) - 0.0).abs() < 1e-4);
  }

  /// Kink gone at the endpoint: reach 1, and the whole return trip is one straight
  /// line from the target back to home.
  #[test]
  fn the_kink_dies_at_the_endpoint() {
    let mut s = Segments::new(0.0, 10.0, 0.0, 0.0, Shape::Linear);
    s.cur = s.eval(0.4, true);
    s.repin(0.4);
    s.s1 = 40.0; // kink at 0.4
    // Reach the top.
    s.cur = s.eval(1.0, true);
    assert!((s.cur - 40.0).abs() < 1e-4);
    s.repin(1.0);
    // Descending from the top: a single line (0,0)-(1,40), no kink anywhere.
    for &(f, want) in &[(0.75_f32, 30.0_f32), (0.5, 20.0), (0.25, 10.0), (0.0, 0.0)] {
      let got = s.eval(f, false);
      assert!((got - want).abs() < 1e-3, "at f={f} want {want} got {got} -- one clean line");
    }
  }

  /// The quartic fade: silent at one end, full at the other, eased (quartic) between,
  /// and continuous under a mid-flight kink exactly like the pitch map.
  #[test]
  fn a_quartic_fade_is_exact_at_the_ends_and_eased_between() {
    let s = Segments::new(0.0, 1.0, 0.0, 0.0, Shape::Quartic);
    assert_eq!(s.eval(0.0, true), 0.0, "silent at home");
    assert_eq!(s.eval(1.0, true), 1.0, "full at the far end");
    // Quartic: halfway in pedal is only 0.5^4 = 0.0625 in amplitude -- eased out of
    // silence.
    assert!((s.eval(0.5, true) - 0.0625).abs() < 1e-4);
  }

  #[test]
  fn a_quartic_fade_pins_a_kink_without_a_jump() {
    let mut s = Segments::new(0.0, 1.0, 0.0, 0.0, Shape::Quartic);
    s.cur = s.eval(0.6, true); // 0.6^4 = 0.1296
    assert!((s.cur - 0.1296).abs() < 1e-4);
    // Re-pin (a new fade target mid-flight); the value at 0.6 must not jump.
    s.repin(0.6);
    assert!((s.eval(0.6, true) - 0.1296).abs() < 1e-4, "continuous across the kink");
    assert!((s.eval(1.0, true) - 1.0).abs() < 1e-4, "still exact at the top");
  }

  // ---- home hysteresis ----

  #[test]
  fn home_flips_only_past_the_hysteresis_band() {
    let mut st = PedalSlideState::new();
    st.enter(0.5);
    // Pick a target so there's a pitch pairing to watch (fingered voice at pitch 10).
    st.pick(20, &[(finger((0, 0)), 10)]);
    assert_eq!(st.home(), Home::Low, "started at f=0, home is low");
    st.on_pedal(0.5); // right at the midpoint: no flip
    assert_eq!(st.home(), Home::Low, "at exactly 0.5 the band is not yet crossed");
    st.on_pedal(0.54); // inside the band
    assert_eq!(st.home(), Home::Low, "still inside the +/-0.05 band");
    st.on_pedal(0.56); // past 0.55
    assert_eq!(st.home(), Home::High, "crossed the far edge -> home flips high");
    // Coming back, it must drop below 0.45 to flip back.
    st.on_pedal(0.5);
    assert_eq!(st.home(), Home::High, "hysteresis holds it high near the middle");
    st.on_pedal(0.44);
    assert_eq!(st.home(), Home::Low, "past the near edge -> back to low");
  }

  #[test]
  fn the_flip_reports_a_refile_but_moves_no_value() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    st.on_pedal(0.0);
    st.pick(30, &[(finger((0, 0)), 10)]); // home low = 10, target high = 30
    // Sweep up past the flip.
    st.on_pedal(0.4);
    st.on_pedal(0.56);
    let refiles = st.take_refiles();
    assert_eq!(refiles.len(), 1, "the one pitch pairing re-files");
    assert_eq!(refiles[0].from, 10, "was painted at the low (home) pitch");
    assert_eq!(refiles[0].to, 30, "now painted at the high (new home = target) pitch");
    assert_eq!(
      refiles[0].voice_old, refiles[0].voice_new,
      "a finger-still-down voice keeps its CELL key across the flip -- so the held map, \
       not a re-key, is what must follow it (the phase-B fix)",
    );
    assert!(st.take_refiles().is_empty(), "draining consumes them");
  }

  // ---- target matching ----

  #[test]
  fn matching_is_ascending_nearest_ties_to_the_lower_target() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    // Voices A=10, B=20; targets X=12, Y=25. Ascending: A takes 12, B takes 25.
    let voices = [(finger((0, 0)), 10), (finger((1, 1)), 20)];
    st.match_targets(&[12, 25], &voices);
    let pa = st.pairings().iter().find(|p| p.voice == finger((0, 0))).unwrap();
    let pb = st.pairings().iter().find(|p| p.voice == finger((1, 1))).unwrap();
    // far is high (home low): s1 is the target.
    assert_eq!(pa.seg.s1, 12.0);
    assert_eq!(pb.seg.s1, 25.0);
  }

  #[test]
  fn a_single_pick_retargets_the_nearest_voice_and_re_picks_re_retarget_it() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    st.on_pedal(0.0);
    // One voice at 10: picking 40 makes it the target; picking 12 RE-targets the same
    // voice (the vision's slide-to-one-then-another workflow), not a second fade-in.
    let out = st.pick(40, &[(finger((0, 0)), 10)]);
    assert!(out.spawn_fade_ins.is_empty(), "one voice, one pick: a plain pitch pairing");
    assert_eq!(st.targets(), vec![40]);
    let out = st.pick(12, &[(finger((0, 0)), 10)]);
    assert!(out.spawn_fade_ins.is_empty(), "the same voice is re-targeted, nothing swells in");
    assert_eq!(st.targets(), vec![12]);
  }

  #[test]
  fn a_pick_with_no_edit_voices_is_a_one_note_swell() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    let out = st.pick(25, &[]);
    assert_eq!(out.spawn_fade_ins, vec![25], "no edit voices: the pick swells a new voice in");
  }

  #[test]
  fn a_target_set_larger_than_the_voice_set_swells_the_extras_in() {
    // The batch matcher (chord case, phase B): one voice, two targets -> the voice
    // takes the nearer, the other swells in.
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    let out = st.match_targets(&[12, 40], &[(finger((0, 0)), 10)]);
    assert_eq!(out.spawn_fade_ins, vec![40], "the voice takes 12 (nearer); 40 swells in");
  }

  // ---- chord recall (phase B) ----

  /// The vision's chord rule: a recalled chord's pitches join the target set; a voice
  /// that already has a target keeps it, an untargeted edit voice matches the nearest
  /// available target, and an extra target swells in.
  #[test]
  fn chord_recall_keeps_targeted_voices_matches_untargeted_and_swells_extras() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    st.on_pedal(0.0);
    // Voice A=10 is already sliding to 30 (a prior single pick).
    st.pick(30, &[(finger((0, 0)), 10)]);
    // Recall chord {12, 30, 50} with edit voices A=10 (targeted) and B=20 (untargeted).
    let voices = [(finger((0, 0)), 10), (finger((1, 1)), 20)];
    let out = st.match_targets(&[12, 30, 50], &voices);
    let pa = st.pairings().iter().find(|p| p.voice == finger((0, 0))).unwrap();
    assert_eq!(pa.seg.s1, 30.0, "the already-targeted voice keeps its target (30 is taken)");
    let pb = st.pairings().iter().find(|p| p.voice == finger((1, 1))).unwrap();
    assert_eq!(pb.kind, Kind::Pitch);
    assert_eq!(pb.seg.s1, 12.0, "the untargeted voice matches the nearest AVAILABLE target");
    assert_eq!(out.spawn_fade_ins, vec![50], "the leftover target swells a new voice in");
  }

  /// Duplicate chord pitches are one destination, and a chord pitch a voice already
  /// sits on is already satisfied -- no pairing, no fade, the voice left alone.
  #[test]
  fn a_chord_pitch_a_voice_already_sits_on_is_already_satisfied() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    // Edit voice A=10; recall chord {10, 10, 20} (a duplicate + a pitch A is already at).
    let out = st.match_targets(&[10, 10, 20], &[(finger((0, 0)), 10)]);
    assert!(
      st.pairings().iter().all(|p| p.voice != finger((0, 0))),
      "the voice already parked on a chord pitch is left alone -- no pairing, no fade"
    );
    assert_eq!(out.spawn_fade_ins, vec![20], "the deduped other pitch swells in once");
  }

  /// At most one chord selected: selecting a second replaces the first's un-consumed
  /// targets -- an overlapping pitch's fade is REUSED (stays swelling in, one voice per
  /// pitch, so the drone key never collides), a dropped pitch recedes, a new one swells.
  #[test]
  fn selecting_a_new_chord_replaces_the_previous_chords_un_consumed_fades() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    st.on_pedal(0.0);
    // Chord A {40, 60}, no edit voices: both swell in; the wiring registers them.
    let out = st.match_targets(&[40, 60], &[]);
    assert_eq!(out.spawn_fade_ins, vec![40, 60]);
    st.register_fade_in(finger((5, 5)), 40);
    st.register_fade_in(finger((6, 6)), 60);
    // Chord B {40, 70}: 40 overlaps -> reused, 60 dropped -> recedes, 70 -> fresh swell.
    let out2 = st.match_targets(&[40, 70], &[]);
    let f40 = st.pairings().iter().find(|p| p.pitch.round() as i32 == 40).unwrap();
    assert_eq!(f40.kind, Kind::FadeIn, "the re-requested pitch's fade is reused, not doubled");
    let f60 = st.pairings().iter().find(|p| p.pitch.round() as i32 == 60).unwrap();
    assert_eq!(f60.kind, Kind::FadeOut, "the dropped chord's un-consumed target recedes");
    assert_eq!(out2.spawn_fade_ins, vec![70], "only the genuinely new pitch swells in");
    // Exactly one voice per pitch -- no drone-key collision.
    assert_eq!(st.fade_pitches().iter().filter(|p| **p == 40).count(), 1);
  }

  /// A chord joined mid-flight kinks at the current fraction rather than jumping: a
  /// kept pitch pairing does not move, and a reused fade keeps its current amplitude.
  #[test]
  fn a_mid_flight_chord_join_kinks_without_jumping() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    st.on_pedal(0.0);
    // A pitch voice sliding, and a fade swelling, both partway up.
    st.pick(30, &[(finger((0, 0)), 10)]);
    st.match_targets(&[30, 50], &[(finger((0, 0)), 10)]); // A keeps 30, 50 registered next
    st.register_fade_in(finger((5, 5)), 50);
    st.on_pedal(0.5);
    let a_cur = st.pairings().iter().find(|p| p.voice == finger((0, 0))).unwrap().seg.cur;
    let f_amp = st
      .pairings()
      .iter()
      .find(|p| p.voice == finger((5, 5)))
      .unwrap()
      .current_amp()
      .unwrap();
    assert!((a_cur - 20.0).abs() < 1e-3, "A slid to 20 at f=0.5");
    // Re-select an overlapping chord {30, 50, 80} mid-flight.
    st.match_targets(&[30, 50, 80], &[(finger((0, 0)), 10)]);
    let a_cur2 = st.pairings().iter().find(|p| p.voice == finger((0, 0))).unwrap().seg.cur;
    let f_amp2 = st
      .pairings()
      .iter()
      .find(|p| p.voice == finger((5, 5)))
      .unwrap()
      .current_amp()
      .unwrap();
    assert!((a_cur2 - a_cur).abs() < 1e-6, "the kept pitch slide does not jump");
    assert!((f_amp2 - f_amp).abs() < 1e-6, "the reused fade does not jump");
  }

  #[test]
  fn fewer_targets_than_voices_fades_the_leftovers_out() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    let voices = [(finger((0, 0)), 10), (finger((1, 1)), 20)];
    st.match_targets(&[11], &voices);
    let leftover = st.pairings().iter().find(|p| p.voice == finger((1, 1))).unwrap();
    assert_eq!(leftover.kind, Kind::FadeOut, "the unmatched (higher) voice fades out");
    let matched = st.pairings().iter().find(|p| p.voice == finger((0, 0))).unwrap();
    assert_eq!(matched.kind, Kind::Pitch, "the matched voice slides");
  }

  // ---- the far-side-only retarget rule ----

  #[test]
  fn a_retarget_only_moves_the_far_goalpost() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    st.on_pedal(0.0);
    st.pick(30, &[(finger((0, 0)), 10)]); // home low 10, far high 30
    st.on_pedal(0.5); // slide up but stay inside the hysteresis band; pitch = 10+20*0.5 = 20
    assert_eq!(st.home(), Home::Low, "still home-low at the midpoint");
    let before = st.pairings()[0].seg.cur;
    assert!((before - 20.0).abs() < 1e-3);
    // Pick a new target (still home low): replaces the FAR (high) endpoint only.
    st.pick(40, &[(finger((0, 0)), 20)]);
    let p = &st.pairings()[0];
    assert_eq!(p.seg.s0, 10.0, "the home (low) goalpost is untouched");
    assert_eq!(p.seg.s1, 40.0, "only the far (high) goalpost moved");
    assert!((p.seg.cur - 20.0).abs() < 1e-3, "the current pitch did not jump");
  }

  // ---- state-level drive + a full flight ----

  #[test]
  fn a_full_flight_drives_the_voice_from_home_to_target() {
    let mut st = PedalSlideState::new();
    st.enter(1.0);
    st.on_pedal(0.0);
    st.pick(22, &[(finger((0, 0)), 10)]);
    st.on_pedal(0.0);
    let d0 = st.drives();
    assert_eq!(d0.len(), 1);
    assert!((d0[0].pitch - 10.0).abs() < 1e-4, "at the bottom the voice is home");
    assert_eq!(d0[0].amp, None, "a pitch slide does not drive amplitude");
    st.on_pedal(1.0);
    let d1 = st.drives();
    assert!((d1[0].pitch - 22.0).abs() < 1e-4, "at the top it has reached the target");
  }

  #[test]
  fn a_fade_in_voice_swells_its_amplitude_with_the_pedal() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    st.on_pedal(0.0);
    st.pick(25, &[]); // no edit voices: one-note swell
    st.register_fade_in(finger((9, 9)), 25);
    st.on_pedal(0.0);
    let d0 = st.drives();
    let fade = d0.iter().find(|d| d.voice == finger((9, 9))).unwrap();
    assert_eq!(fade.amp, Some(0.0), "silent at the bottom");
    assert!((fade.pitch - 25.0).abs() < 1e-4, "at its fixed pitch");
    st.on_pedal(1.0);
    let d1 = st.drives();
    let fade = d1.iter().find(|d| d.voice == finger((9, 9))).unwrap();
    assert_eq!(fade.amp, Some(1.0), "full at the top");
  }

  #[test]
  fn exit_freezes_and_hands_back_the_frozen_volume() {
    let mut st = PedalSlideState::new();
    st.enter(0.42);
    st.pick(20, &[(finger((0, 0)), 10)]);
    assert!(!st.is_empty());
    let vol = st.exit();
    assert!((vol - 0.42).abs() < 1e-6, "the frozen volume comes back for catch-up");
    assert!(!st.mode(), "mode is off");
    assert!(st.is_empty(), "targets and pairings are dropped");
  }

  // ---- the anchored-kink volume return (2_discussion amendment) ----

  #[test]
  fn the_volume_return_is_continuous_at_mode_exit_and_immediate_both_ways() {
    // Slide mode turned off with the pedal at 0.5 and the volume frozen at 0.5.
    let mut vr = VolumeReturn::new(0.5, 0.5);
    // A first reading at the same spot holds the frozen volume -- no jump.
    assert_eq!(vr.on_pedal(0.5), Some(0.5), "continuous at the exit point");
    // Pushing UP raises the volume immediately (toward 1), quartic-eased.
    let up = vr.on_pedal(0.75).unwrap();
    assert!(up > 0.5, "raising the pedal raises the volume at once, got {up}");
    // Back down THROUGH the frozen point keeps lowering it -- no dead zone (the bug
    // the amendment fixes): reverse re-pins, so the way down is a fresh segment.
    let mid = vr.on_pedal(0.5).unwrap();
    assert!(mid < up, "coming back down lowers the volume immediately, got {mid}");
    let low = vr.on_pedal(0.25).unwrap();
    assert!(low < mid, "and keeps lowering past the old freeze point, got {low}");
  }

  #[test]
  fn the_volume_return_reverts_to_the_ordinary_taper_at_an_endpoint() {
    let mut vr = VolumeReturn::new(0.4, 0.6);
    assert!(vr.on_pedal(0.3).is_some(), "still on the kinked map mid-travel");
    assert_eq!(vr.on_pedal(0.0), None, "reaching the heel reverts to the taper");
    assert!(vr.reverted());
    assert_eq!(vr.on_pedal(0.5), None, "and it stays reverted");

    // The toe end reverts too.
    let mut vr = VolumeReturn::new(0.4, 0.6);
    assert_eq!(vr.on_pedal(1.0), None, "reaching the toe reverts");
    assert!(vr.reverted());
  }

  #[test]
  fn led_roles_split_home_and_target() {
    let mut st = PedalSlideState::new();
    st.enter(0.0);
    st.on_pedal(0.0);
    st.pick(30, &[(finger((0, 0)), 10)]);
    let (home, target) = st.led_roles();
    assert!(home.contains(&10), "home lights the current-side pitch");
    assert!(target.contains(&30), "the target flashes at the far side");
  }
}
