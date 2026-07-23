use super::record::RecordControl;
use super::scale::ScaleControl;

#[derive(Clone)]
pub(crate) struct RemapRig {
  pub(crate) lowest_hz: f64,
  pub(crate) edo: i16,
  pub(crate) x_step: i16,
  pub(crate) y_step: i16,
  pub(crate) remap_idiom: RemapIdiom,
  pub(crate) grid_w: i32,
  pub(crate) grid_h: i32,
  pub(crate) initial_map: [i16; 12],
  // Cells for the seven record-control buttons, sourced from the TOML so the
  // rig is the single source of truth for which monome cells the runtime
  // owns. Tests use `default_record_controls()` to seed the canonical layout.
  pub(crate) record_controls: Vec<(RecordControl, (i32, i32))>,
  // The scale-slot grid rect [x0, y0, x1, y1] (inclusive), and the store/empty
  // arm-button cells, all sourced from the TOML. `None`/empty means the rig
  // has no scale-saving feature.
  pub(crate) scale_slots: Option<[i32; 4]>,
  pub(crate) scale_controls: Vec<(ScaleControl, (i32, i32))>,
  // A monobright grid (an old Series 256) can't dim a single LED, so rendering
  // fakes DIM with a fast flash. Stamped from the device id at discovery
  // (`monome::is_monobright`), like the grid size; false until then.
  pub(crate) monobright: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemapIdiom {
  Loose,
  Snap,
}

impl RemapRig {
  pub(crate) fn new(
    lowest_hz: f64,
    edo: i16,
    x_step: i16,
    y_step: i16,
    remap_idiom: RemapIdiom,
    grid_w: i32,
    grid_h: i32,
  ) -> Self {
    RemapRig {
      lowest_hz,
      edo,
      x_step,
      y_step,
      remap_idiom,
      grid_w,
      grid_h,
      initial_map: evenly_spaced_map(edo),
      record_controls: default_record_controls(),
      scale_slots: None,
      scale_controls: vec![],
      monobright: false,
    }
  }

  pub(crate) fn with_grid_size(&self, grid_w: i32, grid_h: i32) -> Self {
    RemapRig {
      lowest_hz: self.lowest_hz,
      edo: self.edo,
      x_step: self.x_step,
      y_step: self.y_step,
      remap_idiom: self.remap_idiom,
      grid_w,
      grid_h,
      initial_map: self.initial_map,
      record_controls: self.record_controls.clone(),
      scale_slots: self.scale_slots,
      scale_controls: self.scale_controls.clone(),
      monobright: self.monobright,
    }
  }

  pub(crate) fn with_monobright(mut self, monobright: bool) -> Self {
    self.monobright = monobright;
    self
  }

  pub(crate) fn with_record_controls(
    mut self,
    record_controls: Vec<(RecordControl, (i32, i32))>,
  ) -> Self {
    self.record_controls = record_controls;
    self
  }

  pub(crate) fn with_scale_slots(mut self, scale_slots: Option<[i32; 4]>) -> Self {
    self.scale_slots = scale_slots;
    self
  }

  pub(crate) fn with_scale_controls(
    mut self,
    scale_controls: Vec<(ScaleControl, (i32, i32))>,
  ) -> Self {
    self.scale_controls = scale_controls;
    self
  }

  /// Number of scale slots, derived from the `scale_slots` rect (row-major).
  pub(crate) fn scale_slot_count(&self) -> usize {
    match self.scale_slots {
      Some([x0, y0, x1, y1]) => {
        ((x1 - x0 + 1).max(0) * (y1 - y0 + 1).max(0)) as usize
      }
      None => 0,
    }
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
