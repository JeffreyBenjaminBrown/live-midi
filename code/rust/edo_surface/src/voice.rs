//! What the instrument is sounding, and how each sounding thing is addressed.
//!
//! This module exists because the type it replaces lied. The old one was
//! `VoiceSource::Accreted { chord: ChordId, pitch: i32 }`, and in this instrument
//! neither field meant what it said: `chord` held a tag saying which grid and which
//! kind of voice, and `pitch` held a packed monomekey whenever the voice was fingered.
//! A sibling variant named `Fingered` existed and went unused, because fingered notes
//! were `Accreted` too.
//!
//! That cost real debugging time rather than merely reading badly. Code that selected
//! voices by pitch silently matched no fingered note at all, because their `pitch`
//! field was a monomekey; the failure was quiet, and only a test caught it. The
//! documentation had grown a paragraph apologising for each false name.
//!
//! So the names here are load-bearing. `Fingered` means fingered. Nothing holds a
//! monomekey in a field called pitch. Nothing is called a chord, because this
//! instrument has no chords: chord storage is a real feature, and it belongs to the
//! sawwave runtime.

use std::collections::HashMap;

/// Which grid a voice belongs to. The instrument has two.
pub type GridIndex = usize;

/// One button on a monome grid, addressed as `(x, y)`.
///
/// This is the only sense of "key" the library uses, and it is spelled out to keep it
/// that way: the old code called a button a key, called a map lookup a key, and called
/// a drone's pitch its key.
pub type MonomeKey = (i32, i32);

/// An absolute step in the tuning, octave included. A pitch class is this modulo the
/// EDO, and the two are different things: most LED rules work in pitch classes,
/// because a sounding note lights all its octave-equivalents.
pub type Pitch = i32;

/// Distinguishes two fading voices that would otherwise collide.
pub type FadingId = u64;

/// How one sounding thing is addressed.
///
/// The variant says what kind of voice it is, so no arithmetic decodes it and no
/// numeric range means anything. The old type needed `SUSTAIN_BASE` and
/// `RETIRED_BASE` offsets to recover from one field what this says outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Voice {
  /// A note under a finger, addressed by the monomekey it was struck on.
  ///
  /// Deliberately not addressed by pitch: an edit can move a fingered note's pitch
  /// away from the pitch its own monomekey means. So the monomekey is the stable
  /// identity, and what it currently sounds is a separate question that only
  /// [`Sounding`] can answer.
  Fingered { grid: GridIndex, monomekey: MonomeKey },
  /// A note ringing with no finger on it, addressed by its pitch.
  ///
  /// Addressed by pitch because pitch is what sustain and edit both name; a drone has
  /// no monomekey holding it down.
  Drone { grid: GridIndex, pitch: Pitch },
  /// A cut note fading to silence over its release. Nothing ever looks one up; the id
  /// only stops two of them colliding while they ring out.
  Fading { grid: GridIndex, id: FadingId },
}

impl Voice {
  /// Which grid this voice belongs to. Every kind has one, which is why gain, pulse
  /// and clear can all act per grid without caring what kind of voice they touch.
  pub fn grid(&self) -> GridIndex {
    match self {
      Voice::Fingered { grid, .. } | Voice::Drone { grid, .. } | Voice::Fading { grid, .. } => *grid,
    }
  }

  /// The pitch this voice is addressed by, if it is addressed by pitch at all.
  ///
  /// `None` for a fingered voice, and that is the point rather than an omission: a
  /// fingered voice's pitch is not in its address, so asking for it here cannot
  /// silently return the wrong answer. Ask [`Sounding`] instead.
  pub fn addressed_pitch(&self) -> Option<Pitch> {
    match self {
      Voice::Drone { pitch, .. } => Some(*pitch),
      Voice::Fingered { .. } | Voice::Fading { .. } => None,
    }
  }

  /// Is this voice still addressable, or is it merely ringing out? A fading voice is
  /// gone as far as the instrument is concerned.
  pub fn is_live(&self) -> bool {
    !matches!(self, Voice::Fading { .. })
  }
}

