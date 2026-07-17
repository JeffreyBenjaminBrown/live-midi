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
/// Edit is not a variant here, because Jeff's spec says edit "isn't a third reason for
/// being for a voice. It is a property that can be true of any voice." What edit does
/// is DEFER a fingered voice's release, which is a different claim: the voice is still
/// the finger's, and the finger has merely let go for now.
///
/// That distinction is what makes the ghost class impossible. An edit property cannot
/// outlive the voice it is a property of, so a clear cannot silence a voice and leave
/// something dancing around nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
  /// A finger is physically down on the voice's `home`.
  Finger,
  /// The finger lifted, but the voice is edited, so its release waits.
  ///
  /// Jeff: "A fingered voice in edit mode should not end on key-up... When edit mode is
  /// released, the fingered voice resumes normal release behavior." So this state ends
  /// one of two ways: the finger comes back to `home` and the voice retriggers, or edit
  /// mode ends and the voice does what the lift would have done.
  ///
  /// This is why a voice remembers its `home` even with no finger on it: "If the
  /// monomekey for that voice is pressed again, it retriggers the fingered voice there
  /// rather than adding a second voice."
  EditDeferred,
  /// It is in the sustained set, and only a clear or an un-sustain ends it.
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
  /// The monomekey this voice lives at: where its finger is, or where the finger that
  /// last held it was. A press here retriggers this voice rather than starting a second
  /// one, and a drag moves it.
  ///
  /// It is not the identity either. The register scrolls, so a monomekey does not
  /// name a fixed pitch; and a drag re-homes a voice, so it does not name a fixed
  /// voice for long.
  pub home: MonomeKey,
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

  /// Is a finger physically down on it right now?
  pub fn under_finger(&self) -> bool {
    matches!(self.held, Held::Finger)
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
  /// Which voice lives at each monomekey.
  ///
  /// Not "which finger is down": a voice whose finger lifted while edited still lives
  /// at its home, because pressing there must retrigger it rather than start a second
  /// voice. The entry goes when the voice stops being reachable from the surface.
  home: HashMap<(GridIndex, MonomeKey), VoiceId>,
  ids: VoiceIds,
}

impl Voices {
  pub fn new() -> Self {
    Voices::default()
  }

  /// Start a voice under a finger at `monomekey`, and return its id. Any voice already
  /// living there is displaced, which only happens if the caller failed to retrigger.
  pub fn strike(&mut self, grid: GridIndex, monomekey: MonomeKey, pitch: Pitch) -> VoiceId {
    let id = self.ids.mint();
    self
      .by_id
      .insert(id, Voice { id, grid, pitch, home: monomekey, held: Held::Finger, edited: false });
    self.home.insert((grid, monomekey), id);
    id
  }

  pub fn get(&self, id: VoiceId) -> Option<&Voice> {
    self.by_id.get(&id)
  }

  /// Which voice lives at `monomekey`, whether or not a finger is on it.
  ///
  /// This is what a press consults before striking: if a voice lives here, the press
  /// retriggers it. Jeff: "If the monomekey for that voice is pressed again, it
  /// retriggers the fingered voice there rather than adding a second voice."
  pub fn at(&self, grid: GridIndex, monomekey: MonomeKey) -> Option<VoiceId> {
    self.home.get(&(grid, monomekey)).copied()
  }

