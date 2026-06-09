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
  #[serde(default)]
  pub looper: Option<LooperConfig>,
  /// Instrument-wide AM settings (6_plan 5). Today only the shape family lives here;
  /// it is the config-level morph family the per-note `am.shape` value sweeps within.
  #[serde(default)]
  pub am: Option<AmConfig>,
}

/// `[am]` table: instrument-wide amplitude-modulation settings. The shape *family*
/// is config-level (one per instrument, 6_plan 2.5 / D 3c); the per-note `am.shape`
/// value (set by the editor's shape row) is the morph position within it.
#[derive(Clone, Copy, Debug, Deserialize, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AmConfig {
  #[serde(default)]
  pub shape: AmShapeConfig,
}

#[derive(Clone, Copy, Debug, Deserialize, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AmShapeConfig {
  #[serde(default)]
  pub family: AmShapeFamilyConfig,
}

/// Mirrors the runtime `sawwave::types::AmShapeFamily` (which the lib cannot see);
/// `resolve_settings` converts. Default = `sin_to_square`, matching the runtime.
#[derive(Clone, Copy, Debug, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AmShapeFamilyConfig {
  #[default]
  SinToSquare,
  TriToSquare,
}

/// Which timbre a `timbre_editor` window edits (6_plan 2.2). In C3b both edit the
/// live timbre; C7 makes `loop` edit the active loop's per-note timbre.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TimbreTarget {
  Live,
  Loop,
}

/// A parameter row's value<->cell mapping, declared per row in a `timbre_editor`
/// window (6_plan 2.6). Mirrors the runtime `timbre_rows::RowRange` (in the binary);
/// `resolve_settings` converts. Holds f32s, so it is `PartialEq` but not `Eq`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RowRangeConfig {
  /// value = lerp(min, max) across the row.
  Linear { min: f32, max: f32 },
  /// value(k) = least * multiplier^k -- the top grows with the row width.
  LogFactor { least: f32, multiplier: f32 },
  /// value spans [least, greatest] at any width -- the step count scales instead.
  LogRange { least: f32, greatest: f32 },
}

// === Relative window coordinates (6_plan 4.1) ===========================

/// One side of a rect. `Top`/`Bottom` are y edges; `Left`/`Right` are x edges.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeName {
  Top,
  Bottom,
  Left,
  Right,
}

/// A reference to another window's edge, plus an offset: `live-timbre.bottom + 1`.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeRef {
  pub target: String,
  pub edge: EdgeName,
  pub offset: i32,
}

/// A single edge, after deserialization: an absolute coordinate (negative counts
/// from the far edge; -1 = last row/col) or a reference to another window's edge.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedEdge {
  Absolute(i32),
  Ref(EdgeRef),
}

/// One edge in config: an integer (absolute) or a `"id.edge +/- n"` string.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum EdgeSpecConfig {
  Absolute(i32),
  Expr(String),
}

/// A window rect (6_plan 4.1): the legacy whole-rect array `[x0,y0,x1,y1]` (all
/// absolute), or a per-edge table whose edges may be absolute or relative. Both
/// forms parse, so absolute configs are unchanged.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RectSpecConfig {
  Absolute([i32; 4]),
  PerEdge {
    top: EdgeSpecConfig,
    bottom: EdgeSpecConfig,
    left: EdgeSpecConfig,
    right: EdgeSpecConfig,
  },
}

/// Parse a relative-edge expression `"<id>.<edge> [+/- n]"`, e.g.
/// `"live-timbre.bottom + 1"` or `"grid.left"`. Whitespace around the offset is
/// optional. The id may contain hyphens (split on the first dot).
pub fn parse_edge_expr(s: &str) -> Result<EdgeRef, String> {
  let s = s.trim();
  let dot = s
    .find('.')
    .ok_or_else(|| format!("edge expr {s:?} must be \"<id>.<edge> [+/- n]\""))?;
  let target = s[..dot].trim();
  if target.is_empty() {
    return Err(format!("edge expr {s:?} is missing a window id"));
  }
  let rest = s[dot + 1..].trim();
  let edge_len = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
  let edge = match &rest[..edge_len] {
    "top" => EdgeName::Top,
    "bottom" => EdgeName::Bottom,
    "left" => EdgeName::Left,
    "right" => EdgeName::Right,
    other => return Err(format!("edge expr {s:?} has unknown edge {other:?}")),
  };
  let offset_str: String = rest[edge_len..].chars().filter(|c| !c.is_whitespace()).collect();
  let offset = if offset_str.is_empty() {
    0
  } else {
    offset_str
      .parse::<i32>()
      .map_err(|_| format!("edge expr {s:?} has a non-integer offset {offset_str:?}"))?
  };
  Ok(EdgeRef { target: target.to_string(), edge, offset })
}

impl RectSpecConfig {
  /// The given edge as Absolute(int) or Ref(other window's edge). The array form
  /// maps `[x0,y0,x1,y1]` to left/top/right/bottom.
  pub fn resolved_edge(&self, which: EdgeName) -> Result<ResolvedEdge, String> {
    match self {
      RectSpecConfig::Absolute(a) => Ok(ResolvedEdge::Absolute(match which {
        EdgeName::Left => a[0],
        EdgeName::Top => a[1],
        EdgeName::Right => a[2],
        EdgeName::Bottom => a[3],
      })),
      RectSpecConfig::PerEdge { top, bottom, left, right } => {
        let e = match which {
          EdgeName::Top => top,
          EdgeName::Bottom => bottom,
          EdgeName::Left => left,
          EdgeName::Right => right,
        };
        match e {
          EdgeSpecConfig::Absolute(v) => Ok(ResolvedEdge::Absolute(*v)),
          EdgeSpecConfig::Expr(s) => Ok(ResolvedEdge::Ref(parse_edge_expr(s)?)),
        }
      }
    }
  }

  /// Validate that every edge parses (absolute or a well-formed reference). Cross-
  /// window reference targets and cycles are checked at resolve time (the runtime).
  pub fn validate(&self) -> Result<(), String> {
    for which in [EdgeName::Top, EdgeName::Bottom, EdgeName::Left, EdgeName::Right] {
      self.resolved_edge(which)?;
    }
    Ok(())
  }
}

