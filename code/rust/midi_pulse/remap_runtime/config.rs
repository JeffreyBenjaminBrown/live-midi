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
    }
  }
}

pub(crate) fn evenly_spaced_map(edo: i16) -> [i16; 12] {
  let mut map = [0; 12];
  for (i, slot) in map.iter_mut().enumerate() {
    *slot = ((i as f64 * edo as f64 / 12.0).round() as i16).rem_euclid(edo);
  }
  map
}
