use std::collections::HashMap;

use crate::config::EdoConfig;

#[derive(Clone)]
pub(crate) struct Edo31State {
  pub(crate) config: EdoConfig,
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

impl Edo31State {
  pub(crate) fn new(config: EdoConfig) -> Self {
    Edo31State {
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

pub(crate) struct SoundingState {
  pub(crate) by_original_note: HashMap<u8, i16>,
  pub(crate) counts: Vec<u16>,
}

impl SoundingState {
  pub(crate) fn new(edo: i16) -> Self {
    SoundingState {
      by_original_note: HashMap::new(),
      counts: vec![0; edo as usize],
    }
  }
}