  /// Re-strike the voice living at `monomekey`: its finger is back down.
  ///
  /// The caller restarts its envelope. Returns the voice, or `None` if none lives here
  /// or it is beyond reach.
  pub fn retrigger(&mut self, grid: GridIndex, monomekey: MonomeKey) -> Option<VoiceId> {
    let id = self.at(grid, monomekey)?;
    let v = self.by_id.get_mut(&id)?;
    if matches!(v.held, Held::Fading) {
      return None;
    }
    v.held = Held::Finger;
    Some(id)
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

  /// Move a voice to `pitch`, and re-home it at `monomekey` -- the monomekey the player
  /// pressed to drag it there.
  ///
  /// Re-homing is what makes Jeff's story work: "pick a new note to drag it there, leave
  /// my finger on it, and take it out of edit mode, and I'm still holding it until I
  /// release it." The finger that dragged it now holds it, so the voice moves house.
  ///
  /// A sustained voice keeps its `Sustained` state, because no finger is claiming it;
  /// it only changes address.
  pub fn drag(&mut self, id: VoiceId, monomekey: MonomeKey, pitch: Pitch) -> bool {
    let Some(v) = self.by_id.get_mut(&id) else { return false };
    if matches!(v.held, Held::Fading) {
      return false;
    }
    let (grid, was) = (v.grid, v.home);
    v.pitch = pitch;
    v.home = monomekey;
    if matches!(v.held, Held::Finger | Held::EditDeferred) {
      v.held = Held::Finger;
    }
    self.home.remove(&(grid, was));
    self.home.insert((grid, monomekey), id);
    true
  }

  /// Move a voice's pitch without moving its home. The pedals and the pulse use this;
  /// the drag gesture uses [`Voices::drag`].
  pub fn set_pitch(&mut self, id: VoiceId, pitch: Pitch) -> bool {
    match self.by_id.get_mut(&id) {
      Some(v) => {
        v.pitch = pitch;
        true
      }
      None => false,
    }
  }

  /// Add a voice to the sustained set, so it keeps sounding with no finger on it.
  pub fn sustain(&mut self, id: VoiceId) -> bool {
    match self.by_id.get_mut(&id) {
      Some(v) if !matches!(v.held, Held::Fading) => {
        v.held = Held::Sustained;
        true
      }
      _ => false,
    }
  }

  /// Take a voice out of the sustained set. Nothing else was holding it up, so it ends.
  pub fn unsustain(&mut self, id: VoiceId) -> bool {
    let Some(v) = self.by_id.get_mut(&id) else { return false };
    if !matches!(v.held, Held::Sustained) {
      return false;
    }
    v.held = Held::Fading;
    let (grid, home) = (v.grid, v.home);
    self.home.remove(&(grid, home));
    true
  }

  /// A finger lifted from `monomekey`.
  ///
  /// The voice ends, unless something defers that. Jeff: "A fingered voice in edit mode
  /// should not end on key-up... But if my finger isn't on it when I take it out of edit
  /// mode, it ends with the end of the edit mode." So an edited voice waits in
  /// [`Held::EditDeferred`], and a sustained one was never the finger's to end.
  pub fn lift(&mut self, grid: GridIndex, monomekey: MonomeKey) -> Option<VoiceId> {
    let id = self.at(grid, monomekey)?;
    let v = self.by_id.get_mut(&id)?;
    if !matches!(v.held, Held::Finger) {
      return Some(id); // sustained, or already waiting: the finger was not holding it
    }
    if v.edited {
      v.held = Held::EditDeferred;
    } else {
      v.held = Held::Fading;
      self.home.remove(&(grid, monomekey));
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

  /// Clear this grid's sustained voices, and no others. Jeff: "The 'clear sustain'
  /// button on the softstep should only clear sustained notes, not fingered notes."
  ///
  /// Their edit properties go with them, because the property lives on the voice.
  pub fn clear_sustained(&mut self, grid: GridIndex) -> Vec<VoiceId> {
    let ids = self.sustained(grid);
    for id in &ids {
      self.unsustain(*id);
    }
    ids
  }

  /// Set or unset a voice's edit property.
  ///
  /// Unsetting it on a voice whose finger already lifted ends that voice, because edit
  /// was the only thing deferring its release: "if my finger isn't on it when I take it
  /// out of edit mode, it ends with the end of the edit mode."
  pub fn set_edited(&mut self, id: VoiceId, edited: bool) -> bool {
    let Some(v) = self.by_id.get_mut(&id) else { return false };
    v.edited = edited;
    if !edited && matches!(v.held, Held::EditDeferred) {
      v.held = Held::Fading;
      let (grid, home) = (v.grid, v.home);
      self.home.remove(&(grid, home));
    }
    true
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

  /// Drop the edit property from every voice on `grid`. This is the clear-edit pedal,
  /// on OSS 6 for LOM and OSS 0 for RNM.
  ///
  /// It silences nothing that a finger holds, but it does end the voices that only edit
  /// mode was keeping alive, by the rule above. Returns those.
  pub fn clear_edited(&mut self, grid: GridIndex) -> Vec<VoiceId> {
    let ids: Vec<VoiceId> = self.edited(grid);
    let mut ended = vec![];
    for id in ids {
      let deferred = matches!(self.by_id.get(&id).map(|v| v.held), Some(Held::EditDeferred));
      self.set_edited(id, false);
      if deferred {
        ended.push(id);
      }
    }
    ended
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
    assert_eq!(v.at(0, (3, 3)), Some(id), "the finger still finds it");
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
    v.lift(0, (3, 3));
    assert!(v.get(id).unwrap().sounding(), "the finger was not what held it up");
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

  /// Jeff, correcting me: "A fingered voice in edit mode should not end on key-up...
  /// When edit mode is released, the fingered voice resumes normal release behavior."
  ///
  /// So edit defers the release rather than being a reason to sound. The distinction
  /// matters: the voice is still the finger's, and the finger has merely let go for
  /// now.
  #[test]
  fn an_edited_fingered_voice_waits_instead_of_ending_when_its_finger_lifts() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.set_edited(id, true);
    v.lift(0, (3, 3));
    assert!(v.get(id).unwrap().sounding(), "edit defers the release");
    assert_eq!(v.get(id).unwrap().held, Held::EditDeferred);
    assert!(!v.get(id).unwrap().under_finger(), "but no finger is on it");
  }

  /// "But if my finger isn't on it when I take it out of edit mode, it ends with the
  /// end of the edit mode."
  #[test]
  fn a_waiting_voice_ends_when_edit_mode_does() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.set_edited(id, true);
    v.lift(0, (3, 3));
    v.set_edited(id, false);
    assert!(!v.get(id).unwrap().sounding(), "nothing was holding it but the edit");
    assert_eq!(v.at(0, (3, 3)), None, "and it no longer lives there");
  }

  /// "...take it out of edit mode, and I'm still holding it until I release it."
  #[test]
  fn leaving_edit_mode_with_a_finger_down_resumes_normal_release() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.set_edited(id, true);
    v.set_edited(id, false); // never lifted
    assert!(v.get(id).unwrap().sounding(), "the finger still holds it");
    assert!(v.get(id).unwrap().under_finger());
    v.lift(0, (3, 3));
    assert!(!v.get(id).unwrap().sounding(), "and now it releases normally");
  }

  /// "If the monomekey for that voice is pressed again, it retriggers the fingered
  /// voice there rather than adding a second voice."
  #[test]
  fn pressing_a_waiting_voices_monomekey_retriggers_it_rather_than_adding_one() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.set_edited(id, true);
    v.lift(0, (3, 3));

    assert_eq!(v.retrigger(0, (3, 3)), Some(id), "the same voice comes back");
    assert_eq!(v.get(id).unwrap().held, Held::Finger);
    assert_eq!(v.len(), 1, "and no second voice was added");
  }

  /// Jeff's whole story, end to end: "I can put the note into edit mode, move my hand
  /// away, pick a new note to drag it there, leave my finger on it, and take it out of
  /// edit mode, and I'm still holding it until I release it."
  #[test]
  fn the_drag_hands_a_waiting_voice_to_the_finger_that_dragged_it() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.set_edited(id, true);
    v.lift(0, (3, 3)); // "move my hand away"
    assert_eq!(v.get(id).unwrap().held, Held::EditDeferred);

    v.drag(id, (7, 7), 35); // "pick a new note to drag it there, leave my finger on it"
    assert_eq!(v.get(id).unwrap().pitch, 35);
    assert_eq!(v.get(id).unwrap().home, (7, 7), "it moved house");
    assert_eq!(v.get(id).unwrap().held, Held::Finger, "the dragging finger now holds it");
    assert_eq!(v.at(0, (3, 3)), None, "and nothing lives at the old monomekey");
    assert_eq!(v.at(0, (7, 7)), Some(id));

    v.set_edited(id, false); // "take it out of edit mode"
    assert!(v.get(id).unwrap().sounding(), "I'm still holding it");
    v.lift(0, (7, 7)); // "until I release it"
    assert!(!v.get(id).unwrap().sounding());
  }

