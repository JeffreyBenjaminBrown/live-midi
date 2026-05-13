use serde::Deserialize;
use std::path::PathBuf;

use crate::{
  CONFIGS_DIR, DEFAULT_EDO, DEFAULT_GRID_H, DEFAULT_GRID_W, DEFAULT_LOWEST_HZ, DEFAULT_X_STEP,
  DEFAULT_Y_STEP, LISTEN_PORT, LISTEN_PORT_ENV,
};

#[derive(Clone)]
pub(crate) struct EdoConfig {
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

impl EdoConfig {
  pub(crate) fn default() -> Self {
    EdoConfig::new(
      DEFAULT_LOWEST_HZ,
      DEFAULT_EDO,
      DEFAULT_X_STEP,
      DEFAULT_Y_STEP,
      RemapIdiom::Snap,
      DEFAULT_GRID_W,
      DEFAULT_GRID_H,
    )
  }

  pub(crate) fn new(
    lowest_hz: f64,
    edo: i16,
    x_step: i16,
    y_step: i16,
    remap_idiom: RemapIdiom,
    grid_w: i32,
    grid_h: i32,
  ) -> Self {
    EdoConfig {
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
    EdoConfig {
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

pub(crate) fn parse_config() -> Result<EdoConfig, Box<dyn std::error::Error>> {
  let args: Vec<String> = std::env::args().skip(1).collect();
  if args.len() > 1 {
    return Err("usage: edo31_piano_monome [CONFIGS_FILE]".into());
  }
  let name = args.first().map(String::as_str).unwrap_or("default");
  load_config(name)
}

#[derive(Deserialize)]
struct ConfigToml {
  lowest_hz: f64,
  edo: i16,
  between_columns: i16,
  between_rows: i16,
  #[serde(default)]
  remap_idiom: ConfigRemapIdiom,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConfigRemapIdiom {
  Snap,
  Loose,
  Loosen,
}

impl Default for ConfigRemapIdiom {
  fn default() -> Self {
    ConfigRemapIdiom::Snap
  }
}

impl ConfigRemapIdiom {
  fn into_runtime(self) -> RemapIdiom {
    match self {
      ConfigRemapIdiom::Snap => RemapIdiom::Snap,
      ConfigRemapIdiom::Loose | ConfigRemapIdiom::Loosen => RemapIdiom::Loose,
    }
  }
}

pub(crate) fn load_config(name: &str) -> Result<EdoConfig, Box<dyn std::error::Error>> {
  if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
    return Err(format!("config file name must not be a path, got {name:?}").into());
  }
  let path = config_path(name);
  let source = std::fs::read_to_string(&path)
    .or_else(|original_error| {
      if path.extension().is_none() {
        let toml_path = path.with_extension("toml");
        std::fs::read_to_string(&toml_path).map_err(|_| original_error)
      } else {
        Err(original_error)
      }
    })
    .map_err(|e| format!("read config {name:?}: {e}"))?;
  let parsed: ConfigToml =
    toml::from_str(&source).map_err(|e| format!("parse config {name:?}: {e}"))?;
  validate_config_toml(&parsed)?;
  Ok(EdoConfig::new(
    parsed.lowest_hz,
    parsed.edo,
    parsed.between_columns,
    parsed.between_rows,
    parsed.remap_idiom.into_runtime(),
    DEFAULT_GRID_W,
    DEFAULT_GRID_H,
  ))
}

fn config_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join(CONFIGS_DIR)
    .join(name)
}

fn validate_config_toml(config: &ConfigToml) -> Result<(), Box<dyn std::error::Error>> {
  if !config.lowest_hz.is_finite() || config.lowest_hz <= 0.0 {
    return Err(format!("lowest_hz must be positive, got {}", config.lowest_hz).into());
  }
  if config.edo <= 0 {
    return Err(format!("edo must be positive, got {}", config.edo).into());
  }
  Ok(())
}

pub(crate) fn configured_listen_port() -> Result<u16, Box<dyn std::error::Error>> {
  let Some(port) = std::env::var_os(LISTEN_PORT_ENV) else {
    return Ok(LISTEN_PORT);
  };
  let port = port.to_string_lossy();
  let value = port
    .parse::<u16>()
    .map_err(|_| format!("{LISTEN_PORT_ENV} must be a UDP port, got {port:?}"))?;
  if value == 0 {
    return Err(format!("{LISTEN_PORT_ENV} must be nonzero").into());
  }
  Ok(value)
}

pub(crate) fn evenly_spaced_map(edo: i16) -> [i16; 12] {
  let mut map = [0; 12];
  for (i, slot) in map.iter_mut().enumerate() {
    *slot = ((i as f64 * edo as f64 / 12.0).round() as i16).rem_euclid(edo);
  }
  map
}

pub(crate) fn remap_idiom_name(remap_idiom: RemapIdiom) -> &'static str {
  match remap_idiom {
    RemapIdiom::Loose => "loose",
    RemapIdiom::Snap => "snap",
  }
}
