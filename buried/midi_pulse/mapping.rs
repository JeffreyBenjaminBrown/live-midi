use crate::rig::{InitialMapRig, PianoMappingRig, RemapIdiomRig, TuningRig};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PianoMapper {
  TwelveN(TwelveNMapping),
  RemappableUn12(RemappableUn12Mapping),
}

impl PianoMapper {
  pub fn from_rig(
    mapping: &PianoMappingRig,
    tunings: &[TuningRig],
    min_channel: u8,
    min_note: u8,
  ) -> Result<Self, String> {
    match mapping {
      PianoMappingRig::TwelveN {
        lowest_note,
        shift_before_mapping,
        edo_per_12,
      } => Ok(PianoMapper::TwelveN(TwelveNMapping::new(
        *lowest_note,
        *shift_before_mapping,
        min_channel,
        min_note,
        *edo_per_12,
      ))),
      PianoMappingRig::RemappableUn12 {
        lowest_note,
        tuning,
        remap_idiom,
        initial_map,
      } => {
        let tuning = tunings
          .iter()
          .find(|candidate| candidate.id == *tuning)
          .ok_or_else(|| format!("unknown tuning id {tuning:?}"))?;
        Ok(PianoMapper::RemappableUn12(RemappableUn12Mapping::new(
          *lowest_note,
          min_channel,
          min_note,
          tuning.edo,
          *remap_idiom,
          *initial_map,
        )))
      }
    }
  }

  pub fn instruction(&self, original_note: u8) -> (i16, i16) {
    match self {
      PianoMapper::TwelveN(mapping) => mapping.instruction(original_note),
      PianoMapper::RemappableUn12(mapping) => mapping.instruction(original_note),
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TwelveNMapping {
  lowest_note: u8,
  shift_before_mapping: i16,
  min_channel: u8,
  min_note: u8,
  edo_per_12: i16,
  pitch_class_shifts: [i16; 12],
}

impl TwelveNMapping {
  pub fn new(
    lowest_note: u8,
    shift_before_mapping: i16,
    min_channel: u8,
    min_note: u8,
    edo_per_12: i16,
  ) -> Self {
    TwelveNMapping {
      lowest_note,
      shift_before_mapping,
      min_channel,
      min_note,
      edo_per_12,
      pitch_class_shifts: [0; 12],
    }
  }

  pub fn set_pitch_class_shift(&mut self, pitch_class: u8, shift: i16) {
    self.pitch_class_shifts[(pitch_class % 12) as usize] = shift;
  }

  pub fn instruction(&self, original_note: u8) -> (i16, i16) {
    let normalized =
      original_note as i16 - self.lowest_note as i16 + self.shift_before_mapping;
    let channel_offset = normalized.div_euclid(12);
    let note_offset = normalized.rem_euclid(12);
    let channel = self.min_channel as i16 + channel_offset;
    let pitch_class = (original_note % 12) as usize;
    let note = self.min_note as i16
      + note_offset * self.edo_per_12
      + self.pitch_class_shifts[pitch_class];
    (channel, note)
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemappableUn12Mapping {
  lowest_note: u8,
  min_channel: u8,
  min_note: u8,
  edo: i16,
  remap_idiom: RemapIdiomRig,
  map: [i16; 12],
}

impl RemappableUn12Mapping {
  pub fn new(
    lowest_note: u8,
    min_channel: u8,
    min_note: u8,
    edo: i16,
    remap_idiom: RemapIdiomRig,
    initial_map: InitialMapRig,
  ) -> Self {
    let map = match initial_map {
      InitialMapRig::Even => evenly_spaced_map(edo),
    };
    RemappableUn12Mapping {
      lowest_note,
      min_channel,
      min_note,
      edo,
      remap_idiom,
      map,
    }
  }

  pub fn map(&self) -> [i16; 12] {
    self.map
  }

  pub fn remap_idiom(&self) -> RemapIdiomRig {
    self.remap_idiom
  }

  pub fn instruction(&self, original_note: u8) -> (i16, i16) {
    let normalized = original_note as i16 - self.lowest_note as i16;
    let octave = normalized.div_euclid(12);
    let preimage = normalized.rem_euclid(12) as usize;
    let pitch = self.map[preimage] + octave * self.edo;
    let channel_offset = pitch.div_euclid(self.edo);
    let note_offset = pitch.rem_euclid(self.edo);
    (
      self.min_channel as i16 + channel_offset,
      self.min_note as i16 + note_offset,
    )
  }
}

pub fn evenly_spaced_map(edo: i16) -> [i16; 12] {
  let mut map = [0; 12];
  for (i, slot) in map.iter_mut().enumerate() {
    *slot = ((i as f64 * edo as f64 / 12.0).round() as i16).rem_euclid(edo);
  }
  map
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn twelve_n_preserves_current_72_edo_mapping() {
    let mapping = TwelveNMapping::new(21, -5, 1, 28, 6);

    assert_eq!(mapping.instruction(21), (0, 70));
    assert_eq!(mapping.instruction(26), (1, 28));
    assert_eq!(mapping.instruction(33), (1, 70));
  }

  #[test]
  fn twelve_n_applies_pitch_class_shift() {
    let mut mapping = TwelveNMapping::new(21, -5, 1, 28, 6);
    mapping.set_pitch_class_shift(9, 2);

    assert_eq!(mapping.instruction(21), (0, 72));
  }

  #[test]
  fn evenly_spaced_31_map_matches_existing_default() {
    assert_eq!(evenly_spaced_map(31), [0, 3, 5, 8, 10, 13, 16, 18, 21, 23, 26, 28]);
  }

  #[test]
  fn remappable_un12_uses_even_map() {
    let mapping = RemappableUn12Mapping::new(
      24,
      1,
      28,
      31,
      RemapIdiomRig::Snap,
      InitialMapRig::Even,
    );

    assert_eq!(mapping.map(), [0, 3, 5, 8, 10, 13, 16, 18, 21, 23, 26, 28]);
    assert_eq!(mapping.instruction(24), (1, 28));
    assert_eq!(mapping.instruction(36), (2, 28));
  }
}
