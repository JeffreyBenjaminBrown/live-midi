//! Pitch-only momentary/toggle chord slots for the tone-target monome instrument.

use std::collections::HashMap;

pub const SLOTS: usize = 13;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockCell {
  ChordMode,
  Arm,
  Slot(usize),
}

/// Top row: slots 0..6 then the separate ACCRETE cell. Bottom row: slots
/// 7..12, CHORD MODE, ARM. The ACCRETE hole belongs to its own overlay.
pub fn block_cell(rect: [i32; 4], cell: (i32, i32)) -> Option<BlockCell> {
  let [x0, y0, x1, y1] = rect;
  let (x, y) = cell;
  if x < x0 || x > x1 || y < y0 || y > y1 {
    return None;
  }
  let dx = (x - x0) as usize;
  match (y - y0, dx) {
    (0, 0..=6) => Some(BlockCell::Slot(dx)),
    (1, 0..=5) => Some(BlockCell::Slot(7 + dx)),
    (1, 6) => Some(BlockCell::ChordMode),
    (1, 7) => Some(BlockCell::Arm),
    _ => None,
  }
}

pub fn slot_cell(rect: [i32; 4], slot: usize) -> (i32, i32) {
  let [x0, y0, ..] = rect;
  if slot < 7 {
    (x0 + slot as i32, y0)
  } else {
    (x0 + (slot - 7) as i32, y0 + 1)
  }
}

pub fn chord_mode_cell(rect: [i32; 4]) -> (i32, i32) {
  (rect[0] + 6, rect[1] + 1)
}

