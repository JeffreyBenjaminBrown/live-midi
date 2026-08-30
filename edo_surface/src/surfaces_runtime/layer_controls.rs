//! Per-grid control state for the three-layer monome instrument.

use super::grid::SELECTOR_CELLS;
use super::settings::DEFAULT_SLOT;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayerTarget {
  FingeredSustained,
  Chord,
  Loop,
}

#[derive(Clone, Copy, Debug)]
pub struct LayerVolumeConfig {
  pub coarse_db: f32,
  pub fine_db: f32,
  pub initial_db: f32,
  pub min_db: f32,
  pub max_db: f32,
}

impl Default for LayerVolumeConfig {
  fn default() -> Self {
    Self { coarse_db: 12.0, fine_db: 2.0, initial_db: -12.0, min_db: -60.0, max_db: 0.0 }
  }
}

#[derive(Clone, Debug)]
pub struct LayerControls {
  pub enabled: bool,
  target: LayerTarget,
  selected: [usize; 3],
  base_db: [f32; SELECTOR_CELLS],
  chord_delta_db: [f32; SELECTOR_CELLS],
  loop_delta_db: [f32; SELECTOR_CELLS],
  config: LayerVolumeConfig,
}

impl LayerControls {
  pub fn disabled() -> Self {
    Self::new(false, LayerVolumeConfig::default())
  }

  pub fn new(enabled: bool, config: LayerVolumeConfig) -> Self {
    Self {
      enabled,
      target: LayerTarget::FingeredSustained,
      selected: [DEFAULT_SLOT; 3],
      base_db: [config.initial_db; SELECTOR_CELLS],
      chord_delta_db: [0.0; SELECTOR_CELLS],
      loop_delta_db: [0.0; SELECTOR_CELLS],
      config,
    }
  }

  fn target_index(target: LayerTarget) -> usize {
    match target {
      LayerTarget::FingeredSustained => 0,
      LayerTarget::Chord => 1,
      LayerTarget::Loop => 2,
    }
  }

  pub fn target(&self) -> LayerTarget {
    self.target
  }

  pub fn cycle_target(&mut self) {
    self.target = match self.target {
      LayerTarget::FingeredSustained => LayerTarget::Chord,
      LayerTarget::Chord => LayerTarget::Loop,
      LayerTarget::Loop => LayerTarget::FingeredSustained,
    };
  }

  pub fn selected_for(&self, target: LayerTarget) -> usize {
    self.selected[Self::target_index(target)]
  }

  pub fn selected_target(&self) -> usize {
    self.selected_for(self.target)
  }

  pub fn set_selected_target(&mut self, slot: usize) {
    self.selected[Self::target_index(self.target)] = slot.min(SELECTOR_CELLS - 1);
  }

  pub fn effective_db(&self, target: LayerTarget, slot: usize) -> f32 {
    let base = self.base_db[slot];
    match target {
      LayerTarget::FingeredSustained => base,
      LayerTarget::Chord =>
        (base + self.chord_delta_db[slot]).clamp(self.config.min_db, self.config.max_db),
      LayerTarget::Loop =>
        (base + self.loop_delta_db[slot]).clamp(self.config.min_db, self.config.max_db),
    }
  }

  pub fn gain_for(&self, target: LayerTarget) -> f32 {
    db_to_gain(self.effective_db(target, self.selected_for(target)))
  }

  pub fn delta_for_cell(&self, cell: usize) -> f32 {
    match cell {
      0 => -self.config.coarse_db,
      1 => -self.config.fine_db,
      2 => self.config.fine_db,
      _ => self.config.coarse_db,
    }
  }

