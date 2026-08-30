//! Per-grid tone-target state for the reduced monome instrument.

use crate::types::VoiceSource;

use super::grid::SELECTOR_CELLS;
use super::settings::DEFAULT_SLOT;

/// Selects which voices receive a tone edit. Here "tone" means the combination
/// of timbre and volume; the target is a control group, not a `VoiceSource`
/// identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToneTarget {
  FingeredSustained,
  Chord,
  Loop,
}

impl ToneTarget {
  /// Whether a surfaces-runtime source belongs to this tone target on `grid`.
  /// Fingered/sustained contains fingers, drones, and their retired tails;
  /// callers exclude release tails when an edit should affect only live voices.
  pub fn matches_source(self, source: &VoiceSource, grid: usize) -> bool {
    match self {
      ToneTarget::FingeredSustained => matches!(
        source,
        VoiceSource::SurfaceFinger { grid: source_grid, .. }
          | VoiceSource::SurfaceDrone { grid: source_grid, .. }
          | VoiceSource::SurfaceRetired { grid: source_grid, .. }
          if *source_grid == grid
      ),
      ToneTarget::Chord => matches!(
        source,
        VoiceSource::SurfaceChord { grid: source_grid, .. } if *source_grid == grid
      ),
      ToneTarget::Loop => matches!(
        source,
        VoiceSource::SurfaceLoop { grid: source_grid, .. } if *source_grid == grid
      ),
    }
  }
}

#[derive(Clone, Copy, Debug)]
pub struct ToneVolumeConfig {
  pub coarse_db: f32,
  pub fine_db: f32,
  pub initial_db: f32,
  pub min_db: f32,
  pub max_db: f32,
}

impl Default for ToneVolumeConfig {
  fn default() -> Self {
    Self { coarse_db: 12.0, fine_db: 2.0, initial_db: -12.0, min_db: -60.0, max_db: 0.0 }
  }
}

#[derive(Clone, Debug)]
pub struct ToneControls {
  pub enabled: bool,
  tone_target: ToneTarget,
  selected: [usize; 3],
  base_db: [f32; SELECTOR_CELLS],
  chord_delta_db: [f32; SELECTOR_CELLS],
  loop_delta_db: [f32; SELECTOR_CELLS],
  config: ToneVolumeConfig,
}

impl ToneControls {
  pub fn disabled() -> Self {
    Self::new(false, ToneVolumeConfig::default())
  }

  pub fn new(enabled: bool, config: ToneVolumeConfig) -> Self {
    Self {
      enabled,
      tone_target: ToneTarget::FingeredSustained,
      selected: [DEFAULT_SLOT; 3],
      base_db: [config.initial_db; SELECTOR_CELLS],
      chord_delta_db: [0.0; SELECTOR_CELLS],
      loop_delta_db: [0.0; SELECTOR_CELLS],
      config,
    }
  }

  fn tone_target_index(tone_target: ToneTarget) -> usize {
    match tone_target {
      ToneTarget::FingeredSustained => 0,
      ToneTarget::Chord => 1,
      ToneTarget::Loop => 2,
    }
  }

  pub fn tone_target(&self) -> ToneTarget {
    self.tone_target
  }

  pub fn cycle_tone_target(&mut self) {
    self.tone_target = match self.tone_target {
      ToneTarget::FingeredSustained => ToneTarget::Chord,
      ToneTarget::Chord => ToneTarget::Loop,
      ToneTarget::Loop => ToneTarget::FingeredSustained,
    };
  }

  pub fn selected_for(&self, tone_target: ToneTarget) -> usize {
    self.selected[Self::tone_target_index(tone_target)]
  }

  pub fn selected_for_tone_target(&self) -> usize {
    self.selected_for(self.tone_target)
  }

  pub fn set_selected_for_tone_target(&mut self, slot: usize) {
    self.selected[Self::tone_target_index(self.tone_target)] = slot.min(SELECTOR_CELLS - 1);
  }

