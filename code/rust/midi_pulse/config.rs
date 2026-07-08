use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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
  /// Keith McMillen SoftStep (KMSS) foot-controller devices, declared once and
  /// referenced by id from `softstep_windows` -- the same idiom as `monomes`.
  #[serde(default)]
  pub softsteps: Vec<SoftstepConfig>,
  /// Windows over a SoftStep: a region of pedals plus a `kind` behavior. The same
  /// windowing idiom as `monome_windows`, adapted to the KMSS's 10 labeled pedals.
  #[serde(default)]
  pub softstep_windows: Vec<SoftstepWindowConfig>,
  #[serde(default)]
  pub display: Option<DisplayConfig>,
  #[serde(default)]
  pub looper: Option<LooperConfig>,
  /// Instrument-wide AM settings (6_plan 5). Today only the shape family lives here;
  /// it is the config-level morph family the per-note `am.shape` value sweeps within.
  #[serde(default)]
  pub am: Option<AmConfig>,
  /// Surfaces-runtime settings (the shared recent-note trail). Absent for non-surfaces
  /// configs; the surfaces runtime falls back to `SurfacesConfig::default`.
  #[serde(default)]
  pub surfaces: Option<SurfacesConfig>,
  /// The four selectable timbres behind a `waveform_selector` strip, left to right
  /// (surfaces runtime). Absent = the plain four waveforms (sine / triangle /
  /// square / saw, everything else off) -- exactly the pre-timbres behavior. When
  /// present there must be exactly four entries.
  #[serde(default)]
  pub timbres: Vec<TimbreConfig>,
}

/// One selectable timbre (`[[timbres]]`): a base waveform plus its numeric
/// parameters. Everything except `waveform` is optional; the defaults are "off"
/// (no AM, no FM) at full amplitude.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TimbreConfig {
  /// One of "sin"/"sine", "tri"/"triangle", "square", "saw".
  pub waveform: WaveformChoice,
  /// Per-timbre linear gain, multiplied below the volume fader. 0..1 typical
  /// (values above 1 amplify and can push the mix into the clamp). Default 1.0.
  #[serde(default = "default_timbre_amplitude")]
  pub amplitude: f32,
  /// Tremolo depth in [0,1]: 0 = off (default), 1 = dips to silence.
  #[serde(default)]
  pub am_depth: f32,
  /// Tremolo rate in Hz (~0.1..10 musical). Default 1.0; inert while depth = 0.
  #[serde(default = "default_timbre_freq")]
  pub am_freq: f32,
  /// Tremolo LFO morph in [0,1]: 0 = smooth (sine/tri end of the config's
  /// `[am]` family), 1 = near-square chop. Default 0.
  #[serde(default)]
  pub am_shape: f32,
  /// Vibrato depth in cents: 0 = off (default); ~5..50 subtle, 100+ = seasick.
  #[serde(default)]
  pub fm_depth_cents: f32,
  /// Vibrato rate in Hz (~0.1..10 musical). Default 1.0; inert while depth = 0.
  #[serde(default = "default_timbre_freq")]
  pub fm_freq: f32,
}

fn default_timbre_amplitude() -> f32 {
  1.0
}

fn default_timbre_freq() -> f32 {
  1.0
}

/// A waveform name in a config file. Accepts Jeff's short spellings.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum WaveformChoice {
  #[serde(alias = "sin")]
  Sine,
  #[serde(alias = "tri")]
  Triangle,
  Square,
  Saw,
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

fn default_clock_bpm() -> f64 {
  300.0
}

fn default_clock_duty() -> f64 {
  0.1
}

/// Looper-wide scalars (see the loop windows below). All durations in
/// milliseconds. Present iff the config declares the looper windows.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LooperConfig {
  /// The metronome clock, in beats per minute (one beat = one cycle). The transport
  /// snaps presses to the nearest cycle boundary, recorded note-ons within a fixed
  /// 50 ms of a boundary snap onto it, and one transport button flashes at this rate.
  #[serde(default = "default_clock_bpm")]
  pub clock_bpm: f64,
  /// The fraction of each metronome cycle the flashing transport button stays lit
  /// (0..1], measured from the boundary. Small values strobe (the eye is coarser in
  /// time than the ear, so a brief pulse reads more clearly than a 50/50 blink).
  #[serde(default = "default_clock_duty")]
  pub clock_duty: f64,
  /// Note-ons within this of each other display as one column of the loop.
  pub cluster_display_ms: u64,
  /// Reflection / octave-equivalent flash half-period (on for this, off for this).
  pub flash_ms: u64,
  /// The edo-grid cell that means "unison" in group-transpose remap.
  pub remap_center: [i32; 2],
}

fn default_trail_clobber_radius() -> i32 {
  27
}

fn default_trails_max() -> usize {
  7
}

/// `[surfaces]` table: instrument-wide settings for the surfaces runtime's shared
/// recent-note trail (the dim backdrop of the last few played pitch classes). A missing
/// table -- or any missing field -- uses these defaults, so behaviour is unchanged for
/// configs that don't declare it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SurfacesConfig {
  /// Trail clobber radius, as a *divisor of the octave*: playing a note clears any
  /// trailed pitch-class within `edo / trail_clobber_radius` steps of it, so close
  /// pitches never crowd the backdrop. Bigger value = tighter radius (clears fewer);
  /// the default 27 is 1/27 of an octave (~44 cents).
  pub trail_clobber_radius: i32,
  /// The most *distinct* pitch classes the shared trail keeps at once (newest first);
  /// older ones drop off the end. Default 7.
  pub trails_max: usize,
  /// The slide feature's candidate window, in ms: a new note may slide from a note
  /// released no longer ago than this. Default 1000.
  pub slide_candidate_window_ms: u64,
  /// The tap-tempo pairing window, in ms: two taps at most this far apart set the
  /// tapped tempo (rolling window). Default 2000.
  pub tap_tempo_window_ms: u64,
  /// How long a slide takes to reach the new pitch, in ms. Default 100.
  pub slide_duration_ms: u64,
}

impl Default for SurfacesConfig {
  fn default() -> Self {
    Self {
      trail_clobber_radius: default_trail_clobber_radius(),
      trails_max: default_trails_max(),
      slide_candidate_window_ms: default_slide_candidate_window_ms(),
      tap_tempo_window_ms: default_tap_tempo_window_ms(),
      slide_duration_ms: default_slide_duration_ms(),
    }
  }
}

fn default_slide_candidate_window_ms() -> u64 {
  1000
}

fn default_tap_tempo_window_ms() -> u64 {
  2000
}

fn default_slide_duration_ms() -> u64 {
  100
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

/// A Keith McMillen SoftStep (KMSS) foot controller. Declared once and referenced
/// by id from `softstep_windows`. The KMSS presents on the ALSA sequencer as the
/// client named "SSCOM"; the runtime opens the input port matching `select`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SoftstepConfig {
  pub id: String,
  #[serde(default)]
  pub select: SoftstepSelect,
}

/// Which MIDI input port to bind. `name_contains` is matched as a substring of the
/// midir input port name (default "SSCOM", the KMSS's ALSA client name) -- never a
/// hardcoded client number, which is reassigned on every replug.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SoftstepSelect {
  pub name_contains: Option<String>,
}

