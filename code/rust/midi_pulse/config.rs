use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
  pub version: u8,
  pub id: String,
  pub title: String,
  #[serde(default)]
  pub tunings: Vec<TuningConfig>,
  #[serde(default)]
  pub monomes: Vec<MonomeConfig>,
  #[serde(default)]
  pub midi: Option<MidiConfig>,
  #[serde(default)]
  pub piano: Option<PianoConfig>,
  #[serde(default)]
  pub sinks: Vec<SinkConfig>,
  #[serde(default)]
  pub monome_windows: Vec<MonomeWindowConfig>,
  #[serde(default)]
  pub display: Option<DisplayConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TuningConfig {
  pub id: String,
  pub edo: i16,
  pub x_step: i16,
  pub y_step: i16,
  pub fundamental_hz: f64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MonomeConfig {
  pub id: String,
  pub listen_port: u16,
  pub prefix: String,
  #[serde(default)]
  pub select: MonomeSelect,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MonomeSelect {
  pub size: Option<[i32; 2]>,
  pub type_contains: Option<String>,
  pub id_contains: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MidiConfig {
  pub input: Option<MidiInputConfig>,
  pub output: MidiOutputConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MidiInputConfig {
  pub virtual_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MidiOutputConfig {
  pub virtual_name: String,
  pub min_channel: u8,
  pub min_note: u8,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PianoConfig {
  pub mapping: PianoMappingConfig,
  #[serde(default)]
  pub regions: Vec<PianoRegionConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PianoMappingConfig {
  TwelveN {
    lowest_note: u8,
    shift_before_mapping: i16,
    edo_per_12: i16,
  },
  RemappableUn12 {
    lowest_note: u8,
    tuning: String,
    remap_idiom: RemapIdiomConfig,
    initial_map: InitialMapConfig,
  },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RemapIdiomConfig {
  Loose,
  Snap,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InitialMapConfig {
  Even,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PianoRegionConfig {
  pub range: [u8; 2],
  pub action: PianoRegionActionConfig,
  pub zero_note: Option<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PianoRegionActionConfig {
  EmitNotes,
  HeldOffsetControl,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SinkConfig {
  Midi {
    id: String,
  },
  CpalSawwave {
    id: String,
    sample_rate: u32,
    buffer_frames: u32,
    amplitude: f32,
    attack_secs: f32,
    release_secs: f32,
    accretion_level: f32,
  },
}

impl SinkConfig {
  pub fn id(&self) -> &str {
    match self {
      SinkConfig::Midi { id } | SinkConfig::CpalSawwave { id, .. } => id,
    }
  }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonomeWindowConfig {
  EdoNoteGrid {
    id: String,
    monome: String,
    rect: [i32; 4],
    tuning: String,
    sink: String,
  },
  ChordWipeButton {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  ChordAccreteToggle {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  ChordEmitModeToggle {
    id: String,
    monome: String,
    rect: [i32; 4],
    emit_is_toggle_initially: bool,
  },
  ChordTargetButton {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  ChordSlotButtons {
    id: String,
    monome: String,
    rect: [i32; 4],
    initial_accretion_target: usize,
  },
  TwelveEdoOffsetBoard {
    id: String,
    monome: String,
    rect: [i32; 4],
    offset_columns: [i32; 2],
    offset_rows: [i32; 2],
    zero_row: i32,
    root_row: i32,
    group_columns: Vec<i32>,
    group_intervals: Vec<Vec<usize>>,
  },
  PreimageRow {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  RemappableUn12Grid {
    id: String,
    monome: String,
    rect: [i32; 4],
    tuning: String,
  },
  RemapUndoButton {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  RecordControl {
    id: String,
    monome: String,
    rect: [i32; 4],
    control: RecordControlKind,
  },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RecordControlKind {
  Start,
  Stop,
  Loop,
  Arm,
  EraseOns,
  EndAll,
  Rscm,
}

impl MonomeWindowConfig {
  pub fn id(&self) -> &str {
    match self {
      MonomeWindowConfig::EdoNoteGrid { id, .. }
      | MonomeWindowConfig::ChordWipeButton { id, .. }
      | MonomeWindowConfig::ChordAccreteToggle { id, .. }
      | MonomeWindowConfig::ChordEmitModeToggle { id, .. }
      | MonomeWindowConfig::ChordTargetButton { id, .. }
      | MonomeWindowConfig::ChordSlotButtons { id, .. }
      | MonomeWindowConfig::TwelveEdoOffsetBoard { id, .. }
      | MonomeWindowConfig::PreimageRow { id, .. }
      | MonomeWindowConfig::RemappableUn12Grid { id, .. }
      | MonomeWindowConfig::RemapUndoButton { id, .. }
      | MonomeWindowConfig::RecordControl { id, .. } => id,
    }
  }

  pub fn monome(&self) -> &str {
    match self {
      MonomeWindowConfig::EdoNoteGrid { monome, .. }
      | MonomeWindowConfig::ChordWipeButton { monome, .. }
      | MonomeWindowConfig::ChordAccreteToggle { monome, .. }
      | MonomeWindowConfig::ChordEmitModeToggle { monome, .. }
      | MonomeWindowConfig::ChordTargetButton { monome, .. }
      | MonomeWindowConfig::ChordSlotButtons { monome, .. }
      | MonomeWindowConfig::TwelveEdoOffsetBoard { monome, .. }
      | MonomeWindowConfig::PreimageRow { monome, .. }
      | MonomeWindowConfig::RemappableUn12Grid { monome, .. }
      | MonomeWindowConfig::RemapUndoButton { monome, .. }
      | MonomeWindowConfig::RecordControl { monome, .. } => monome,
    }
  }

  pub fn rect(&self) -> [i32; 4] {
    match self {
      MonomeWindowConfig::EdoNoteGrid { rect, .. }
      | MonomeWindowConfig::ChordWipeButton { rect, .. }
      | MonomeWindowConfig::ChordAccreteToggle { rect, .. }
      | MonomeWindowConfig::ChordEmitModeToggle { rect, .. }
      | MonomeWindowConfig::ChordTargetButton { rect, .. }
      | MonomeWindowConfig::ChordSlotButtons { rect, .. }
      | MonomeWindowConfig::TwelveEdoOffsetBoard { rect, .. }
      | MonomeWindowConfig::PreimageRow { rect, .. }
      | MonomeWindowConfig::RemappableUn12Grid { rect, .. }
      | MonomeWindowConfig::RemapUndoButton { rect, .. }
      | MonomeWindowConfig::RecordControl { rect, .. } => *rect,
    }
  }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DisplayConfig {
  PitchClassGrid {
    enabled: bool,
    rows: usize,
    cols: usize,
    anchor_12edo_value: usize,
    row_step: usize,
    flash_millis: u64,
    tick_millis: u64,
  },
}

pub fn config_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("code/rust/configs")
}

pub fn config_path(name: &str) -> Result<PathBuf, String> {
  if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
    return Err(format!("config name must not be a path, got {name:?}"));
  }
  Ok(config_dir().join(name).with_extension("toml"))
}

pub fn load_named_config(name: &str) -> Result<Config, String> {
  let path = config_path(name)?;
  load_config_file(&path)
}

pub fn load_config_file(path: &Path) -> Result<Config, String> {
  let source = std::fs::read_to_string(path)
    .map_err(|e| format!("read config {}: {e}", path.display()))?;
  parse_config(&source).map_err(|e| format!("parse config {}: {e}", path.display()))
}

pub fn parse_config(source: &str) -> Result<Config, String> {
  let config: Config = toml::from_str(source).map_err(|e| e.to_string())?;
  validate_config(&config)?;
  Ok(config)
}

pub fn validate_config(config: &Config) -> Result<(), String> {
  if config.version != 1 {
    return Err(format!("version must be 1, got {}", config.version));
  }
  require_unique("tuning", config.tunings.iter().map(|t| t.id.as_str()))?;
  require_unique("monome", config.monomes.iter().map(|m| m.id.as_str()))?;
  require_unique("sink", config.sinks.iter().map(SinkConfig::id))?;
  require_unique("monome window", config.monome_windows.iter().map(MonomeWindowConfig::id))?;
  require_unique(
    "monome listen_port",
    config.monomes.iter().map(|m| m.listen_port.to_string()),
  )?;

  let tuning_ids: HashSet<&str> = config.tunings.iter().map(|t| t.id.as_str()).collect();
  let monome_ids: HashSet<&str> = config.monomes.iter().map(|m| m.id.as_str()).collect();
  let sink_ids: HashSet<&str> = config.sinks.iter().map(SinkConfig::id).collect();

  for tuning in &config.tunings {
    if tuning.edo <= 0 {
      return Err(format!("tuning {:?} edo must be positive", tuning.id));
    }
    if !tuning.fundamental_hz.is_finite() || tuning.fundamental_hz <= 0.0 {
      return Err(format!("tuning {:?} fundamental_hz must be positive", tuning.id));
    }
  }

  if let Some(piano) = &config.piano {
    if let PianoMappingConfig::RemappableUn12 { tuning, .. } = &piano.mapping {
      require_ref("piano.mapping.tuning", tuning, &tuning_ids)?;
    }
    for region in &piano.regions {
      if region.range[0] > region.range[1] {
        return Err(format!("piano region range must be ascending: {:?}", region.range));
      }
      match region.action {
        PianoRegionActionConfig::EmitNotes => {
          if region.zero_note.is_some() {
            return Err("emit_notes piano region cannot set zero_note".to_string());
          }
        }
        PianoRegionActionConfig::HeldOffsetControl => {
          let Some(zero_note) = region.zero_note else {
            return Err("held_offset_control piano region requires zero_note".to_string());
          };
          if zero_note < region.range[0] || zero_note > region.range[1] {
            return Err(format!(
              "held_offset_control zero_note {zero_note} must be inside {:?}",
              region.range,
            ));
          }
        }
      }
    }
  }

  for sink in &config.sinks {
    if let SinkConfig::CpalSawwave {
      sample_rate,
      buffer_frames,
      amplitude,
      attack_secs,
      release_secs,
      accretion_level,
      ..
    } = sink {
      if *sample_rate == 0 || *buffer_frames == 0 {
        return Err(format!("sink {:?} sample_rate and buffer_frames must be positive", sink.id()));
      }
      for (name, value) in [
        ("amplitude", *amplitude),
        ("attack_secs", *attack_secs),
        ("release_secs", *release_secs),
        ("accretion_level", *accretion_level),
      ] {
        if !value.is_finite() || value < 0.0 {
          return Err(format!("sink {:?} {name} must be nonnegative", sink.id()));
        }
      }
    }
  }

  let mut record_control_kinds: HashSet<RecordControlKind> = HashSet::new();
  for window in &config.monome_windows {
    require_ref("monome_window.monome", window.monome(), &monome_ids)?;
    let [x0, y0, x1, y1] = window.rect();
    if x0 > x1 || y0 > y1 {
      return Err(format!("monome window {:?} rect must be ascending", window.id()));
    }
    match window {
      MonomeWindowConfig::EdoNoteGrid { tuning, sink, .. } => {
        require_ref("monome_window.tuning", tuning, &tuning_ids)?;
        require_ref("monome_window.sink", sink, &sink_ids)?;
      }
      MonomeWindowConfig::RemappableUn12Grid { tuning, .. } => {
        require_ref("monome_window.tuning", tuning, &tuning_ids)?;
      }
      MonomeWindowConfig::RecordControl { id, rect, control, .. } => {
        if rect[0] != rect[2] || rect[1] != rect[3] {
          return Err(format!(
            "record_control window {:?} rect must cover exactly one cell",
            id,
          ));
        }
        if !record_control_kinds.insert(*control) {
          return Err(format!(
            "duplicate record_control kind {:?} (in window {:?})",
            control, id,
          ));
        }
      }
      _ => {}
    }
  }

  Ok(())
}

fn require_unique<'a, I, S>(label: &str, values: I) -> Result<(), String>
where
  I: IntoIterator<Item = S>,
  S: Into<std::borrow::Cow<'a, str>>,
{
  let mut seen = HashSet::new();
  for value in values {
    let value = value.into();
    if value.is_empty() {
      return Err(format!("{label} id must not be empty"));
    }
    if !seen.insert(value.to_string()) {
      return Err(format!("duplicate {label} id {:?}", value));
    }
  }
  Ok(())
}

fn require_ref(label: &str, value: &str, valid: &HashSet<&str>) -> Result<(), String> {
  if valid.contains(value) {
    Ok(())
  } else {
    Err(format!("{label} references unknown id {value:?}"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn loads_default_configs() {
    for entry in std::fs::read_dir(config_dir()).expect("read configs dir") {
      let entry = entry.expect("config dir entry");
      let path = entry.path();
      if path.extension().and_then(|s| s.to_str()) == Some("toml") {
        load_config_file(&path)
          .unwrap_or_else(|e| panic!("{} should parse: {e}", path.display()));
      }
    }
  }

  #[test]
  fn rejects_mixed_piano_mapping_variants() {
    let err = parse_config(r#"
version = 1
id = "bad"
title = "Bad"

[midi.output]
virtual_name = "bad-out"
min_channel = 1
min_note = 28

[piano.mapping]
kind = "twelve_n"
lowest_note = 21
shift_before_mapping = -5
edo_per_12 = 6
remap_idiom = "snap"
"#).expect_err("mixed variant fields should fail");

    assert!(err.contains("unknown field") || err.contains("remap_idiom"), "{err}");
  }

  #[test]
  fn record_control_rect_must_be_single_cell() {
    let err = parse_config(r#"
version = 1
id = "bad-rect"
title = "Bad Rect"

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/256-1-cable"

[[monome_windows]]
id = "record-start"
monome = "big"
kind = "record_control"
control = "start"
rect = [13, 0, 14, 0]
"#).expect_err("multi-cell record_control rect should fail");

    assert!(err.contains("exactly one cell"), "{err}");
  }

  #[test]
  fn duplicate_record_control_kinds_are_rejected() {
    let err = parse_config(r#"
version = 1
id = "dup-kind"
title = "Dup Kind"

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/256-1-cable"

[[monome_windows]]
id = "record-start"
monome = "big"
kind = "record_control"
control = "start"
rect = [13, 0, 13, 0]

[[monome_windows]]
id = "record-start-again"
monome = "big"
kind = "record_control"
control = "start"
rect = [14, 0, 14, 0]
"#).expect_err("duplicate record_control kind should fail");

    assert!(err.contains("duplicate record_control kind"), "{err}");
  }

  #[test]
  fn tuning_main_is_a_reference_to_a_tuning_id() {
    let err = parse_config(r#"
version = 1
id = "bad-ref"
title = "Bad Ref"

[midi.output]
virtual_name = "bad-out"
min_channel = 1
min_note = 28

[piano.mapping]
kind = "remappable_un12"
lowest_note = 24
tuning = "main"
remap_idiom = "snap"
initial_map = "even"
"#).expect_err("missing tuning should fail");

    assert!(err.contains("unknown id \"main\""), "{err}");
  }
}