  pub fn effective_db(&self, tone_target: ToneTarget, slot: usize) -> f32 {
    let base = self.base_db[slot];
    match tone_target {
      ToneTarget::FingeredSustained => base,
      ToneTarget::Chord =>
        (base + self.chord_delta_db[slot]).clamp(self.config.min_db, self.config.max_db),
      ToneTarget::Loop =>
        (base + self.loop_delta_db[slot]).clamp(self.config.min_db, self.config.max_db),
    }
  }

  pub fn gain_for(&self, tone_target: ToneTarget) -> f32 {
    db_to_gain(self.effective_db(tone_target, self.selected_for(tone_target)))
  }

  pub fn delta_for_cell(&self, cell: usize) -> f32 {
    match cell {
      0 => -self.config.coarse_db,
      1 => -self.config.fine_db,
      2 => self.config.fine_db,
      _ => self.config.coarse_db,
    }
  }

  /// Apply one relative button to the selected timbre of the current tone target.
  /// Returns that timbre slot; callers use it to decide whether another tone target
  /// currently shares the base that moved.
  pub fn apply_delta_cell(&mut self, cell: usize) -> usize {
    let slot = self.selected_for_tone_target();
    let delta = self.delta_for_cell(cell);
    match self.tone_target {
      ToneTarget::FingeredSustained => {
        self.base_db[slot] =
          (self.base_db[slot] + delta).clamp(self.config.min_db, self.config.max_db);
      }
      ToneTarget::Chord => {
        let before = self.effective_db(ToneTarget::Chord, slot);
        let after = (before + delta).clamp(self.config.min_db, self.config.max_db);
        // A prior BASE move may have pushed base+offset beyond a boundary while
        // deliberately retaining the hidden offset. An explicit CHORD edit must
        // nevertheless move audibly now, so re-anchor the offset to the requested
        // effective value instead of adding to that hidden overhang.
        self.chord_delta_db[slot] = after - self.base_db[slot];
      }
      ToneTarget::Loop => {
        let before = self.effective_db(ToneTarget::Loop, slot);
        let after = (before + delta).clamp(self.config.min_db, self.config.max_db);
        self.loop_delta_db[slot] = after - self.base_db[slot];
      }
    }
    slot
  }
}