  /// A sustained voice that gets dragged changes address but keeps its own reason to
  /// sound: no finger is claiming it.
  #[test]
  fn dragging_a_sustained_voice_does_not_hand_it_to_a_finger() {
    let mut v = voices();
    let id = v.strike(0, (3, 3), 20);
    v.sustain(id);
    v.drag(id, (7, 7), 35);
    assert_eq!(v.get(id).unwrap().held, Held::Sustained);
    assert_eq!(v.get(id).unwrap().pitch, 35);
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
    assert_eq!(v.at(0, (1, 1)), Some(a));
    assert_eq!(v.at(1, (1, 1)), Some(b));
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

  /// The clear-edit pedals (OSS 6 for LOM, OSS 0 for RNM) drop the property from every
  /// voice on one grid. Anything a finger or a sustain holds keeps sounding; anything
  /// only edit was holding ends, by the same rule as leaving edit mode one voice at a
  /// time.
  #[test]
  fn clear_edit_keeps_held_voices_and_ends_the_waiting_ones() {
    let mut v = voices();
    let fingered = v.strike(0, (1, 1), 20);
    let kept = v.strike(0, (2, 2), 30);
    let waiting = v.strike(0, (3, 3), 40);
    v.sustain(kept);
    for id in [fingered, kept, waiting] {
      v.set_edited(id, true);
    }
    v.lift(0, (3, 3)); // `waiting` is now held up by nothing but its edit

    let ended = v.clear_edited(0);
    assert_eq!(ended, vec![waiting], "only the voice edit was carrying ends");
    assert!(v.get(fingered).unwrap().sounding(), "a finger is its own reason");
    assert!(v.get(kept).unwrap().sounding(), "so is a sustain");
    assert!(!v.get(waiting).unwrap().sounding());
    assert!(v.edited(0).is_empty());
  }

  /// Jeff's corner case, which he decided: pressing above a monomekey where one
  /// sustained and one fingered voice are ringing "should end the sustained note,
  /// without adding the fingered note to the sustained set", because otherwise one
  /// press would do two opposite things.
  ///
  /// The rule that falls out: if anything at this pitch is sustained, the gesture
  /// un-sustains, and it never sustains in the same press.
  #[test]
  fn the_sustain_gesture_un_sustains_when_anything_there_is_already_sustained() {
    let mut v = voices();
    let fingered = v.strike(0, (1, 1), 20);
    let already = v.strike(0, (4, 12), 20);
    v.sustain(already);

    // What the caller will compute from the voices at that pitch.
    let at_pitch = v.sounding_pitch(0, 20);
    let any_sustained =
      at_pitch.iter().any(|id| matches!(v.get(*id).unwrap().held, Held::Sustained));
    assert!(any_sustained, "so this press un-sustains rather than sustains");

    for id in &at_pitch {
      if matches!(v.get(*id).unwrap().held, Held::Sustained) {
        v.unsustain(*id);
      }
    }
    assert!(!v.get(already).unwrap().sounding(), "the sustained voice ends");
    assert!(v.get(fingered).unwrap().sounding(), "the fingered one is untouched");
    assert!(v.sustained(0).is_empty(), "and it was not added to the set");
  }
}