pub fn arm_cell(rect: [i32; 4]) -> (i32, i32) {
  (rect[2], rect[3])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChordMode {
  Momentary,
  Toggle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveVoice {
  pub slot: usize,
  pub pitch: i32,
}

#[derive(Debug)]
pub struct MomentaryChords {
  pub slots: [Option<Vec<i32>>; SLOTS],
  pub armed: bool,
  pub chord_mode: ChordMode,
  held: [bool; SLOTS],
  pub live: HashMap<u64, LiveVoice>,
  next_seq: u64,
}

impl Default for MomentaryChords {
  fn default() -> Self {
    Self {
      slots: std::array::from_fn(|_| None),
      armed: false,
      chord_mode: ChordMode::Momentary,
      held: [false; SLOTS],
      live: HashMap::new(),
      next_seq: 1 << 63, // disjoint from legacy chord voices if a bad rig mixes them
    }
  }
}

impl MomentaryChords {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn held_slots(&self) -> Vec<usize> {
    (0..SLOTS).filter(|slot| self.held[*slot]).collect()
  }

  pub fn storage_source_slots(&self) -> Vec<usize> {
    match self.chord_mode {
      ChordMode::Momentary => self.held_slots(),
      ChordMode::Toggle => (0..SLOTS).filter(|slot| self.slot_sounding(*slot)).collect(),
    }
  }

  pub fn slot_sounding(&self, slot: usize) -> bool {
    self.live.values().any(|v| v.slot == slot)
  }

  pub fn live_pitches(&self) -> impl Iterator<Item = i32> + '_ {
    self.live.values().map(|v| v.pitch)
  }

  /// Key-down for a disarmed slot. Repeated down edges while already held do not
  /// duplicate the recall.
  pub fn press(&mut self, slot: usize) -> Vec<(u64, i32)> {
    if self.held[slot] {
      return Vec::new();
    }
    self.held[slot] = true;
    let Some(pitches) = self.slots[slot].clone() else {
      return Vec::new();
    };
    pitches
      .into_iter()
      .map(|pitch| {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.live.insert(seq, LiveVoice { slot, pitch });
        (seq, pitch)
      })
      .collect()
  }

  /// Key-up: end only voices born from this slot's current hold.
  pub fn release(&mut self, slot: usize) -> Vec<u64> {
    self.held[slot] = false;
    if self.chord_mode == ChordMode::Toggle {
      return Vec::new();
    }
    self.end_slot(slot)
  }

  pub fn end_slot(&mut self, slot: usize) -> Vec<u64> {
    let seqs: Vec<u64> =
      self.live.iter().filter(|(_, v)| v.slot == slot).map(|(seq, _)| *seq).collect();
    for seq in &seqs {
      self.live.remove(seq);
    }
    seqs
  }

  /// A toggle-chord-mode key-down either ends this slot or starts a fresh recall.
  pub fn toggle(&mut self, slot: usize) -> (Vec<(u64, i32)>, Vec<u64>) {
    if self.slot_sounding(slot) {
      return (Vec::new(), self.end_slot(slot));
    }
    (self.spawn(slot), Vec::new())
  }

  fn spawn(&mut self, slot: usize) -> Vec<(u64, i32)> {
    let Some(pitches) = self.slots[slot].clone() else {
      return Vec::new();
    };
    pitches
      .into_iter()
      .map(|pitch| {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.live.insert(seq, LiveVoice { slot, pitch });
        (seq, pitch)
      })
      .collect()
  }

  /// Change chord mode. Toggle -> momentary ends every latch immediately;
  /// momentary -> toggle adopts any physically held recalls as latches.
  pub fn toggle_chord_mode(&mut self) -> Vec<u64> {
    self.chord_mode = match self.chord_mode {
      ChordMode::Momentary => ChordMode::Toggle,
      ChordMode::Toggle => ChordMode::Momentary,
    };
    if self.chord_mode == ChordMode::Momentary {
      self.end_all()
    } else {
      Vec::new()
    }
  }

  pub fn save(&mut self, slot: usize, pitches: &[i32]) {
    self.armed = false;
    let pitches = normalized(pitches);
    if !pitches.is_empty() {
      self.slots[slot] = Some(pitches);
    }
  }

  pub fn overwrite_held(&mut self, pitches: &[i32]) -> bool {
    let pitches = normalized(pitches);
    if pitches.is_empty() {
      return false;
    }
    let held = self.storage_source_slots();
    for slot in held {
      self.slots[slot] = Some(pitches.clone());
    }
    true
  }

  pub fn shift_live(&mut self, delta: i32) -> Vec<(u64, i32)> {
    let mut moved = Vec::with_capacity(self.live.len());
    for (seq, voice) in &mut self.live {
      voice.pitch += delta;
      moved.push((*seq, voice.pitch));
    }
    moved
  }

  pub fn end_all(&mut self) -> Vec<u64> {
    let seqs = self.live.keys().copied().collect();
    self.live.clear();
    seqs
  }
}

pub fn normalized(pitches: &[i32]) -> Vec<i32> {
  let mut result = pitches.to_vec();
  result.sort_unstable();
  result.dedup();
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  const RECT: [i32; 4] = [0, 14, 7, 15];

  #[test]
  fn mapping_has_two_controls_and_thirteen_invertible_slots() {
    assert_eq!(block_cell(RECT, chord_mode_cell(RECT)), Some(BlockCell::ChordMode));
    assert_eq!(block_cell(RECT, arm_cell(RECT)), Some(BlockCell::Arm));
    assert_eq!(block_cell(RECT, (7, 14)), None, "ACCRETE owns the one block hole");
    for slot in 0..SLOTS {
      assert_eq!(block_cell(RECT, slot_cell(RECT, slot)), Some(BlockCell::Slot(slot)));
    }
  }

  #[test]
  fn recalls_are_momentary_and_can_sound_together() {
    let mut chords = MomentaryChords::new();
    chords.save(0, &[9, 5]);
    chords.save(1, &[12]);
    assert_eq!(chords.press(0).len(), 2);
    assert_eq!(chords.press(1).len(), 1);
    assert_eq!(chords.live.len(), 3);
    assert_eq!(chords.release(0).len(), 2);
    assert_eq!(chords.live.len(), 1);
  }

  #[test]
  fn toggle_slots_sound_independently_and_returning_to_momentary_releases_them() {
    let mut chords = MomentaryChords::new();
    chords.save(0, &[5]);
    chords.save(1, &[9]);
    assert!(chords.toggle_chord_mode().is_empty());
    assert_eq!(chords.chord_mode, ChordMode::Toggle);

    assert_eq!(chords.toggle(0).0.len(), 1);
    assert_eq!(chords.toggle(1).0.len(), 1);
    assert!(chords.slot_sounding(0) && chords.slot_sounding(1));
    assert_eq!(chords.toggle(0).1.len(), 1);
    assert!(!chords.slot_sounding(0));
    assert!(chords.slot_sounding(1));

    let released = chords.toggle_chord_mode();
    assert_eq!(chords.chord_mode, ChordMode::Momentary);
    assert_eq!(released.len(), 1);
    assert!(chords.live.is_empty());
  }

  #[test]
  fn toggle_storage_sources_are_the_latched_slots() {
    let mut chords = MomentaryChords::new();
    chords.save(2, &[1]);
    chords.save(4, &[2]);
    chords.toggle_chord_mode();
    chords.toggle(2);
    chords.toggle(4);
    assert_eq!(chords.storage_source_slots(), vec![2, 4]);
    assert!(chords.overwrite_held(&[8, 8, 3]));
    assert_eq!(chords.slots[2].as_deref(), Some([3, 8].as_slice()));
    assert_eq!(chords.slots[4].as_deref(), Some([3, 8].as_slice()));
  }

  #[test]
  fn held_source_overwrite_uses_sorted_unique_pitches() {
    let mut chords = MomentaryChords::new();
    chords.save(0, &[1]);
    chords.press(0);
    assert!(chords.overwrite_held(&[9, 5, 9]));
    assert_eq!(chords.slots[0].as_deref(), Some([5, 9].as_slice()));
  }

  #[test]
  fn shifting_live_does_not_change_storage() {
    let mut chords = MomentaryChords::new();
    chords.save(0, &[3, 7]);
    chords.press(0);
    chords.shift_live(41);
    assert_eq!(chords.slots[0].as_deref(), Some([3, 7].as_slice()));
    assert!(chords.live.values().all(|v| [44, 48].contains(&v.pitch)));
  }
}
