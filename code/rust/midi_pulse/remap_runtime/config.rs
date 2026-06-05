use super::record::RecordControl;

#[derive(Clone)]
pub(crate) struct RemapConfig {
  pub(crate) lowest_hz: f64,
  pub(crate) edo: i16,
  pub(crate) x_step: i16,
  pub(crate) y_step: i16,
  pub(crate) remap_idiom: RemapIdiom,
  pub(crate) grid_w: i32,
  pub(crate) grid_h: i32,
  pub(crate) initial_map: [i16; 12],
  // Cells for the seven record-control buttons, sourced from the TOML so the
  // config is the single source of truth for which monome cells the runtime
  // owns. Tests use `default_record_controls()` to seed the canonical layout.
  pub(crate) record_controls: Vec<(RecordControl, (i32, i32))>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemapIdiom {
  Loose,
  Snap,
}

impl RemapConfig {
  pub(crate) fn new(
    lowest_hz: f64,
    edo: i16,
    x_step: i16,
    y_step: i16,
    remap_idiom: RemapIdiom,
    grid_w: i32,
    grid_h: i32,
  ) -> Self {
    RemapConfig {
      lowest_hz,
      edo,
      x_step,
      y_step,
      remap_idiom,
      grid_w,
      grid_h,
      initial_map: evenly_spaced_map(edo),
      record_controls: default_record_controls(),
    }
  }

  pub(crate) fn with_grid_size(&self, grid_w: i32, grid_h: i32) -> Self {
    RemapConfig {
      lowest_hz: self.lowest_hz,
      edo: self.edo,
      x_step: self.x_step,
      y_step: self.y_step,
      remap_idiom: self.remap_idiom,
      grid_w,
      grid_h,
      initial_map: self.initial_map,
      record_controls: self.record_controls.clone(),
    }
  }

  pub(crate) fn with_record_controls(
    mut self,
    record_controls: Vec<(RecordControl, (i32, i32))>,
  ) -> Self {
    self.record_controls = record_controls;
    self
  }
}

pub(crate) fn default_record_controls() -> Vec<(RecordControl, (i32, i32))> {
  vec![
    (RecordControl::Start, (13, 0)),
    (RecordControl::Stop, (14, 0)),
    (RecordControl::Loop, (15, 0)),
    (RecordControl::Arm, (13, 1)),
    (RecordControl::EraseOns, (14, 1)),
    (RecordControl::EndAll, (15, 1)),
    (RecordControl::Rscm, (15, 2)),
  ]
}

pub(crate) fn evenly_spaced_map(edo: i16) -> [i16; 12] {
  let mut map = [0; 12];
  for (i, slot) in map.iter_mut().enumerate() {
    *slot = ((i as f64 * edo as f64 / 12.0).round() as i16).rem_euclid(edo);
  }
  map
}