  /// Apply one relative button to the selected timbre of the current target.
  /// Returns that timbre slot; callers use it to decide whether the other layer
  /// currently shares the base that moved.
  pub fn apply_delta_cell(&mut self, cell: usize) -> usize {
    let slot = self.selected_target();
    let delta = self.delta_for_cell(cell);
    match self.target {
      LayerTarget::FingeredSustained => {
        self.base_db[slot] =
          (self.base_db[slot] + delta).clamp(self.config.min_db, self.config.max_db);
      }
      LayerTarget::Chord => {
        let before = self.effective_db(LayerTarget::Chord, slot);
        let after = (before + delta).clamp(self.config.min_db, self.config.max_db);
        // A prior BASE move may have pushed base+offset beyond a boundary while
        // deliberately retaining the hidden offset. An explicit CHORD edit must
        // nevertheless move audibly now, so re-anchor the offset to the requested
        // effective value instead of adding to that hidden overhang.
        self.chord_delta_db[slot] = after - self.base_db[slot];
      }
      LayerTarget::Loop => {
        let before = self.effective_db(LayerTarget::Loop, slot);
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
  fn bases_move_all_layers_but_layer_deltas_move_only_their_layer() {
    let mut s = LayerControls::new(true, LayerVolumeConfig::default());
    let slot = s.selected_target();
    assert_eq!(s.effective_db(LayerTarget::FingeredSustained, slot), -12.0);
    assert_eq!(s.effective_db(LayerTarget::Chord, slot), -12.0);
    assert_eq!(s.effective_db(LayerTarget::Loop, slot), -12.0);
    s.apply_delta_cell(1); // base -2
    assert_eq!(s.effective_db(LayerTarget::FingeredSustained, slot), -14.0);
    assert_eq!(s.effective_db(LayerTarget::Chord, slot), -14.0);
    assert_eq!(s.effective_db(LayerTarget::Loop, slot), -14.0);
    s.cycle_target();
    s.apply_delta_cell(3); // chord +12 only
    assert_eq!(s.effective_db(LayerTarget::FingeredSustained, slot), -14.0);
    assert_eq!(s.effective_db(LayerTarget::Chord, slot), -2.0);
    assert_eq!(s.effective_db(LayerTarget::Loop, slot), -14.0);
    s.cycle_target();
    s.apply_delta_cell(1); // loop -2 only
    assert_eq!(s.effective_db(LayerTarget::FingeredSustained, slot), -14.0);
    assert_eq!(s.effective_db(LayerTarget::Chord, slot), -2.0);
    assert_eq!(s.effective_db(LayerTarget::Loop, slot), -16.0);
  }

  #[test]
  fn volume_stays_inside_the_configured_bounds() {
    let mut s = LayerControls::new(true, LayerVolumeConfig::default());
    for _ in 0..20 {
      s.apply_delta_cell(0);
    }
    assert_eq!(s.effective_db(LayerTarget::FingeredSustained, DEFAULT_SLOT), -60.0);
    s.cycle_target();
    for _ in 0..20 {
      s.apply_delta_cell(3);
    }
    assert_eq!(s.effective_db(LayerTarget::Chord, DEFAULT_SLOT), 0.0);
  }

  #[test]
  fn each_layer_remembers_its_own_timbre() {
    let mut s = LayerControls::new(true, LayerVolumeConfig::default());
    s.set_selected_target(0);
    s.cycle_target();
    s.set_selected_target(3);
    assert_eq!(s.selected_for(LayerTarget::FingeredSustained), 0);
    assert_eq!(s.selected_for(LayerTarget::Chord), 3);
    s.cycle_target();
    s.set_selected_target(2);
    assert_eq!(s.selected_for(LayerTarget::Loop), 2);
  }

  #[test]
  fn values_are_isolated_between_timbres_and_chord_offset_survives_base_clamping() {
    let mut s = LayerControls::new(true, LayerVolumeConfig::default());
    s.set_selected_target(0);
    s.apply_delta_cell(1); // sine base -14
    s.cycle_target();
    s.set_selected_target(3);
    s.apply_delta_cell(3); // saw chord -12 -> 0
    assert_eq!(s.effective_db(LayerTarget::FingeredSustained, 0), -14.0);
    assert_eq!(s.effective_db(LayerTarget::Chord, 0), -14.0);
    assert_eq!(s.effective_db(LayerTarget::FingeredSustained, 3), -12.0);
    assert_eq!(s.effective_db(LayerTarget::Chord, 3), 0.0);

    s.cycle_target();
    s.cycle_target();
    s.set_selected_target(3);
    s.apply_delta_cell(0); // saw base clamps at -24; its +12 chord offset remains
    assert_eq!(s.effective_db(LayerTarget::FingeredSustained, 3), -24.0);
    assert_eq!(s.effective_db(LayerTarget::Chord, 3), -12.0);
  }

  #[test]
  fn a_chord_edit_moves_immediately_even_after_a_base_move_hides_offset_past_a_bound() {
    let mut s = LayerControls::new(true, LayerVolumeConfig::default());
    s.cycle_target();
    s.apply_delta_cell(3); // chord -12 -> 0: offset +12
    s.cycle_target();
    s.cycle_target();
    s.apply_delta_cell(3); // base -12 -> 0; retained offset is now hidden above max
    assert_eq!(s.effective_db(LayerTarget::Chord, DEFAULT_SLOT), 0.0);
    s.cycle_target();
    s.apply_delta_cell(1); // explicit chord -2 must be audible, not eaten by overhang
    assert_eq!(s.effective_db(LayerTarget::Chord, DEFAULT_SLOT), -2.0);
  }
}
