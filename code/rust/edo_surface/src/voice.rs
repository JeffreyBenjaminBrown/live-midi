//! What the instrument is sounding, and how each sounding thing is identified.
//!
//! A voice is identified by a VoiceId and by nothing else. That is the whole design,
//! and everything here follows from it.
//!
//! Two things this instrument does make every other identity wrong. A voice's pitch
//! changes, because an edit can drag it, so pitch cannot identify a voice. And two
//! sustained voices are allowed to sound the same pitch, phasing against each other,
//! so pitch cannot even distinguish them.
//!
//! The old library learned this the expensive way. It addressed a sustained voice by
//! its pitch, so two of them collided on one map entry, and `sustain_note` quietly
//! released the incoming voice rather than storing it. Jeff has been playing an
//! instrument that silently discards one of two same-pitch sustained voices for as
//! long as the feature has existed. Nothing reported it, because the code did exactly
//! what its types allowed.
//!
//! It addressed a fingered voice by a monomekey packed into a field named `pitch`, in
//! a variant named `Accreted`, alongside a `chord` field naming no chord and a
//! `Fingered` variant that fingered voices did not use. Code that selected voices by
//! pitch matched no fingered voice at all, and failed silently.
//!
//! So: the id carries no data that can change or repeat. Pitch and monomekey become
//! things a voice HAS, looked up through indexes that are maintained in one place,
//! rather than things a voice IS.

use std::collections::{HashMap, HashSet};

/// Which grid a voice belongs to. The instrument has two.
pub type GridIndex = usize;

/// One button on a monome grid, addressed as `(x, y)`.
///
/// This is the only sense of "key" the library uses, and it is spelled out to keep it
/// that way. The old code called a button a key, called a map lookup a key, and called
/// a drone's pitch its key.
pub type MonomeKey = (i32, i32);

/// An absolute step in the tuning, octave included. A pitch class is this modulo the
/// EDO, and the two differ: most LED rules work in pitch classes, because a sounding
/// voice lights all its octave-equivalents.
pub type Pitch = i32;

/// A voice's identity, for its whole life.
///
/// Opaque and never reused. It deliberately carries no information: a value that means
/// something is a value that can stop being true, and every identity bug in the old
/// library came from an id that meant something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VoiceId(u64);

/// Hands out VoiceIds. One per instrument.
#[derive(Debug, Default)]
pub struct VoiceIds {
  next: u64,
}

impl VoiceIds {
  pub fn new() -> Self {
    VoiceIds { next: 0 }
  }

  pub fn mint(&mut self) -> VoiceId {
    let id = VoiceId(self.next);
    self.next += 1;
    id
  }
}

/// Why a voice is still sounding.
///
/// A voice sounds while a finger holds it, or while it is sustained, and for no other
/// reason. Edit is not here: Jeff's spec says edit "isn't a third reason for being for
/// a voice. It is a property that can be true of any voice."
///
/// That distinction is what makes the ghost class impossible. An edit property cannot
/// outlive the voice it is a property of, any more than a colour can outlive the thing
/// it painted. The old model let edit keep a voice alive, so a clear could silence the
/// voice and leave its pitch marked as edited, dancing around nothing, with no gesture
/// able to reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
  /// A finger is on `monomekey`. The voice ends when that finger lifts.
  ///
  /// The monomekey is where the finger went down, which is not where this voice's
  /// pitch is drawn once the register scrolls. It is an index for finding the voice
  /// again on release, and it is not the voice's identity.
  Finger { monomekey: MonomeKey },
  /// Nothing physical holds it. It sounds because it is in the sustained set, and it
  /// ends when a clear or an un-sustain removes it.
  Sustained,
  /// It is ringing out after being cut, and nothing can reach it any more.
  Fading,
}

/// One voice's identity and bookkeeping. The synth's per-voice audio state (frequency,
/// envelope, timbre, glide, pulse) hangs off the same id, and lives in the synth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Voice {
  pub id: VoiceId,
  pub grid: GridIndex,
  /// What it is sounding right now. An edit can change this, which is precisely why it
  /// is not the identity.
  pub pitch: Pitch,
  pub held: Held,
  /// Whether the player has this voice selected for editing. A property of the voice,
  /// so it dies with the voice.
  pub edited: bool,
}

impl Voice {
  /// Can the player still hear it?
  pub fn sounding(&self) -> bool {
    !matches!(self.held, Held::Fading)
  }
}

/// Every voice the instrument is sounding, on both grids, with the indexes that make
/// the questions it asks cheap.
///
/// The indexes exist because the identity deliberately carries no data. That trade is
/// the point: an index can be rebuilt and can be maintained in one place, whereas an
/// identity that encodes a pitch is wrong the moment the pitch changes, everywhere at
/// once, silently.
#[derive(Debug, Default)]
pub struct Voices {
  by_id: HashMap<VoiceId, Voice>,
  /// Which voice a finger is holding. This is what a lift looks up, and it is the only
  /// reason a monomekey is recorded at all.
  under_finger: HashMap<(GridIndex, MonomeKey), VoiceId>,
  ids: VoiceIds,
}