/// What each finger on one grid is currently SOUNDING.
///
/// Not what its monomekey nominally means under the current register. The two agree
/// until an edit moves a voice, and after that this map is the only record of the
/// truth. It is the bridge between the two ways a voice can be addressed: to reach the
/// fingered voice sounding some pitch, you look the pitch up here to get its
/// monomekey.
///
/// The old code kept this as a bare `HashMap` and let a drag re-pitch a voice without
/// updating it, so releasing looked up the pitch the note used to be, found it neither
/// sustained nor edited, and cut a note it should have kept. Wrapping it makes the
/// re-file a method rather than a line somebody must remember to write.
#[derive(Debug, Default)]
pub struct Sounding {
  by_monomekey: HashMap<MonomeKey, Pitch>,
}

impl Sounding {
  pub fn new() -> Self {
    Sounding { by_monomekey: HashMap::new() }
  }

  /// Record that the finger on `monomekey` is now sounding `pitch`.
  pub fn struck(&mut self, monomekey: MonomeKey, pitch: Pitch) {
    self.by_monomekey.insert(monomekey, pitch);
  }

  /// Record that the finger on `monomekey` has lifted, and report what it was
  /// sounding.
  pub fn lifted(&mut self, monomekey: MonomeKey) -> Option<Pitch> {
    self.by_monomekey.remove(&monomekey)
  }

  /// What the finger on `monomekey` is sounding, if one is down.
  pub fn under(&self, monomekey: MonomeKey) -> Option<Pitch> {
    self.by_monomekey.get(&monomekey).copied()
  }

  /// Re-file a finger's voice after an edit moved its pitch.
  ///
  /// Every caller that re-pitches a live fingered voice must call this, or the map
  /// starts lying and every lookup through it silently asks about the wrong pitch.
  /// Returns whether a finger was actually there to re-file.
  pub fn moved(&mut self, monomekey: MonomeKey, to: Pitch) -> bool {
    match self.by_monomekey.get_mut(&monomekey) {
      Some(slot) => {
        *slot = to;
        true
      }
      None => false,
    }
  }

  /// Is any finger sounding `pitch` right now?
  pub fn any_sounding(&self, pitch: Pitch) -> bool {
    self.by_monomekey.values().any(|p| *p == pitch)
  }

  /// The monomekey of a finger sounding `pitch`, if one is.
  ///
  /// This is the lookup that makes a fingered voice reachable by pitch at all, and it
  /// is the only one: nothing else may guess a monomekey from a pitch.
  pub fn monomekey_sounding(&self, pitch: Pitch) -> Option<MonomeKey> {
    self.by_monomekey.iter().find(|(_, p)| **p == pitch).map(|(k, _)| *k)
  }

