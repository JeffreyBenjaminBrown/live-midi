use std::collections::HashMap;
use std::time::Instant;

use super::config::RemapConfig;

#[derive(Clone)]
pub(crate) struct RemappableEdoState {
  pub(crate) config: RemapConfig,
  pub(crate) map: [i16; 12],
  pub(crate) deltas: [i16; 12],
  pub(crate) loose: [LooseState; 12],
  pub(crate) history: Vec<RemapSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RemapSnapshot {
  pub(crate) map: [i16; 12],
  pub(crate) deltas: [i16; 12],
  pub(crate) loose: [LooseState; 12],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LooseState {
  Loose,
  Fixed,
}

pub(crate) struct PreimageRowState {
  pub(crate) active_by_cell: HashMap<(i32, i32), usize>,
  pub(crate) counts: [u16; 12],
  pub(crate) flash_until: [Option<Instant>; 12],
}

impl PreimageRowState {
  pub(crate) fn new() -> Self {
    PreimageRowState {
      active_by_cell: HashMap::new(),
      counts: [0; 12],
      flash_until: [None; 12],
    }
  }

  pub(crate) fn press(&mut self, cell: (i32, i32), preimage: usize, now: Instant) {
    if let Some(old_preimage) = self.active_by_cell.insert(cell, preimage) {
      decrement_count(&mut self.counts[old_preimage]);
    }
    self.counts[preimage] += 1;
    self.flash_until[preimage] = Some(now + super::PREIMAGE_ROW_FLASH_MIN);
  }

  pub(crate) fn release(&mut self, cell: (i32, i32)) -> bool {
    let Some(preimage) = self.active_by_cell.remove(&cell) else {
      return false;
    };
    decrement_count(&mut self.counts[preimage]);
    true
  }
}

fn decrement_count(count: &mut u16) {
  if *count > 0 {
    *count -= 1;
  }
}

impl RemappableEdoState {
  pub(crate) fn new(config: RemapConfig) -> Self {
    RemappableEdoState {
      map: config.initial_map,
      config,
      deltas: [0; 12],
      loose: [LooseState::Fixed; 12],
      history: vec![],
    }
  }

  pub(crate) fn snapshot(&self) -> RemapSnapshot {
    RemapSnapshot {
      map: self.map,
      deltas: self.deltas,
      loose: self.loose,
    }
  }
}

pub(crate) struct SoundingPitchCounts {
  pub(crate) by_original_note: HashMap<u8, i16>,
  pub(crate) held_order: Vec<u8>,
  pub(crate) counts: Vec<u16>,
}

impl SoundingPitchCounts {
  pub(crate) fn new(edo: i16) -> Self {
    SoundingPitchCounts {
      by_original_note: HashMap::new(),
      held_order: Vec::new(),
      counts: vec![0; edo as usize],
    }
  }

  pub(crate) fn has_held_notes(&self) -> bool {
    !self.held_order.is_empty()
  }

  pub(crate) fn most_recent_step(&self) -> Option<i16> {
    self
      .held_order
      .last()
      .and_then(|note| self.by_original_note.get(note))
      .copied()
  }
}