impl SoftstepSelect {
  /// The port-name substring to match, defaulting to the KMSS client name.
  pub fn name_substring(&self) -> &str {
    self.name_contains.as_deref().unwrap_or("SSCOM")
  }
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
    /// Internal oversampling factor for the synth render (1 = off). >1 runs the
    /// nonlinear/multiplicative mix at N x the rate and decimates, so audio-rate
    /// AM/FM/waveshaping doesn't alias. Defaults to 1 so existing configs are
    /// unchanged.
    #[serde(default = "default_oversample")]
    oversample: u32,
    /// The global distortion's scale `s` (the soft-clipper's asymptote; see
    /// learnings/distortion.org). Smaller = earlier/heavier bite; ~0.3 heavy,
    /// ~1.0 strong, >=5 nearly clean. Only heard while a `distortion_toggle`
    /// window is on.
    #[serde(default = "default_distortion_scale")]
    distortion_scale: f32,
    /// The global distortion's shape `k` (elbow harshness): 1 = gentlest,
    /// 2 = the smooth sweet spot, ~4+ = hard-ish, very large = hard clip.
    #[serde(default = "default_distortion_shape")]
    distortion_shape: f32,
    /// Pluck envelope (see TODO/misc.org "synth attacks should be louder"): after the
    /// attack peaks, the envelope decays exponentially toward `sustain_level` x the
    /// peak, so fresh strikes ring out over held notes -- a rough plucked-string
    /// curve that plateaus instead of dying (held notes and accrete drones keep
    /// ringing). >= 1.0 disables the decay (flat envelope); 0.2..0.5 is a natural
    /// pluck. Default 0.35.
    #[serde(default = "default_sustain_level")]
    sustain_level: f32,
    /// The pluck decay's time constant, in seconds (~0.3 snappy, ~1.5 slow ring;
    /// <= 0 disables). Default 0.5.
    #[serde(default = "default_decay_secs")]
    decay_secs: f32,
  },
  /// A one-shot sample player: each trigger plays a loaded WAV to completion,
  /// mixed polyphonically. Drives the `drumkit` softstep window. Uses the same
  /// cpal/JACK output path as `cpal_synth` (launch under `pw-jack`).
  CpalSampler {
    id: String,
    sample_rate: u32,
    buffer_frames: u32,
    amplitude: f32,
  },
}

fn default_oversample() -> u32 {
  1
}

fn default_distortion_scale() -> f32 {
  1.0 // "strong" on a unit-scale mix -- clearly a pedal, not a subtlety
}

fn default_distortion_shape() -> f32 {
  2.0 // the smooth sweet spot (y / sqrt(1 + (y/s)^2))
}

fn default_sustain_level() -> f32 {
  0.35 // ~ -9 dB below the strike peak: audible pluck, notes still ring
}

fn default_decay_secs() -> f32 {
  0.5 // a guitar-ish decay time constant
}