impl Voices {
  pub fn new() -> Self {
    Voices::default()
  }

  /// Start a voice under a finger, and return its id.
  pub fn strike(&mut self, grid: GridIndex, monomekey: MonomeKey, pitch: Pitch) -> VoiceId {
    let id = self.ids.mint();
    self.by_id.insert(
      id,
      Voice { id, grid, pitch, held: Held::Finger { monomekey }, edited: false },
    );
    self.under_finger.insert((grid, monomekey), id);
    id
  }

  pub fn get(&self, id: VoiceId) -> Option<&Voice> {
    self.by_id.get(&id)
  }

  /// Which voice is the finger on `monomekey` holding?
  pub fn under(&self, grid: GridIndex, monomekey: MonomeKey) -> Option<VoiceId> {
    self.under_finger.get(&(grid, monomekey)).copied()
  }

  /// Every voice sounding `pitch` on `grid`, in id order.
  ///
  /// Several is normal and legal: two fingers can collide on one pitch, and two
  /// sustained voices can share one. Jeff: "Now we have two sustained voices at the
  /// same pitch. Still fine. They'll phase; whatever." So this returns all of them,
  /// and callers act on all of them, because nothing in a gesture distinguishes them.
  pub fn sounding_pitch(&self, grid: GridIndex, pitch: Pitch) -> Vec<VoiceId> {
    let mut ids: Vec<VoiceId> = self
      .by_id
      .values()
      .filter(|v| v.grid == grid && v.pitch == pitch && v.sounding())
      .map(|v| v.id)
      .collect();
    ids.sort();
    ids
  }

  /// Is anything on `grid` sounding `pitch`?
  pub fn any_sounding(&self, grid: GridIndex, pitch: Pitch) -> bool {
    self.by_id.values().any(|v| v.grid == grid && v.pitch == pitch && v.sounding())
  }

  /// Move a voice's pitch. The indexes need no repair, because none of them is keyed by
  /// pitch. That is the whole reason this design exists.
  pub fn set_pitch(&mut self, id: VoiceId, pitch: Pitch) -> bool {
    match self.by_id.get_mut(&id) {
      Some(v) => {
        v.pitch = pitch;
        true
      }
      None => false,
    }
  }

  /// Add a voice to the sustained set, so it keeps sounding once the finger lifts.
  /// The finger index drops it, since a lift can no longer end it.
  pub fn sustain(&mut self, id: VoiceId) -> bool {
    let Some(v) = self.by_id.get_mut(&id) else { return false };
    if let Held::Finger { monomekey } = v.held {
      self.under_finger.remove(&(v.grid, monomekey));
    }
    if matches!(v.held, Held::Fading) {
      return false;
    }
    v.held = Held::Sustained;
    true
  }

  /// Take a voice out of the sustained set. It has nothing holding it up afterwards,
  /// so it starts fading.
  pub fn unsustain(&mut self, id: VoiceId) -> bool {
    match self.by_id.get_mut(&id) {
      Some(v) if matches!(v.held, Held::Sustained) => {
        v.held = Held::Fading;
        true
      }
      _ => false,
    }
  }

  /// A finger lifted. The voice ends unless it was sustained, in which case the finger
  /// was not holding it up in the first place.
  pub fn lift(&mut self, grid: GridIndex, monomekey: MonomeKey) -> Option<VoiceId> {
    let id = self.under_finger.remove(&(grid, monomekey))?;
    if let Some(v) = self.by_id.get_mut(&id) {
      if matches!(v.held, Held::Finger { .. }) {
        v.held = Held::Fading;
      }
    }
    Some(id)
  }

  /// Every sustained voice on `grid`.
  pub fn sustained(&self, grid: GridIndex) -> Vec<VoiceId> {
    let mut ids: Vec<VoiceId> = self
      .by_id
      .values()
      .filter(|v| v.grid == grid && matches!(v.held, Held::Sustained))
      .map(|v| v.id)
      .collect();
    ids.sort();
    ids
  }

  /// Clear this grid's sustained voices, and no others. Fingered voices are untouched,
  /// as Jeff specified: "The 'clear sustain' button on the softstep should only clear
  /// sustained notes, not fingered notes."
  ///
  /// Their edit properties go with them, because the property lives on the voice. No
  /// separate bookkeeping needs telling.
  pub fn clear_sustained(&mut self, grid: GridIndex) -> Vec<VoiceId> {
    let ids = self.sustained(grid);
    for id in &ids {
      if let Some(v) = self.by_id.get_mut(id) {
        v.held = Held::Fading;
      }
    }
    ids
  }