pub fn db_to_gain(db: f32) -> f32 {
  10.0_f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bases_move_all_tone_targets_but_offsets_move_only_their_tone_target() {
    let mut s = ToneControls::new(true, ToneVolumeConfig::default());
    let slot = s.selected_for_tone_target();
    assert_eq!(s.effective_db(ToneTarget::FingeredSustained, slot), -12.0);
    assert_eq!(s.effective_db(ToneTarget::Chord, slot), -12.0);
    assert_eq!(s.effective_db(ToneTarget::Loop, slot), -12.0);
    s.apply_delta_cell(1); // base -2
    assert_eq!(s.effective_db(ToneTarget::FingeredSustained, slot), -14.0);
    assert_eq!(s.effective_db(ToneTarget::Chord, slot), -14.0);
    assert_eq!(s.effective_db(ToneTarget::Loop, slot), -14.0);
    s.cycle_tone_target();
    s.apply_delta_cell(3); // chord +12 only
    assert_eq!(s.effective_db(ToneTarget::FingeredSustained, slot), -14.0);
    assert_eq!(s.effective_db(ToneTarget::Chord, slot), -2.0);
    assert_eq!(s.effective_db(ToneTarget::Loop, slot), -14.0);
    s.cycle_tone_target();
    s.apply_delta_cell(1); // loop -2 only
    assert_eq!(s.effective_db(ToneTarget::FingeredSustained, slot), -14.0);
    assert_eq!(s.effective_db(ToneTarget::Chord, slot), -2.0);
    assert_eq!(s.effective_db(ToneTarget::Loop, slot), -16.0);
  }

  #[test]
  fn tone_targets_classify_every_surface_voice_source() {
    let finger = VoiceSource::SurfaceFinger { grid: 2, cell: (3, 4) };
    let drone = VoiceSource::SurfaceDrone { grid: 2, pitch: 5 };
    let retired = VoiceSource::SurfaceRetired { grid: 2, seq: 6 };
    let chord = VoiceSource::SurfaceChord { grid: 2, seq: 7 };
    let loop_voice = VoiceSource::SurfaceLoop { grid: 2, seq: 8 };

    for source in [&finger, &drone, &retired] {
      assert!(ToneTarget::FingeredSustained.matches_source(source, 2));
      assert!(!ToneTarget::FingeredSustained.matches_source(source, 1));
    }
    assert!(ToneTarget::Chord.matches_source(&chord, 2));
    assert!(ToneTarget::Loop.matches_source(&loop_voice, 2));
    assert!(!ToneTarget::Chord.matches_source(&loop_voice, 2));
    assert!(!ToneTarget::Loop.matches_source(&chord, 2));
  }

  #[test]
  fn volume_stays_inside_the_configured_bounds() {
    let mut s = ToneControls::new(true, ToneVolumeConfig::default());
    for _ in 0..20 {
      s.apply_delta_cell(0);
    }
    assert_eq!(s.effective_db(ToneTarget::FingeredSustained, DEFAULT_SLOT), -60.0);
    s.cycle_tone_target();
    for _ in 0..20 {
      s.apply_delta_cell(3);
    }
    assert_eq!(s.effective_db(ToneTarget::Chord, DEFAULT_SLOT), 0.0);
  }

  #[test]
  fn each_tone_target_remembers_its_own_timbre() {
    let mut s = ToneControls::new(true, ToneVolumeConfig::default());
    s.set_selected_for_tone_target(0);
    s.cycle_tone_target();
    s.set_selected_for_tone_target(3);
    assert_eq!(s.selected_for(ToneTarget::FingeredSustained), 0);
    assert_eq!(s.selected_for(ToneTarget::Chord), 3);
    s.cycle_tone_target();
    s.set_selected_for_tone_target(2);
    assert_eq!(s.selected_for(ToneTarget::Loop), 2);
  }

  #[test]
  fn values_are_isolated_between_timbres_and_chord_offset_survives_base_clamping() {
    let mut s = ToneControls::new(true, ToneVolumeConfig::default());
    s.set_selected_for_tone_target(0);
    s.apply_delta_cell(1); // sine base -14
    s.cycle_tone_target();
    s.set_selected_for_tone_target(3);
    s.apply_delta_cell(3); // saw chord -12 -> 0
    assert_eq!(s.effective_db(ToneTarget::FingeredSustained, 0), -14.0);
    assert_eq!(s.effective_db(ToneTarget::Chord, 0), -14.0);
    assert_eq!(s.effective_db(ToneTarget::FingeredSustained, 3), -12.0);
    assert_eq!(s.effective_db(ToneTarget::Chord, 3), 0.0);

    s.cycle_tone_target();
    s.cycle_tone_target();
    s.set_selected_for_tone_target(3);
    s.apply_delta_cell(0); // saw base clamps at -24; its +12 chord offset remains
    assert_eq!(s.effective_db(ToneTarget::FingeredSustained, 3), -24.0);
    assert_eq!(s.effective_db(ToneTarget::Chord, 3), -12.0);
  }

  #[test]
  fn a_chord_edit_moves_immediately_even_after_a_base_move_hides_offset_past_a_bound() {
    let mut s = ToneControls::new(true, ToneVolumeConfig::default());
    s.cycle_tone_target();
    s.apply_delta_cell(3); // chord -12 -> 0: offset +12
    s.cycle_tone_target();
    s.cycle_tone_target();
    s.apply_delta_cell(3); // base -12 -> 0; retained offset is now hidden above max
    assert_eq!(s.effective_db(ToneTarget::Chord, DEFAULT_SLOT), 0.0);
    s.cycle_tone_target();
    s.apply_delta_cell(1); // explicit chord -2 must be audible, not eaten by overhang
    assert_eq!(s.effective_db(ToneTarget::Chord, DEFAULT_SLOT), -2.0);
  }
}
