use crate::config::{PianoRegionActionConfig, PianoRegionConfig};
use crate::mapping::PianoMapper;
use crate::{midi, piano_transform};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct PianoRuntime {
  mapper: PianoMapper,
  regions: Vec<PianoRegionConfig>,
  held_offsets: HashMap<u8, i16>,
  ongoing: Arc<Mutex<HashMap<u8, piano_transform::TransformedNote>>>,
}

impl PianoRuntime {
  pub fn new(mapper: PianoMapper, regions: Vec<PianoRegionConfig>) -> Self {
    PianoRuntime {
      mapper,
      regions,
      held_offsets: HashMap::new(),
      ongoing: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  pub fn transform_message(&mut self, message: &[u8]) -> Vec<Vec<u8>> {
    if message.len() < 3 || !midi::is_note_event(message) {
      return vec![message.to_vec()];
    }

    let note = message[1];
    let Some(region) = self.region_for_note(note) else {
      return vec![];
    };

    match region.action {
      PianoRegionActionConfig::EmitNotes => {
        if midi::is_note_on(message) {
          self.apply_held_offsets_to_mapper(note);
        }
        piano_transform::transform_message(message, &self.ongoing, |original_note| {
          self.mapper.instruction(original_note)
        })
      }
      PianoRegionActionConfig::HeldOffsetControl => {
        self.handle_held_offset_control(message, region.zero_note.expect("validated zero_note"));
        vec![]
      }
    }
  }

  fn region_for_note(&self, note: u8) -> Option<PianoRegionConfig> {
    self
      .regions
      .iter()
      .find(|region| region.range[0] <= note && note <= region.range[1])
      .cloned()
  }

  fn handle_held_offset_control(&mut self, message: &[u8], zero_note: u8) {
    let note = message[1];
    if midi::is_note_on(message) {
      self.held_offsets.insert(note, note as i16 - zero_note as i16);
    } else if midi::is_note_off(message) {
      self.held_offsets.remove(&note);
    }
  }

  fn apply_held_offsets_to_mapper(&mut self, note: u8) {
    let PianoMapper::TwelveN(mapping) = &mut self.mapper else {
      return;
    };
    if self.held_offsets.is_empty() {
      return;
    }
    let total_shift: i16 = self.held_offsets.values().sum();
    mapping.set_pitch_class_shift(note % 12, total_shift);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::mapping::TwelveNMapping;

  fn runtime() -> PianoRuntime {
    PianoRuntime::new(
      PianoMapper::TwelveN(TwelveNMapping::new(21, -5, 1, 28, 6)),
      vec![
        PianoRegionConfig {
          range: [0, 96],
          action: PianoRegionActionConfig::EmitNotes,
          zero_note: None,
        },
        PianoRegionConfig {
          range: [97, 108],
          action: PianoRegionActionConfig::HeldOffsetControl,
          zero_note: Some(102),
        },
      ],
    )
  }

  #[test]
  fn emit_region_transforms_notes() {
    let mut runtime = runtime();

    assert_eq!(runtime.transform_message(&[0x90, 26, 64]), vec![vec![0x91, 28, 64]]);
    assert_eq!(runtime.transform_message(&[0x80, 26, 0]), vec![vec![0x81, 28, 0]]);
  }

  #[test]
  fn held_offset_region_updates_pitch_class_shift_without_output() {
    let mut runtime = runtime();

    assert!(runtime.transform_message(&[0x90, 104, 64]).is_empty());
    assert_eq!(runtime.transform_message(&[0x90, 21, 64]), vec![vec![0x90, 72, 64]]);
    assert_eq!(runtime.transform_message(&[0x80, 21, 0]), vec![vec![0x80, 72, 0]]);
    assert!(runtime.transform_message(&[0x80, 104, 0]).is_empty());
    assert_eq!(runtime.transform_message(&[0x90, 33, 64]), vec![vec![0x91, 72, 64]]);
  }
}
