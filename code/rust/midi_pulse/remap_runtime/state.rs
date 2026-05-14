use std::collections::HashMap;

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
  pub(crate) counts: Vec<u16>,
}

impl SoundingPitchCounts {
  pub(crate) fn new(edo: i16) -> Self {
    SoundingPitchCounts {
      by_original_note: HashMap::new(),
      counts: vec![0; edo as usize],
    }
  }
}