/// Looper-wide scalars (see the loop windows below). All durations in
/// milliseconds. Present iff the config declares the looper windows.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LooperConfig {
  /// Events within this of a loop boundary snap to it (to the next pass start).
  pub quantize_record_ms: u64,
  /// Note-ons within this of each other display as one column of the loop.
  pub cluster_display_ms: u64,
  /// Reflection / octave-equivalent flash half-period (on for this, off for this).
  pub flash_ms: u64,
  /// The edo-grid cell that means "unison" in group-transpose remap.
  pub remap_center: [i32; 2],
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
  CpalSynth {
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
      SinkConfig::Midi { id } | SinkConfig::CpalSynth { id, .. } => id,
    }
  }
}

// Not `Eq`: the `TimbreEditor` variant carries f32 row ranges (like `LoopEvent`
// dropped `Eq` for its f32 timbre in C6).
#[derive(Clone, Debug, Deserialize, PartialEq)]
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
  // A flexible grid of scale slots. The number of slots is whatever the rect
  // covers, so the config alone decides how many scales can be saved.
  ScaleSlots {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // A single-cell arm button for the scale-saving feature (store/empty).
  ScaleControl {
    id: String,
    monome: String,
    rect: [i32; 4],
    control: ScaleControlKind,
  },
  // ---- Looper windows (see 6_plan.org). ----
  // The 3x2 shift pad overlaid on the lower-right of the edo grid.
  EdoShiftPad {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // The grid of loop slots; one slot is active. The number of slots is whatever
  // the rect covers.
  LoopSlots {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // A single-cell transport button acting on the active slot (start/stop/play).
  LoopControl {
    id: String,
    monome: String,
    rect: [i32; 4],
    control: LoopControlKind,
  },
  // Toggles fine vs group-transpose loop-remap.
  LoopRemapModeToggle {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // Copy a loop slot to another (press, then 'from', then 'to').
  LoopCopyButton {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // One-press undo of the last loop remap. Its own kind (NOT remap_undo_button,
  // which is entangled in the all-or-nothing "remap" window group); it shares the
  // undo *mechanism* in code, not the window kind.
  LoopRemapUndo {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // The 2D loop display compound: one rect derives a row-picker column, a
  // column-picker row, a reserved corner, and the main time x pitch area.
  LoopDisplay {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // The on-grid timbre editor (6_plan 2.6/2.7). One absolute rect (~7 rows tall),
  // a stack of radio parameter rows. `target` picks live vs loop editing. Each
  // value row's range is configurable; omitted rows fall back to 6_plan 5 defaults.
  TimbreEditor {
    id: String,
    monome: String,
    rect: [i32; 4],
    target: TimbreTarget,
    #[serde(default = "default_amplitude_row")]
    amplitude: RowRangeConfig,
    #[serde(default = "default_am_amplitude_row")]
    am_amplitude: RowRangeConfig,
    #[serde(default = "default_am_frequency_row")]
    am_frequency: RowRangeConfig,
    #[serde(default = "default_am_shape_row")]
    am_shape: RowRangeConfig,
    #[serde(default = "default_fm_amplitude_row")]
    fm_amplitude: RowRangeConfig,
    #[serde(default = "default_fm_frequency_row")]
    fm_frequency: RowRangeConfig,
  },
}

// Per-row default ranges (6_plan 5). Used when a `timbre_editor` omits a row.
fn default_amplitude_row() -> RowRangeConfig {
  RowRangeConfig::LogRange { least: 0.0009, greatest: 0.15 }
}
fn default_am_amplitude_row() -> RowRangeConfig {
  RowRangeConfig::Linear { min: 0.0, max: 1.0 }
}
fn default_am_frequency_row() -> RowRangeConfig {
  RowRangeConfig::LogFactor { least: 0.25, multiplier: 2.0 }
}
fn default_am_shape_row() -> RowRangeConfig {
  RowRangeConfig::Linear { min: 0.0, max: 1.0 }
}
fn default_fm_amplitude_row() -> RowRangeConfig {
  RowRangeConfig::LogFactor { least: 5.0, multiplier: 2.0 }
}
fn default_fm_frequency_row() -> RowRangeConfig {
  RowRangeConfig::LogFactor { least: 0.25, multiplier: 2.0 }
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScaleControlKind {
  /// Arms "the next slot pressed is written with the current scale".
  Store,
  /// Arms "the next slot pressed is emptied".
  Empty,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LoopControlKind {
  /// Begin recording into the active slot (overwrites it).
  Start,
  /// Stop the active slot's current activity -- recording OR playback.
  Stop,
  /// Stop recording and loop the active slot as the sole sounding loop.
  Play,
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
      | MonomeWindowConfig::RecordControl { id, .. }
      | MonomeWindowConfig::ScaleSlots { id, .. }
      | MonomeWindowConfig::ScaleControl { id, .. }
      | MonomeWindowConfig::EdoShiftPad { id, .. }
      | MonomeWindowConfig::LoopSlots { id, .. }
      | MonomeWindowConfig::LoopControl { id, .. }
      | MonomeWindowConfig::LoopRemapModeToggle { id, .. }
      | MonomeWindowConfig::LoopCopyButton { id, .. }
      | MonomeWindowConfig::LoopRemapUndo { id, .. }
      | MonomeWindowConfig::LoopDisplay { id, .. }
      | MonomeWindowConfig::TimbreEditor { id, .. } => id,
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
      | MonomeWindowConfig::RecordControl { monome, .. }
      | MonomeWindowConfig::ScaleSlots { monome, .. }
      | MonomeWindowConfig::ScaleControl { monome, .. }
      | MonomeWindowConfig::EdoShiftPad { monome, .. }
      | MonomeWindowConfig::LoopSlots { monome, .. }
      | MonomeWindowConfig::LoopControl { monome, .. }
      | MonomeWindowConfig::LoopRemapModeToggle { monome, .. }
      | MonomeWindowConfig::LoopCopyButton { monome, .. }
      | MonomeWindowConfig::LoopRemapUndo { monome, .. }
      | MonomeWindowConfig::LoopDisplay { monome, .. }
      | MonomeWindowConfig::TimbreEditor { monome, .. } => monome,
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
      | MonomeWindowConfig::RecordControl { rect, .. }
      | MonomeWindowConfig::ScaleSlots { rect, .. }
      | MonomeWindowConfig::ScaleControl { rect, .. }
      | MonomeWindowConfig::EdoShiftPad { rect, .. }
      | MonomeWindowConfig::LoopSlots { rect, .. }
      | MonomeWindowConfig::LoopControl { rect, .. }
      | MonomeWindowConfig::LoopRemapModeToggle { rect, .. }
      | MonomeWindowConfig::LoopCopyButton { rect, .. }
      | MonomeWindowConfig::LoopRemapUndo { rect, .. }
      | MonomeWindowConfig::LoopDisplay { rect, .. }
      | MonomeWindowConfig::TimbreEditor { rect, .. } => *rect,
    }
  }

  /// A human-readable kind label (matches the config `kind = "..."` tag).
  pub fn kind_name(&self) -> &'static str {
    match self {
      MonomeWindowConfig::EdoNoteGrid { .. } => "edo_note_grid",
      MonomeWindowConfig::ChordWipeButton { .. } => "chord_wipe_button",
      MonomeWindowConfig::ChordAccreteToggle { .. } => "chord_accrete_toggle",
      MonomeWindowConfig::ChordEmitModeToggle { .. } => "chord_emit_mode_toggle",
      MonomeWindowConfig::ChordTargetButton { .. } => "chord_target_button",
      MonomeWindowConfig::ChordSlotButtons { .. } => "chord_slot_buttons",
      MonomeWindowConfig::TwelveEdoOffsetBoard { .. } => "twelve_edo_offset_board",
      MonomeWindowConfig::PreimageRow { .. } => "preimage_row",
      MonomeWindowConfig::RemappableUn12Grid { .. } => "remappable_un12_grid",
      MonomeWindowConfig::RemapUndoButton { .. } => "remap_undo_button",
      MonomeWindowConfig::RecordControl { .. } => "record_control",
      MonomeWindowConfig::ScaleSlots { .. } => "scale_slots",
      MonomeWindowConfig::ScaleControl { .. } => "scale_control",
      MonomeWindowConfig::EdoShiftPad { .. } => "edo_shift_pad",
      MonomeWindowConfig::LoopSlots { .. } => "loop_slots",
      MonomeWindowConfig::LoopControl { .. } => "loop_control",
      MonomeWindowConfig::LoopRemapModeToggle { .. } => "loop_remap_mode_toggle",
      MonomeWindowConfig::LoopCopyButton { .. } => "loop_copy_button",
      MonomeWindowConfig::LoopRemapUndo { .. } => "loop_remap_undo",
      MonomeWindowConfig::LoopDisplay { .. } => "loop_display",
      MonomeWindowConfig::TimbreEditor { .. } => "timbre_editor",
    }
  }

  /// The timbre-editor row ranges + target, in row order (amplitude, AM amp/freq/
  /// shape, FM amp/freq), or None for other window kinds. Used by `resolve_settings`.
  #[allow(clippy::type_complexity)]
  pub fn timbre_editor_rows(
    &self,
  ) -> Option<(TimbreTarget, [RowRangeConfig; 6])> {
    match self {
      MonomeWindowConfig::TimbreEditor {
        target,
        amplitude,
        am_amplitude,
        am_frequency,
        am_shape,
        fm_amplitude,
        fm_frequency,
        ..
      } => Some((
        *target,
        [*amplitude, *am_amplitude, *am_frequency, *am_shape, *fm_amplitude, *fm_frequency],
      )),
      _ => None,
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
    if let SinkConfig::CpalSynth {
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
  let mut scale_control_kinds: HashSet<ScaleControlKind> = HashSet::new();
  let mut loop_control_kinds: HashSet<LoopControlKind> = HashSet::new();
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
      MonomeWindowConfig::ScaleControl { id, rect, control, .. } => {
        if rect[0] != rect[2] || rect[1] != rect[3] {
          return Err(format!(
            "scale_control window {:?} rect must cover exactly one cell",
            id,
          ));
        }
        if !scale_control_kinds.insert(*control) {
          return Err(format!(
            "duplicate scale_control kind {:?} (in window {:?})",
            control, id,
          ));
        }
      }
      MonomeWindowConfig::LoopControl { id, rect, control, .. } => {
        if rect[0] != rect[2] || rect[1] != rect[3] {
          return Err(format!(
            "loop_control window {:?} rect must cover exactly one cell",
            id,
          ));
        }
        if !loop_control_kinds.insert(*control) {
          return Err(format!(
            "duplicate loop_control kind {:?} (in window {:?})",
            control, id,
          ));
        }
      }
      MonomeWindowConfig::TimbreEditor { id, .. } => {
        // Unfolded the editor is 7 rows (control + amplitude + 5 FX rows) and needs
        // the control row's fold + 4 waveform cells (>=5 wide). 6_plan 2.7.
        if x1 - x0 + 1 < 5 {
          return Err(format!(
            "timbre_editor window {id:?} rect must be at least 5 cells wide",
          ));
        }
        if y1 - y0 + 1 < 7 {
          return Err(format!(
            "timbre_editor window {id:?} rect must be at least 7 rows tall (unfolded)",
          ));
        }
        if let Some((_, rows)) = window.timbre_editor_rows() {
          for row in rows {
            validate_row_range(id, &row)?;
          }
        }
      }
      _ => {}
    }
  }

  validate_window_groups(config)?;
  validate_looper(config)?;

  Ok(())
}

/// The looper feature is all-or-nothing in its own right, with richer constraints
/// than the generic window-group mechanism can express (exact counts, a minimum
/// display size, the remap-center bounds). It deliberately does NOT touch the
/// existing "remap"/"record"/"scale" groups: the looper has its own
/// `loop_remap_undo` kind, so no existing config's validation changes.
fn validate_looper(config: &Config) -> Result<(), String> {
  let count = |pred: fn(&MonomeWindowConfig) -> bool| {
    config.monome_windows.iter().filter(|w| pred(w)).count()
  };
  let loop_slots = count(|w| matches!(w, MonomeWindowConfig::LoopSlots { .. }));
  let loop_displays = count(|w| matches!(w, MonomeWindowConfig::LoopDisplay { .. }));
  let any_loop_window = config.monome_windows.iter().any(|w| {
    matches!(
      w,
      MonomeWindowConfig::EdoShiftPad { .. }
        | MonomeWindowConfig::LoopSlots { .. }
        | MonomeWindowConfig::LoopControl { .. }
        | MonomeWindowConfig::LoopRemapModeToggle { .. }
        | MonomeWindowConfig::LoopCopyButton { .. }
        | MonomeWindowConfig::LoopRemapUndo { .. }
        | MonomeWindowConfig::LoopDisplay { .. }
    )
  });

  // The [looper] table and the looper windows imply each other.
  if any_loop_window && config.looper.is_none() {
    return Err("looper windows require a [looper] table".to_string());
  }
  if config.looper.is_some() && !any_loop_window {
    return Err("a [looper] table requires the looper windows".to_string());
  }
  let Some(looper) = &config.looper else {
    return Ok(());
  };

  // Core windows: a slot grid, a display, and the three transport controls. The
  // shift pad, mode toggle, copy and undo are optional add-ons.
  if loop_slots != 1 {
    return Err(format!("a looper config needs exactly one loop_slots window, found {loop_slots}"));
  }
  if loop_displays != 1 {
    return Err(format!("a looper config needs exactly one loop_display window, found {loop_displays}"));
  }
  for control in [LoopControlKind::Start, LoopControlKind::Stop, LoopControlKind::Play] {
    let present = config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::LoopControl { control: c, .. } if *c == control)
    });
    if !present {
      return Err(format!("a looper config needs a loop_control with control = {control:?}"));
    }
  }

  // The display rect must be at least 2x2 so the row-picker column, the
  // column-picker row, and a main area can be derived from it.
  for window in &config.monome_windows {
    if let MonomeWindowConfig::LoopDisplay { id, rect, .. } = window {
      let [x0, y0, x1, y1] = *rect;
      if x1 - x0 < 1 || y1 - y0 < 1 {
        return Err(format!(
          "loop_display window {id:?} rect must be at least 2x2 (cols x rows)",
        ));
      }
    }
  }

  // The edo_note_grid rect must fit its monome's grid (an oversized rect would let
  // a press land on a cell the LED render never iterates -- an invisible, stuck
  // voice), and the group-transpose "unison" key (remap_center) must lie inside it.
  let edo = config.monome_windows.iter().find_map(|w| match w {
    MonomeWindowConfig::EdoNoteGrid { monome, rect, .. } => Some((monome.clone(), *rect)),
    _ => None,
  });
  let Some((edo_monome_id, [ex0, ey0, ex1, ey1])) = edo else {
    return Err("a looper config needs an edo_note_grid window".to_string());
  };
  let [gw, gh] = config
    .monomes
    .iter()
    .find(|m| m.id == edo_monome_id)
    .and_then(|m| m.select.size)
    .unwrap_or([16, 16]);
  if ex0 < 0 || ey0 < 0 || ex1 >= gw || ey1 >= gh {
    return Err(format!(
      "edo_note_grid rect [{ex0}, {ey0}, {ex1}, {ey1}] must fit the {gw}x{gh} grid",
    ));
  }
  let [cx, cy] = looper.remap_center;
  if cx < ex0 || cx > ex1 || cy < ey0 || cy > ey1 {
    return Err(format!(
      "looper remap_center {:?} must lie inside the edo_note_grid rect [{ex0}, {ey0}, {ex1}, {ey1}]",
      looper.remap_center,
    ));
  }

  // The looper scalars must be positive.
  for (name, value) in [
    ("quantize_record_ms", looper.quantize_record_ms),
    ("cluster_display_ms", looper.cluster_display_ms),
    ("flash_ms", looper.flash_ms),
  ] {
    if value == 0 {
      return Err(format!("looper {name} must be positive"));
    }
  }

  // No orphan monomes: every declared monome must be used by some window.
  for monome in &config.monomes {
    let used = config.monome_windows.iter().any(|w| w.monome() == monome.id);
    if !used {
      return Err(format!("looper config declares monome {:?} but no window uses it", monome.id));
    }
  }

  Ok(())
}

/// A timbre-editor parameter row must be finite and monotonic. Log rows need a
/// positive base; a growth factor must exceed 1 or the row collapses.
fn validate_row_range(id: &str, row: &RowRangeConfig) -> Result<(), String> {
  let finite = |v: f32| v.is_finite();
  match *row {
    RowRangeConfig::Linear { min, max } => {
      if !finite(min) || !finite(max) || max < min {
        return Err(format!("timbre_editor window {id:?} linear row needs finite min <= max"));
      }
    }
    RowRangeConfig::LogFactor { least, multiplier } => {
      if !finite(least) || least <= 0.0 || !finite(multiplier) || multiplier <= 1.0 {
        return Err(format!(
          "timbre_editor window {id:?} log_factor row needs least > 0 and multiplier > 1",
        ));
      }
    }
    RowRangeConfig::LogRange { least, greatest } => {
      if !finite(least) || least <= 0.0 || !finite(greatest) || greatest < least {
        return Err(format!(
          "timbre_editor window {id:?} log_range row needs 0 < least <= greatest",
        ));
      }
    }
  }
  Ok(())
}

/// Windows come in all-or-nothing groups, and a group may depend on another
/// group. A config is modular -- it may omit a whole group -- but any group it
/// declares must appear in its entirety, and a group's dependencies must also be
/// present. (A config with no recording buttons is valid; it just launches a
/// program with no recording abilities.)
fn validate_window_groups(config: &Config) -> Result<(), String> {
  let groups = window_groups(config);
  // Each group is all-or-nothing: declaring any member requires all of them.
  for group in &groups {
    if group.active() {
      let missing = group.missing();
      if !missing.is_empty() {
        return Err(format!(
          "the {:?} window group is all-or-nothing; it is missing {:?}",
          group.label, missing,
        ));
      }
    }
  }
  // Inter-group dependencies: an active group needs its dependencies active too.
  for group in &groups {
    if !group.active() {
      continue;
    }
    for dep in &group.depends_on {
      let dep_active = groups.iter().any(|g| g.label == *dep && g.active());
      if !dep_active {
        return Err(format!(
          "the {:?} window group requires the {:?} window group, which the config does not declare",
          group.label, dep,
        ));
      }
    }
  }
  Ok(())
}

/// A set of windows that must appear together, plus the groups it depends on.
struct WindowGroup {
  label: &'static str,
  /// Each member is named (for error messages) and marked present-or-not.
  members: Vec<(&'static str, bool)>,
  depends_on: Vec<&'static str>,
}

impl WindowGroup {
  /// A group is "active" once any of its members is declared.
  fn active(&self) -> bool {
    self.members.iter().any(|(_, present)| *present)
  }

  fn missing(&self) -> Vec<&'static str> {
    self
      .members
      .iter()
      .filter(|(_, present)| !present)
      .map(|(name, _)| *name)
      .collect()
  }
}

fn window_groups(config: &Config) -> Vec<WindowGroup> {
  let has_kind =
    |pred: fn(&MonomeWindowConfig) -> bool| config.monome_windows.iter().any(pred);
  let has_control = |control: RecordControlKind| {
    config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::RecordControl { control: c, .. } if *c == control)
    })
  };
  let has_scale_control = |control: ScaleControlKind| {
    config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::ScaleControl { control: c, .. } if *c == control)
    })
  };
  vec![
    // The remap group: the big edo grid, the 12-edo row that labels it, and the
    // undo button that reverts remaps.
    WindowGroup {
      label: "remap",
      members: vec![
        (
          "remappable_un12_grid",
          has_kind(|w| matches!(w, MonomeWindowConfig::RemappableUn12Grid { .. })),
        ),
        (
          "preimage_row",
          has_kind(|w| matches!(w, MonomeWindowConfig::PreimageRow { .. })),
        ),
        (
          "remap_undo_button",
          has_kind(|w| matches!(w, MonomeWindowConfig::RemapUndoButton { .. })),
        ),
      ],
      depends_on: vec![],
    },
    // The record group: the seven recording buttons. They only make sense
    // alongside the remap group, so it depends on "remap".
    WindowGroup {
      label: "record",
      members: vec![
        ("record_control(start)", has_control(RecordControlKind::Start)),
        ("record_control(stop)", has_control(RecordControlKind::Stop)),
        ("record_control(loop)", has_control(RecordControlKind::Loop)),
        ("record_control(arm)", has_control(RecordControlKind::Arm)),
        ("record_control(erase_ons)", has_control(RecordControlKind::EraseOns)),
        ("record_control(end_all)", has_control(RecordControlKind::EndAll)),
        ("record_control(rscm)", has_control(RecordControlKind::Rscm)),
      ],
      depends_on: vec!["remap"],
    },
    // The scale group: the slot grid plus the store/empty arm buttons. Saving a
    // scale only makes sense alongside the remap group, so it depends on it.
    WindowGroup {
      label: "scale",
      members: vec![
        (
          "scale_slots",
          has_kind(|w| matches!(w, MonomeWindowConfig::ScaleSlots { .. })),
        ),
        ("scale_control(store)", has_scale_control(ScaleControlKind::Store)),
        ("scale_control(empty)", has_scale_control(ScaleControlKind::Empty)),
      ],
      depends_on: vec!["remap"],
    },
  ]
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
  fn config_without_recording_buttons_is_valid() {
    // A modular config that omits the recording feature entirely must parse:
    // it just launches a program with no recording abilities.
    parse_config(r#"
version = 1
id = "no-record"
title = "No Record"

[[tunings]]
id = "main"
edo = 31
x_step = 6
y_step = 1
fundamental_hz = 80

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/256-1-cable"

[[monome_windows]]
id = "preimage-row"
monome = "big"
kind = "preimage_row"
rect = [0, 0, 11, 0]

[[monome_windows]]
id = "remap-grid"
monome = "big"
kind = "remappable_un12_grid"
rect = [0, 1, 9, 15]
tuning = "main"

[[monome_windows]]
id = "undo"
monome = "big"
kind = "remap_undo_button"
rect = [15, 15, 15, 15]
"#).expect("a config without recording buttons should be valid");
  }

  #[test]
  fn partial_remap_group_is_rejected() {
    // The remap group is all-or-nothing: a grid without its 12-edo row and undo
    // button is invalid.
    let err = parse_config(r#"
version = 1
id = "lonely-grid"
title = "Lonely Grid"

[[tunings]]
id = "main"
edo = 31
x_step = 6
y_step = 1
fundamental_hz = 80

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/256-1-cable"

[[monome_windows]]
id = "remap-grid"
monome = "big"
kind = "remappable_un12_grid"
rect = [0, 1, 9, 15]
tuning = "main"
"#).expect_err("a grid without preimage_row/undo should fail");

    assert!(err.contains("all-or-nothing"), "{err}");
    assert!(err.contains("preimage_row"), "{err}");
  }

  #[test]
  fn partial_record_group_is_rejected() {
    // The record group is all-or-nothing: declaring some recording buttons but
    // not all of them is invalid, even with the remap group fully present.
    let err = parse_config(r#"
version = 1
id = "partial-record"
title = "Partial Record"

[[tunings]]
id = "main"
edo = 31
x_step = 6
y_step = 1
fundamental_hz = 80

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/256-1-cable"

[[monome_windows]]
id = "preimage-row"
monome = "big"
kind = "preimage_row"
rect = [0, 0, 11, 0]

[[monome_windows]]
id = "remap-grid"
monome = "big"
kind = "remappable_un12_grid"
rect = [0, 1, 9, 15]
tuning = "main"

[[monome_windows]]
id = "undo"
monome = "big"
kind = "remap_undo_button"
rect = [15, 15, 15, 15]

[[monome_windows]]
id = "record-start"
monome = "big"
kind = "record_control"
control = "start"
rect = [13, 0, 13, 0]

[[monome_windows]]
id = "record-stop"
monome = "big"
kind = "record_control"
control = "stop"
rect = [14, 0, 14, 0]
"#).expect_err("a partial record group should fail");

    assert!(err.contains("all-or-nothing"), "{err}");
    assert!(err.contains("record_control(arm)"), "{err}");
  }

  #[test]
  fn record_group_requires_remap_group() {
    // The full record group is present, but it depends on the remap group,
    // which this config omits entirely.
    let err = parse_config(r#"
version = 1
id = "record-no-remap"
title = "Record No Remap"

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
id = "record-stop"
monome = "big"
kind = "record_control"
control = "stop"
rect = [14, 0, 14, 0]

[[monome_windows]]
id = "record-loop"
monome = "big"
kind = "record_control"
control = "loop"
rect = [15, 0, 15, 0]

[[monome_windows]]
id = "record-arm"
monome = "big"
kind = "record_control"
control = "arm"
rect = [13, 1, 13, 1]

[[monome_windows]]
id = "record-erase-ons"
monome = "big"
kind = "record_control"
control = "erase_ons"
rect = [14, 1, 14, 1]

[[monome_windows]]
id = "record-end-all"
monome = "big"
kind = "record_control"
control = "end_all"
rect = [15, 1, 15, 1]

[[monome_windows]]
id = "record-rscm"
monome = "big"
kind = "record_control"
control = "rscm"
rect = [15, 2, 15, 2]
"#).expect_err("a record group without the remap group should fail");

    assert!(err.contains("requires the \"remap\""), "{err}");
  }

  #[test]
  fn scale_slots_config_is_valid() {
    // The remap group plus a complete scale group (slot grid + store + empty).
    parse_config(r#"
version = 1
id = "save-scales"
title = "Save Scales"

[[tunings]]
id = "main"
edo = 58
x_step = 8
y_step = 1
fundamental_hz = 80

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/256-1-cable"

[[monome_windows]]
id = "preimage-row"
monome = "big"
kind = "preimage_row"
rect = [0, 0, 11, 0]

[[monome_windows]]
id = "remap-grid"
monome = "big"
kind = "remappable_un12_grid"
rect = [0, 1, 9, 15]
tuning = "main"

[[monome_windows]]
id = "undo"
monome = "big"
kind = "remap_undo_button"
rect = [15, 15, 15, 15]

[[monome_windows]]
id = "scale-slots"
monome = "big"
kind = "scale_slots"
rect = [12, 0, 15, 3]

[[monome_windows]]
id = "scale-store"
monome = "big"
kind = "scale_control"
control = "store"
rect = [15, 4, 15, 4]

[[monome_windows]]
id = "scale-empty"
monome = "big"
kind = "scale_control"
control = "empty"
rect = [14, 4, 14, 4]
"#).expect("a remap + complete scale config should be valid");
  }

  #[test]
  fn partial_scale_group_is_rejected() {
    // Slot grid present, but the store/empty arm buttons are missing.
    let err = parse_config(r#"
version = 1
id = "partial-scale"
title = "Partial Scale"

[[tunings]]
id = "main"
edo = 58
x_step = 8
y_step = 1
fundamental_hz = 80

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/256-1-cable"

[[monome_windows]]
id = "preimage-row"
monome = "big"
kind = "preimage_row"
rect = [0, 0, 11, 0]

[[monome_windows]]
id = "remap-grid"
monome = "big"
kind = "remappable_un12_grid"
rect = [0, 1, 9, 15]
tuning = "main"

[[monome_windows]]
id = "undo"
monome = "big"
kind = "remap_undo_button"
rect = [15, 15, 15, 15]

[[monome_windows]]
id = "scale-slots"
monome = "big"
kind = "scale_slots"
rect = [12, 0, 15, 3]
"#).expect_err("a scale group without its arm buttons should fail");

    assert!(err.contains("all-or-nothing"), "{err}");
    assert!(err.contains("scale_control(store)"), "{err}");
  }

  #[test]
  fn scale_group_requires_remap_group() {
    // A complete scale group, but no remap group to make scales with.
    let err = parse_config(r#"
version = 1
id = "scale-no-remap"
title = "Scale No Remap"

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/256-1-cable"

[[monome_windows]]
id = "scale-slots"
monome = "big"
kind = "scale_slots"
rect = [12, 0, 15, 3]

[[monome_windows]]
id = "scale-store"
monome = "big"
kind = "scale_control"
control = "store"
rect = [15, 4, 15, 4]

[[monome_windows]]
id = "scale-empty"
monome = "big"
kind = "scale_control"
control = "empty"
rect = [14, 4, 14, 4]
"#).expect_err("a scale group without the remap group should fail");

    assert!(err.contains("requires the \"remap\""), "{err}");
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

  // ---- Looper config validation. ----

  const LOOPER_HEADER: &str = r#"version = 1
id = "looper"
title = "Looper"

[[tunings]]
id = "main"
edo = 58
x_step = 8
y_step = 1
fundamental_hz = 80

[[sinks]]
id = "saw"
kind = "cpal_synth"
sample_rate = 48000
buffer_frames = 128
amplitude = 0.15
attack_secs = 0.003
release_secs = 0.05
accretion_level = 0.5

"#;
  const LOOPER_TABLE: &str = r#"[looper]
quantize_record_ms = 70
cluster_display_ms = 100
flash_ms = 50
remap_center = [7, 7]

"#;
  const LOOPER_MONOMES: &str = r#"[[monomes]]
id = "edo"
listen_port = 9000
prefix = "/looper-edo"

[[monomes]]
id = "loops"
listen_port = 9001
prefix = "/looper-loops"

"#;
  const LOOPER_EDO_GRID: &str = r#"[[monome_windows]]
id = "edo-grid"
monome = "edo"
kind = "edo_note_grid"
rect = [0, 0, 15, 15]
tuning = "main"
sink = "saw"

"#;
  const LOOPER_SLOTS: &str = r#"[[monome_windows]]
id = "loop-slots"
monome = "loops"
kind = "loop_slots"
rect = [0, 0, 3, 3]

"#;
  const LOOPER_START: &str = r#"[[monome_windows]]
id = "loop-start"
monome = "loops"
kind = "loop_control"
control = "start"
rect = [0, 3, 0, 3]

"#;
  const LOOPER_STOP: &str = r#"[[monome_windows]]
id = "loop-stop"
monome = "loops"
kind = "loop_control"
control = "stop"
rect = [1, 3, 1, 3]

"#;
  const LOOPER_PLAY: &str = r#"[[monome_windows]]
id = "loop-play"
monome = "loops"
kind = "loop_control"
control = "play"
rect = [2, 3, 2, 3]

"#;
  const LOOPER_DISPLAY: &str = r#"[[monome_windows]]
id = "loop-display"
monome = "loops"
kind = "loop_display"
rect = [0, 5, 15, 15]
"#;

  fn valid_looper_toml() -> String {
    format!(
      "{LOOPER_HEADER}{LOOPER_TABLE}{LOOPER_MONOMES}{LOOPER_EDO_GRID}{LOOPER_SLOTS}{LOOPER_START}{LOOPER_STOP}{LOOPER_PLAY}{LOOPER_DISPLAY}",
    )
  }

  #[test]
  fn looper_config_is_valid() {
    parse_config(&valid_looper_toml()).expect("a complete looper config should be valid");
  }

  #[test]
  fn looper_windows_require_looper_table() {
    let err = parse_config(&valid_looper_toml().replace(LOOPER_TABLE, ""))
      .expect_err("looper windows without a [looper] table should fail");
    assert!(err.contains("[looper] table"), "{err}");
  }

  #[test]
  fn looper_table_requires_looper_windows() {
    // A [looper] table atop a plain sawwave grid (no loop_* windows) is invalid.
    let toml = format!("{LOOPER_HEADER}{LOOPER_TABLE}{LOOPER_MONOMES}{LOOPER_EDO_GRID}");
    let err = parse_config(&toml).expect_err("a [looper] table without looper windows should fail");
    assert!(err.contains("requires the looper windows"), "{err}");
  }

  #[test]
  fn loop_display_must_be_at_least_2x2() {
    let err = parse_config(&valid_looper_toml().replace("rect = [0, 5, 15, 15]", "rect = [0, 5, 0, 5]"))
      .expect_err("a 1x1 loop_display should fail");
    assert!(err.contains("at least 2x2"), "{err}");
  }

  #[test]
  fn remap_center_must_be_inside_edo_grid() {
    let err = parse_config(&valid_looper_toml().replace("remap_center = [7, 7]", "remap_center = [20, 20]"))
      .expect_err("an out-of-bounds remap_center should fail");
    assert!(err.contains("must lie inside"), "{err}");
  }

  #[test]
  fn looper_requires_all_three_transport_controls() {
    let err = parse_config(&valid_looper_toml().replace(LOOPER_PLAY, ""))
      .expect_err("a looper missing the play control should fail");
    assert!(err.contains("control = Play"), "{err}");
  }

  #[test]
  fn duplicate_loop_control_kind_is_rejected() {
    let dup = valid_looper_toml().replace(
      "control = \"play\"\nrect = [2, 3, 2, 3]",
      "control = \"start\"\nrect = [2, 3, 2, 3]",
    );
    let err = parse_config(&dup).expect_err("duplicate loop_control kind should fail");
    assert!(err.contains("duplicate loop_control kind"), "{err}");
  }

  #[test]
  fn loop_control_rect_must_be_single_cell() {
    let err = parse_config(&valid_looper_toml().replace("rect = [0, 3, 0, 3]", "rect = [0, 3, 1, 3]"))
      .expect_err("a multi-cell loop_control rect should fail");
    assert!(err.contains("exactly one cell"), "{err}");
  }

  // ---- C3b: timbre_editor window + [am.shape] ----

  const TIMBRE_EDITOR_MIN: &str = r#"
[[monome_windows]]
id = "edo-timbre"
monome = "edo"
kind = "timbre_editor"
target = "loop"
rect = [0, 0, 15, 6]
"#;

  fn looper_with_editor(editor: &str) -> String {
    format!("{}{editor}", valid_looper_toml())
  }

  #[test]
  fn timbre_editor_parses_with_default_rows() {
    let config = parse_config(&looper_with_editor(TIMBRE_EDITOR_MIN))
      .expect("a timbre_editor with omitted rows should use 6_plan 5 defaults");
    let ed = config
      .monome_windows
      .iter()
      .find_map(MonomeWindowConfig::timbre_editor_rows)
      .expect("the editor is present");
    assert_eq!(ed.0, TimbreTarget::Loop);
    assert_eq!(ed.1[0], RowRangeConfig::LogRange { least: 0.0009, greatest: 0.15 }, "amplitude default");
    assert_eq!(ed.1[1], RowRangeConfig::Linear { min: 0.0, max: 1.0 }, "am depth default");
    assert_eq!(ed.1[2], RowRangeConfig::LogFactor { least: 0.25, multiplier: 2.0 }, "am freq default");
    assert_eq!(ed.1[4], RowRangeConfig::LogFactor { least: 5.0, multiplier: 2.0 }, "fm cents default");
  }

  #[test]
  fn timbre_editor_parses_explicit_rows() {
    let editor = r#"
[[monome_windows]]
id = "edo-timbre"
monome = "edo"
kind = "timbre_editor"
target = "live"
rect = [0, 0, 15, 6]
amplitude    = { kind = "log_range",  least = 0.001, greatest = 0.2 }
am_amplitude = { kind = "linear",     min = 0.0, max = 0.5 }
"#;
    let config = parse_config(&looper_with_editor(editor)).expect("explicit rows should parse");
    let ed = config.monome_windows.iter().find_map(MonomeWindowConfig::timbre_editor_rows).unwrap();
    assert_eq!(ed.0, TimbreTarget::Live);
    assert_eq!(ed.1[0], RowRangeConfig::LogRange { least: 0.001, greatest: 0.2 });
    assert_eq!(ed.1[1], RowRangeConfig::Linear { min: 0.0, max: 0.5 });
    // An omitted row still falls back to its default.
    assert_eq!(ed.1[2], RowRangeConfig::LogFactor { least: 0.25, multiplier: 2.0 });
  }

  #[test]
  fn am_shape_family_parses_and_defaults() {
    // Absent [am.shape] -> no am table (the runtime defaults to sin_to_square).
    let none = parse_config(&looper_with_editor(TIMBRE_EDITOR_MIN)).unwrap();
    assert!(none.am.is_none(), "no [am] table when omitted");
    // Explicit family parses.
    let with = parse_config(&format!(
      "{}\n[am.shape]\nfamily = \"tri_to_square\"\n",
      looper_with_editor(TIMBRE_EDITOR_MIN)
    ))
    .expect("am.shape should parse");
    assert_eq!(with.am.unwrap().shape.family, AmShapeFamilyConfig::TriToSquare);
  }

  #[test]
  fn timbre_editor_rect_must_be_tall_enough() {
    let short = TIMBRE_EDITOR_MIN.replace("rect = [0, 0, 15, 6]", "rect = [0, 0, 15, 2]");
    let err = parse_config(&looper_with_editor(&short)).expect_err("a 3-row editor should fail");
    assert!(err.contains("at least 7 rows"), "{err}");
  }

  #[test]
  fn timbre_editor_rect_must_be_wide_enough() {
    let narrow = TIMBRE_EDITOR_MIN.replace("rect = [0, 0, 15, 6]", "rect = [0, 0, 3, 6]");
    let err = parse_config(&looper_with_editor(&narrow)).expect_err("a 4-wide editor should fail");
    assert!(err.contains("at least 5 cells wide"), "{err}");
  }

  #[test]
  fn timbre_editor_rejects_a_degenerate_log_row() {
    let bad = format!(
      "{}am_frequency = {{ kind = \"log_factor\", least = 0.25, multiplier = 1.0 }}\n",
      TIMBRE_EDITOR_MIN
    );
    let err = parse_config(&looper_with_editor(&bad)).expect_err("multiplier 1.0 collapses the row");
    assert!(err.contains("multiplier > 1"), "{err}");
  }

  // ---- C5a: relative window coordinates ----

  #[test]
  fn parse_edge_expr_forms() {
    assert_eq!(
      parse_edge_expr("live-timbre.bottom + 1").unwrap(),
      EdgeRef { target: "live-timbre".into(), edge: EdgeName::Bottom, offset: 1 },
    );
    assert_eq!(parse_edge_expr("grid.left").unwrap().offset, 0, "no offset -> 0");
    assert_eq!(parse_edge_expr("a.top-2").unwrap().offset, -2, "unspaced negative offset");
    assert_eq!(parse_edge_expr("a.right +3").unwrap().offset, 3);
    assert!(parse_edge_expr("noedge").is_err(), "missing dot");
    assert!(parse_edge_expr("a.middle").is_err(), "unknown edge");
    assert!(parse_edge_expr(".top").is_err(), "missing id");
  }

  #[test]
  fn rect_spec_parses_array_and_per_edge() {
    #[derive(serde::Deserialize)]
    struct W {
      rect: RectSpecConfig,
    }
    let arr: W = toml::from_str("rect = [0, 0, 15, 6]").unwrap();
    assert_eq!(arr.rect.resolved_edge(EdgeName::Left).unwrap(), ResolvedEdge::Absolute(0));
    assert_eq!(arr.rect.resolved_edge(EdgeName::Right).unwrap(), ResolvedEdge::Absolute(15));
    arr.rect.validate().unwrap();

    let per: W = toml::from_str(
      "[rect]\ntop = \"live-timbre.bottom + 1\"\nbottom = -1\nleft = 0\nright = -1\n",
    )
    .unwrap();
    assert_eq!(per.rect.resolved_edge(EdgeName::Bottom).unwrap(), ResolvedEdge::Absolute(-1));
    assert!(matches!(
      per.rect.resolved_edge(EdgeName::Top).unwrap(),
      ResolvedEdge::Ref(EdgeRef { edge: EdgeName::Bottom, offset: 1, .. })
    ));
    per.rect.validate().unwrap();
  }

  #[test]
  fn timbre_looper_config_has_both_editors() {
    // The full instrument (7_layout.org): a loop-timbre editor on edo + a live-timbre
    // editor on loops, with the looper stack at its unfolded positions.
    let config = load_named_config("monome-looper-58-8-1-timbre").expect("timbre config loads");
    let editors: Vec<(String, TimbreTarget)> = config
      .monome_windows
      .iter()
      .filter_map(|w| w.timbre_editor_rows().map(|(t, _)| (w.monome().to_string(), t)))
      .collect();
    assert_eq!(editors.len(), 2, "two timbre editors");
    assert!(editors.iter().any(|(m, t)| m == "edo" && *t == TimbreTarget::Loop), "edo loop editor");
    assert!(editors.iter().any(|(m, t)| m == "loops" && *t == TimbreTarget::Live), "loops live editor");
    // The loop display is at its unfolded position (reflowed up at runtime).
    let display = config.monome_windows.iter().find(|w| w.kind_name() == "loop_display").unwrap();
    assert_eq!(display.rect(), [0, 9, 15, 15], "loop display sits below the unfolded editor");
  }

  #[test]
  fn rect_spec_rejects_a_bad_edge_expr() {
    #[derive(serde::Deserialize)]
    struct W {
      rect: RectSpecConfig,
    }
    let w: W = toml::from_str(
      "[rect]\ntop = \"x.sideways\"\nbottom = 0\nleft = 0\nright = 0\n",
    )
    .unwrap();
    assert!(w.rect.validate().is_err(), "an unknown edge name should fail validation");
  }
}