  /// Every pitch under a finger.
  pub fn pitches(&self) -> impl Iterator<Item = Pitch> + '_ {
    self.by_monomekey.values().copied()
  }

  /// Every finger, as monomekey and the pitch it sounds.
  pub fn iter(&self) -> impl Iterator<Item = (MonomeKey, Pitch)> + '_ {
    self.by_monomekey.iter().map(|(k, p)| (*k, *p))
  }

  pub fn is_empty(&self) -> bool {
    self.by_monomekey.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_kind_of_voice_knows_its_grid() {
    assert_eq!(Voice::Fingered { grid: 1, monomekey: (3, 4) }.grid(), 1);
    assert_eq!(Voice::Drone { grid: 0, pitch: 20 }.grid(), 0);
    assert_eq!(Voice::Fading { grid: 1, id: 7 }.grid(), 1);
  }

  /// The whole point of the type. A fingered voice has no pitch in its address, so
  /// code cannot read one out and be quietly wrong. The old struct answered this
  /// question with a packed monomekey and no warning.
  #[test]
  fn only_a_drone_is_addressed_by_pitch() {
    assert_eq!(Voice::Drone { grid: 0, pitch: 20 }.addressed_pitch(), Some(20));
    assert_eq!(Voice::Fingered { grid: 0, monomekey: (3, 4) }.addressed_pitch(), None);
    assert_eq!(Voice::Fading { grid: 0, id: 1 }.addressed_pitch(), None);
  }

  /// Two grids sounding the same pitch are two voices, and two fingers on one grid
  /// are two voices. Neither collides, because the address carries both facts.
  #[test]
  fn voices_do_not_collide_across_grids_or_monomekeys() {
    let a = Voice::Fingered { grid: 0, monomekey: (3, 4) };
    let b = Voice::Fingered { grid: 1, monomekey: (3, 4) };
    let c = Voice::Fingered { grid: 0, monomekey: (4, 3) };
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(Voice::Drone { grid: 0, pitch: 20 }, Voice::Drone { grid: 1, pitch: 20 });
  }

  /// A drone and a finger can sound one pitch at once, so their addresses must differ.
  /// The instrument relies on this: cutting the drone must leave the finger alone.
  #[test]
  fn a_drone_and_a_finger_have_different_addresses() {
    assert_ne!(
      Voice::Drone { grid: 0, pitch: 20 },
      Voice::Fingered { grid: 0, monomekey: (3, 4) },
    );
  }

  /// Two cut notes ring out at once when you retrigger twice quickly, so their ids
  /// keep them apart.
  #[test]
  fn fading_voices_are_distinguished_by_id() {
    assert_ne!(Voice::Fading { grid: 0, id: 1 }, Voice::Fading { grid: 0, id: 2 });
  }

  #[test]
  fn a_fading_voice_is_not_live() {
    assert!(Voice::Fingered { grid: 0, monomekey: (1, 1) }.is_live());
    assert!(Voice::Drone { grid: 0, pitch: 20 }.is_live());
    assert!(!Voice::Fading { grid: 0, id: 1 }.is_live());
  }

  // ---- Sounding: the bridge ----

  #[test]
  fn a_struck_finger_reports_what_it_sounds() {
    let mut s = Sounding::new();
    s.struck((3, 3), 20);
    assert_eq!(s.under((3, 3)), Some(20));
    assert!(s.any_sounding(20));
    assert_eq!(s.monomekey_sounding(20), Some((3, 3)));
  }

  /// The bug this type is shaped to prevent: an edit moves a fingered voice's pitch,
  /// and every later lookup must see the new pitch, not the one the note used to be.
  #[test]
  fn moving_a_finger_re_files_it_under_the_new_pitch() {
    let mut s = Sounding::new();
    s.struck((3, 3), 20);
    assert!(s.moved((3, 3), 35));

    assert_eq!(s.under((3, 3)), Some(35), "the finger now sounds the new pitch");
    assert!(s.any_sounding(35));
    assert!(!s.any_sounding(20), "and nothing still thinks it sounds the old one");
    assert_eq!(s.monomekey_sounding(35), Some((3, 3)));
    assert_eq!(s.monomekey_sounding(20), None);
  }

  #[test]
  fn moving_a_finger_that_is_not_down_reports_so_and_adds_nothing() {
    let mut s = Sounding::new();
    assert!(!s.moved((9, 9), 35));
    assert!(s.is_empty(), "a move must not invent a finger");
  }

  #[test]
  fn a_lifted_finger_reports_what_it_was_sounding_and_then_is_gone() {
    let mut s = Sounding::new();
    s.struck((3, 3), 20);
    assert_eq!(s.lifted((3, 3)), Some(20));
    assert_eq!(s.under((3, 3)), None);
    assert!(!s.any_sounding(20));
    assert_eq!(s.lifted((3, 3)), None, "lifting twice reports nothing the second time");
  }

  /// Two monomekeys can collide to one pitch, and each is its own finger and its own
  /// voice. Lifting one must not make the instrument think the pitch has stopped.
  #[test]
  fn two_fingers_can_sound_one_pitch() {
    let mut s = Sounding::new();
    s.struck((3, 3), 20);
    s.struck((4, 12), 20);
    assert!(s.any_sounding(20));
    s.lifted((3, 3));
    assert!(s.any_sounding(20), "the other finger still sounds it");
    s.lifted((4, 12));
    assert!(!s.any_sounding(20));
  }

  #[test]
  fn striking_the_same_monomekey_again_replaces_what_it_sounds() {
    let mut s = Sounding::new();
    s.struck((3, 3), 20);
    s.struck((3, 3), 25);
    assert_eq!(s.under((3, 3)), Some(25));
    assert!(!s.any_sounding(20));
  }
}
