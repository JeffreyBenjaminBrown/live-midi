use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rig {
  pub version: u8,
  pub id: String,
  pub title: String,
  #[serde(default)]
  pub tunings: Vec<TuningRig>,
  #[serde(default)]
  pub monomes: Vec<MonomeRig>,
  #[serde(default)]
  pub midi: Option<MidiRig>,
  #[serde(default)]
  pub piano: Option<PianoRig>,
  #[serde(default)]
  pub sinks: Vec<SinkRig>,
  #[serde(default)]
  pub monome_windows: Vec<MonomeWindowRig>,
  /// Keith McMillen SoftStep (KMSS) foot-controller devices, declared once and
  /// referenced by id from `softstep_windows` -- the same idiom as `monomes`.
  #[serde(default)]
  pub softsteps: Vec<SoftstepRig>,
  /// Windows over a SoftStep: a region of pedals plus a `kind` behavior. The same
  /// windowing idiom as `monome_windows`, adapted to the KMSS's 10 labeled pedals.
  #[serde(default)]
  pub softstep_windows: Vec<SoftstepWindowRig>,
  #[serde(default)]
  pub display: Option<DisplayRig>,
  #[serde(default)]
  pub looper: Option<LooperRig>,
  /// Instrument-wide AM settings (6_plan 5). Today only the shape family lives here;
  /// it is the rig-level morph family the per-note `am.shape` value sweeps within.
  #[serde(default)]
  pub am: Option<AmRig>,
  /// `[trail]` table: the surfaces runtime's shared recent-note trail. Absent for
  /// non-surfaces rigs; the surfaces runtime falls back to `TrailRig::default`.
  #[serde(default)]
  pub trail: Option<TrailRig>,
  /// `[slide]` table: the slide feature's timing knobs (surfaces runtime). Absent
  /// falls back to `SlideRig::default`.
  #[serde(default)]
  pub slide: Option<SlideRig>,
  /// `[tap_tempo]` table: the tap-tempo pairing window (surfaces runtime). Absent
  /// falls back to `TapTempoRig::default`.
  #[serde(default)]
  pub tap_tempo: Option<TapTempoRig>,
  /// The four selectable timbres behind a `waveform_selector` strip, left to right
  /// (surfaces runtime). Absent = the plain four waveforms (sine / triangle /
  /// square / saw, everything else off) -- exactly the pre-timbres behavior. When
  /// present there must be exactly four entries.
  #[serde(default)]
  pub timbres: Vec<TimbreRig>,
  /// The EX-P expression pedals (surfaces runtime), each bound as a LINEAR volume
  /// pedal for one monome's grid. Absent = no pedals, behavior unchanged.
  #[serde(default)]
  pub expression_pedals: Vec<ExpressionPedalRig>,
  /// Echo each fingered note to the terminal (`press grid=.. x=.. y=.. f=.. Hz`).
  /// Off by default so a startup warning -- e.g. the surfaces runtime's red report of
  /// components that could not load for missing gear -- stays on screen instead of
  /// scrolling away under a stream of key echoes while you play. Turn it on for
  /// debugging. Honored by the surfaces and sawwave runtimes.
  #[serde(default)]
  pub echo_input: bool,
}

/// One selectable timbre (`[[timbres]]`): a base waveform plus its numeric
/// parameters. Everything except `waveform` is optional; the defaults are "off"
/// (no AM, no FM) at full amplitude.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TimbreRig {
  /// One of "sin"/"sine", "tri"/"triangle", "square", "saw".
  pub waveform: WaveformChoice,
  /// Per-timbre linear gain, multiplied below the volume fader. 0..1 typical
  /// (values above 1 amplify and can push the mix into the clamp). Default 1.0.
  #[serde(default = "default_timbre_amplitude")]
  pub amplitude: f32,
  /// *Absolute* tremolo depth in [0,1]: 0 = off (default), 1 = dips to silence.
  /// "Absolute" = the LFO rate is a fixed Hz, disregarding the voice's pitch
  /// (the `rel_*` family below tracks it instead).
  #[serde(default)]
  pub abs_am_depth: f32,
  /// Absolute tremolo rate in Hz (~0.1..10 musical). Default 1.0; inert while
  /// depth = 0.
  #[serde(default = "default_timbre_freq")]
  pub abs_am_hz: f32,
  /// Absolute tremolo LFO morph in [0,1]: 0 = smooth (sine/tri end of the rig's
  /// `[am]` family), 1 = near-square chop. Default 0. (The relative AM's LFO is
  /// a plain sine; this knob does not touch it.)
  #[serde(default)]
  pub am_shape: f32,
  /// *Absolute* vibrato depth in cents: 0 = off (default); ~5..50 subtle, 100+ =
  /// seasick. Exponential (a pitch offset), so it can never reach 0 Hz.
  #[serde(default)]
  pub abs_fm_depth_cents: f32,
  /// Absolute vibrato rate in Hz (~0.1..10 musical). Default 1.0; inert while
  /// depth = 0.
  #[serde(default = "default_timbre_freq")]
  pub abs_fm_hz: f32,
  /// *Relative* AM depth in [0,1] -- a second, independent tremolo whose LFO rate
  /// is `rel_am_freq` x the voice's pitch. 0 = off (default). Both AMs may run on
  /// one voice (typically abs slow, rel fast).
  #[serde(default)]
  pub rel_am_depth: f32,
  /// Relative AM rate as a unitless multiple of the voice's frequency (1 = the
  /// voice's own pitch -- audio-rate, ring-mod territory; give the sink
  /// `oversample`). 0 = off. Default 1.0; inert while depth = 0.
  #[serde(default = "default_timbre_freq")]
  pub rel_am_freq: f32,
  /// *Relative* FM depth as a multiple of the voice's frequency -- linear FM,
  /// independent of the absolute vibrato: 0 = off (default), 1 sweeps the carrier
  /// all the way from 2*f down to 0*f, and > 1 goes through zero (the oscillator
  /// runs backward).
  #[serde(default)]
  pub rel_fm_depth: f32,
  /// Relative FM rate as a unitless multiple of the voice's frequency (see
  /// `rel_am_freq`). 0 = off. Default 1.0; inert while depth = 0.
  #[serde(default = "default_timbre_freq")]
  pub rel_fm_freq: f32,
}

fn default_timbre_amplitude() -> f32 {
  1.0
}

/// One EX-P expression pedal (they reach us via the MPC-20 host bridge -- see
/// `expression_pedals.rs`, NOT the SoftSteps' own EX-P jacks, whose CC the decoder
/// deliberately drops) bound as a volume pedal for one monome's grid: the pedal's
/// reliable ~1..119 CC travel maps to a position 0..1, tapered into the amplitude
/// factor by the standard fader law -- an EXPONENTIAL (dB-linear) curve over most
/// of the travel, SPLICED to a linear fade over the first `curve_initial_lin_frac`
/// of it so full heel is exactly silent (an exponential alone never reaches 0).
/// Full toe = unity. Applied to that grid's sounding and future voices, slewed in
/// the engine (`voices::GAIN_SLEW_SECS`) so sweeps don't zipper. Until a pedal
/// first moves it contributes unity, so an unplugged pedal never mutes a grid.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExpressionPedalRig {
  /// The MPC-20 MIDI channel this pedal arrives on: 1 or 2 (both pedals send CC 21,
  /// one per channel).
  pub channel: u8,
  /// The monome whose grid volume this pedal drives. Must have an `edo_note_grid`.
  pub monome: String,
  /// The fraction of the travel, from the heel, that fades LINEARLY from silence up
  /// to the exponential's floor. In [0, 1); 0 = no splice (silence snaps on at the
  /// exact bottom). Default 0.1.
  #[serde(default = "default_curve_initial_lin_frac")]
  pub curve_initial_lin_frac: f32,
  /// The dB span of the exponential remainder of the travel: gain runs dB-linearly
  /// from -this (where the splice meets it) up to 0 dB at full toe. > 0. Default 50.
  #[serde(default = "default_curve_remainder_exp_db")]
  pub curve_remainder_exp_db: f32,
}

fn default_curve_initial_lin_frac() -> f32 {
  0.1
}

fn default_curve_remainder_exp_db() -> f32 {
  50.0
}

fn default_timbre_freq() -> f32 {
  1.0
}

/// A waveform name in a rig file. Accepts Jeff's short spellings.
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
/// is rig-level (one per instrument, 6_plan 2.5 / D 3c); the per-note `am.shape`
/// value (set by the editor's shape row) is the morph position within it.
#[derive(Clone, Copy, Debug, Deserialize, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AmRig {
  #[serde(default)]
  pub shape: AmShapeRig,
}

#[derive(Clone, Copy, Debug, Deserialize, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AmShapeRig {
  #[serde(default)]
  pub family: AmShapeFamilyRig,
}

/// Mirrors the runtime `sawwave::types::AmShapeFamily` (which the lib cannot see);
/// `resolve_settings` converts. Default = `sin_to_square`, matching the runtime.
#[derive(Clone, Copy, Debug, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AmShapeFamilyRig {
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
pub enum RowRangeRig {
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

/// One edge in rig: an integer (absolute) or a `"id.edge +/- n"` string.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum EdgeSpecRig {
  Absolute(i32),
  Expr(String),
}