  /// Set or unset a voice's edit property.
  pub fn set_edited(&mut self, id: VoiceId, edited: bool) -> bool {
    match self.by_id.get_mut(&id) {
      Some(v) => {
        v.edited = edited;
        true
      }
      None => false,
    }
  }

  /// Every edited voice on `grid`.
  pub fn edited(&self, grid: GridIndex) -> Vec<VoiceId> {
    let mut ids: Vec<VoiceId> = self
      .by_id
      .values()
      .filter(|v| v.grid == grid && v.edited && v.sounding())
      .map(|v| v.id)
      .collect();
    ids.sort();
    ids
  }

  /// Drop the edit property from every voice on `grid`, silencing nothing. This is the
  /// clear-edit pedal.
  pub fn clear_edited(&mut self, grid: GridIndex) {
    for v in self.by_id.values_mut() {
      if v.grid == grid {
        v.edited = false;
      }
    }
  }

  /// Every pitch class sounding on `grid`, for the LEDs.
  pub fn sounding_classes(&self, grid: GridIndex, edo: i32) -> HashSet<i32> {
    self
      .by_id
      .values()
      .filter(|v| v.grid == grid && v.sounding())
      .map(|v| v.pitch.rem_euclid(edo))
      .collect()
  }

  /// Forget a faded voice once the synth has finished ringing it out.
  pub fn retire(&mut self, id: VoiceId) {
    self.by_id.remove(&id);
  }

  pub fn len(&self) -> usize {
    self.by_id.len()
  }

  pub fn is_empty(&self) -> bool {
    self.by_id.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn voices() -> Voices {
    Voices::new()
  }

  #[test]
  fn an_id_is_never_reused() {
    let mut v = voices();
    let a = v.strike(0, (1, 1), 20);
    let b = v.strike(0, (2, 2), 20);
    v.lift(0, (1, 1));
    v.retire(a);
    let c = v.strike(0, (1, 1), 20);
    assert_ne!(a, b);
    assert_ne!(a, c, "a retired id must not come back and address someone else's voice");
    assert_ne!(b, c);
  }

  /// The case the old library silently broke. Jeff: "Now we have two sustained voices
  /// at the same pitch. Still fine. They'll phase; whatever." The old code checked
  /// whether a drone already existed at that pitch and released the incoming voice
  /// instead of storing it, so one of the two just vanished.
  #[test]
  fn two_sustained_voices_can_share_a_pitch() {
    let mut v = voices();
    let a = v.strike(0, (1, 1), 20);
    let b = v.strike(0, (4, 12), 20); // a second monomekey, same pitch
    v.sustain(a);
    v.sustain(b);

    assert_eq!(v.sustained(0), vec![a, b], "both survive, and both sound");
    assert_eq!(v.sounding_pitch(0, 20), vec![a, b]);
  }

  /// The collision story, exactly as specified: a fingered voice and a sustained voice
  /// at one pitch are independent in both directions.
  #[test]
  fn a_finger_and_a_sustained_voice_at_one_pitch_do_not_disturb_each_other() {
    let mut v = voices();
    let held = v.strike(0, (1, 1), 20);
    let kept = v.strike(0, (4, 12), 20);
    v.sustain(kept);

    // "lifting the finger ends the fingered voice, without changing the sustained one"
    v.lift(0, (1, 1));
    assert!(!v.get(held).unwrap().sounding());
    assert!(v.get(kept).unwrap().sounding(), "the sustained voice is untouched");

    // "clearing sustained voices likewise won't affect the fingered voice"
    let mut v = voices();
    let held = v.strike(0, (1, 1), 20);
    let kept = v.strike(0, (4, 12), 20);
    v.sustain(kept);
    v.clear_sustained(0);
    assert!(!v.get(kept).unwrap().sounding());
    assert!(v.get(held).unwrap().sounding(), "the fingered voice plays on");
  }

  /// "But that fingered voice could then be added to the sustained set. Now we have two
  /// sustained voices at the same pitch. Still fine."
  #[test]
  fn a_fingered_voice_can_join_a_sustained_one_at_the_same_pitch() {
    let mut v = voices();
    let first = v.strike(0, (1, 1), 20);
    v.sustain(first);
    let second = v.strike(0, (4, 12), 20);
    v.sustain(second);
    assert_eq!(v.sustained(0), vec![first, second]);
  }

  /// Pitch is not an identity, so moving one changes nothing about how a voice is
  /// found. No index needs repairing, which is the entire argument for the design.
  #[test]
  fn moving_a_voices_pitch_leaves_every_index_intact() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    assert!(v.set_pitch(id, 35));

    assert_eq!(v.get(id).unwrap().pitch, 35);
    assert_eq!(v.under(0, (3, 3)), Some(id), "the finger still finds it");
    assert!(v.any_sounding(0, 35));
    assert!(!v.any_sounding(0, 20), "and nothing thinks it still sounds the old pitch");
  }