impl SinkConfig {
  pub fn id(&self) -> &str {
    match self {
      SinkConfig::Midi { id }
      | SinkConfig::CpalSynth { id, .. }
      | SinkConfig::CpalSampler { id, .. } => id,
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
  // A 4-cell waveform picker (sine / triangle / square / saw) overlaid on the edo
  // grid. Used by the surfaces runtime: each grid's strip sets the timbre of the
  // monome named by `controls` -- the strip's own grid in the current rigs, but any
  // play grid is legal (cross-control). Purely a radio selector -- no gain/AM/FM,
  // fixed volume.
  WaveformSelector {
    id: String,
    monome: String,
    rect: [i32; 4],
    /// The monome id whose play voices this strip re-timbres (usually `monome`
    /// itself; a different id cross-controls that grid instead).
    controls: String,
  },
  // A one-row volume strip overlaid on the edo grid (surfaces runtime). The 12 cells
  // right of the waveform selector on the top row; the active cell is lit, the rest
  // dark. Log-spaced over a fixed dB range; the top cell is unity. Like the waveform
  // strip it sets the loudness of the monome named by `controls` (its own grid in the
  // current rigs), and it is *live* (moving it rescales that grid's sounding voices,
  // not just future notes).
  VolumeStrip {
    id: String,
    monome: String,
    rect: [i32; 4],
    /// The monome id whose play voices this strip sets the volume of.
    controls: String,
  },
  // A single-cell on/off toggle for the GLOBAL distortion (surfaces runtime): the
  // summed synth mix runs through the soft-clipper while on. The scale/shape live on
  // the cpal_synth sink (`distortion_scale` / `distortion_shape`); this button is
  // just the live on/off.
  DistortionToggle {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // A single-cell on/off toggle for the slide feature (surfaces runtime): while on,
  // a new note re-triggers the nearest recently-released pitch and glides it into
  // the new one. Window/candidate knobs live in the `[surfaces]` table.
  SlideToggle {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // A single-cell on/off toggle for mono mode (surfaces runtime): while on, each new
  // note on a grid cuts off that grid's other fingered notes (and the slide
  // candidate set is effectively a singleton).
  MonoToggle {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // The 3x2 polyrhythm pad (surfaces runtime), by convention in the top-right:
  //   | x3 | x2 | tap |
  //   | /3 | /2 | =1  |
  // Tap twice within [surfaces].tap_tempo_window_ms to set the tapped tempo; the
  // factor buttons scale it (exact 2^a * 3^b); notes struck while a tempo is
  // applied pulse with a descending sawtooth at that tempo. See TODO/misc.org
  // "polyrhythm interface".
  TapTempoPad {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // A single-cell on/off toggle (surfaces runtime): while on, the KMSS pedals
  // 1/2/3 and 8/9/0 mirror the accrete button trio (clear / needs_holding /
  // accrete) instead of playing samples. See TODO/misc.org "feet accrete".
  SoftstepAccretesToggle {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // A single-cell sustain ("accrete") button overlaid on the edo grid (surfaces
  // runtime). Three of these per grid -- clear / needs_holding / accrete -- let
  // notes join a sustained set that rings after the fingers lift, until cleared.
  AccreteControl {
    id: String,
    monome: String,
    rect: [i32; 4],
    control: AccreteControlKind,
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
    // The save-undo double-press window in ms (6_plan 2.8/5). Loop-editor only.
    #[serde(default = "default_save_undo_ms")]
    save_undo_double_ms: u64,
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
fn default_save_undo_ms() -> u64 {
  200
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

// The surfaces sustain ("accrete") buttons -- see TODO/misc.org "sustain (accrete)".
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AccreteControlKind {
  /// Silence and flush the whole sustained set (key-down; lit while pressed).
  Clear,
  /// Toggle whether `accrete` AND `erase` must be *held* (vs toggling a mode).
  NeedsHolding,
  /// Hold (or toggle, per `needs_holding`) to add played/held notes to the
  /// sustained set, which rings until cleared.
  Accrete,
  /// Hold (or toggle, per `needs_holding`) to REMOVE pressed/held pitches from
  /// the sustained set -- each keeps sounding until its finger lifts. When both
  /// erase and accrete are live, erase wins. Optional (the trio is not).
  Erase,
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
      | MonomeWindowConfig::WaveformSelector { id, .. }
      | MonomeWindowConfig::VolumeStrip { id, .. }
      | MonomeWindowConfig::DistortionToggle { id, .. }
      | MonomeWindowConfig::SlideToggle { id, .. }
      | MonomeWindowConfig::MonoToggle { id, .. }
      | MonomeWindowConfig::SoftstepAccretesToggle { id, .. }
      | MonomeWindowConfig::TapTempoPad { id, .. }
      | MonomeWindowConfig::AccreteControl { id, .. }
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
      | MonomeWindowConfig::WaveformSelector { monome, .. }
      | MonomeWindowConfig::VolumeStrip { monome, .. }
      | MonomeWindowConfig::DistortionToggle { monome, .. }
      | MonomeWindowConfig::SlideToggle { monome, .. }
      | MonomeWindowConfig::MonoToggle { monome, .. }
      | MonomeWindowConfig::SoftstepAccretesToggle { monome, .. }
      | MonomeWindowConfig::TapTempoPad { monome, .. }
      | MonomeWindowConfig::AccreteControl { monome, .. }
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
      | MonomeWindowConfig::WaveformSelector { rect, .. }
      | MonomeWindowConfig::VolumeStrip { rect, .. }
      | MonomeWindowConfig::DistortionToggle { rect, .. }
      | MonomeWindowConfig::SlideToggle { rect, .. }
      | MonomeWindowConfig::MonoToggle { rect, .. }
      | MonomeWindowConfig::SoftstepAccretesToggle { rect, .. }
      | MonomeWindowConfig::TapTempoPad { rect, .. }
      | MonomeWindowConfig::AccreteControl { rect, .. }
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
      MonomeWindowConfig::WaveformSelector { .. } => "waveform_selector",
      MonomeWindowConfig::VolumeStrip { .. } => "volume_strip",
      MonomeWindowConfig::DistortionToggle { .. } => "distortion_toggle",
      MonomeWindowConfig::SlideToggle { .. } => "slide_toggle",
      MonomeWindowConfig::MonoToggle { .. } => "mono_toggle",
      MonomeWindowConfig::SoftstepAccretesToggle { .. } => "softstep_accretes_toggle",
      MonomeWindowConfig::TapTempoPad { .. } => "tap_tempo_pad",
      MonomeWindowConfig::AccreteControl { .. } => "accrete_control",
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

  /// A timbre_editor's target + its save-undo double-press window (ms), or None for
  /// other window kinds. Used by `resolve_settings` (save-undo is loop-editor only).
  pub fn timbre_editor_save_undo_ms(&self) -> Option<(TimbreTarget, u64)> {
    match self {
      MonomeWindowConfig::TimbreEditor { target, save_undo_double_ms, .. } => {
        Some((*target, *save_undo_double_ms))
      }
      _ => None,
    }
  }
}

/// One pedal's assignment inside a `drumkit` window: the printed pedal label
/// (1..9, then 0) and either a sample file to fire or `ditto = true`. `gain` is the
/// voice's FULL-velocity level for a sample pad; the tether runtime scales it down
/// for softer hits (see `drumkit_runtime::decode`). A `ditto` pad ignores `gain` --
/// it replays the most recently played sample in its window at THAT hit's already-
/// resolved (velocity-scaled) gain, "a generalized double-bass pedal" (see
/// `drumkit_runtime::mod` for the trigger logic). Validation requires exactly one of
/// `sample` / `ditto = true` per pad, and at most one ditto pad per window.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DrumPadConfig {
  pub pedal: u8,
  #[serde(default)]
  pub sample: Option<String>,
  #[serde(default = "default_pad_gain")]
  pub gain: f32,
  #[serde(default)]
  pub ditto: bool,
}

fn default_pad_gain() -> f32 {
  1.0
}

// --- Shared SoftStep detection & velocity parameters (configs/softstep.toml) ------------
// These drive the drumkit decoder. There is no reason for them to vary per config, so they
// live in one shared file loaded by `load_softstep_params`, not in each config's window.
fn default_on_sum() -> u16 {
  20
}
fn default_off_sum() -> u16 {
  20
}
fn default_attack_ms() -> u64 {
  14
}
fn default_velocity_full_scale() -> u16 {
  460
}
fn default_velocity_db_range() -> f32 {
  20.0
}
fn default_debounce_ms() -> u64 {
  100
}
fn default_silence_to_zero_ms() -> u64 {
  25
}

/// Hit-detection & velocity parameters for the SoftStep, shared by every config that uses
/// it. Loaded once from `configs/softstep.toml` (not from any per-config window), so one
/// set of numbers drives the drumkit. Every field is optional; a missing file or key uses
/// the default. The Python meter (`code/python/softstep/meter/`) mirrors these.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SoftstepParams {
  /// A hit fires when a pad's sum-of-4 exceeds this.
  #[serde(default = "default_on_sum")]
  pub on_sum: u16,
  /// The pad re-arms once its sum-of-4 falls below this.
  #[serde(default = "default_off_sum")]
  pub off_sum: u16,
  /// After firing on onset, keep watching this long to raise velocity to a later peak.
  #[serde(default = "default_attack_ms")]
  pub attack_ms: u64,
  /// Pad sum-of-4 (0..508) that maps to velocity 127.
  #[serde(default = "default_velocity_full_scale")]
  pub velocity_full_scale: u16,
  /// dB between the softest (vel 1) and hardest (vel 127) hit.
  #[serde(default = "default_velocity_db_range")]
  pub velocity_db_range: f32,
  /// Minimum gap between two hits on the SAME pad (contact-bounce guard); 0 = off.
  #[serde(default = "default_debounce_ms")]
  pub debounce_ms: u64,
  /// A sensor with no CC for this long reads 0 (de-stick); 0 = off.
  #[serde(default = "default_silence_to_zero_ms")]
  pub silence_to_zero_ms: u64,
}

impl Default for SoftstepParams {
  fn default() -> Self {
    SoftstepParams {
      on_sum: default_on_sum(),
      off_sum: default_off_sum(),
      attack_ms: default_attack_ms(),
      velocity_full_scale: default_velocity_full_scale(),
      velocity_db_range: default_velocity_db_range(),
      debounce_ms: default_debounce_ms(),
      silence_to_zero_ms: default_silence_to_zero_ms(),
    }
  }
}

/// Load the shared SoftStep parameters from `configs/softstep.toml`. A missing file uses
/// the defaults; a present file with a parse error or unknown key is a hard error.
pub fn load_softstep_params() -> Result<SoftstepParams, String> {
  let path = config_dir().join("softstep.toml");
  match std::fs::read_to_string(&path) {
    Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SoftstepParams::default()),
    Err(e) => Err(format!("{}: {e}", path.display())),
  }
}

/// A window over a SoftStep: a `kind` behavior bound to a declared `softstep`
/// device. The same windowing idiom as `monome_windows`; new arrangements are new
/// kinds (or new configs). Today the only kind is `drumkit`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SoftstepWindowConfig {
  /// Maps pedals to one-shot drum samples played through a `cpal_sampler` sink.
  /// Each pad's `sample` resolves under the top-level `drum-samples/` folder
  /// (`drum_samples_dir`); a `sample` may name a subpath for multi-kit layouts.
  Drumkit {
    id: String,
    softstep: String,
    sink: String,
    // Detection/velocity knobs (debounce, de-stick, thresholds) are NOT here -- they are
    // shared across configs and live in configs/softstep.toml (see `SoftstepParams`).
    pads: Vec<DrumPadConfig>,
  },
}

impl SoftstepWindowConfig {
  pub fn id(&self) -> &str {
    match self {
      SoftstepWindowConfig::Drumkit { id, .. } => id,
    }
  }

  pub fn softstep(&self) -> &str {
    match self {
      SoftstepWindowConfig::Drumkit { softstep, .. } => softstep,
    }
  }

  /// A human-readable kind label (matches the config `kind = "..."` tag).
  pub fn kind_name(&self) -> &'static str {
    match self {
      SoftstepWindowConfig::Drumkit { .. } => "drumkit",
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

/// Top-level folder holding drum-sample WAVs. A `drumkit` window's pads resolve
/// under here: `drum_samples_dir()/<pad.sample>` (a `sample` may name a subpath).
pub fn drum_samples_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("drum-samples")
}

/// Mock-rig configs (mock ports/prefixes) live here, separate from `config_dir` so
/// the real-config sweep (`loads_default_configs`) doesn't pick them up. Resolved by
/// name as a fallback (see `config_path`), so `<name>-mock` loads by name too.
pub fn mock_config_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("code/rust/mocks")
}

pub fn config_path(name: &str) -> Result<PathBuf, String> {
  if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
    return Err(format!("config name must not be a path, got {name:?}"));
  }
  // Prefer a real config; fall back to the mock dir so a mock-rig config resolves by
  // name (the integration tests and `cargo run -- <name>-mock`). If neither exists,
  // return the configs/ path so the not-found error names the expected location.
  let primary = config_dir().join(name).with_extension("toml");
  if !primary.exists() {
    let mock = mock_config_dir().join(name).with_extension("toml");
    if mock.exists() {
      return Ok(mock);
    }
  }
  Ok(primary)
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
  require_unique("softstep", config.softsteps.iter().map(|s| s.id.as_str()))?;
  require_unique(
    "softstep window",
    config.softstep_windows.iter().map(SoftstepWindowConfig::id),
  )?;
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
      oversample,
      ..
    } = sink {
      if *sample_rate == 0 || *buffer_frames == 0 {
        return Err(format!("sink {:?} sample_rate and buffer_frames must be positive", sink.id()));
      }
      if *oversample == 0 {
        return Err(format!("sink {:?} oversample must be >= 1", sink.id()));
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
    if let SinkConfig::CpalSampler { sample_rate, buffer_frames, amplitude, .. } = sink {
      if *sample_rate == 0 || *buffer_frames == 0 {
        return Err(format!("sink {:?} sample_rate and buffer_frames must be positive", sink.id()));
      }
      if !amplitude.is_finite() || *amplitude < 0.0 {
        return Err(format!("sink {:?} amplitude must be nonnegative", sink.id()));
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
  validate_shift_pads(config)?;
  validate_waveform_selectors(config)?;
  validate_volume_strips(config)?;
  validate_accrete_controls(config)?;
  validate_single_cell_toggles(config)?;
  validate_tap_tempo_pads(config)?;
  validate_timbres(config)?;
  validate_looper(config)?;
  validate_softsteps(config)?;

  Ok(())
}

/// The `[[timbres]]` table: when present it must have exactly four entries (the
/// waveform strip has four cells), and every numeric parameter must be in range.
fn validate_timbres(config: &Config) -> Result<(), String> {
  if config.timbres.is_empty() {
    return Ok(());
  }
  if config.timbres.len() != 4 {
    return Err(format!(
      "[[timbres]] must have exactly 4 entries (the waveform strip's four cells), got {}",
      config.timbres.len(),
    ));
  }
  for (i, t) in config.timbres.iter().enumerate() {
    if t.amplitude < 0.0 {
      return Err(format!("timbre {i}: amplitude must be >= 0, got {}", t.amplitude));
    }
    if !(0.0..=1.0).contains(&t.am_depth) {
      return Err(format!("timbre {i}: am_depth must be in 0..=1, got {}", t.am_depth));
    }
    if !(0.0..=1.0).contains(&t.am_shape) {
      return Err(format!("timbre {i}: am_shape must be in 0..=1, got {}", t.am_shape));
    }
    if t.fm_depth_cents < 0.0 {
      return Err(format!("timbre {i}: fm_depth_cents must be >= 0, got {}", t.fm_depth_cents));
    }
    if t.am_freq <= 0.0 || t.fm_freq <= 0.0 {
      return Err(format!("timbre {i}: am_freq/fm_freq must be > 0"));
    }
  }
  Ok(())
}

/// A `tap_tempo_pad` is exactly 3x2 cells on a monome that has an `edo_note_grid`,
/// at most one per monome (its six sub-cells are the fixed x3/x2/tap and /3/2/=1
/// layout).
fn validate_tap_tempo_pads(config: &Config) -> Result<(), String> {
  let mut seen: HashSet<&str> = HashSet::new();
  for window in &config.monome_windows {
    let MonomeWindowConfig::TapTempoPad { id, monome, rect } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if x1 - x0 != 2 || y1 - y0 != 1 {
      return Err(format!(
        "tap_tempo_pad window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must be exactly 3x2 cells",
      ));
    }
    let has_edo_grid = config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "tap_tempo_pad window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = config
      .monomes
      .iter()
      .find(|m| m.id == *monome)
      .and_then(|m| m.select.size)
      .unwrap_or([16, 16]);
    if x0 < 0 || y0 < 0 || x1 >= gw || y1 >= gh {
      return Err(format!(
        "tap_tempo_pad window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must fit the {gw}x{gh} grid",
      ));
    }
    if !seen.insert(monome.as_str()) {
      return Err(format!("monome {monome:?} declares more than one tap_tempo_pad (window {id:?})"));
    }
  }
  Ok(())
}

/// The single-cell global toggles (`distortion_toggle`, `slide_toggle`,
/// `mono_toggle`): each is one cell on a monome that has an `edo_note_grid`, at most
/// one of each kind per monome (a second would be a redundant twin of the same
/// global switch).
fn validate_single_cell_toggles(config: &Config) -> Result<(), String> {
  // (kind label, id, monome, rect) for every toggle window.
  let toggles = config.monome_windows.iter().filter_map(|w| match w {
    MonomeWindowConfig::DistortionToggle { id, monome, rect } => {
      Some(("distortion_toggle", id, monome, *rect))
    }
    MonomeWindowConfig::SlideToggle { id, monome, rect } => {
      Some(("slide_toggle", id, monome, *rect))
    }
    MonomeWindowConfig::MonoToggle { id, monome, rect } => {
      Some(("mono_toggle", id, monome, *rect))
    }
    MonomeWindowConfig::SoftstepAccretesToggle { id, monome, rect } => {
      Some(("softstep_accretes_toggle", id, monome, *rect))
    }
    _ => None,
  });
  let mut seen: HashSet<(&str, &str)> = HashSet::new();
  for (kind, id, monome, rect) in toggles {
    let [x0, y0, x1, y1] = rect;
    if x0 != x1 || y0 != y1 {
      return Err(format!(
        "{kind} window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must cover exactly one cell",
      ));
    }
    let has_edo_grid = config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "{kind} window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = config
      .monomes
      .iter()
      .find(|m| m.id == *monome)
      .and_then(|m| m.select.size)
      .unwrap_or([16, 16]);
    if x0 < 0 || y0 < 0 || x1 >= gw || y1 >= gh {
      return Err(format!(
        "{kind} window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must fit the {gw}x{gh} grid",
      ));
    }
    if !seen.insert((kind, monome.as_str())) {
      return Err(format!("monome {monome:?} declares more than one {kind} (window {id:?})"));
    }
  }
  Ok(())
}

/// The `accrete_control` sustain buttons (surfaces runtime). Each is a single cell on
/// a monome that has an `edo_note_grid` (the play surface whose notes it sustains).
/// Per monome the trio is all-or-nothing -- declaring any accrete_control requires
/// clear / needs_holding / accrete, each at most once -- since the trio only makes
/// sense together (accrete with no clear would be an un-silenceable drone). The
/// `erase` kind is optional on top of the trio (misc.org "erase button").
fn validate_accrete_controls(config: &Config) -> Result<(), String> {
  let mut per_monome: HashMap<&str, Vec<AccreteControlKind>> = HashMap::new();
  for window in &config.monome_windows {
    let MonomeWindowConfig::AccreteControl { id, monome, rect, control } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if x0 != x1 || y0 != y1 {
      return Err(format!(
        "accrete_control window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must cover exactly one cell",
      ));
    }
    let has_edo_grid = config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "accrete_control window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = config
      .monomes
      .iter()
      .find(|m| m.id == *monome)
      .and_then(|m| m.select.size)
      .unwrap_or([16, 16]);
    if x0 < 0 || y0 < 0 || x1 >= gw || y1 >= gh {
      return Err(format!(
        "accrete_control window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must fit the {gw}x{gh} grid",
      ));
    }
    let kinds = per_monome.entry(monome.as_str()).or_default();
    if kinds.contains(control) {
      return Err(format!(
        "duplicate accrete_control kind {control:?} on monome {monome:?} (in window {id:?})",
      ));
    }
    kinds.push(*control);
  }
  for (monome, kinds) in &per_monome {
    for required in
      [AccreteControlKind::Clear, AccreteControlKind::NeedsHolding, AccreteControlKind::Accrete]
    {
      if !kinds.contains(&required) {
        return Err(format!(
          "monome {monome:?} declares accrete_control windows but is missing kind {required:?} \
           (the clear / needs_holding / accrete trio is all-or-nothing per monome)",
        ));
      }
    }
  }
  Ok(())
}

/// A `volume_strip` is the surfaces runtime's one-row loudness fader. Its rect must be a
/// single row at least two cells wide, and `controls` must name a declared monome that
/// has an `edo_note_grid` (the grid whose voices it sets the volume of) -- the same
/// cross-grid wiring as `waveform_selector`.
fn validate_volume_strips(config: &Config) -> Result<(), String> {
  let monome_ids: HashSet<&str> = config.monomes.iter().map(|m| m.id.as_str()).collect();
  for window in &config.monome_windows {
    let MonomeWindowConfig::VolumeStrip { id, monome, rect, controls } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if y1 != y0 || x1 - x0 < 1 {
      return Err(format!(
        "volume_strip window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must be one row, >= 2 cells wide",
      ));
    }
    require_ref("volume_strip.controls", controls, &monome_ids)?;
    let controlled_has_grid = config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::EdoNoteGrid { monome: m, .. } if m == controls)
    });
    if !controlled_has_grid {
      return Err(format!(
        "volume_strip window {id:?} controls monome {controls:?}, which has no edo_note_grid",
      ));
    }
    let [gw, gh] = config
      .monomes
      .iter()
      .find(|m| m.id == *monome)
      .and_then(|m| m.select.size)
      .unwrap_or([16, 16]);
    if x0 < 0 || y0 < 0 || x1 >= gw || y1 >= gh {
      return Err(format!(
        "volume_strip window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must fit the {gw}x{gh} grid",
      ));
    }
  }
  Ok(())
}

/// A `waveform_selector` is the surfaces runtime's 4-cell timbre picker. Its rect
/// must cover exactly the four waveform cells (one row, four wide), and `controls`
/// must name a declared monome that has an `edo_note_grid` (the grid whose voices
/// it re-timbres). `controls` may differ from the selector's own `monome` -- that
/// cross-grid wiring is exactly what the surfaces config uses.
fn validate_waveform_selectors(config: &Config) -> Result<(), String> {
  let monome_ids: HashSet<&str> = config.monomes.iter().map(|m| m.id.as_str()).collect();
  for window in &config.monome_windows {
    let MonomeWindowConfig::WaveformSelector { id, monome, rect, controls } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if x1 - x0 != 3 || y1 != y0 {
      return Err(format!(
        "waveform_selector window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must cover exactly 4 cells in one row",
      ));
    }
    require_ref("waveform_selector.controls", controls, &monome_ids)?;
    let controlled_has_grid = config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::EdoNoteGrid { monome: m, .. } if m == controls)
    });
    if !controlled_has_grid {
      return Err(format!(
        "waveform_selector window {id:?} controls monome {controls:?}, which has no edo_note_grid",
      ));
    }
    // The strip is drawn on its own grid, so its rect must fit that monome.
    let [gw, gh] = config
      .monomes
      .iter()
      .find(|m| m.id == *monome)
      .and_then(|m| m.select.size)
      .unwrap_or([16, 16]);
    if x0 < 0 || y0 < 0 || x1 >= gw || y1 >= gh {
      return Err(format!(
        "waveform_selector window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must fit the {gw}x{gh} grid",
      ));
    }
  }
  Ok(())
}

/// An `edo_shift_pad` is a generally-valid window (not tied to the looper stack):
/// it needs an `edo_note_grid` on the *same* monome to scroll, and its rect must
/// fit that monome's grid. This runs for every config, so a plain sawwave grid,
/// the surfaces runtime's grids, and the looper all validate a shift pad the same
/// way. (The looper's own `validate_looper` separately checks the edo grid fits.)
fn validate_shift_pads(config: &Config) -> Result<(), String> {
  for window in &config.monome_windows {
    let MonomeWindowConfig::EdoShiftPad { id, monome, rect } = window else {
      continue;
    };
    let has_edo_grid = config.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowConfig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "edo_shift_pad window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = config
      .monomes
      .iter()
      .find(|m| m.id == *monome)
      .and_then(|m| m.select.size)
      .unwrap_or([16, 16]);
    let [x0, y0, x1, y1] = *rect;
    if x0 < 0 || y0 < 0 || x1 >= gw || y1 >= gh {
      return Err(format!(
        "edo_shift_pad window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must fit the {gw}x{gh} grid",
      ));
    }
  }
  Ok(())
}

/// SoftStep windows: every window must bind a declared softstep and a declared
/// `cpal_sampler` sink; pedals are the 10 printed labels 0..9, claimed by at most
/// one window per device; sample filenames must be safe and present. Validation is
/// filesystem-free (sample *existence* is checked at load time, with a clear error)
/// so configs stay unit-testable without fixture WAVs.
fn validate_softsteps(config: &Config) -> Result<(), String> {
  let softstep_ids: HashSet<&str> = config.softsteps.iter().map(|s| s.id.as_str()).collect();
  let sampler_sink_ids: HashSet<&str> = config
    .sinks
    .iter()
    .filter(|s| matches!(s, SinkConfig::CpalSampler { .. }))
    .map(SinkConfig::id)
    .collect();

  // pedals already claimed on a given device, so windows partition (never overlap).
  let mut claimed: std::collections::HashMap<&str, HashSet<u8>> = std::collections::HashMap::new();

  for window in &config.softstep_windows {
    require_ref("softstep_window.softstep", window.softstep(), &softstep_ids)?;
    match window {
      SoftstepWindowConfig::Drumkit { id, sink, pads, .. } => {
        if !sampler_sink_ids.contains(sink.as_str()) {
          return Err(format!(
            "drumkit window {id:?} sink {sink:?} must be a declared cpal_sampler sink",
          ));
        }
        if pads.is_empty() {
          return Err(format!("drumkit window {id:?} needs at least one pad"));
        }
        let device_claims = claimed.entry(window.softstep()).or_default();
        let mut this_window: HashSet<u8> = HashSet::new();
        // At most one ditto pad per window (it retriggers "the" last hit in this
        // window; two would leave which-one-means-what ambiguous).
        let mut ditto_pedal: Option<u8> = None;
        for pad in pads {
          if pad.pedal > 9 {
            return Err(format!(
              "drumkit window {id:?} pedal {} out of range (the KMSS labels are 0..9)",
              pad.pedal,
            ));
          }
          if !this_window.insert(pad.pedal) {
            return Err(format!(
              "drumkit window {id:?} assigns pedal {} more than once",
              pad.pedal,
            ));
          }
          if !device_claims.insert(pad.pedal) {
            return Err(format!(
              "pedal {} on softstep {:?} is claimed by more than one window",
              pad.pedal,
              window.softstep(),
            ));
          }
          if pad.sample.is_some() == pad.ditto {
            return Err(format!(
              "drumkit window {id:?} pad {} needs exactly one of `sample` or `ditto = true`",
              pad.pedal,
            ));
          }
          if pad.ditto {
            if let Some(first) = ditto_pedal {
              return Err(format!(
                "drumkit window {id:?} has two ditto pads (pedals {first} and {}); at most one per window",
                pad.pedal,
              ));
            }
            ditto_pedal = Some(pad.pedal);
            continue; // no gain/sample-path checks for a ditto pad
          }
          if !pad.gain.is_finite() || pad.gain < 0.0 {
            return Err(format!("drumkit window {id:?} pad {} gain must be nonnegative", pad.pedal));
          }
          let sample = pad.sample.as_ref().expect("checked above: sample is Some when ditto is false");
          validate_asset_subpath("drumkit sample", id, sample)?;
        }
      }
    }
  }

  // No orphan devices: every declared softstep must be used by some window.
  for softstep in &config.softsteps {
    let used = config.softstep_windows.iter().any(|w| w.softstep() == softstep.id);
    if !used {
      return Err(format!(
        "config declares softstep {:?} but no window uses it",
        softstep.id,
      ));
    }
  }

  Ok(())
}

/// A sample/dir path must be a non-empty *relative* subpath of the assets root --
/// no absolute paths and no `..` components escaping it.
fn validate_asset_subpath(label: &str, id: &str, value: &str) -> Result<(), String> {
  if value.is_empty() {
    return Err(format!("{label} in window {id:?} must not be empty"));
  }
  let path = Path::new(value);
  if path.is_absolute() {
    return Err(format!("{label} {value:?} in window {id:?} must be a relative path"));
  }
  if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
    return Err(format!("{label} {value:?} in window {id:?} must not contain '..'"));
  }
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
  // NB: edo_shift_pad is deliberately NOT in this set. It is a generally-valid
  // window (validated by `validate_shift_pads`), so a plain edo grid -- or the
  // surfaces runtime's two grids -- can carry a scroll pad without pulling in the
  // whole looper stack. Looper configs still trip this via their loop_display etc.
  let any_loop_window = config.monome_windows.iter().any(|w| {
    matches!(
      w,
      MonomeWindowConfig::LoopSlots { .. }
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

  if !(looper.clock_bpm > 0.0) {
    return Err("looper clock_bpm must be positive".to_string());
  }
  if !(looper.clock_duty > 0.0 && looper.clock_duty <= 1.0) {
    return Err("looper clock_duty must be in (0, 1]".to_string());
  }

  // The looper scalars must be positive.
  for (name, value) in [
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
      // softstep.toml is a shared SoftstepParams file, not a full runnable Config.
      if path.file_name().and_then(|s| s.to_str()) == Some("softstep.toml") {
        continue;
      }
      if path.extension().and_then(|s| s.to_str()) == Some("toml") {
        load_config_file(&path)
          .unwrap_or_else(|e| panic!("{} should parse: {e}", path.display()));
      }
    }
  }

  #[test]
  fn softstep_params_parse_and_default() {
    // A missing file -> defaults (debounce is now 100 ms).
    assert_eq!(SoftstepParams::default().debounce_ms, 100, "default debounce");
    assert_eq!(SoftstepParams::default().on_sum, 20);
    // The shipped softstep.toml parses and every field is present.
    let params = load_softstep_params().expect("configs/softstep.toml parses");
    assert!(params.velocity_full_scale > 0 && params.attack_ms > 0);
    // Partial files fill in defaults; unknown keys are rejected.
    let partial: SoftstepParams = toml::from_str("on_sum = 7").unwrap();
    assert_eq!(partial.on_sum, 7);
    assert_eq!(partial.debounce_ms, 100, "unset key uses the default");
    assert!(toml::from_str::<SoftstepParams>("bogus = 1").is_err(), "unknown key rejected");
  }

  #[test]
  fn resolves_and_loads_mock_configs_by_name() {
    // Mock-rig configs live in mocks/, not configs/, but must resolve by name so the
    // mock-grid tests and `cargo run -- <name>-mock` find them.
    let name = "monome-looper-58-8-1-timbre-mock";
    let path = config_path(name).expect("mock name resolves");
    assert!(path.starts_with(mock_config_dir()), "should resolve into mocks/, got {}", path.display());
    load_named_config(name).expect("mock config loads from the mock dir");
    // A genuinely missing name still points at configs/ (the not-found error location).
    assert!(config_path("definitely-not-a-real-config").unwrap().starts_with(config_dir()));
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
clock_bpm = 300
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
  fn clock_duty_must_be_in_range() {
    let err = parse_config(&valid_looper_toml().replace("clock_bpm = 300", "clock_bpm = 300\nclock_duty = 1.5"))
      .expect_err("a clock_duty above 1 should fail");
    assert!(err.contains("clock_duty"), "{err}");
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
  fn save_undo_double_ms_parses_and_defaults() {
    let su = |toml: &str| {
      parse_config(toml)
        .unwrap()
        .monome_windows
        .iter()
        .find_map(MonomeWindowConfig::timbre_editor_save_undo_ms)
    };
    // Omitted -> default 200 ms.
    assert_eq!(su(&looper_with_editor(TIMBRE_EDITOR_MIN)), Some((TimbreTarget::Loop, 200)));
    // Explicit value parses.
    let explicit = format!("{TIMBRE_EDITOR_MIN}save_undo_double_ms = 350\n");
    assert_eq!(su(&looper_with_editor(&explicit)), Some((TimbreTarget::Loop, 350)));
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

  // ---- KMSS drumkit (softstep windows) ----

  const DRUMKIT_TOML: &str = r#"version = 1
id = "kmss-drumkit"
title = "KMSS Drumkit"

[[softsteps]]
id = "feet"

[[sinks]]
id = "drums"
kind = "cpal_sampler"
sample_rate = 48000
buffer_frames = 128
amplitude = 0.8

[[softstep_windows]]
id = "kit"
softstep = "feet"
kind = "drumkit"
sink = "drums"
pads = [
  { pedal = 1, sample = "kick.wav" },
  { pedal = 2, sample = "snare.wav" },
  { pedal = 0, sample = "wood_block.wav" },
]
"#;

  #[test]
  fn drumkit_config_is_valid_with_defaults() {
    let config = parse_config(DRUMKIT_TOML).expect("a complete drumkit config should be valid");
    assert_eq!(config.softsteps.len(), 1);
    assert_eq!(config.softsteps[0].select.name_substring(), "SSCOM", "default select substring");
    let SoftstepWindowConfig::Drumkit { pads, .. } = &config.softstep_windows[0];
    assert_eq!(pads[0].gain, 1.0, "default pad gain");
    assert_eq!(pads.len(), 3);
  }

  #[test]
  fn drumkit_pedal_out_of_range_is_rejected() {
    let err = parse_config(&DRUMKIT_TOML.replace("pedal = 1,", "pedal = 10,"))
      .expect_err("pedal 10 is out of the 0..9 label range");
    assert!(err.contains("out of range"), "{err}");
  }

  #[test]
  fn drumkit_duplicate_pedal_in_one_window_is_rejected() {
    let err = parse_config(&DRUMKIT_TOML.replace("pedal = 2,", "pedal = 1,"))
      .expect_err("the same pedal assigned twice should fail");
    assert!(err.contains("more than once"), "{err}");
  }

  #[test]
  fn drumkit_sink_must_be_a_cpal_sampler() {
    let err = parse_config(&DRUMKIT_TOML.replace("kind = \"cpal_sampler\"", "kind = \"midi\"")
      // a midi sink has no sample_rate/buffer_frames/amplitude fields
      .replace("sample_rate = 48000\nbuffer_frames = 128\namplitude = 0.8\n", ""))
      .expect_err("a non-sampler sink should be rejected for a drumkit");
    assert!(err.contains("cpal_sampler"), "{err}");
  }

  #[test]
  fn drumkit_unknown_softstep_ref_is_rejected() {
    let err = parse_config(&DRUMKIT_TOML.replace("softstep = \"feet\"", "softstep = \"nope\""))
      .expect_err("an unknown softstep reference should fail");
    assert!(err.contains("unknown id \"nope\""), "{err}");
  }

  #[test]
  fn drumkit_orphan_softstep_is_rejected() {
    let toml = format!("{DRUMKIT_TOML}\n[[softsteps]]\nid = \"unused\"\n");
    let err = parse_config(&toml).expect_err("a softstep no window uses should fail");
    assert!(err.contains("no window uses it"), "{err}");
  }

  #[test]
  fn drumkit_sample_path_must_not_escape_assets() {
    let err = parse_config(&DRUMKIT_TOML.replace("\"kick.wav\"", "\"../secrets/kick.wav\""))
      .expect_err("a sample path with .. should fail");
    assert!(err.contains("'..'"), "{err}");
  }

  #[test]
  fn pedal_claimed_by_two_windows_is_rejected() {
    let toml = format!(
      "{DRUMKIT_TOML}\n[[softstep_windows]]\nid = \"kit2\"\nsoftstep = \"feet\"\nkind = \"drumkit\"\nsink = \"drums\"\npads = [{{ pedal = 1, sample = \"other.wav\" }}]\n",
    );
    let err = parse_config(&toml).expect_err("two windows claiming pedal 1 should fail");
    assert!(err.contains("more than one window"), "{err}");
  }

  // ---- ditto pads ----

  #[test]
  fn ditto_pad_is_valid_with_no_sample() {
    let toml = DRUMKIT_TOML.replace("{ pedal = 0, sample = \"wood_block.wav\" },", "{ pedal = 0, ditto = true },");
    let config = parse_config(&toml).expect("a ditto pad with no sample should be valid");
    let SoftstepWindowConfig::Drumkit { pads, .. } = &config.softstep_windows[0];
    let ditto = pads.iter().find(|p| p.pedal == 0).expect("pedal 0");
    assert!(ditto.ditto, "ditto flag set");
    assert_eq!(ditto.sample, None, "a ditto pad names no sample");
  }

  #[test]
  fn pad_with_neither_sample_nor_ditto_is_rejected() {
    let toml = DRUMKIT_TOML.replace("{ pedal = 0, sample = \"wood_block.wav\" },", "{ pedal = 0 },");
    let err = parse_config(&toml).expect_err("a pad with neither sample nor ditto should fail");
    assert!(err.contains("exactly one of `sample` or `ditto = true`"), "{err}");
  }

  #[test]
  fn pad_with_both_sample_and_ditto_is_rejected() {
    let toml = DRUMKIT_TOML.replace(
      "{ pedal = 0, sample = \"wood_block.wav\" },",
      "{ pedal = 0, sample = \"wood_block.wav\", ditto = true },",
    );
    let err = parse_config(&toml).expect_err("a pad with both sample and ditto should fail");
    assert!(err.contains("exactly one of `sample` or `ditto = true`"), "{err}");
  }

  #[test]
  fn two_ditto_pads_in_one_window_is_rejected() {
    let toml = format!(
      "{}\n",
      DRUMKIT_TOML
        .replace("{ pedal = 0, sample = \"wood_block.wav\" },", "{ pedal = 0, ditto = true },")
        .replace("{ pedal = 2, sample = \"snare.wav\" },", "{ pedal = 2, ditto = true },"),
    );
    let err = parse_config(&toml).expect_err("two ditto pads in one window should fail");
    assert!(err.contains("two ditto pads"), "{err}");
  }

  // ---- edo_shift_pad as a generally-valid window (freed from the looper) ----

  #[test]
  fn shift_pad_on_a_plain_grid_is_valid() {
    // A plain sawwave grid + a scroll pad, with NO looper stack, must parse:
    // the shift pad is no longer trapped in the looper's all-or-nothing set.
    parse_config(r#"
version = 1
id = "grid-plus-scroll"
title = "Grid plus scroll"

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

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/big"
select.size = [16, 16]

[[monome_windows]]
id = "edo-grid"
monome = "big"
kind = "edo_note_grid"
rect = [0, 0, 15, 15]
tuning = "main"
sink = "saw"

[[monome_windows]]
id = "scroll"
monome = "big"
kind = "edo_shift_pad"
rect = [13, 14, 15, 15]
"#).expect("a plain grid with a scroll pad should be valid");
  }

  #[test]
  fn shift_pad_without_an_edo_grid_is_rejected() {
    let err = parse_config(r#"
version = 1
id = "orphan-scroll"
title = "Orphan scroll"

[[monomes]]
id = "big"
listen_port = 9000
prefix = "/big"

[[monome_windows]]
id = "scroll"
monome = "big"
kind = "edo_shift_pad"
rect = [13, 14, 15, 15]
"#).expect_err("a shift pad with no edo grid on its monome should fail");
    assert!(err.contains("needs an edo_note_grid on the same monome"), "{err}");
  }

  // ---- waveform_selector (surfaces cross-grid timbre picker) ----

  const SURFACES_MIN: &str = r#"version = 1
id = "surfaces-min"
title = "Surfaces min"

[[tunings]]
id = "main"
edo = 58
x_step = 8
y_step = 1
fundamental_hz = 80

[[sinks]]
id = "synth"
kind = "cpal_synth"
sample_rate = 48000
buffer_frames = 128
amplitude = 0.15
attack_secs = 0.003
release_secs = 0.05
accretion_level = 0.5

[[monomes]]
id = "a"
listen_port = 9000
prefix = "/a"
select.size = [16, 16]

[[monomes]]
id = "b"
listen_port = 9001
prefix = "/b"
select.size = [16, 16]

[[monome_windows]]
id = "grid-a"
monome = "a"
kind = "edo_note_grid"
rect = [0, 0, 15, 15]
tuning = "main"
sink = "synth"

[[monome_windows]]
id = "wave-a"
monome = "a"
kind = "waveform_selector"
rect = [0, 0, 3, 0]
controls = "b"

[[monome_windows]]
id = "grid-b"
monome = "b"
kind = "edo_note_grid"
rect = [0, 0, 15, 15]
tuning = "main"
sink = "synth"

[[monome_windows]]
id = "wave-b"
monome = "b"
kind = "waveform_selector"
rect = [0, 0, 3, 0]
controls = "a"
"#;

  #[test]
  fn waveform_selector_cross_control_is_valid() {
    let config = parse_config(SURFACES_MIN).expect("two grids that cross-control timbre should be valid");
    let selectors: Vec<(&str, &str)> = config
      .monome_windows
      .iter()
      .filter_map(|w| match w {
        MonomeWindowConfig::WaveformSelector { monome, controls, .. } => Some((monome.as_str(), controls.as_str())),
        _ => None,
      })
      .collect();
    assert!(selectors.contains(&("a", "b")), "grid a's strip controls b");
    assert!(selectors.contains(&("b", "a")), "grid b's strip controls a");
  }

  #[test]
  fn waveform_selector_must_cover_four_cells() {
    let err = parse_config(&SURFACES_MIN.replace("rect = [0, 0, 3, 0]\ncontrols = \"b\"", "rect = [0, 0, 2, 0]\ncontrols = \"b\""))
      .expect_err("a 3-wide selector should fail");
    assert!(err.contains("exactly 4 cells"), "{err}");
  }

  #[test]
  fn waveform_selector_controls_must_be_a_declared_monome() {
    let err = parse_config(&SURFACES_MIN.replace("controls = \"b\"", "controls = \"nope\""))
      .expect_err("an unknown controls target should fail");
    assert!(err.contains("unknown id \"nope\""), "{err}");
  }

  #[test]
  fn waveform_selector_controls_must_have_an_edo_grid() {
    // Point a's strip at a monome with no edo_note_grid.
    let toml = SURFACES_MIN
      .replace("controls = \"b\"", "controls = \"c\"")
      .replace(
        "[[monome_windows]]\nid = \"grid-a\"",
        "[[monomes]]\nid = \"c\"\nlisten_port = 9002\nprefix = \"/c\"\n\n[[monome_windows]]\nid = \"grid-a\"",
      )
      // c must be used by *some* window or an orphan check could fire elsewhere; give it a lone strip.
      + "\n[[monome_windows]]\nid = \"wave-c\"\nmonome = \"c\"\nkind = \"waveform_selector\"\nrect = [0, 0, 3, 0]\ncontrols = \"a\"\n";
    let err = parse_config(&toml).expect_err("controlling a gridless monome should fail");
    assert!(err.contains("no edo_note_grid"), "{err}");
  }

  // ---- accrete_control (surfaces sustain buttons) ----

  /// The clear / needs_holding / accrete trio on monome "a", bottom row left.
  const ACCRETE_TRIO: &str = r#"
[[monome_windows]]
id = "acc-clear"
monome = "a"
kind = "accrete_control"
rect = [0, 15, 0, 15]
control = "clear"

[[monome_windows]]
id = "acc-hold"
monome = "a"
kind = "accrete_control"
rect = [1, 15, 1, 15]
control = "needs_holding"

[[monome_windows]]
id = "acc-accrete"
monome = "a"
kind = "accrete_control"
rect = [2, 15, 2, 15]
control = "accrete"
"#;

  #[test]
  fn accrete_control_trio_is_valid() {
    let toml = format!("{SURFACES_MIN}{ACCRETE_TRIO}");
    let config = parse_config(&toml).expect("a full accrete trio should validate");
    let kinds: Vec<AccreteControlKind> = config
      .monome_windows
      .iter()
      .filter_map(|w| match w {
        MonomeWindowConfig::AccreteControl { control, .. } => Some(*control),
        _ => None,
      })
      .collect();
    assert_eq!(kinds.len(), 3, "all three buttons parse");
    assert!(kinds.contains(&AccreteControlKind::Clear));
    assert!(kinds.contains(&AccreteControlKind::NeedsHolding));
    assert!(kinds.contains(&AccreteControlKind::Accrete));
  }

  #[test]
  fn accrete_control_trio_is_all_or_nothing_per_monome() {
    // Drop the accrete button: the trio is incomplete, so the config must fail.
    let cut = ACCRETE_TRIO.find("\n[[monome_windows]]\nid = \"acc-accrete\"").unwrap();
    let toml = format!("{SURFACES_MIN}{}", &ACCRETE_TRIO[..cut]);
    let err = parse_config(&toml).expect_err("a partial trio should fail");
    assert!(err.contains("missing kind Accrete"), "{err}");
  }

  #[test]
  fn accrete_control_must_be_a_single_cell() {
    let toml =
      format!("{SURFACES_MIN}{}", ACCRETE_TRIO.replace("rect = [0, 15, 0, 15]", "rect = [0, 15, 1, 15]"));
    let err = parse_config(&toml).expect_err("a 2-cell accrete button should fail");
    assert!(err.contains("exactly one cell"), "{err}");
  }

  #[test]
  fn accrete_control_kinds_must_be_unique_per_monome() {
    let toml = format!(
      "{SURFACES_MIN}{}",
      ACCRETE_TRIO.replace("control = \"needs_holding\"", "control = \"clear\""),
    );
    let err = parse_config(&toml).expect_err("two clear buttons on one monome should fail");
    assert!(err.contains("duplicate accrete_control kind"), "{err}");
  }

  #[test]
  fn accrete_control_needs_an_edo_grid_on_its_monome() {
    // Declare the trio on a monome that has no edo_note_grid.
    let toml = format!(
      "{SURFACES_MIN}\n[[monomes]]\nid = \"c\"\nlisten_port = 9002\nprefix = \"/c\"\n{}",
      ACCRETE_TRIO.replace("monome = \"a\"", "monome = \"c\""),
    );
    let err = parse_config(&toml).expect_err("accrete buttons on a gridless monome should fail");
    assert!(err.contains("needs an edo_note_grid"), "{err}");
  }

  // ---- distortion_toggle (surfaces global distortion on/off) ----

  const DISTORTION_TOGGLE: &str = r#"
[[monome_windows]]
id = "dist-a"
monome = "a"
kind = "distortion_toggle"
rect = [0, 1, 0, 1]
"#;

  #[test]
  fn distortion_toggle_is_valid_and_sink_defaults_apply() {
    let toml = format!("{SURFACES_MIN}{DISTORTION_TOGGLE}");
    let config = parse_config(&toml).expect("a single-cell distortion toggle validates");
    assert!(config
      .monome_windows
      .iter()
      .any(|w| matches!(w, MonomeWindowConfig::DistortionToggle { .. })));
    // The sink's distortion curve defaults (scale 1.0, shape 2.0) fill in when absent.
    let SinkConfig::CpalSynth { distortion_scale, distortion_shape, .. } = &config.sinks[0] else {
      panic!("first sink is the synth");
    };
    assert_eq!(*distortion_scale, 1.0);
    assert_eq!(*distortion_shape, 2.0);
  }

  #[test]
  fn distortion_toggle_must_be_a_single_cell() {
    let toml = format!(
      "{SURFACES_MIN}{}",
      DISTORTION_TOGGLE.replace("rect = [0, 1, 0, 1]", "rect = [0, 1, 1, 1]"),
    );
    let err = parse_config(&toml).expect_err("a 2-cell toggle should fail");
    assert!(err.contains("exactly one cell"), "{err}");
  }

  // ---- [[timbres]] (the four selectable timbres behind the waveform strip) ----

  const TIMBRES_FOUR: &str = r#"
[[timbres]]
waveform = "sin"
[[timbres]]
waveform = "tri"
am_depth = 0.5
am_freq = 3.0
am_shape = 1.0
[[timbres]]
waveform = "square"
amplitude = 0.6
[[timbres]]
waveform = "saw"
fm_depth_cents = 30.0
fm_freq = 6.0
"#;

  #[test]
  fn timbres_parse_with_short_names_and_off_defaults() {
    let toml = format!("{SURFACES_MIN}{TIMBRES_FOUR}");
    let config = parse_config(&toml).expect("a 4-entry [[timbres]] table validates");
    assert_eq!(config.timbres.len(), 4);
    assert_eq!(config.timbres[0].waveform, WaveformChoice::Sine, "'sin' alias");
    assert_eq!(config.timbres[1].waveform, WaveformChoice::Triangle, "'tri' alias");
    // Defaults: full amplitude, modulation off.
    assert_eq!(config.timbres[0].amplitude, 1.0);
    assert_eq!(config.timbres[0].am_depth, 0.0);
    assert_eq!(config.timbres[0].fm_depth_cents, 0.0);
    // Explicit values land.
    assert_eq!(config.timbres[1].am_depth, 0.5);
    assert_eq!(config.timbres[2].amplitude, 0.6);
    assert_eq!(config.timbres[3].fm_freq, 6.0);
  }

  #[test]
  fn timbres_must_have_exactly_four_entries() {
    let three = TIMBRES_FOUR.rsplit_once("[[timbres]]").unwrap().0;
    let err = parse_config(&format!("{SURFACES_MIN}{three}")).expect_err("3 entries fail");
    assert!(err.contains("exactly 4"), "{err}");
  }

  #[test]
  fn timbres_reject_out_of_range_parameters() {
    let toml = format!("{SURFACES_MIN}{}", TIMBRES_FOUR.replace("am_depth = 0.5", "am_depth = 1.5"));
    let err = parse_config(&toml).expect_err("am_depth > 1 fails");
    assert!(err.contains("am_depth"), "{err}");
  }

  #[test]
  fn slide_and_mono_toggles_validate_like_distortion() {
    let toggles = r#"
[[monome_windows]]
id = "slide-a"
monome = "a"
kind = "slide_toggle"
rect = [1, 1, 1, 1]

[[monome_windows]]
id = "mono-a"
monome = "a"
kind = "mono_toggle"
rect = [1, 2, 1, 2]
"#;
    let config = parse_config(&format!("{SURFACES_MIN}{toggles}")).expect("both toggles validate");
    assert!(config.monome_windows.iter().any(|w| matches!(w, MonomeWindowConfig::SlideToggle { .. })));
    assert!(config.monome_windows.iter().any(|w| matches!(w, MonomeWindowConfig::MonoToggle { .. })));
    // The [surfaces] slide knobs default sensibly.
    let surfaces = config.surfaces.unwrap_or_default();
    assert_eq!(surfaces.slide_candidate_window_ms, 1000);
    assert_eq!(surfaces.slide_duration_ms, 100);
    // A 2-cell slide toggle fails like any single-cell toggle.
    let err = parse_config(&format!(
      "{SURFACES_MIN}{}",
      toggles.replace("rect = [1, 1, 1, 1]", "rect = [1, 1, 2, 1]"),
    ))
    .expect_err("a 2-cell slide toggle should fail");
    assert!(err.contains("exactly one cell"), "{err}");
  }

  #[test]
  fn tap_tempo_pad_must_be_three_by_two() {
    let pad = r#"
[[monome_windows]]
id = "poly-a"
monome = "a"
kind = "tap_tempo_pad"
rect = [13, 0, 15, 1]
"#;
    let config = parse_config(&format!("{SURFACES_MIN}{pad}")).expect("a 3x2 pad validates");
    assert!(config.monome_windows.iter().any(|w| matches!(w, MonomeWindowConfig::TapTempoPad { .. })));
    assert_eq!(config.surfaces.unwrap_or_default().tap_tempo_window_ms, 2000, "default window");
    let err = parse_config(&format!("{SURFACES_MIN}{}", pad.replace("rect = [13, 0, 15, 1]", "rect = [13, 0, 15, 2]")))
      .expect_err("a 3x3 pad should fail");
    assert!(err.contains("exactly 3x2"), "{err}");
  }

  #[test]
  fn distortion_toggle_at_most_one_per_monome() {
    let twin = DISTORTION_TOGGLE
      .replace("dist-a", "dist-a2")
      .replace("rect = [0, 1, 0, 1]", "rect = [1, 1, 1, 1]");
    let toml = format!("{SURFACES_MIN}{DISTORTION_TOGGLE}{twin}");
    let err = parse_config(&toml).expect_err("two toggles on one monome should fail");
    assert!(err.contains("more than one distortion_toggle"), "{err}");
  }
}