/// A window rect (6_plan 4.1): the legacy whole-rect array `[x0,y0,x1,y1]` (all
/// absolute), or a per-edge table whose edges may be absolute or relative. Both
/// forms parse, so absolute rigs are unchanged.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RectSpecRig {
  Absolute([i32; 4]),
  PerEdge {
    top: EdgeSpecRig,
    bottom: EdgeSpecRig,
    left: EdgeSpecRig,
    right: EdgeSpecRig,
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

impl RectSpecRig {
  /// The given edge as Absolute(int) or Ref(other window's edge). The array form
  /// maps `[x0,y0,x1,y1]` to left/top/right/bottom.
  pub fn resolved_edge(&self, which: EdgeName) -> Result<ResolvedEdge, String> {
    match self {
      RectSpecRig::Absolute(a) => Ok(ResolvedEdge::Absolute(match which {
        EdgeName::Left => a[0],
        EdgeName::Top => a[1],
        EdgeName::Right => a[2],
        EdgeName::Bottom => a[3],
      })),
      RectSpecRig::PerEdge { top, bottom, left, right } => {
        let e = match which {
          EdgeName::Top => top,
          EdgeName::Bottom => bottom,
          EdgeName::Left => left,
          EdgeName::Right => right,
        };
        match e {
          EdgeSpecRig::Absolute(v) => Ok(ResolvedEdge::Absolute(*v)),
          EdgeSpecRig::Expr(s) => Ok(ResolvedEdge::Ref(parse_edge_expr(s)?)),
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
/// milliseconds. Present iff the rig declares the looper windows.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LooperRig {
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

fn default_trail_max() -> usize {
  7
}

/// `[trail]` table: instrument-wide settings for the surfaces runtime's shared
/// recent-note trail (the dim backdrop of the last few played pitch classes). A missing
/// table -- or any missing field -- uses these defaults, so behaviour is unchanged for
/// rigs that don't declare it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TrailRig {
  /// Trail clobber radius, as a *divisor of the octave*: playing a note clears any
  /// trailed pitch-class within `edo / clobber_radius` steps of it, so close
  /// pitches never crowd the backdrop. Bigger value = tighter radius (clears fewer);
  /// the default 27 is 1/27 of an octave (~44 cents).
  pub clobber_radius: i32,
  /// The most *distinct* pitch classes the shared trail keeps at once (newest first);
  /// older ones drop off the end. Default 7.
  pub max: usize,
}

impl Default for TrailRig {
  fn default() -> Self {
    Self { clobber_radius: default_trail_clobber_radius(), max: default_trail_max() }
  }
}

fn default_slide_candidate_window_ms() -> u64 {
  1000
}

fn default_slide_duration_ms() -> u64 {
  100
}

/// `[slide]` table: the slide feature's timing knobs (surfaces runtime). A missing
/// table -- or any missing field -- uses these defaults.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SlideRig {
  /// The slide feature's candidate window, in ms: a new note may slide from a note
  /// released no longer ago than this. Default 1000.
  pub candidate_window_ms: u64,
  /// How long a slide takes to reach the new pitch, in ms. Default 100.
  pub duration_ms: u64,
}

impl Default for SlideRig {
  fn default() -> Self {
    Self {
      candidate_window_ms: default_slide_candidate_window_ms(),
      duration_ms: default_slide_duration_ms(),
    }
  }
}

fn default_tap_tempo_window_ms() -> u64 {
  2000
}

/// `[tap_tempo]` table: the tap-tempo pairing window (surfaces runtime). A missing
/// table -- or a missing field -- uses this default.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TapTempoRig {
  /// The tap-tempo pairing window, in ms: two taps at most this far apart set the
  /// tapped tempo (rolling window). Default 2000.
  pub window_ms: u64,
}

impl Default for TapTempoRig {
  fn default() -> Self {
    Self { window_ms: default_tap_tempo_window_ms() }
  }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TuningRig {
  pub id: String,
  pub edo: i16,
  pub x_step: i16,
  pub y_step: i16,
  pub fundamental_hz: f64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MonomeRig {
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
pub struct SoftstepRig {
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
pub struct MidiRig {
  pub input: Option<MidiInputRig>,
  pub output: MidiOutputRig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MidiInputRig {
  pub virtual_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MidiOutputRig {
  pub virtual_name: String,
  pub min_channel: u8,
  pub min_note: u8,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PianoRig {
  pub mapping: PianoMappingRig,
  #[serde(default)]
  pub regions: Vec<PianoRegionRig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PianoMappingRig {
  TwelveN {
    lowest_note: u8,
    shift_before_mapping: i16,
    edo_per_12: i16,
  },
  RemappableUn12 {
    lowest_note: u8,
    tuning: String,
    remap_idiom: RemapIdiomRig,
    initial_map: InitialMapRig,
  },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RemapIdiomRig {
  Loose,
  Snap,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InitialMapRig {
  Even,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PianoRegionRig {
  pub range: [u8; 2],
  pub action: PianoRegionActionRig,
  pub zero_note: Option<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PianoRegionActionRig {
  EmitNotes,
  HeldOffsetControl,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SinkRig {
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
    /// Internal oversampling factor for the synth render (1 = off). >1 runs the
    /// nonlinear/multiplicative mix at N x the rate and decimates, so audio-rate
    /// AM/FM/waveshaping doesn't alias. Defaults to 1 so existing rigs are
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
    /// 2 = the smooth sweet spot, ~4+ = hard-ish, very large = hard clip. Below 1
    /// the curve bends from the origin and eats level fast (k = 0.5 costs ~5 dB on
    /// a single note) -- legal, but see `distortion_auto_makeup`.
    #[serde(default = "default_distortion_shape")]
    distortion_shape: f32,
    /// A trim on the distortion's loudness compensation. With `distortion_auto_makeup`
    /// on this multiplies the automatic gain: equal RMS is not equal *loudness* (the
    /// distorted signal is brighter, so it reads louder), so pull this to ~0.8 if the
    /// compensated sound feels hot. With auto off it *is* the makeup -- a plain
    /// constant gain on the distorted bus. Must be > 0. Default 1.0.
    #[serde(default = "default_distortion_makeup")]
    distortion_makeup: f32,
    /// Restore the distorted bus to the RMS the clean bus would have had, so a
    /// `distortion_toggle` changes timbre and not volume, and note count affects
    /// loudness exactly as it does undistorted. Computed from the voice envelopes,
    /// not an audio detector, so it neither lags nor pumps. Set false for the old
    /// uncompensated clipper. See `learnings/distortion-volume-compensation.org`.
    #[serde(default = "default_distortion_auto_makeup")]
    distortion_auto_makeup: bool,
    /// Lag the applied makeup behind its target, in milliseconds. `0` (the default) is
    /// the exact correction, which reproduces the clean bus's envelope and so preserves
    /// attacks exactly. Nonzero does NOT protect attacks -- it does the opposite, since
    /// the makeup must *rise* at a strike and lagging it under-compensates precisely
    /// there (200 ms costs ~4 dB of attack contrast and adds a swell). What it does buy
    /// is the clipper's own dynamic compression: as this grows the makeup approaches a
    /// plain constant, i.e. `distortion_auto_makeup = false`. A character knob.
    #[serde(default = "default_distortion_makeup_slew_ms")]
    distortion_makeup_slew_ms: f32,
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

fn default_distortion_makeup() -> f32 {
  1.0 // exact RMS restoration; trim by ear from there
}

fn default_distortion_auto_makeup() -> bool {
  true // a distortion toggle should change the timbre, not the volume
}

fn default_distortion_makeup_slew_ms() -> f32 {
  0.0 // exact: no lag, no swell, attacks preserved
}

fn default_sustain_level() -> f32 {
  0.35 // ~ -9 dB below the strike peak: audible pluck, notes still ring
}

fn default_decay_secs() -> f32 {
  0.5 // a guitar-ish decay time constant
}

impl SinkRig {
  pub fn id(&self) -> &str {
    match self {
      SinkRig::Midi { id }
      | SinkRig::CpalSynth { id, .. }
      | SinkRig::CpalSampler { id, .. } => id,
    }
  }
}

// Not `Eq`: the `TimbreEditor` variant carries f32 row ranges (like `LoopEvent`
// dropped `Eq` for its f32 timbre in C6).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonomeWindowRig {
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
  // covers, so the rig alone decides how many scales can be saved.
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
  // the new one. Window/candidate knobs live in the `[slide]` table.
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
  // A single-cell momentary button (surfaces runtime) driving this monome's EDIT
  // MODE in bulk, mirroring the accrete (sustain) controls -- see
  // `EditmodeControlKind` for what `clear` and `accrete` do. The same jobs as the
  // softstep `editmode_control` pedals, on-grid.
  EditmodeControl {
    id: String,
    monome: String,
    rect: [i32; 4],
    control: EditmodeControlKind,
  },
  // The 3x2 factored-pulse pad (surfaces runtime), by convention in the top-right:
  //   | x3 | x2 | tap |
  //   | /3 | /2 | =1  |
  // Tap twice within [tap_tempo].window_ms to set the tapped tempo; the
  // factor buttons scale it (exact 2^a * 3^b); notes struck while a tempo is
  // applied pulse with a unipolar triangle at that tempo. Only the tap cell
  // taps tempo -- the pad as a whole is the factored-pulse control. See
  // TODO/misc.org "polyrhythm interface".
  FactoredPulsePad {
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
  // A single-cell toggle for FINE TRANSPOSE (surfaces runtime;
  // queues/branch-2.org): while on, a dancing X marks a center pitch, play
  // presses set a scalar transpose of the grid's whole edit selection (pressed
  // pitch minus the X's center), and the octave corners move the X. At most one
  // per monome, on a monome with an edo_note_grid.
  FineTransposeToggle {
    id: String,
    monome: String,
    rect: [i32; 4],
  },
  // The chord-storage block (surfaces runtime; TODO/chord-storage-v2): a 5x2
  // compound overlaid on the edo grid. Top row = slots 1..5, bottom row = the ARM
  // button then slots 6..9. Arm + slot press saves every voice the monome is
  // sounding into that slot; a disarmed slot press toggles the stored chord on
  // (recall) and off. Each monome's nine slots are independent of the other's.
  ChordBlock {
    id: String,
    monome: String,
    rect: [i32; 4],
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
    amplitude: RowRangeRig,
    #[serde(default = "default_am_amplitude_row")]
    am_amplitude: RowRangeRig,
    #[serde(default = "default_am_frequency_row")]
    am_frequency: RowRangeRig,
    #[serde(default = "default_am_shape_row")]
    am_shape: RowRangeRig,
    #[serde(default = "default_fm_amplitude_row")]
    fm_amplitude: RowRangeRig,
    #[serde(default = "default_fm_frequency_row")]
    fm_frequency: RowRangeRig,
    // The save-undo double-press window in ms (6_plan 2.8/5). Loop-editor only.
    #[serde(default = "default_save_undo_ms")]
    save_undo_double_ms: u64,
  },
}

// Per-row default ranges (6_plan 5). Used when a `timbre_editor` omits a row.
fn default_amplitude_row() -> RowRangeRig {
  RowRangeRig::LogRange { least: 0.0009, greatest: 0.15 }
}
fn default_am_amplitude_row() -> RowRangeRig {
  RowRangeRig::Linear { min: 0.0, max: 1.0 }
}
fn default_am_frequency_row() -> RowRangeRig {
  RowRangeRig::LogFactor { least: 0.25, multiplier: 2.0 }
}
fn default_am_shape_row() -> RowRangeRig {
  RowRangeRig::Linear { min: 0.0, max: 1.0 }
}
fn default_fm_amplitude_row() -> RowRangeRig {
  RowRangeRig::LogFactor { least: 5.0, multiplier: 2.0 }
}
fn default_fm_frequency_row() -> RowRangeRig {
  RowRangeRig::LogFactor { least: 0.25, multiplier: 2.0 }
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

impl MonomeWindowRig {
  pub fn id(&self) -> &str {
    match self {
      MonomeWindowRig::EdoNoteGrid { id, .. }
      | MonomeWindowRig::ChordWipeButton { id, .. }
      | MonomeWindowRig::ChordAccreteToggle { id, .. }
      | MonomeWindowRig::ChordEmitModeToggle { id, .. }
      | MonomeWindowRig::ChordTargetButton { id, .. }
      | MonomeWindowRig::ChordSlotButtons { id, .. }
      | MonomeWindowRig::TwelveEdoOffsetBoard { id, .. }
      | MonomeWindowRig::PreimageRow { id, .. }
      | MonomeWindowRig::RemappableUn12Grid { id, .. }
      | MonomeWindowRig::RemapUndoButton { id, .. }
      | MonomeWindowRig::RecordControl { id, .. }
      | MonomeWindowRig::ScaleSlots { id, .. }
      | MonomeWindowRig::ScaleControl { id, .. }
      | MonomeWindowRig::WaveformSelector { id, .. }
      | MonomeWindowRig::VolumeStrip { id, .. }
      | MonomeWindowRig::DistortionToggle { id, .. }
      | MonomeWindowRig::SlideToggle { id, .. }
      | MonomeWindowRig::MonoToggle { id, .. }
      | MonomeWindowRig::EditmodeControl { id, .. }
      | MonomeWindowRig::FactoredPulsePad { id, .. }
      | MonomeWindowRig::AccreteControl { id, .. }
      | MonomeWindowRig::FineTransposeToggle { id, .. }
      | MonomeWindowRig::ChordBlock { id, .. }
      | MonomeWindowRig::EdoShiftPad { id, .. }
      | MonomeWindowRig::LoopSlots { id, .. }
      | MonomeWindowRig::LoopControl { id, .. }
      | MonomeWindowRig::LoopRemapModeToggle { id, .. }
      | MonomeWindowRig::LoopCopyButton { id, .. }
      | MonomeWindowRig::LoopRemapUndo { id, .. }
      | MonomeWindowRig::LoopDisplay { id, .. }
      | MonomeWindowRig::TimbreEditor { id, .. } => id,
    }
  }

  pub fn monome(&self) -> &str {
    match self {
      MonomeWindowRig::EdoNoteGrid { monome, .. }
      | MonomeWindowRig::ChordWipeButton { monome, .. }
      | MonomeWindowRig::ChordAccreteToggle { monome, .. }
      | MonomeWindowRig::ChordEmitModeToggle { monome, .. }
      | MonomeWindowRig::ChordTargetButton { monome, .. }
      | MonomeWindowRig::ChordSlotButtons { monome, .. }
      | MonomeWindowRig::TwelveEdoOffsetBoard { monome, .. }
      | MonomeWindowRig::PreimageRow { monome, .. }
      | MonomeWindowRig::RemappableUn12Grid { monome, .. }
      | MonomeWindowRig::RemapUndoButton { monome, .. }
      | MonomeWindowRig::RecordControl { monome, .. }
      | MonomeWindowRig::ScaleSlots { monome, .. }
      | MonomeWindowRig::ScaleControl { monome, .. }
      | MonomeWindowRig::WaveformSelector { monome, .. }
      | MonomeWindowRig::VolumeStrip { monome, .. }
      | MonomeWindowRig::DistortionToggle { monome, .. }
      | MonomeWindowRig::SlideToggle { monome, .. }
      | MonomeWindowRig::MonoToggle { monome, .. }
      | MonomeWindowRig::EditmodeControl { monome, .. }
      | MonomeWindowRig::FactoredPulsePad { monome, .. }
      | MonomeWindowRig::AccreteControl { monome, .. }
      | MonomeWindowRig::FineTransposeToggle { monome, .. }
      | MonomeWindowRig::ChordBlock { monome, .. }
      | MonomeWindowRig::EdoShiftPad { monome, .. }
      | MonomeWindowRig::LoopSlots { monome, .. }
      | MonomeWindowRig::LoopControl { monome, .. }
      | MonomeWindowRig::LoopRemapModeToggle { monome, .. }
      | MonomeWindowRig::LoopCopyButton { monome, .. }
      | MonomeWindowRig::LoopRemapUndo { monome, .. }
      | MonomeWindowRig::LoopDisplay { monome, .. }
      | MonomeWindowRig::TimbreEditor { monome, .. } => monome,
    }
  }

  pub fn rect(&self) -> [i32; 4] {
    match self {
      MonomeWindowRig::EdoNoteGrid { rect, .. }
      | MonomeWindowRig::ChordWipeButton { rect, .. }
      | MonomeWindowRig::ChordAccreteToggle { rect, .. }
      | MonomeWindowRig::ChordEmitModeToggle { rect, .. }
      | MonomeWindowRig::ChordTargetButton { rect, .. }
      | MonomeWindowRig::ChordSlotButtons { rect, .. }
      | MonomeWindowRig::TwelveEdoOffsetBoard { rect, .. }
      | MonomeWindowRig::PreimageRow { rect, .. }
      | MonomeWindowRig::RemappableUn12Grid { rect, .. }
      | MonomeWindowRig::RemapUndoButton { rect, .. }
      | MonomeWindowRig::RecordControl { rect, .. }
      | MonomeWindowRig::ScaleSlots { rect, .. }
      | MonomeWindowRig::ScaleControl { rect, .. }
      | MonomeWindowRig::WaveformSelector { rect, .. }
      | MonomeWindowRig::VolumeStrip { rect, .. }
      | MonomeWindowRig::DistortionToggle { rect, .. }
      | MonomeWindowRig::SlideToggle { rect, .. }
      | MonomeWindowRig::MonoToggle { rect, .. }
      | MonomeWindowRig::EditmodeControl { rect, .. }
      | MonomeWindowRig::FactoredPulsePad { rect, .. }
      | MonomeWindowRig::AccreteControl { rect, .. }
      | MonomeWindowRig::FineTransposeToggle { rect, .. }
      | MonomeWindowRig::ChordBlock { rect, .. }
      | MonomeWindowRig::EdoShiftPad { rect, .. }
      | MonomeWindowRig::LoopSlots { rect, .. }
      | MonomeWindowRig::LoopControl { rect, .. }
      | MonomeWindowRig::LoopRemapModeToggle { rect, .. }
      | MonomeWindowRig::LoopCopyButton { rect, .. }
      | MonomeWindowRig::LoopRemapUndo { rect, .. }
      | MonomeWindowRig::LoopDisplay { rect, .. }
      | MonomeWindowRig::TimbreEditor { rect, .. } => *rect,
    }
  }

  /// A human-readable kind label (matches the rig `kind = "..."` tag).
  pub fn kind_name(&self) -> &'static str {
    match self {
      MonomeWindowRig::EdoNoteGrid { .. } => "edo_note_grid",
      MonomeWindowRig::ChordWipeButton { .. } => "chord_wipe_button",
      MonomeWindowRig::ChordAccreteToggle { .. } => "chord_accrete_toggle",
      MonomeWindowRig::ChordEmitModeToggle { .. } => "chord_emit_mode_toggle",
      MonomeWindowRig::ChordTargetButton { .. } => "chord_target_button",
      MonomeWindowRig::ChordSlotButtons { .. } => "chord_slot_buttons",
      MonomeWindowRig::TwelveEdoOffsetBoard { .. } => "twelve_edo_offset_board",
      MonomeWindowRig::PreimageRow { .. } => "preimage_row",
      MonomeWindowRig::RemappableUn12Grid { .. } => "remappable_un12_grid",
      MonomeWindowRig::RemapUndoButton { .. } => "remap_undo_button",
      MonomeWindowRig::RecordControl { .. } => "record_control",
      MonomeWindowRig::ScaleSlots { .. } => "scale_slots",
      MonomeWindowRig::ScaleControl { .. } => "scale_control",
      MonomeWindowRig::WaveformSelector { .. } => "waveform_selector",
      MonomeWindowRig::VolumeStrip { .. } => "volume_strip",
      MonomeWindowRig::DistortionToggle { .. } => "distortion_toggle",
      MonomeWindowRig::SlideToggle { .. } => "slide_toggle",
      MonomeWindowRig::MonoToggle { .. } => "mono_toggle",
      MonomeWindowRig::EditmodeControl { .. } => "editmode_control",
      MonomeWindowRig::FactoredPulsePad { .. } => "factored_pulse_pad",
      MonomeWindowRig::AccreteControl { .. } => "accrete_control",
      MonomeWindowRig::FineTransposeToggle { .. } => "fine_transpose_toggle",
      MonomeWindowRig::ChordBlock { .. } => "chord_block",
      MonomeWindowRig::EdoShiftPad { .. } => "edo_shift_pad",
      MonomeWindowRig::LoopSlots { .. } => "loop_slots",
      MonomeWindowRig::LoopControl { .. } => "loop_control",
      MonomeWindowRig::LoopRemapModeToggle { .. } => "loop_remap_mode_toggle",
      MonomeWindowRig::LoopCopyButton { .. } => "loop_copy_button",
      MonomeWindowRig::LoopRemapUndo { .. } => "loop_remap_undo",
      MonomeWindowRig::LoopDisplay { .. } => "loop_display",
      MonomeWindowRig::TimbreEditor { .. } => "timbre_editor",
    }
  }

  /// The timbre-editor row ranges + target, in row order (amplitude, AM amp/freq/
  /// shape, FM amp/freq), or None for other window kinds. Used by `resolve_settings`.
  #[allow(clippy::type_complexity)]
  pub fn timbre_editor_rows(
    &self,
  ) -> Option<(TimbreTarget, [RowRangeRig; 6])> {
    match self {
      MonomeWindowRig::TimbreEditor {
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
      MonomeWindowRig::TimbreEditor { target, save_undo_double_ms, .. } => {
        Some((*target, *save_undo_double_ms))
      }
      _ => None,
    }
  }
}

/// One pedal's assignment inside a `drumkit` window: the printed pedal label
/// (1..9, then 0) and either a sample file to fire or `ditto = true`. `gain` is the
/// voice's FULL-pressure level for a sample pad; the tether runtime scales it down
/// for softer hits (see `drumkit_runtime::decode`). A `ditto` pad ignores `gain` --
/// it replays the most recently played sample in its window at THAT hit's already-
/// resolved (pressure-scaled) gain, "a generalized double-bass pedal" (see
/// `drumkit_runtime::mod` for the trigger logic). Validation requires exactly one of
/// `sample` / `ditto = true` per pad, and at most one ditto pad per window.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DrumPadRig {
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

// --- Shared SoftStep detection & pressure parameters (rigs/softstep.toml) ------------
// These drive the drumkit decoder. There is no reason for them to vary per rig, so they
// live in one shared file loaded by `load_softstep_params`, not in each rig's window.
fn default_on_sum() -> u16 {
  20
}
fn default_off_sum() -> u16 {
  20
}
fn default_attack_ms() -> u64 {
  14
}
fn default_pressure_full_scale() -> u16 {
  460
}
fn default_gain_db_range() -> f32 {
  20.0
}
fn default_silence_to_zero_ms() -> u64 {
  25
}
fn default_factor_settle_ms() -> u64 {
  150
}
fn default_factor_release_ms() -> u64 {
  25
}

fn default_pressure_threshold_sum() -> u16 {
  200
}

/// Hit-detection & pressure parameters for the SoftStep, shared by every rig that uses
/// it. Loaded once from `rigs/softstep.toml` (not from any per-rig window), so one
/// set of numbers drives the drumkit. Every field is optional; a missing file or key uses
/// the default. The Python meter (`tools/softstep/meter/`) mirrors these.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SoftstepParams {
  /// A hit fires when a pad's sum-of-4 exceeds this.
  #[serde(default = "default_on_sum")]
  pub on_sum: u16,
  /// The pad re-arms once its sum-of-4 falls below this.
  #[serde(default = "default_off_sum")]
  pub off_sum: u16,
  /// After firing on onset, keep watching this long to raise pressure to a later peak.
  #[serde(default = "default_attack_ms")]
  pub attack_ms: u64,
  /// Pad sum-of-4 (0..508) that maps to full pressure (1.0). (Field name is historical;
  /// the decoder emits a continuous 0.0..1.0 pressure now, not a 0..127 velocity.)
  #[serde(default = "default_pressure_full_scale")]
  pub pressure_full_scale: u16,
  /// dB between the softest (pressure 0.0) and hardest (pressure 1.0) hit.
  #[serde(default = "default_gain_db_range")]
  pub gain_db_range: f32,
  /// A sensor with no CC for this long reads 0 (de-stick); 0 = off.
  #[serde(default = "default_silence_to_zero_ms")]
  pub silence_to_zero_ms: u64,
  /// For a momentary TIMING pedal on `Settle` (the tempo-factor pedals), the minimum time
  /// from a fire until it may fire again. This caps how fast deliberate repeats can land.
  /// Only `Settle` pads use it (see `DebounceMode::Settle`).
  #[serde(default = "default_factor_settle_ms")]
  pub factor_settle_ms: u64,
  /// How long a pad's sum must stay quiet before it may fire again. BOTH debounce modes
  /// use it: it is `Standard`'s quiet gate (the sum must hold steady this long, released,
  /// before a new hit is accepted -- a bounce keeps changing and so never re-arms), and
  /// the inactivity half of `Settle`'s re-arm rule. Named `factor_release_ms` for its
  /// original Settle-only role.
  #[serde(default = "default_factor_release_ms")]
  pub factor_release_ms: u64,
  /// Sum-of-4 at or above which a strike counts as HARD rather than light -- the
  /// "one pad, two purposes" trick (tap lightly for one job, stomp for another).
  ///
  /// A raw sum, NOT a pressure or a velocity, so it is comparable to `on_sum` /
  /// `pressure_full_scale` and readable straight off the meter's `sum` column.
  ///
  /// 200 is Jeff's pick to get moving, not a measurement: the only figure ever
  /// recorded is "hard hits measure ~430-460 against the 508 ceiling", and no
  /// light-press figure exists at all, so there is no observed range to halve. Revisit
  /// with a meter capture (`tools/softstep/meter/main.py`).
  ///
  /// Measures the ATTACK PEAK, so it reads how fast you stomp, not how hard you end up
  /// standing -- meaningless for a pedal you HOLD (accrete), correct for a tap.
  #[serde(default = "default_pressure_threshold_sum")]
  pub pressure_threshold_sum: u16,
}

impl Default for SoftstepParams {
  fn default() -> Self {
    SoftstepParams {
      on_sum: default_on_sum(),
      off_sum: default_off_sum(),
      attack_ms: default_attack_ms(),
      pressure_full_scale: default_pressure_full_scale(),
      gain_db_range: default_gain_db_range(),
      silence_to_zero_ms: default_silence_to_zero_ms(),
      factor_settle_ms: default_factor_settle_ms(),
      factor_release_ms: default_factor_release_ms(),
      pressure_threshold_sum: default_pressure_threshold_sum(),
    }
  }
}

/// Load the shared SoftStep parameters from `rigs/softstep.toml`. A missing file uses
/// the defaults; a present file with a parse error or unknown key is a hard error.
pub fn load_softstep_params() -> Result<SoftstepParams, String> {
  let path = rig_dir().join("softstep.toml");
  match std::fs::read_to_string(&path) {
    Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SoftstepParams::default()),
    Err(e) => Err(format!("{}: {e}", path.display())),
  }
}

/// A window over a SoftStep: a `kind` behavior bound to a declared `softstep`
/// device. The same windowing idiom as `monome_windows`; new arrangements are new
/// kinds (or new rigs). Today the only kind is `drumkit`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SoftstepWindowRig {
  /// Maps pedals to one-shot drum samples played through a `cpal_sampler` sink.
  /// Each pad's `sample` resolves under the top-level `drum-samples/` folder
  /// (`drum_samples_dir`); a `sample` may name a subpath for multi-kit layouts.
  Drumkit {
    id: String,
    softstep: String,
    sink: String,
    // Detection/pressure knobs (debounce, de-stick, thresholds) are NOT here -- they are
    // shared across rigs and live in rigs/softstep.toml (see `SoftstepParams`).
    pads: Vec<DrumPadRig>,
  },
  /// One pedal driving one monome's accrete bank -- the same `AccreteControlKind`
  /// the on-grid buttons use, so a rig can put sustain under a foot instead of
  /// spending grid cells on it. Unconditional and stated in the rig -- unlike the
  /// retired `softstep_accretes_toggle` mirror (deleted 2026-07; it hardcoded pedals
  /// 1/2/3 and 8/9/0 and needed an on-grid toggle to turn it on), this binding does
  /// not depend on any on-grid state.
  AccreteControl {
    id: String,
    softstep: String,
    /// Printed label, 0..9.
    pedal: u8,
    /// The monome whose bank this drives. Must have an `edo_note_grid`.
    monome: String,
    control: AccreteControlKind,
  },
  /// The pedal that taps the ONE global tempo. Two taps within
  /// `[tap_tempo].window_ms` define it; tapping only defines it, and starts
  /// nothing.
  TapTempoPedal { id: String, softstep: String, pedal: u8 },
  /// One pedal nudging one monome's pulse. What it acts on depends on that monome's
  /// edit state: with no note in edit mode it moves the GRID's factor (as the on-grid
  /// polyrhythm pad does); with notes in edit mode the multipliers retune *those
  /// notes* in place. `Unity` is the =1 switch: it never *retunes* edit-mode notes
  /// that way, but an on/off transition of the switch itself SETS their
  /// factored-pulse rate directly -- the grid's applied tempo on, 0 off
  /// (`surfaces_runtime::hooks::factored_pulse_press`); the lone press that snaps
  /// an already-cycling grid's tempo factor to unity is neither and leaves them
  /// alone.
  PulseFactorPedal {
    id: String,
    softstep: String,
    pedal: u8,
    monome: String,
    factor: PulseFactorRig,
  },
  /// One pedal driving one monome's EDIT MODE in bulk, mirroring the
  /// `accrete_control` sustain pedals (queue.org "accrete-editmode pedals like
  /// accrete-sustain") -- see `EditmodeControlKind` for what `clear` and `accrete`
  /// do. The same jobs as an on-grid `editmode_control` button.
  EditmodeControl {
    id: String,
    softstep: String,
    pedal: u8,
    /// The monome whose edit mode this drives. Must have an `edo_note_grid`.
    monome: String,
    control: EditmodeControlKind,
  },
}

/// What an `editmode_control` (pedal or on-grid button) does to its monome's edit
/// mode, symmetric with the sustain bank's clear/accrete:
///
/// - `clear`: key-down removes EVERY pitch from edit mode. Each voice then rings
///   on iff it still has another reason (a finger, or the sustain bank); an
///   edit-only drone ends by the ordinary release ramp. Unlike the sustain
///   accrete's `clear`, nothing leaves the sustain bank -- each clear removes only
///   its OWN reason, so the full kill is both clears.
/// - `accrete`: key-down puts every voice currently sounding on this grid -- every
///   fingered voice and every sustained voice -- into edit mode. A one-shot, not a
///   hold: the moment anything is edited the grid becomes a pitch-picker (presses
///   drag rather than play), so there are no "notes played while held" to capture.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum EditmodeControlKind {
  Clear,
  Accrete,
}

/// A pulse-factor pedal's job. The exponent factors are what the on-grid polyrhythm
/// pad already applies (`2^a * 3^b`, so x3-then-/3 is exactly unity).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
pub enum PulseFactorRig {
  #[serde(rename = "x2")]
  Double,
  #[serde(rename = "x3")]
  Triple,
  #[serde(rename = "div2")]
  Half,
  #[serde(rename = "div3")]
  Third,
  /// The `=1` switch: start cycling / unity the factor / stop, per the double-tap
  /// dance the on-grid pad uses.
  #[serde(rename = "unity")]
  Unity,
}

impl SoftstepWindowRig {
  pub fn id(&self) -> &str {
    match self {
      SoftstepWindowRig::Drumkit { id, .. }
      | SoftstepWindowRig::AccreteControl { id, .. }
      | SoftstepWindowRig::TapTempoPedal { id, .. }
      | SoftstepWindowRig::PulseFactorPedal { id, .. }
      | SoftstepWindowRig::EditmodeControl { id, .. } => id,
    }
  }

  pub fn softstep(&self) -> &str {
    match self {
      SoftstepWindowRig::Drumkit { softstep, .. }
      | SoftstepWindowRig::AccreteControl { softstep, .. }
      | SoftstepWindowRig::TapTempoPedal { softstep, .. }
      | SoftstepWindowRig::PulseFactorPedal { softstep, .. }
      | SoftstepWindowRig::EditmodeControl { softstep, .. } => softstep,
    }
  }

  /// A human-readable kind label (matches the rig `kind = "..."` tag).
  pub fn kind_name(&self) -> &'static str {
    match self {
      SoftstepWindowRig::Drumkit { .. } => "drumkit",
      SoftstepWindowRig::AccreteControl { .. } => "accrete_control",
      SoftstepWindowRig::TapTempoPedal { .. } => "tap_tempo_pedal",
      SoftstepWindowRig::PulseFactorPedal { .. } => "pulse_factor_pedal",
      SoftstepWindowRig::EditmodeControl { .. } => "editmode_control",
    }
  }

  /// The pedals this window claims on its device. A drumkit claims one per pad;
  /// every other kind is a single pedal.
  pub fn pedals(&self) -> Vec<u8> {
    match self {
      SoftstepWindowRig::Drumkit { pads, .. } => pads.iter().map(|p| p.pedal).collect(),
      SoftstepWindowRig::AccreteControl { pedal, .. }
      | SoftstepWindowRig::TapTempoPedal { pedal, .. }
      | SoftstepWindowRig::PulseFactorPedal { pedal, .. }
      | SoftstepWindowRig::EditmodeControl { pedal, .. } => vec![*pedal],
    }
  }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DisplayRig {
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

pub fn rig_dir() -> PathBuf {
  // edo_surface/ is one level under the repo root; rigs/ is a top-level sibling.
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rigs")
}

/// Top-level folder holding drum-sample WAVs. A `drumkit` window's pads resolve
/// under here: `drum_samples_dir()/<pad.sample>` (a `sample` may name a subpath).
pub fn drum_samples_dir() -> PathBuf {
  // edo_surface/ is one level under the repo root; drum-samples/ is a top-level sibling.
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../drum-samples")
}

/// Mock-rig rigs (mock ports/prefixes) live here, separate from `rig_dir` so
/// the real-rig sweep (`loads_default_rigs`) doesn't pick them up. Resolved by
/// name as a fallback (see `rig_path`), so `<name>-mock` loads by name too.
pub fn mock_rig_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mocks")
}

pub fn rig_path(name: &str) -> Result<PathBuf, String> {
  if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
    return Err(format!("rig name must not be a path, got {name:?}"));
  }
  // Prefer a real rig over the mock dir (the integration tests / `cargo run --
  // <name>-mock`). Within a dir, `.toml` is authoritative when present: a rig's `.org`
  // sibling is currently its *documentation*, not its definition, so a rig only loads
  // as `.org` once its `.toml` is gone (migration writes the `.org` -- which folds the
  // doc into its bodies -- and deletes the `.toml`). See TODO/toml-to-org-rig-format.org.
  for dir in [rig_dir(), mock_rig_dir()] {
    for ext in ["toml", "org"] {
      let candidate = dir.join(name).with_extension(ext);
      if candidate.exists() {
        return Ok(candidate);
      }
    }
  }
  Ok(rig_dir().join(name).with_extension("toml"))
}

pub fn load_named_rig(name: &str) -> Result<Rig, String> {
  let path = rig_path(name)?;
  load_rig_file(&path)
}

pub fn load_rig_file(path: &Path) -> Result<Rig, String> {
  let source = std::fs::read_to_string(path)
    .map_err(|e| format!("read rig {}: {e}", path.display()))?;
  let parsed = if path.extension().and_then(|e| e.to_str()) == Some("org") {
    crate::rig_org::parse_org_rig(&source)
  } else {
    parse_rig(&source)
  };
  parsed.map_err(|e| format!("parse rig {}: {e}", path.display()))
}

pub fn parse_rig(source: &str) -> Result<Rig, String> {
  let rig: Rig = toml::from_str(source).map_err(|e| e.to_string())?;
  validate_rig(&rig)?;
  Ok(rig)
}

pub fn validate_rig(rig: &Rig) -> Result<(), String> {
  if rig.version != 1 {
    return Err(format!("version must be 1, got {}", rig.version));
  }
  require_unique("tuning", rig.tunings.iter().map(|t| t.id.as_str()))?;
  require_unique("monome", rig.monomes.iter().map(|m| m.id.as_str()))?;
  require_unique("sink", rig.sinks.iter().map(SinkRig::id))?;
  require_unique("monome window", rig.monome_windows.iter().map(MonomeWindowRig::id))?;
  require_unique("softstep", rig.softsteps.iter().map(|s| s.id.as_str()))?;
  require_unique(
    "softstep window",
    rig.softstep_windows.iter().map(SoftstepWindowRig::id),
  )?;
  require_unique(
    "monome listen_port",
    rig.monomes.iter().map(|m| m.listen_port.to_string()),
  )?;

  let tuning_ids: HashSet<&str> = rig.tunings.iter().map(|t| t.id.as_str()).collect();
  let monome_ids: HashSet<&str> = rig.monomes.iter().map(|m| m.id.as_str()).collect();
  let sink_ids: HashSet<&str> = rig.sinks.iter().map(SinkRig::id).collect();

  for tuning in &rig.tunings {
    if tuning.edo <= 0 {
      return Err(format!("tuning {:?} edo must be positive", tuning.id));
    }
    if !tuning.fundamental_hz.is_finite() || tuning.fundamental_hz <= 0.0 {
      return Err(format!("tuning {:?} fundamental_hz must be positive", tuning.id));
    }
  }

  if let Some(piano) = &rig.piano {
    if let PianoMappingRig::RemappableUn12 { tuning, .. } = &piano.mapping {
      require_ref("piano.mapping.tuning", tuning, &tuning_ids)?;
    }
    for region in &piano.regions {
      if region.range[0] > region.range[1] {
        return Err(format!("piano region range must be ascending: {:?}", region.range));
      }
      match region.action {
        PianoRegionActionRig::EmitNotes => {
          if region.zero_note.is_some() {
            return Err("emit_notes piano region cannot set zero_note".to_string());
          }
        }
        PianoRegionActionRig::HeldOffsetControl => {
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

  for sink in &rig.sinks {
    if let SinkRig::CpalSynth {
      sample_rate,
      buffer_frames,
      amplitude,
      attack_secs,
      release_secs,
      oversample,
      distortion_makeup,
      distortion_makeup_slew_ms,
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
      ] {
        if !value.is_finite() || value < 0.0 {
          return Err(format!("sink {:?} {name} must be nonnegative", sink.id()));
        }
      }
      // A zero or negative makeup would silence (or invert) the distorted bus.
      if !distortion_makeup.is_finite() || *distortion_makeup <= 0.0 {
        return Err(format!("sink {:?} distortion_makeup must be positive", sink.id()));
      }
      if !distortion_makeup_slew_ms.is_finite() || *distortion_makeup_slew_ms < 0.0 {
        return Err(format!("sink {:?} distortion_makeup_slew_ms must be nonnegative", sink.id()));
      }
    }
    if let SinkRig::CpalSampler { sample_rate, buffer_frames, amplitude, .. } = sink {
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
  for window in &rig.monome_windows {
    require_ref("monome_window.monome", window.monome(), &monome_ids)?;
    let [x0, y0, x1, y1] = window.rect();
    if x0 > x1 || y0 > y1 {
      return Err(format!("monome window {:?} rect must be ascending", window.id()));
    }
    match window {
      MonomeWindowRig::EdoNoteGrid { tuning, sink, .. } => {
        require_ref("monome_window.tuning", tuning, &tuning_ids)?;
        require_ref("monome_window.sink", sink, &sink_ids)?;
      }
      MonomeWindowRig::RemappableUn12Grid { tuning, .. } => {
        require_ref("monome_window.tuning", tuning, &tuning_ids)?;
      }
      MonomeWindowRig::RecordControl { id, rect, control, .. } => {
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
      MonomeWindowRig::ScaleControl { id, rect, control, .. } => {
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
      MonomeWindowRig::LoopControl { id, rect, control, .. } => {
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
      MonomeWindowRig::TimbreEditor { id, .. } => {
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

  validate_window_groups(rig)?;
  validate_shift_pads(rig)?;
  validate_waveform_selectors(rig)?;
  validate_volume_strips(rig)?;
  validate_accrete_controls(rig)?;
  validate_single_cell_toggles(rig)?;
  validate_factored_pulse_pads(rig)?;
  validate_chord_blocks(rig)?;
  validate_timbres(rig)?;
  validate_expression_pedals(rig)?;
  validate_looper(rig)?;
  validate_softsteps(rig)?;

  Ok(())
}

/// The `[[expression_pedals]]` bindings: channel 1 or 2, each channel and each
/// monome claimed at most once, and the monome must be a play grid (it has voices
/// to fade).
fn validate_expression_pedals(rig: &Rig) -> Result<(), String> {
  let mut channels: HashSet<u8> = HashSet::new();
  let mut monomes: HashSet<&str> = HashSet::new();
  for p in &rig.expression_pedals {
    if !(1..=2).contains(&p.channel) {
      return Err(format!(
        "expression pedal channel must be 1 or 2 (the MPC-20's two channels), got {}",
        p.channel,
      ));
    }
    if !channels.insert(p.channel) {
      return Err(format!("two expression pedals declare channel {}", p.channel));
    }
    let has_grid = rig.monome_windows.iter().any(
      |w| matches!(w, MonomeWindowRig::EdoNoteGrid { monome: m, .. } if *m == p.monome),
    );
    if !has_grid {
      return Err(format!(
        "expression pedal (channel {}) targets monome {:?}, which has no edo_note_grid",
        p.channel, p.monome,
      ));
    }
    if !monomes.insert(p.monome.as_str()) {
      return Err(format!("monome {:?} has two expression pedals", p.monome));
    }
    if !p.curve_initial_lin_frac.is_finite()
      || !(0.0..1.0).contains(&p.curve_initial_lin_frac)
    {
      return Err(format!(
        "expression pedal (channel {}): curve_initial_lin_frac must be in 0..1 \
         (1 would leave no travel for the exponential), got {}",
        p.channel, p.curve_initial_lin_frac,
      ));
    }
    if !p.curve_remainder_exp_db.is_finite() || p.curve_remainder_exp_db <= 0.0 {
      return Err(format!(
        "expression pedal (channel {}): curve_remainder_exp_db must be > 0, got {}",
        p.channel, p.curve_remainder_exp_db,
      ));
    }
  }
  Ok(())
}

/// The `[[timbres]]` table: when present it must have exactly four entries (the
/// waveform strip has four cells), and every numeric parameter must be in range.
fn validate_timbres(rig: &Rig) -> Result<(), String> {
  if rig.timbres.is_empty() {
    return Ok(());
  }
  if rig.timbres.len() != 4 {
    return Err(format!(
      "[[timbres]] must have exactly 4 entries (the waveform strip's four cells), got {}",
      rig.timbres.len(),
    ));
  }
  for (i, t) in rig.timbres.iter().enumerate() {
    if t.amplitude < 0.0 {
      return Err(format!("timbre {i}: amplitude must be >= 0, got {}", t.amplitude));
    }
    if !(0.0..=1.0).contains(&t.abs_am_depth) {
      return Err(format!("timbre {i}: abs_am_depth must be in 0..=1, got {}", t.abs_am_depth));
    }
    if !(0.0..=1.0).contains(&t.am_shape) {
      return Err(format!("timbre {i}: am_shape must be in 0..=1, got {}", t.am_shape));
    }
    if t.abs_fm_depth_cents < 0.0 {
      return Err(format!(
        "timbre {i}: abs_fm_depth_cents must be >= 0, got {}",
        t.abs_fm_depth_cents
      ));
    }
    if t.abs_am_hz <= 0.0 || t.abs_fm_hz <= 0.0 {
      return Err(format!("timbre {i}: abs_am_hz/abs_fm_hz must be > 0"));
    }
    if !(0.0..=1.0).contains(&t.rel_am_depth) {
      return Err(format!("timbre {i}: rel_am_depth must be in 0..=1, got {}", t.rel_am_depth));
    }
    // rel_fm_depth may exceed 1 -- that is the through-zero region -- but a negative
    // depth is just a phase-flipped LFO; keep the sign convention single.
    if t.rel_fm_depth < 0.0 {
      return Err(format!("timbre {i}: rel_fm_depth must be >= 0, got {}", t.rel_fm_depth));
    }
    // 0 is a legal relative rate ("off"); negative rates are not a thing.
    if t.rel_am_freq < 0.0 || t.rel_fm_freq < 0.0 {
      return Err(format!("timbre {i}: rel_am_freq/rel_fm_freq must be >= 0"));
    }
  }
  Ok(())
}

/// A `factored_pulse_pad` is exactly 3x2 cells on a monome that has an
/// `edo_note_grid`, at most one per monome (its six sub-cells are the fixed
/// x3/x2/tap and /3/2/=1 layout).
fn validate_factored_pulse_pads(rig: &Rig) -> Result<(), String> {
  let mut seen: HashSet<&str> = HashSet::new();
  for window in &rig.monome_windows {
    let MonomeWindowRig::FactoredPulsePad { id, monome, rect } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if x1 - x0 != 2 || y1 - y0 != 1 {
      return Err(format!(
        "factored_pulse_pad window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must be exactly 3x2 cells",
      ));
    }
    let has_edo_grid = rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "factored_pulse_pad window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = rig
      .monomes
      .iter()
      .find(|m| m.id == *monome)
      .and_then(|m| m.select.size)
      .unwrap_or([16, 16]);
    if x0 < 0 || y0 < 0 || x1 >= gw || y1 >= gh {
      return Err(format!(
        "factored_pulse_pad window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must fit the {gw}x{gh} grid",
      ));
    }
    if !seen.insert(monome.as_str()) {
      return Err(format!(
        "monome {monome:?} declares more than one factored_pulse_pad (window {id:?})"
      ));
    }
  }
  Ok(())
}

/// A `chord_block` is exactly 5x2 cells on a monome that has an `edo_note_grid`,
/// at most one per monome (its ten sub-cells are the fixed slots 1..5 over
/// arm + slots 6..9 layout).
fn validate_chord_blocks(rig: &Rig) -> Result<(), String> {
  let mut seen: HashSet<&str> = HashSet::new();
  for window in &rig.monome_windows {
    let MonomeWindowRig::ChordBlock { id, monome, rect } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if x1 - x0 != 4 || y1 - y0 != 1 {
      return Err(format!(
        "chord_block window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must be exactly 5x2 cells",
      ));
    }
    let has_edo_grid = rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "chord_block window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = rig
      .monomes
      .iter()
      .find(|m| m.id == *monome)
      .and_then(|m| m.select.size)
      .unwrap_or([16, 16]);
    if x0 < 0 || y0 < 0 || x1 >= gw || y1 >= gh {
      return Err(format!(
        "chord_block window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must fit the {gw}x{gh} grid",
      ));
    }
    if !seen.insert(monome.as_str()) {
      return Err(format!(
        "monome {monome:?} declares more than one chord_block (window {id:?})"
      ));
    }
  }
  Ok(())
}

/// The single-cell per-monome buttons (`distortion_toggle`, `slide_toggle`,
/// `mono_toggle`, ..., plus the momentary `editmode_control` buttons): each is one cell on
/// a monome that has an `edo_note_grid`, at most one of each kind per monome (a
/// second would be a redundant twin of the same control).
fn validate_single_cell_toggles(rig: &Rig) -> Result<(), String> {
  // (kind label, id, monome, rect) for every such window.
  let toggles = rig.monome_windows.iter().filter_map(|w| match w {
    MonomeWindowRig::DistortionToggle { id, monome, rect } => {
      Some(("distortion_toggle", id, monome, *rect))
    }
    MonomeWindowRig::SlideToggle { id, monome, rect } => {
      Some(("slide_toggle", id, monome, *rect))
    }
    MonomeWindowRig::MonoToggle { id, monome, rect } => {
      Some(("mono_toggle", id, monome, *rect))
    }
    MonomeWindowRig::FineTransposeToggle { id, monome, rect } => {
      Some(("fine_transpose_toggle", id, monome, *rect))
    }
    MonomeWindowRig::EditmodeControl { id, monome, rect, control } => {
      // Per-control uniqueness: label by control so a monome may carry one clear
      // AND one accrete button, but not two of either.
      Some((
        match control {
          EditmodeControlKind::Clear => "editmode_control (clear)",
          EditmodeControlKind::Accrete => "editmode_control (accrete)",
        },
        id,
        monome,
        *rect,
      ))
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
    let has_edo_grid = rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "{kind} window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = rig
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
fn validate_accrete_controls(rig: &Rig) -> Result<(), String> {
  let mut per_monome: HashMap<&str, Vec<AccreteControlKind>> = HashMap::new();
  for window in &rig.monome_windows {
    let MonomeWindowRig::AccreteControl { id, monome, rect, control } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if x0 != x1 || y0 != y1 {
      return Err(format!(
        "accrete_control window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must cover exactly one cell",
      ));
    }
    let has_edo_grid = rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "accrete_control window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = rig
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
fn validate_volume_strips(rig: &Rig) -> Result<(), String> {
  let monome_ids: HashSet<&str> = rig.monomes.iter().map(|m| m.id.as_str()).collect();
  for window in &rig.monome_windows {
    let MonomeWindowRig::VolumeStrip { id, monome, rect, controls } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if y1 != y0 || x1 - x0 < 1 {
      return Err(format!(
        "volume_strip window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must be one row, >= 2 cells wide",
      ));
    }
    require_ref("volume_strip.controls", controls, &monome_ids)?;
    let controlled_has_grid = rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::EdoNoteGrid { monome: m, .. } if m == controls)
    });
    if !controlled_has_grid {
      return Err(format!(
        "volume_strip window {id:?} controls monome {controls:?}, which has no edo_note_grid",
      ));
    }
    let [gw, gh] = rig
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
/// cross-grid wiring is exactly what the surfaces rig uses.
fn validate_waveform_selectors(rig: &Rig) -> Result<(), String> {
  let monome_ids: HashSet<&str> = rig.monomes.iter().map(|m| m.id.as_str()).collect();
  for window in &rig.monome_windows {
    let MonomeWindowRig::WaveformSelector { id, monome, rect, controls } = window else {
      continue;
    };
    let [x0, y0, x1, y1] = *rect;
    if x1 - x0 != 3 || y1 != y0 {
      return Err(format!(
        "waveform_selector window {id:?} rect [{x0}, {y0}, {x1}, {y1}] must cover exactly 4 cells in one row",
      ));
    }
    require_ref("waveform_selector.controls", controls, &monome_ids)?;
    let controlled_has_grid = rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::EdoNoteGrid { monome: m, .. } if m == controls)
    });
    if !controlled_has_grid {
      return Err(format!(
        "waveform_selector window {id:?} controls monome {controls:?}, which has no edo_note_grid",
      ));
    }
    // The strip is drawn on its own grid, so its rect must fit that monome.
    let [gw, gh] = rig
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
/// fit that monome's grid. This runs for every rig, so a plain sawwave grid,
/// the surfaces runtime's grids, and the looper all validate a shift pad the same
/// way. (The looper's own `validate_looper` separately checks the edo grid fits.)
fn validate_shift_pads(rig: &Rig) -> Result<(), String> {
  for window in &rig.monome_windows {
    let MonomeWindowRig::EdoShiftPad { id, monome, rect } = window else {
      continue;
    };
    let has_edo_grid = rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::EdoNoteGrid { monome: m, .. } if m == monome)
    });
    if !has_edo_grid {
      return Err(format!(
        "edo_shift_pad window {id:?} needs an edo_note_grid on the same monome {monome:?}",
      ));
    }
    let [gw, gh] = rig
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
/// so rigs stay unit-testable without fixture WAVs.
fn validate_softsteps(rig: &Rig) -> Result<(), String> {
  let softstep_ids: HashSet<&str> = rig.softsteps.iter().map(|s| s.id.as_str()).collect();
  let sampler_sink_ids: HashSet<&str> = rig
    .sinks
    .iter()
    .filter(|s| matches!(s, SinkRig::CpalSampler { .. }))
    .map(SinkRig::id)
    .collect();

  // pedals already claimed on a given device, so windows partition (never overlap).
  let mut claimed: std::collections::HashMap<&str, HashSet<u8>> = std::collections::HashMap::new();

  // Every kind claims pedals, so range + one-owner-per-pedal are checked once here
  // rather than per kind -- a pedal bound to both a drum and an accrete control has
  // no defined behavior, and the old check only looked at drumkit pads.
  let mut tap_pedal: Option<(&str, u8)> = None;
  let mut factor_claims: HashSet<(&str, PulseFactorRig)> = HashSet::new();
  let mut editmode_claims: HashSet<(&str, EditmodeControlKind)> = HashSet::new();
  let monome_ids: HashSet<&str> = rig.monomes.iter().map(|m| m.id.as_str()).collect();
  // A pedal that drives a grid's accrete bank or pulse needs that grid to be a play
  // surface; the same requirement the on-grid accrete_control / factored_pulse_pad carry.
  let play_grid_monomes: HashSet<&str> = rig
    .monome_windows
    .iter()
    .filter_map(|w| match w {
      MonomeWindowRig::EdoNoteGrid { monome, .. } => Some(monome.as_str()),
      _ => None,
    })
    .collect();
  for window in &rig.softstep_windows {
    require_ref("softstep_window.softstep", window.softstep(), &softstep_ids)?;
    for pedal in window.pedals() {
      if pedal > 9 {
        return Err(format!(
          "softstep window {:?} pedal {pedal} out of range (the KMSS labels are 0..9)",
          window.id(),
        ));
      }
    }
    // Cross-kind pedal ownership (the drumkit's own per-window duplicate check still
    // runs below; this catches collisions BETWEEN windows, including across kinds).
    if !matches!(window, SoftstepWindowRig::Drumkit { .. }) {
      let device_claims = claimed.entry(window.softstep()).or_default();
      for pedal in window.pedals() {
        if !device_claims.insert(pedal) {
          return Err(format!(
            "pedal {pedal} on softstep {:?} is claimed by more than one window",
            window.softstep(),
          ));
        }
      }
    }
    match window {
      SoftstepWindowRig::AccreteControl { id, monome, .. } => {
        require_ref("softstep_window.monome", monome, &monome_ids)?;
        if !play_grid_monomes.contains(monome.as_str()) {
          return Err(format!(
            "accrete_control window {id:?} targets monome {monome:?}, which has no edo_note_grid",
          ));
        }
      }
      SoftstepWindowRig::TapTempoPedal { id, pedal, .. } => {
        // ONE global tempo, so one tap pedal: two would be the same button twice,
        // and a reader could not tell which defines it.
        if let Some((first, first_pedal)) = tap_pedal {
          return Err(format!(
            "rig has two tap_tempo_pedal windows ({first:?} on pedal {first_pedal} and \
             {id:?} on pedal {pedal}); the tapped tempo is global, so there is one",
          ));
        }
        tap_pedal = Some((id, *pedal));
      }
      SoftstepWindowRig::PulseFactorPedal { id, monome, factor, .. } => {
        require_ref("softstep_window.monome", monome, &monome_ids)?;
        if !play_grid_monomes.contains(monome.as_str()) {
          return Err(format!(
            "pulse_factor_pedal window {id:?} targets monome {monome:?}, which has no edo_note_grid",
          ));
        }
        if !factor_claims.insert((monome.as_str(), *factor)) {
          return Err(format!(
            "monome {monome:?} has two {factor:?} pulse_factor_pedal windows ({id:?} is the second)",
          ));
        }
      }
      SoftstepWindowRig::EditmodeControl { id, monome, control, .. } => {
        require_ref("softstep_window.monome", monome, &monome_ids)?;
        if !play_grid_monomes.contains(monome.as_str()) {
          return Err(format!(
            "editmode_control window {id:?} targets monome {monome:?}, which has no edo_note_grid",
          ));
        }
        // One of each control per monome: a second would be a redundant twin.
        if !editmode_claims.insert((monome.as_str(), *control)) {
          return Err(format!(
            "monome {monome:?} has two {control:?} editmode_control windows ({id:?} is the second)",
          ));
        }
      }
      SoftstepWindowRig::Drumkit { .. } => {}
    }
    match window {
      SoftstepWindowRig::Drumkit { id, sink, pads, .. } => {
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
      // The single-pedal kinds are fully checked above (pedal range, one owner per
      // pedal, monome refs, tap uniqueness, per-monome factor/delete uniqueness).
      SoftstepWindowRig::AccreteControl { .. }
      | SoftstepWindowRig::TapTempoPedal { .. }
      | SoftstepWindowRig::PulseFactorPedal { .. }
      | SoftstepWindowRig::EditmodeControl { .. } => {}
    }
  }

  // No orphan devices: every declared softstep must be used by some window.
  for softstep in &rig.softsteps {
    let used = rig.softstep_windows.iter().any(|w| w.softstep() == softstep.id);
    if !used {
      return Err(format!(
        "rig declares softstep {:?} but no window uses it",
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
/// `loop_remap_undo` kind, so no existing rig's validation changes.
fn validate_looper(rig: &Rig) -> Result<(), String> {
  let count = |pred: fn(&MonomeWindowRig) -> bool| {
    rig.monome_windows.iter().filter(|w| pred(w)).count()
  };
  let loop_slots = count(|w| matches!(w, MonomeWindowRig::LoopSlots { .. }));
  let loop_displays = count(|w| matches!(w, MonomeWindowRig::LoopDisplay { .. }));
  // NB: edo_shift_pad is deliberately NOT in this set. It is a generally-valid
  // window (validated by `validate_shift_pads`), so a plain edo grid -- or the
  // surfaces runtime's two grids -- can carry a scroll pad without pulling in the
  // whole looper stack. Looper rigs still trip this via their loop_display etc.
  let any_loop_window = rig.monome_windows.iter().any(|w| {
    matches!(
      w,
      MonomeWindowRig::LoopSlots { .. }
        | MonomeWindowRig::LoopControl { .. }
        | MonomeWindowRig::LoopRemapModeToggle { .. }
        | MonomeWindowRig::LoopCopyButton { .. }
        | MonomeWindowRig::LoopRemapUndo { .. }
        | MonomeWindowRig::LoopDisplay { .. }
    )
  });

  // The [looper] table and the looper windows imply each other.
  if any_loop_window && rig.looper.is_none() {
    return Err("looper windows require a [looper] table".to_string());
  }
  if rig.looper.is_some() && !any_loop_window {
    return Err("a [looper] table requires the looper windows".to_string());
  }
  let Some(looper) = &rig.looper else {
    return Ok(());
  };

  // Core windows: a slot grid, a display, and the three transport controls. The
  // shift pad, mode toggle, copy and undo are optional add-ons.
  if loop_slots != 1 {
    return Err(format!("a looper rig needs exactly one loop_slots window, found {loop_slots}"));
  }
  if loop_displays != 1 {
    return Err(format!("a looper rig needs exactly one loop_display window, found {loop_displays}"));
  }
  for control in [LoopControlKind::Start, LoopControlKind::Stop, LoopControlKind::Play] {
    let present = rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::LoopControl { control: c, .. } if *c == control)
    });
    if !present {
      return Err(format!("a looper rig needs a loop_control with control = {control:?}"));
    }
  }

  // The display rect must be at least 2x2 so the row-picker column, the
  // column-picker row, and a main area can be derived from it.
  for window in &rig.monome_windows {
    if let MonomeWindowRig::LoopDisplay { id, rect, .. } = window {
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
  let edo = rig.monome_windows.iter().find_map(|w| match w {
    MonomeWindowRig::EdoNoteGrid { monome, rect, .. } => Some((monome.clone(), *rect)),
    _ => None,
  });
  let Some((edo_monome_id, [ex0, ey0, ex1, ey1])) = edo else {
    return Err("a looper rig needs an edo_note_grid window".to_string());
  };
  let [gw, gh] = rig
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
  for monome in &rig.monomes {
    let used = rig.monome_windows.iter().any(|w| w.monome() == monome.id);
    if !used {
      return Err(format!("looper rig declares monome {:?} but no window uses it", monome.id));
    }
  }

  Ok(())
}

/// A timbre-editor parameter row must be finite and monotonic. Log rows need a
/// positive base; a growth factor must exceed 1 or the row collapses.
fn validate_row_range(id: &str, row: &RowRangeRig) -> Result<(), String> {
  let finite = |v: f32| v.is_finite();
  match *row {
    RowRangeRig::Linear { min, max } => {
      if !finite(min) || !finite(max) || max < min {
        return Err(format!("timbre_editor window {id:?} linear row needs finite min <= max"));
      }
    }
    RowRangeRig::LogFactor { least, multiplier } => {
      if !finite(least) || least <= 0.0 || !finite(multiplier) || multiplier <= 1.0 {
        return Err(format!(
          "timbre_editor window {id:?} log_factor row needs least > 0 and multiplier > 1",
        ));
      }
    }
    RowRangeRig::LogRange { least, greatest } => {
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
/// group. A rig is modular -- it may omit a whole group -- but any group it
/// declares must appear in its entirety, and a group's dependencies must also be
/// present. (A rig with no recording buttons is valid; it just launches a
/// program with no recording abilities.)
fn validate_window_groups(rig: &Rig) -> Result<(), String> {
  let groups = window_groups(rig);
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
          "the {:?} window group requires the {:?} window group, which the rig does not declare",
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

fn window_groups(rig: &Rig) -> Vec<WindowGroup> {
  let has_kind =
    |pred: fn(&MonomeWindowRig) -> bool| rig.monome_windows.iter().any(pred);
  let has_control = |control: RecordControlKind| {
    rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::RecordControl { control: c, .. } if *c == control)
    })
  };
  let has_scale_control = |control: ScaleControlKind| {
    rig.monome_windows.iter().any(|w| {
      matches!(w, MonomeWindowRig::ScaleControl { control: c, .. } if *c == control)
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
          has_kind(|w| matches!(w, MonomeWindowRig::RemappableUn12Grid { .. })),
        ),
        (
          "preimage_row",
          has_kind(|w| matches!(w, MonomeWindowRig::PreimageRow { .. })),
        ),
        (
          "remap_undo_button",
          has_kind(|w| matches!(w, MonomeWindowRig::RemapUndoButton { .. })),
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
          has_kind(|w| matches!(w, MonomeWindowRig::ScaleSlots { .. })),
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
  fn loads_default_rigs() {
    for entry in std::fs::read_dir(rig_dir()).expect("read rigs dir") {
      let entry = entry.expect("rig dir entry");
      let path = entry.path();
      let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
      // Skip non-rigs: the shared SoftstepParams file, and the docs -- a rig's own
      // reference doc ends in `_readme.org`; README.org / RUNTIME-NOTES.org are general.
      // Dotfiles too: an open+modified Emacs buffer leaves a `.#<rig>.org` lock, which
      // is a DANGLING symlink ending in `.org` -- reading it fails with ENOENT.
      if name == "softstep.toml"
        || name == "README.org"
        || name == "RUNTIME-NOTES.org"
        || name.ends_with("_readme.org")
        || name.starts_with('.')
      {
        continue;
      }
      // Rigs are `.org` now (`.toml` still parses during/after the migration).
      if matches!(path.extension().and_then(|s| s.to_str()), Some("org") | Some("toml")) {
        load_rig_file(&path)
          .unwrap_or_else(|e| panic!("{} should parse: {e}", path.display()));
      }
    }
  }

  #[test]
  fn softstep_params_parse_and_default() {
    // A missing file -> defaults.
    assert_eq!(SoftstepParams::default().factor_release_ms, 25, "default quiet window");
    assert_eq!(SoftstepParams::default().on_sum, 20);
    // The shipped softstep.toml parses and every field is present.
    let params = load_softstep_params().expect("rigs/softstep.toml parses");
    assert!(params.pressure_full_scale > 0 && params.attack_ms > 0);
    // Partial files fill in defaults; unknown keys are rejected.
    let partial: SoftstepParams = toml::from_str("on_sum = 7").unwrap();
    assert_eq!(partial.on_sum, 7);
    assert_eq!(partial.factor_release_ms, 25, "unset key uses the default");
    assert!(toml::from_str::<SoftstepParams>("bogus = 1").is_err(), "unknown key rejected");
  }

  #[test]
  fn resolves_and_loads_mock_rigs_by_name() {
    // Mock-rig rigs live in mocks/, not rigs/, but must resolve by name so the
    // mock-grid tests and `cargo run -- <name>-mock` find them.
    let name = "monome-looper-58-8-1-timbre-mock";
    let path = rig_path(name).expect("mock name resolves");
    assert!(path.starts_with(mock_rig_dir()), "should resolve into mocks/, got {}", path.display());
    load_named_rig(name).expect("mock rig loads from the mock dir");
    // A genuinely missing name still points at rigs/ (the not-found error location).
    assert!(rig_path("definitely-not-a-real-rig").unwrap().starts_with(rig_dir()));
  }

  #[test]
  fn rejects_mixed_piano_mapping_variants() {
    let err = parse_rig(r#"
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
    let err = parse_rig(r#"
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
    let err = parse_rig(r#"
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
  fn rig_without_recording_buttons_is_valid() {
    // A modular rig that omits the recording feature entirely must parse:
    // it just launches a program with no recording abilities.
    parse_rig(r#"
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
"#).expect("a rig without recording buttons should be valid");
  }

  #[test]
  fn partial_remap_group_is_rejected() {
    // The remap group is all-or-nothing: a grid without its 12-edo row and undo
    // button is invalid.
    let err = parse_rig(r#"
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
    let err = parse_rig(r#"
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
    // which this rig omits entirely.
    let err = parse_rig(r#"
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
  fn scale_slots_rig_is_valid() {
    // The remap group plus a complete scale group (slot grid + store + empty).
    parse_rig(r#"
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
"#).expect("a remap + complete scale rig should be valid");
  }

  #[test]
  fn partial_scale_group_is_rejected() {
    // Slot grid present, but the store/empty arm buttons are missing.
    let err = parse_rig(r#"
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
    let err = parse_rig(r#"
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
    let err = parse_rig(r#"
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

  // ---- Looper rig validation. ----

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
  fn looper_rig_is_valid() {
    parse_rig(&valid_looper_toml()).expect("a complete looper rig should be valid");
  }

  #[test]
  fn clock_duty_must_be_in_range() {
    let err = parse_rig(&valid_looper_toml().replace("clock_bpm = 300", "clock_bpm = 300\nclock_duty = 1.5"))
      .expect_err("a clock_duty above 1 should fail");
    assert!(err.contains("clock_duty"), "{err}");
  }

  #[test]
  fn looper_windows_require_looper_table() {
    let err = parse_rig(&valid_looper_toml().replace(LOOPER_TABLE, ""))
      .expect_err("looper windows without a [looper] table should fail");
    assert!(err.contains("[looper] table"), "{err}");
  }

  #[test]
  fn looper_table_requires_looper_windows() {
    // A [looper] table atop a plain sawwave grid (no loop_* windows) is invalid.
    let toml = format!("{LOOPER_HEADER}{LOOPER_TABLE}{LOOPER_MONOMES}{LOOPER_EDO_GRID}");
    let err = parse_rig(&toml).expect_err("a [looper] table without looper windows should fail");
    assert!(err.contains("requires the looper windows"), "{err}");
  }

  #[test]
  fn loop_display_must_be_at_least_2x2() {
    let err = parse_rig(&valid_looper_toml().replace("rect = [0, 5, 15, 15]", "rect = [0, 5, 0, 5]"))
      .expect_err("a 1x1 loop_display should fail");
    assert!(err.contains("at least 2x2"), "{err}");
  }

  #[test]
  fn remap_center_must_be_inside_edo_grid() {
    let err = parse_rig(&valid_looper_toml().replace("remap_center = [7, 7]", "remap_center = [20, 20]"))
      .expect_err("an out-of-bounds remap_center should fail");
    assert!(err.contains("must lie inside"), "{err}");
  }

  #[test]
  fn looper_requires_all_three_transport_controls() {
    let err = parse_rig(&valid_looper_toml().replace(LOOPER_PLAY, ""))
      .expect_err("a looper missing the play control should fail");
    assert!(err.contains("control = Play"), "{err}");
  }

  #[test]
  fn duplicate_loop_control_kind_is_rejected() {
    let dup = valid_looper_toml().replace(
      "control = \"play\"\nrect = [2, 3, 2, 3]",
      "control = \"start\"\nrect = [2, 3, 2, 3]",
    );
    let err = parse_rig(&dup).expect_err("duplicate loop_control kind should fail");
    assert!(err.contains("duplicate loop_control kind"), "{err}");
  }

  #[test]
  fn loop_control_rect_must_be_single_cell() {
    let err = parse_rig(&valid_looper_toml().replace("rect = [0, 3, 0, 3]", "rect = [0, 3, 1, 3]"))
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
    let rig = parse_rig(&looper_with_editor(TIMBRE_EDITOR_MIN))
      .expect("a timbre_editor with omitted rows should use 6_plan 5 defaults");
    let ed = rig
      .monome_windows
      .iter()
      .find_map(MonomeWindowRig::timbre_editor_rows)
      .expect("the editor is present");
    assert_eq!(ed.0, TimbreTarget::Loop);
    assert_eq!(ed.1[0], RowRangeRig::LogRange { least: 0.0009, greatest: 0.15 }, "amplitude default");
    assert_eq!(ed.1[1], RowRangeRig::Linear { min: 0.0, max: 1.0 }, "am depth default");
    assert_eq!(ed.1[2], RowRangeRig::LogFactor { least: 0.25, multiplier: 2.0 }, "am freq default");
    assert_eq!(ed.1[4], RowRangeRig::LogFactor { least: 5.0, multiplier: 2.0 }, "fm cents default");
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
    let rig = parse_rig(&looper_with_editor(editor)).expect("explicit rows should parse");
    let ed = rig.monome_windows.iter().find_map(MonomeWindowRig::timbre_editor_rows).unwrap();
    assert_eq!(ed.0, TimbreTarget::Live);
    assert_eq!(ed.1[0], RowRangeRig::LogRange { least: 0.001, greatest: 0.2 });
    assert_eq!(ed.1[1], RowRangeRig::Linear { min: 0.0, max: 0.5 });
    // An omitted row still falls back to its default.
    assert_eq!(ed.1[2], RowRangeRig::LogFactor { least: 0.25, multiplier: 2.0 });
  }

  #[test]
  fn am_shape_family_parses_and_defaults() {
    // Absent [am.shape] -> no am table (the runtime defaults to sin_to_square).
    let none = parse_rig(&looper_with_editor(TIMBRE_EDITOR_MIN)).unwrap();
    assert!(none.am.is_none(), "no [am] table when omitted");
    // Explicit family parses.
    let with = parse_rig(&format!(
      "{}\n[am.shape]\nfamily = \"tri_to_square\"\n",
      looper_with_editor(TIMBRE_EDITOR_MIN)
    ))
    .expect("am.shape should parse");
    assert_eq!(with.am.unwrap().shape.family, AmShapeFamilyRig::TriToSquare);
  }

  #[test]
  fn save_undo_double_ms_parses_and_defaults() {
    let su = |toml: &str| {
      parse_rig(toml)
        .unwrap()
        .monome_windows
        .iter()
        .find_map(MonomeWindowRig::timbre_editor_save_undo_ms)
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
    let err = parse_rig(&looper_with_editor(&short)).expect_err("a 3-row editor should fail");
    assert!(err.contains("at least 7 rows"), "{err}");
  }

  #[test]
  fn timbre_editor_rect_must_be_wide_enough() {
    let narrow = TIMBRE_EDITOR_MIN.replace("rect = [0, 0, 15, 6]", "rect = [0, 0, 3, 6]");
    let err = parse_rig(&looper_with_editor(&narrow)).expect_err("a 4-wide editor should fail");
    assert!(err.contains("at least 5 cells wide"), "{err}");
  }

  #[test]
  fn timbre_editor_rejects_a_degenerate_log_row() {
    let bad = format!(
      "{}am_frequency = {{ kind = \"log_factor\", least = 0.25, multiplier = 1.0 }}\n",
      TIMBRE_EDITOR_MIN
    );
    let err = parse_rig(&looper_with_editor(&bad)).expect_err("multiplier 1.0 collapses the row");
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
      rect: RectSpecRig,
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
  fn timbre_looper_rig_has_both_editors() {
    // The full instrument (7_layout.org): a loop-timbre editor on edo + a live-timbre
    // editor on loops, with the looper stack at its unfolded positions.
    let rig = load_named_rig("monome-looper-58-8-1-timbre").expect("timbre rig loads");
    let editors: Vec<(String, TimbreTarget)> = rig
      .monome_windows
      .iter()
      .filter_map(|w| w.timbre_editor_rows().map(|(t, _)| (w.monome().to_string(), t)))
      .collect();
    assert_eq!(editors.len(), 2, "two timbre editors");
    assert!(editors.iter().any(|(m, t)| m == "edo" && *t == TimbreTarget::Loop), "edo loop editor");
    assert!(editors.iter().any(|(m, t)| m == "loops" && *t == TimbreTarget::Live), "loops live editor");
    // The loop display is at its unfolded position (reflowed up at runtime).
    let display = rig.monome_windows.iter().find(|w| w.kind_name() == "loop_display").unwrap();
    assert_eq!(display.rect(), [0, 9, 15, 15], "loop display sits below the unfolded editor");
  }

  #[test]
  fn rect_spec_rejects_a_bad_edge_expr() {
    #[derive(serde::Deserialize)]
    struct W {
      rect: RectSpecRig,
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

  /// Two monomes + one softstep with the pedal-control kinds. Minimal but complete:
  /// each grid is a play surface, so accrete/pulse pedals have something to target.
  const PEDAL_CONTROLS_TOML: &str = r#"version = 1
id = "pedal-controls"
title = "Pedal controls"

[[tunings]]
id = "main"
edo = 46
x_step = 9
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
id = "grid-b"
monome = "b"
kind = "edo_note_grid"
rect = [0, 0, 15, 15]
tuning = "main"
sink = "synth"

[[softsteps]]
id = "old"
select.name_contains = "SSCOM"

[[softstep_windows]]
id = "clear-a"
softstep = "old"
kind = "accrete_control"
pedal = 1
monome = "a"
control = "clear"

[[softstep_windows]]
id = "accrete-a"
softstep = "old"
kind = "accrete_control"
pedal = 2
monome = "a"
control = "accrete"

[[softstep_windows]]
id = "tap"
softstep = "old"
kind = "tap_tempo_pedal"
pedal = 8

[[softstep_windows]]
id = "x2-a"
softstep = "old"
kind = "pulse_factor_pedal"
pedal = 5
monome = "a"
factor = "x2"
"#;

  #[test]
  fn a_rig_can_bind_pedals_to_accrete_tap_and_pulse() {
    let rig = parse_rig(PEDAL_CONTROLS_TOML).expect("pedal-control kinds should parse");
    assert_eq!(rig.softstep_windows.len(), 4);
    let kinds: Vec<&str> = rig.softstep_windows.iter().map(|w| w.kind_name()).collect();
    assert_eq!(kinds, ["accrete_control", "accrete_control", "tap_tempo_pedal", "pulse_factor_pedal"]);
    // Every kind reports the pedals it claims, so ownership is checkable uniformly.
    let pedals: Vec<Vec<u8>> = rig.softstep_windows.iter().map(|w| w.pedals()).collect();
    assert_eq!(pedals, [vec![1], vec![2], vec![8], vec![5]]);
  }

  /// The collision the OLD check could not see: it only walked drumkit pads, so a
  /// pedal bound to both a drum and a control had no defined behavior and loaded fine.
  #[test]
  fn one_pedal_cannot_be_claimed_by_two_windows_even_across_kinds() {
    let toml = PEDAL_CONTROLS_TOML.replace(
      "id = \"accrete-a\"\nsoftstep = \"old\"\nkind = \"accrete_control\"\npedal = 2",
      "id = \"accrete-a\"\nsoftstep = \"old\"\nkind = \"accrete_control\"\npedal = 1",
    );
    let err = parse_rig(&toml).expect_err("pedal 1 is claimed twice");
    assert!(err.contains("claimed by more than one window"), "{err}");
  }

  #[test]
  fn a_pedal_label_above_9_is_rejected() {
    let toml = PEDAL_CONTROLS_TOML.replace("kind = \"tap_tempo_pedal\"\npedal = 8", "kind = \"tap_tempo_pedal\"\npedal = 10");
    let err = parse_rig(&toml).expect_err("the KMSS labels are 0..9");
    assert!(err.contains("out of range"), "{err}");
  }

  /// The tapped tempo is ONE global value, so two tap pedals would be the same
  /// button twice and a reader could not tell which defines it.
  #[test]
  fn two_tap_tempo_pedals_are_rejected() {
    let toml = format!(
      "{PEDAL_CONTROLS_TOML}\n[[softstep_windows]]\nid = \"tap2\"\nsoftstep = \"old\"\nkind = \"tap_tempo_pedal\"\npedal = 3\n",
    );
    let err = parse_rig(&toml).expect_err("one global tempo means one tap pedal");
    assert!(err.contains("two tap_tempo_pedal"), "{err}");
  }

  #[test]
  fn two_pedals_giving_one_monome_the_same_factor_are_rejected() {
    let toml = format!(
      "{PEDAL_CONTROLS_TOML}\n[[softstep_windows]]\nid = \"x2-a-again\"\nsoftstep = \"old\"\nkind = \"pulse_factor_pedal\"\npedal = 4\nmonome = \"a\"\nfactor = \"x2\"\n",
    );
    let err = parse_rig(&toml).expect_err("monome a already has an x2 pedal");
    assert!(err.contains("two"), "{err}");
  }

  /// The same factor on the OTHER monome is fine -- that is the whole point of
  /// per-monome factors.
  #[test]
  fn the_other_monome_may_have_the_same_factor() {
    let toml = format!(
      "{PEDAL_CONTROLS_TOML}\n[[softstep_windows]]\nid = \"x2-b\"\nsoftstep = \"old\"\nkind = \"pulse_factor_pedal\"\npedal = 1\nmonome = \"b\"\nfactor = \"x2\"\n",
    );
    // pedal 1 is taken by clear-a, so use a free one.
    let toml = toml.replace("id = \"x2-b\"\nsoftstep = \"old\"\nkind = \"pulse_factor_pedal\"\npedal = 1", "id = \"x2-b\"\nsoftstep = \"old\"\nkind = \"pulse_factor_pedal\"\npedal = 4");
    parse_rig(&toml).expect("each monome gets its own x2");
  }

  #[test]
  fn a_pedal_control_targeting_a_monome_with_no_play_surface_is_rejected() {
    // Drop grid-b's play surface entirely, leaving monome `b` declared but silent,
    // then point an accrete pedal at it: there is nothing to accrete from.
    let toml = PEDAL_CONTROLS_TOML.replace(
      "[[monome_windows]]\nid = \"grid-b\"\nmonome = \"b\"\nkind = \"edo_note_grid\"\nrect = [0, 0, 15, 15]\ntuning = \"main\"\nsink = \"synth\"\n",
      "",
    );
    assert!(!toml.contains("grid-b"), "the grid-b window should be gone");
    let toml = toml.replace("monome = \"a\"\ncontrol = \"clear\"", "monome = \"b\"\ncontrol = \"clear\"");
    let err = parse_rig(&toml).expect_err("accrete needs a play surface to accrete from");
    assert!(err.contains("edo_note_grid"), "{err}");
  }

  #[test]
  fn a_pedal_control_referencing_an_unknown_monome_is_rejected() {
    let toml = PEDAL_CONTROLS_TOML.replace("monome = \"a\"\ncontrol = \"clear\"", "monome = \"nope\"\ncontrol = \"clear\"");
    let err = parse_rig(&toml).expect_err("unknown monome ref");
    assert!(err.contains("nope"), "{err}");
  }

  #[test]
  fn drumkit_rig_is_valid_with_defaults() {
    let rig = parse_rig(DRUMKIT_TOML).expect("a complete drumkit rig should be valid");
    assert_eq!(rig.softsteps.len(), 1);
    assert_eq!(rig.softsteps[0].select.name_substring(), "SSCOM", "default select substring");
    let SoftstepWindowRig::Drumkit { pads, .. } = &rig.softstep_windows[0] else {
      panic!("expected a drumkit window");
    };
    assert_eq!(pads[0].gain, 1.0, "default pad gain");
    assert_eq!(pads.len(), 3);
  }

  /// The two sibling rigs differ in exactly one place: what the NEWER SoftStep does.
  /// Everything else -- grids, overlays, and the older board's sustain/edit pedals --
  /// must stay identical, so a change to one is a deliberate change to both.
  #[test]
  fn the_two_edogrid_rigs_differ_only_in_the_newer_boards_job() {
    let pulse = load_named_rig("2-edogrids_ss-accrete_ss-pulse").expect("pulse rig loads");
    let drums = load_named_rig("2-edogrids_ss-accrete_ss-drums").expect("drums rig loads");

    assert_eq!(pulse.monomes, drums.monomes, "same two grids, pinned by the same serials");
    assert_eq!(pulse.monome_windows, drums.monome_windows, "same on-grid overlays");
    assert_eq!(pulse.expression_pedals, drums.expression_pedals, "same EX-P volume pedals");
    assert_eq!(pulse.tunings, drums.tunings, "same 46-EDO tuning");

    // Both declare the same two boards; only the newer one's windows differ.
    let boards = |r: &Rig| -> Vec<String> {
      r.softsteps.iter().map(|s| s.select.name_substring().to_string()).collect()
    };
    assert_eq!(boards(&pulse), boards(&drums), "same two boards, matched the same way");
    let on_old = |r: &Rig| -> Vec<String> {
      r.softstep_windows
        .iter()
        .filter(|w| w.softstep() == "old")
        .map(|w| format!("{}:{}", w.id(), w.kind_name()))
        .collect()
    };
    assert_eq!(on_old(&pulse), on_old(&drums), "the older board's job is untouched");
  }

  /// The drum map is bound to PRINTED LABELS, but the newer board is physically
  /// rotated 180 degrees, so the labels here are deliberately NOT the ones
  /// `2-monomes_kmss-drums` used -- they are chosen so each drum lands under the
  /// same foot. Rebinding these to "match" the older rig would mirror the kit.
  #[test]
  fn the_drum_kit_sits_on_the_newer_board_in_foot_order() {
    let rig = load_named_rig("2-edogrids_ss-accrete_ss-drums").expect("drums rig loads");
    let kit = rig
      .softstep_windows
      .iter()
      .find(|w| matches!(w, SoftstepWindowRig::Drumkit { .. }))
      .expect("the rig declares a drumkit");
    assert_eq!(kit.softstep(), "new", "the kit is on the NEWER board");

    let SoftstepWindowRig::Drumkit { pads, .. } = kit else { unreachable!() };
    // As Jeff stands: far row reads 5 4 3 2 1, near row reads 0 9 8 7 6.
    let far: Vec<&str> = [5u8, 4, 3, 2, 1].iter().map(|p| pad_voice(pads, *p)).collect();
    let near: Vec<&str> = [0u8, 9, 8, 7, 6].iter().map(|p| pad_voice(pads, *p)).collect();
    assert_eq!(far, ["high_tom.wav", "low_tom.wav", "wood_block.wav", "cowbell.wav",
                     "crash_electronic.wav"]);
    assert_eq!(near, ["snare.wav", "kick.wav", "<ditto>", "hat.wav", "open_hat.wav"]);
  }

  fn pad_voice(pads: &[DrumPadRig], pedal: u8) -> &str {
    let pad = pads.iter().find(|p| p.pedal == pedal).expect("every pedal is bound");
    if pad.ditto {
      "<ditto>"
    } else {
      pad.sample.as_deref().expect("a non-ditto pad names a sample")
    }
  }

  #[test]
  fn drumkit_pedal_out_of_range_is_rejected() {
    let err = parse_rig(&DRUMKIT_TOML.replace("pedal = 1,", "pedal = 10,"))
      .expect_err("pedal 10 is out of the 0..9 label range");
    assert!(err.contains("out of range"), "{err}");
  }

  #[test]
  fn drumkit_duplicate_pedal_in_one_window_is_rejected() {
    let err = parse_rig(&DRUMKIT_TOML.replace("pedal = 2,", "pedal = 1,"))
      .expect_err("the same pedal assigned twice should fail");
    assert!(err.contains("more than once"), "{err}");
  }

  #[test]
  fn drumkit_sink_must_be_a_cpal_sampler() {
    let err = parse_rig(&DRUMKIT_TOML.replace("kind = \"cpal_sampler\"", "kind = \"midi\"")
      // a midi sink has no sample_rate/buffer_frames/amplitude fields
      .replace("sample_rate = 48000\nbuffer_frames = 128\namplitude = 0.8\n", ""))
      .expect_err("a non-sampler sink should be rejected for a drumkit");
    assert!(err.contains("cpal_sampler"), "{err}");
  }

  #[test]
  fn drumkit_unknown_softstep_ref_is_rejected() {
    let err = parse_rig(&DRUMKIT_TOML.replace("softstep = \"feet\"", "softstep = \"nope\""))
      .expect_err("an unknown softstep reference should fail");
    assert!(err.contains("unknown id \"nope\""), "{err}");
  }

  #[test]
  fn drumkit_orphan_softstep_is_rejected() {
    let toml = format!("{DRUMKIT_TOML}\n[[softsteps]]\nid = \"unused\"\n");
    let err = parse_rig(&toml).expect_err("a softstep no window uses should fail");
    assert!(err.contains("no window uses it"), "{err}");
  }

  #[test]
  fn drumkit_sample_path_must_not_escape_assets() {
    let err = parse_rig(&DRUMKIT_TOML.replace("\"kick.wav\"", "\"../secrets/kick.wav\""))
      .expect_err("a sample path with .. should fail");
    assert!(err.contains("'..'"), "{err}");
  }

  #[test]
  fn pedal_claimed_by_two_windows_is_rejected() {
    let toml = format!(
      "{DRUMKIT_TOML}\n[[softstep_windows]]\nid = \"kit2\"\nsoftstep = \"feet\"\nkind = \"drumkit\"\nsink = \"drums\"\npads = [{{ pedal = 1, sample = \"other.wav\" }}]\n",
    );
    let err = parse_rig(&toml).expect_err("two windows claiming pedal 1 should fail");
    assert!(err.contains("more than one window"), "{err}");
  }

  // ---- ditto pads ----

  #[test]
  fn ditto_pad_is_valid_with_no_sample() {
    let toml = DRUMKIT_TOML.replace("{ pedal = 0, sample = \"wood_block.wav\" },", "{ pedal = 0, ditto = true },");
    let rig = parse_rig(&toml).expect("a ditto pad with no sample should be valid");
    let SoftstepWindowRig::Drumkit { pads, .. } = &rig.softstep_windows[0] else {
      panic!("expected a drumkit window");
    };
    let ditto = pads.iter().find(|p| p.pedal == 0).expect("pedal 0");
    assert!(ditto.ditto, "ditto flag set");
    assert_eq!(ditto.sample, None, "a ditto pad names no sample");
  }

  #[test]
  fn pad_with_neither_sample_nor_ditto_is_rejected() {
    let toml = DRUMKIT_TOML.replace("{ pedal = 0, sample = \"wood_block.wav\" },", "{ pedal = 0 },");
    let err = parse_rig(&toml).expect_err("a pad with neither sample nor ditto should fail");
    assert!(err.contains("exactly one of `sample` or `ditto = true`"), "{err}");
  }

  #[test]
  fn pad_with_both_sample_and_ditto_is_rejected() {
    let toml = DRUMKIT_TOML.replace(
      "{ pedal = 0, sample = \"wood_block.wav\" },",
      "{ pedal = 0, sample = \"wood_block.wav\", ditto = true },",
    );
    let err = parse_rig(&toml).expect_err("a pad with both sample and ditto should fail");
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
    let err = parse_rig(&toml).expect_err("two ditto pads in one window should fail");
    assert!(err.contains("two ditto pads"), "{err}");
  }

  // ---- edo_shift_pad as a generally-valid window (freed from the looper) ----

  #[test]
  fn shift_pad_on_a_plain_grid_is_valid() {
    // A plain sawwave grid + a scroll pad, with NO looper stack, must parse:
    // the shift pad is no longer trapped in the looper's all-or-nothing set.
    parse_rig(r#"
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
    let err = parse_rig(r#"
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
    let rig = parse_rig(SURFACES_MIN).expect("two grids that cross-control timbre should be valid");
    let selectors: Vec<(&str, &str)> = rig
      .monome_windows
      .iter()
      .filter_map(|w| match w {
        MonomeWindowRig::WaveformSelector { monome, controls, .. } => Some((monome.as_str(), controls.as_str())),
        _ => None,
      })
      .collect();
    assert!(selectors.contains(&("a", "b")), "grid a's strip controls b");
    assert!(selectors.contains(&("b", "a")), "grid b's strip controls a");
  }

  #[test]
  fn waveform_selector_must_cover_four_cells() {
    let err = parse_rig(&SURFACES_MIN.replace("rect = [0, 0, 3, 0]\ncontrols = \"b\"", "rect = [0, 0, 2, 0]\ncontrols = \"b\""))
      .expect_err("a 3-wide selector should fail");
    assert!(err.contains("exactly 4 cells"), "{err}");
  }

  #[test]
  fn waveform_selector_controls_must_be_a_declared_monome() {
    let err = parse_rig(&SURFACES_MIN.replace("controls = \"b\"", "controls = \"nope\""))
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
    let err = parse_rig(&toml).expect_err("controlling a gridless monome should fail");
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
    let rig = parse_rig(&toml).expect("a full accrete trio should validate");
    let kinds: Vec<AccreteControlKind> = rig
      .monome_windows
      .iter()
      .filter_map(|w| match w {
        MonomeWindowRig::AccreteControl { control, .. } => Some(*control),
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
    // Drop the accrete button: the trio is incomplete, so the rig must fail.
    let cut = ACCRETE_TRIO.find("\n[[monome_windows]]\nid = \"acc-accrete\"").unwrap();
    let toml = format!("{SURFACES_MIN}{}", &ACCRETE_TRIO[..cut]);
    let err = parse_rig(&toml).expect_err("a partial trio should fail");
    assert!(err.contains("missing kind Accrete"), "{err}");
  }

  #[test]
  fn accrete_control_must_be_a_single_cell() {
    let toml =
      format!("{SURFACES_MIN}{}", ACCRETE_TRIO.replace("rect = [0, 15, 0, 15]", "rect = [0, 15, 1, 15]"));
    let err = parse_rig(&toml).expect_err("a 2-cell accrete button should fail");
    assert!(err.contains("exactly one cell"), "{err}");
  }

  #[test]
  fn accrete_control_kinds_must_be_unique_per_monome() {
    let toml = format!(
      "{SURFACES_MIN}{}",
      ACCRETE_TRIO.replace("control = \"needs_holding\"", "control = \"clear\""),
    );
    let err = parse_rig(&toml).expect_err("two clear buttons on one monome should fail");
    assert!(err.contains("duplicate accrete_control kind"), "{err}");
  }

  #[test]
  fn accrete_control_needs_an_edo_grid_on_its_monome() {
    // Declare the trio on a monome that has no edo_note_grid.
    let toml = format!(
      "{SURFACES_MIN}\n[[monomes]]\nid = \"c\"\nlisten_port = 9002\nprefix = \"/c\"\n{}",
      ACCRETE_TRIO.replace("monome = \"a\"", "monome = \"c\""),
    );
    let err = parse_rig(&toml).expect_err("accrete buttons on a gridless monome should fail");
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
    let rig = parse_rig(&toml).expect("a single-cell distortion toggle validates");
    assert!(rig
      .monome_windows
      .iter()
      .any(|w| matches!(w, MonomeWindowRig::DistortionToggle { .. })));
    // The sink's distortion curve defaults (scale 1.0, shape 2.0) fill in when absent.
    let SinkRig::CpalSynth { distortion_scale, distortion_shape, .. } = &rig.sinks[0] else {
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
    let err = parse_rig(&toml).expect_err("a 2-cell toggle should fail");
    assert!(err.contains("exactly one cell"), "{err}");
  }

  // ---- [[timbres]] (the four selectable timbres behind the waveform strip) ----

  const TIMBRES_FOUR: &str = r#"
[[timbres]]
waveform = "sin"
[[timbres]]
waveform = "tri"
abs_am_depth = 0.5
abs_am_hz = 3.0
am_shape = 1.0
rel_am_depth = 0.7
rel_am_freq = 0.5
[[timbres]]
waveform = "square"
amplitude = 0.6
[[timbres]]
waveform = "saw"
abs_fm_depth_cents = 30.0
abs_fm_hz = 6.0
rel_fm_depth = 1.5
rel_fm_freq = 2.0
"#;

  #[test]
  fn timbres_parse_with_short_names_and_off_defaults() {
    let toml = format!("{SURFACES_MIN}{TIMBRES_FOUR}");
    let rig = parse_rig(&toml).expect("a 4-entry [[timbres]] table validates");
    assert_eq!(rig.timbres.len(), 4);
    assert_eq!(rig.timbres[0].waveform, WaveformChoice::Sine, "'sin' alias");
    assert_eq!(rig.timbres[1].waveform, WaveformChoice::Triangle, "'tri' alias");
    // Defaults: full amplitude, modulation (abs and rel alike) off.
    assert_eq!(rig.timbres[0].amplitude, 1.0);
    assert_eq!(rig.timbres[0].abs_am_depth, 0.0);
    assert_eq!(rig.timbres[0].abs_fm_depth_cents, 0.0);
    assert_eq!(rig.timbres[0].rel_am_depth, 0.0);
    assert_eq!(rig.timbres[0].rel_fm_depth, 0.0);
    assert_eq!(rig.timbres[0].rel_am_freq, 1.0);
    assert_eq!(rig.timbres[0].rel_fm_freq, 1.0);
    // Explicit values land.
    assert_eq!(rig.timbres[1].abs_am_depth, 0.5);
    assert_eq!(rig.timbres[1].rel_am_depth, 0.7);
    assert_eq!(rig.timbres[1].rel_am_freq, 0.5);
    assert_eq!(rig.timbres[2].amplitude, 0.6);
    assert_eq!(rig.timbres[3].abs_fm_hz, 6.0);
    // rel_fm_depth > 1 is legal: that is the through-zero region.
    assert_eq!(rig.timbres[3].rel_fm_depth, 1.5);
    assert_eq!(rig.timbres[3].rel_fm_freq, 2.0);
  }

  // ---- [[expression_pedals]] (the EX-P volume pedals) ----

  const EXPRESSION_PEDALS: &str = r#"
[[expression_pedals]]
channel = 1
monome = "a"
[[expression_pedals]]
channel = 2
monome = "b"
"#;

  #[test]
  fn expression_pedals_parse_and_default_to_none() {
    let rig = parse_rig(&format!("{SURFACES_MIN}{EXPRESSION_PEDALS}")).expect("two pedals validate");
    assert_eq!(rig.expression_pedals.len(), 2);
    assert_eq!(rig.expression_pedals[0].channel, 1);
    assert_eq!(rig.expression_pedals[1].monome, "b");
    // Absent = empty: rigs without pedals are untouched.
    let rig = parse_rig(SURFACES_MIN).expect("no pedals is fine");
    assert!(rig.expression_pedals.is_empty());
  }

  #[test]
  fn expression_pedals_reject_bad_channels_and_double_claims() {
    let bad = |s: &str, contains: &str| {
      let err = parse_rig(&format!("{SURFACES_MIN}{s}")).expect_err("must fail");
      assert!(err.contains(contains), "{err}");
    };
    bad(&EXPRESSION_PEDALS.replace("channel = 2", "channel = 3"), "1 or 2");
    bad(&EXPRESSION_PEDALS.replace("channel = 2", "channel = 1"), "two expression pedals");
    bad(&EXPRESSION_PEDALS.replace("monome = \"b\"", "monome = \"a\""), "two expression pedals");
    bad(&EXPRESSION_PEDALS.replace("monome = \"b\"", "monome = \"nope\""), "no edo_note_grid");
    // The taper knobs: the linear splice must leave room for the exponential, and
    // the exponential must span a positive dB range.
    let with = |extra: &str| format!("{EXPRESSION_PEDALS}{extra}");
    bad(&with("curve_initial_lin_frac = 1.0\n"), "curve_initial_lin_frac");
    bad(&with("curve_remainder_exp_db = 0.0\n"), "curve_remainder_exp_db");
  }

  #[test]
  fn timbres_must_have_exactly_four_entries() {
    let three = TIMBRES_FOUR.rsplit_once("[[timbres]]").unwrap().0;
    let err = parse_rig(&format!("{SURFACES_MIN}{three}")).expect_err("3 entries fail");
    assert!(err.contains("exactly 4"), "{err}");
  }

  #[test]
  fn timbres_reject_out_of_range_parameters() {
    let toml = format!(
      "{SURFACES_MIN}{}",
      TIMBRES_FOUR.replace("abs_am_depth = 0.5", "abs_am_depth = 1.5"),
    );
    let err = parse_rig(&toml).expect_err("abs_am_depth > 1 fails");
    assert!(err.contains("abs_am_depth"), "{err}");
    // The relative AM depth has the same 0..=1 range...
    let toml = format!(
      "{SURFACES_MIN}{}",
      TIMBRES_FOUR.replace("rel_am_depth = 0.7", "rel_am_depth = 1.2"),
    );
    let err = parse_rig(&toml).expect_err("rel_am_depth > 1 fails");
    assert!(err.contains("rel_am_depth"), "{err}");
    // ...but a negative relative rate is rejected (0 itself is legal: "off").
    let toml = format!(
      "{SURFACES_MIN}{}",
      TIMBRES_FOUR.replace("rel_fm_freq = 2.0", "rel_fm_freq = -1.0"),
    );
    let err = parse_rig(&toml).expect_err("negative rel_fm_freq fails");
    assert!(err.contains("rel_fm_freq"), "{err}");
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
    let rig = parse_rig(&format!("{SURFACES_MIN}{toggles}")).expect("both toggles validate");
    assert!(rig.monome_windows.iter().any(|w| matches!(w, MonomeWindowRig::SlideToggle { .. })));
    assert!(rig.monome_windows.iter().any(|w| matches!(w, MonomeWindowRig::MonoToggle { .. })));
    // The [slide] knobs default sensibly.
    let slide = rig.slide.unwrap_or_default();
    assert_eq!(slide.candidate_window_ms, 1000);
    assert_eq!(slide.duration_ms, 100);
    // A 2-cell slide toggle fails like any single-cell toggle.
    let err = parse_rig(&format!(
      "{SURFACES_MIN}{}",
      toggles.replace("rect = [1, 1, 1, 1]", "rect = [1, 1, 2, 1]"),
    ))
    .expect_err("a 2-cell slide toggle should fail");
    assert!(err.contains("exactly one cell"), "{err}");
  }

  #[test]
  fn factored_pulse_pad_must_be_three_by_two() {
    let pad = r#"
[[monome_windows]]
id = "poly-a"
monome = "a"
kind = "factored_pulse_pad"
rect = [13, 0, 15, 1]
"#;
    let rig = parse_rig(&format!("{SURFACES_MIN}{pad}")).expect("a 3x2 pad validates");
    assert!(rig.monome_windows.iter().any(|w| matches!(w, MonomeWindowRig::FactoredPulsePad { .. })));
    assert_eq!(rig.tap_tempo.unwrap_or_default().window_ms, 2000, "default window");
    let err = parse_rig(&format!("{SURFACES_MIN}{}", pad.replace("rect = [13, 0, 15, 1]", "rect = [13, 0, 15, 2]")))
      .expect_err("a 3x3 pad should fail");
    assert!(err.contains("exactly 3x2"), "{err}");
  }

  #[test]
  fn chord_block_must_be_five_by_two_on_a_play_grid_at_most_once() {
    let block = r#"
[[monome_windows]]
id = "chords-a"
monome = "a"
kind = "chord_block"
rect = [5, 0, 9, 1]
"#;
    let rig = parse_rig(&format!("{SURFACES_MIN}{block}")).expect("a 5x2 block validates");
    assert!(rig.monome_windows.iter().any(|w| matches!(w, MonomeWindowRig::ChordBlock { .. })));

    // Wrong shape.
    let err = parse_rig(&format!(
      "{SURFACES_MIN}{}",
      block.replace("rect = [5, 0, 9, 1]", "rect = [5, 0, 8, 1]"),
    ))
    .expect_err("a 4x2 block should fail");
    assert!(err.contains("exactly 5x2"), "{err}");

    // No edo_note_grid on that monome: a third, grid-less monome "c".
    let gridless = r#"
[[monomes]]
id = "c"
listen_port = 9002
prefix = "/c"
select.size = [16, 16]
"#;
    let err = parse_rig(&format!(
      "{SURFACES_MIN}{gridless}{}",
      block.replace("monome = \"a\"", "monome = \"c\""),
    ))
    .expect_err("a block on a grid-less monome should fail");
    assert!(err.contains("needs an edo_note_grid"), "{err}");

    // Two blocks on one monome.
    let twin = block.replace("chords-a", "chords-a2").replace("rect = [5, 0, 9, 1]", "rect = [5, 2, 9, 3]");
    let err = parse_rig(&format!("{SURFACES_MIN}{block}{twin}"))
      .expect_err("two blocks on one monome should fail");
    assert!(err.contains("more than one chord_block"), "{err}");

    // Off the grid.
    let err = parse_rig(&format!(
      "{SURFACES_MIN}{}",
      block.replace("rect = [5, 0, 9, 1]", "rect = [12, 0, 16, 1]"),
    ))
    .expect_err("a block past the right edge should fail");
    assert!(err.contains("must fit"), "{err}");
  }

  #[test]
  fn distortion_toggle_at_most_one_per_monome() {
    let twin = DISTORTION_TOGGLE
      .replace("dist-a", "dist-a2")
      .replace("rect = [0, 1, 0, 1]", "rect = [1, 1, 1, 1]");
    let toml = format!("{SURFACES_MIN}{DISTORTION_TOGGLE}{twin}");
    let err = parse_rig(&toml).expect_err("two toggles on one monome should fail");
    assert!(err.contains("more than one distortion_toggle"), "{err}");
  }
}