  /// The bug this replaces: a lift used to look the voice up by the pitch its
  /// monomekey nominally meant, so after a drag it found the wrong pitch, decided the
  /// voice was neither sustained nor edited, and cut a voice it should have kept.
  #[test]
  fn lifting_a_dragged_finger_ends_the_right_voice() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.set_pitch(id, 35);
    assert_eq!(v.lift(0, (3, 3)), Some(id), "the monomekey still names its voice");
    assert!(!v.get(id).unwrap().sounding());
  }

  #[test]
  fn a_sustained_voice_survives_the_lift_of_the_finger_that_started_it() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.sustain(id);
    assert_eq!(v.lift(0, (3, 3)), None, "the finger no longer holds it up");
    assert!(v.get(id).unwrap().sounding());
  }

  #[test]
  fn un_sustaining_ends_a_voice_that_nothing_else_holds() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.sustain(id);
    assert!(v.unsustain(id));
    assert!(!v.get(id).unwrap().sounding());
  }

  // ---- edit is a property, not a reason ----

  /// Jeff: "edit really isn't a third reason for being for a voice. It is a property
  /// that can be true of any voice." So an edited fingered voice dies on the lift like
  /// any other, and you sustain it if you want it to stay.
  #[test]
  fn an_edited_fingered_voice_still_dies_when_its_finger_lifts() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.set_edited(id, true);
    v.lift(0, (3, 3));
    assert!(!v.get(id).unwrap().sounding(), "edit is not a reason to keep sounding");
  }

  /// The ghost class, made impossible. A property cannot outlive the thing it is a
  /// property of, so a clear cannot leave an edited pitch behind with no voice under
  /// it. Nothing needs to remember to tidy up.
  #[test]
  fn clearing_sustain_takes_the_edit_property_with_it() {
    let mut v = voices();
    let id = v.strike(0, (1, 1), 20);
    v.sustain(id);
    v.set_edited(id, true);
    assert_eq!(v.edited(0), vec![id]);

    v.clear_sustained(0);
    assert!(v.edited(0).is_empty(), "no edited voice survives the voices being cleared");
  }

  #[test]
  fn clearing_edit_silences_nothing() {
    let mut v = voices();
    let id = v.strike(0, (1, 1), 20);
    v.sustain(id);
    v.set_edited(id, true);
    v.clear_edited(0);
    assert!(v.edited(0).is_empty());
    assert!(v.get(id).unwrap().sounding(), "clear-edit is not a clear-sustain");
  }

  /// "There's no way to specify which to edit, so all of them should gain the edit
  /// property." The gesture names a pitch; the caller edits every voice it finds there.
  #[test]
  fn a_pitch_names_every_voice_sounding_it_so_a_gesture_can_act_on_all() {
    let mut v = voices();
    let a = v.strike(0, (1, 1), 20);
    let b = v.strike(0, (4, 12), 20);
    let other = v.strike(0, (2, 2), 30);
    v.sustain(a);

    assert_eq!(v.sounding_pitch(0, 20), vec![a, b], "both, fingered and sustained alike");
    assert!(!v.sounding_pitch(0, 20).contains(&other));
  }

  // ---- the grids stay apart ----

  #[test]
  fn one_grids_gestures_never_reach_the_other() {
    let mut v = voices();
    let a = v.strike(0, (1, 1), 20);
    let b = v.strike(1, (1, 1), 20);
    v.sustain(a);
    v.sustain(b);
    v.set_edited(a, true);
    v.set_edited(b, true);

    v.clear_sustained(0);
    assert!(!v.get(a).unwrap().sounding());
    assert!(v.get(b).unwrap().sounding(), "grid 1 is its own business");

    v.clear_edited(0);
    assert_eq!(v.edited(1), vec![b]);
  }

  #[test]
  fn the_same_monomekey_on_two_grids_holds_two_voices() {
    let mut v = voices();
    let a = v.strike(0, (1, 1), 20);
    let b = v.strike(1, (1, 1), 20);
    assert_ne!(a, b);
    assert_eq!(v.under(0, (1, 1)), Some(a));
    assert_eq!(v.under(1, (1, 1)), Some(b));
  }

  #[test]
  fn a_faded_voice_stops_answering_and_can_be_retired() {
    let mut v = voices();
    let id = v.strike(0, (1, 1), 20);
    v.lift(0, (1, 1));
    assert!(!v.any_sounding(0, 20), "a fading voice is not sounding");
    assert!(v.sounding_pitch(0, 20).is_empty());
    v.retire(id);
    assert!(v.is_empty());
  }
}
