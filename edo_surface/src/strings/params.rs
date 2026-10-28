//! Every parameter of the strings engine, their defaults (the values
//! the rig ships), their validation, and the curves that turn the
//! per-register anchors into a value at any pitch.
//!
//! The field names are the rig keys: `string.t60_seconds_at_55hz`
//! in a rig is `StringsParams.string.t60_seconds_at_55hz` here. The
//! rig file carries the longer prose on each; the doc comments here
//! give units and purpose. The seven nested structs are the rig's
//! own schema: a `cpal_strings` sink deserializes its `string`,
//! `exciter`, `energy`, `bridge`, `body`, `howl` and `modal` tables
//! straight into them, a key or table the rig omits takes its value
//! from `StringsParams::default()`, and an unknown key is an error.
//!
//! The struct is `Copy` (no `Vec`) because the runtime's live,
//! hot-reloadable parameters are `Copy` and are read every audio
//! callback; anything derived and heap-backed (the body's mode table,
//! the delay lines) lives in the renderer.

use serde::Deserialize;

/// The pitch of the lower anchor of the decay-time and inharmonicity
/// curves.
pub const LOW_ANCHOR_HZ: f32 = 55.0;

/// The pitch of the upper anchor of the decay-time and inharmonicity
/// curves, five octaves above the lower.
pub const HIGH_ANCHOR_HZ: f32 = 1760.0;

/// The most dispersion allpass sections one loop may carry. A loop
/// holds this many section states in place, so changing
/// `string.dispersion_sections` while a note sounds allocates nothing.
pub const MAX_DISPERSION_SECTIONS: u32 = 16;

/// The most resonators one modal voice may have. A modal voice holds
/// this many mode states in place, so changing `modal.mode_count`
/// while a note sounds allocates nothing.
pub const MAX_MODAL_MODES: u32 = 32;

#[derive(Copy, Debug, Clone, PartialEq)]
pub struct StringsParams {
  pub string: StringParams,
  pub exciter: ExciterParams,
  pub energy: EnergyParams,
  pub bridge: BridgeParams,
  pub body: BodyParams,
  pub howl: HowlParams,
  pub modal: ModalParams, }

/// One loop's decay, inharmonicity and unison. The two-anchor curves
/// are log-log: the logarithm of the value is linear in `log2(f0)`
/// through the 55 Hz and 1760 Hz anchors, extrapolated beyond them
/// (a power law in `f0`, so it stays positive at every pitch).
#[derive(Copy, Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StringParams {
  /// Seconds for a 55 Hz note's fundamental to fall 60 dB. The
  /// low anchor of the decay-time curve `T60(f0)`.
  pub t60_seconds_at_55hz: f32,
  /// Seconds for a 1760 Hz note's fundamental to fall 60 dB. The
  /// high anchor of `T60(f0)`.
  pub t60_seconds_at_1760hz: f32,
  /// Hz. The reference frequency at which the loss filter meets
  /// `high_frequency_t60_ratio`. At or above Nyquist (or at or below
  /// the note's `f0`) the loss filter is flat.
  pub high_frequency_hz: f32,
  /// Unitless, in (0, 1]. A partial at `high_frequency_hz` decays
  /// with `T60(f0)` times this; smaller is a darker, duller tail.
  pub high_frequency_t60_ratio: f32,
  /// Loops per note, 1 to 3. More loops beat against each other and
  /// give the two-stage (prompt, then after-) decay.
  pub unison_count: u32,
  /// Cents. The k-th loop of `unison_count` is detuned by
  /// `(k - (unison_count - 1) / 2) * unison_detune_cents`.
  pub unison_detune_cents: f32,
  /// Unitless stiffness coefficient `B` of `f_n = n f0 sqrt(1 + B
  /// n^2)` at 55 Hz. The low anchor of the inharmonicity curve.
  pub inharmonicity_b_at_55hz: f32,
  /// Unitless `B` at 1760 Hz. The high anchor of the inharmonicity
  /// curve.
  pub inharmonicity_b_at_1760hz: f32,
  /// Count, 0 to 16. First-order allpass sections in each loop's
  /// dispersion chain; 0 removes the chain. More sections stretch
  /// more partials accurately.
  pub dispersion_sections: u32, }

/// What starts a note: the table read into the loops at key-down.
#[derive(Copy, Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExciterParams {
  pub kind: ExciterKind,
  /// Peak of the excitation table, in loop amplitude units (the
  /// output clamps at 0.95). In (0, 1].
  pub pick_energy: f32,
  /// Unitless, > 0. The excitation lowpass cuts off at this times
  /// `f0` (clamped below Nyquist); higher is a brighter attack.
  pub brightness: f32,
  /// Fraction of the string length from the bridge at which it is
  /// plucked or struck, in (0, 0.5]. Harmonics that are multiples of
  /// `1 / position` vanish (the position comb).
  pub position: f32,
  /// Count, 1 to 8. Raised-cosine force pulses of a strike (a hammer
  /// that bounces). Ignored by the pluck.
  pub strike_pulses: u32,
  /// Fraction of one period that each strike pulse lasts, > 0; the
  /// pulses and the gaps between them must fit in one period. Wider
  /// is a softer hammer. Ignored by the pluck.
  pub hammer_contact_fraction: f32,
  /// Unitless, in [0, 1]. How much of the body's knock (its impulse
  /// response, windowed) is convolved into the table.
  pub knock_mix: f32,
  /// Milliseconds, in (0, 1000]. The knock's length; its window falls
  /// 60 dB over this time.
  pub knock_ms: f32, }

#[derive(Copy, Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExciterKind {
  /// Filtered white noise one period long: a plectrum or finger.
  Pluck,
  /// A few raised-cosine pulses: a hammer.
  Strike, }

/// How the energy pedal puts energy into a string around key-down.
#[derive(Copy, Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnergyParams {
  pub kind: EnergyKind,
  /// Milliseconds, in [0, 10000]. The energy window opens this long
  /// before key-down; the pedal's history over it is the pre-charge.
  pub ex_ante_energizing_ms: f32,
  /// Milliseconds, in [0, 60000]. The window closes this long after
  /// key-down; over it the live pedal drives the string.
  pub ex_post_energizing_ms: f32,
  /// Unitless, > 0. The pedal position is raised to this power; above
  /// 1 the travel's bottom does little and its top much.
  pub pedal_energy_exponent: f32,
  /// Nepers per second, >= 0. The amplitude growth rate the drive
  /// aims for at full pedal and the window's peak.
  pub growth_rate_per_second_max: f32,
  /// Loop amplitude units, > 0. The drive saturates as `A tanh(v /
  /// A)` with `A` this times `pedal ^ pedal_energy_exponent`, which
  /// bounds what it can add: a light pedal blooms to a low ceiling,
  /// a full one to this.
  pub drive_limit: f32,
  /// Unitless, >= 0. Scales the pedal-history sum into the amplitude
  /// written into the loops at key-down.
  pub precharge_gain: f32,
  /// Loop velocity units, in (0, 2]. The bow's speed at full pedal
  /// and the window's peak (the `bow` kind only); the level a bowed
  /// string settles at follows it. `drive_limit` does not apply to
  /// the bow.
  pub bow_velocity: f32,
  /// Unitless slope of the bow's friction table, in [1, 10] (the
  /// `bow` kind only): larger narrows the speed difference at which
  /// the bow still grips, so it slips sooner. With `bow_velocity` it
  /// decides whether the bow speaks: their product from about 0.8 to
  /// 2 does.
  pub bow_force: f32,
  /// Hz, > 0. The Strouhal frequency at full pedal (the `wind` kind
  /// only); the partial nearest it blooms.
  pub wind_speed_hz_max: f32,
  /// Unitless, > 0. The quality factor of the wind's noise bandpass.
  pub wind_q: f32,
  /// Unitless, >= 0. The wind noise's gain into the loop, at the
  /// window's peak. `drive_limit` does not apply to the wind.
  pub wind_gain: f32, }

#[derive(Copy, Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnergyKind {
  /// A feedback driver: the loop's own wave, saturated, added back.
  Ebow,
  /// A friction bow (the STK bow table).
  Bow,
  /// Noise through a bandpass at the Strouhal frequency.
  Wind, }

/// The shared bridge every sounding loop meets.
#[derive(Copy, Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BridgeParams {
  /// Unitless, > 0. The bridge impedance over one loop's: near 0 is a
  /// light bridge (strong coupling), large is heavy (weak, piano-like
  /// coupling). Being real, it is a resistance: a lighter bridge also
  /// drains every string faster, the more so the higher the pitch, so
  /// smaller is stronger sympathy and shorter notes together.
  pub heaviness: f32,
  /// Hz, > 0. Cutoff of the one-pole lowpass on the bridge velocity;
  /// lower keeps the coupling to the low partials.
  pub damping_hz: f32, }

/// The radiating body: parallel resonators on the summed output, and
/// the source of the exciter's knock.
#[derive(Copy, Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BodyParams {
  /// Unitless, in [0, 1]. Output is `(1 - mix) dry + mix wet`; 0 is
  /// exactly the dry sum.
  pub mix: f32,
  /// Count, 1 to 128. Resonators in the bank.
  pub mode_count: u32,
  /// Hz, > 0. The lowest resonator's nominal center.
  pub lowest_hz: f32,
  /// Hz, above `lowest_hz`. The highest resonator's nominal center.
  pub highest_hz: f32,
  /// Unitless, > 0. Each resonator's bandwidth is its center over
  /// this; higher rings longer and narrower.
  pub q: f32,
  /// Any integer. Seeds the per-mode jitter of the centers; another
  /// seed is another body of the same size.
  pub seed: u32, }

/// The optional active loop that sustains everything it hears.
#[derive(Copy, Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HowlParams {
  /// Unitless, >= 0. Gain of the summed loops into the tanh; 0 turns
  /// the howl path off.
  pub gain: f32,
  /// Loop amplitude units, in [0, 1]. The tanh's output is scaled by
  /// this before it is added into every loop; a sustained note
  /// settles near `4 / pi * level / (1 - g)`, `g` its round-trip gain.
  pub level: f32,
  /// Milliseconds, in [0.5, 20]. The howl path's delay.
  pub delay_ms: f32, }

/// The register above which a note is a few resonators, not loops.
#[derive(Copy, Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModalParams {
  /// Hz, >= 0. A note above this is a modal voice; 0 disables the
  /// crossover.
  pub crossover_hz: f32,
  /// Count, 1 to 32. Resonators per modal voice, at the stiff
  /// string's partials; those at or above 0.45 times the sample rate
  /// are left out.
  pub mode_count: u32, }

impl Default for StringsParams {
  fn default() -> StringsParams {
    StringsParams {
      string: StringParams {
        t60_seconds_at_55hz: 10.0,
        t60_seconds_at_1760hz: 1.5,
        high_frequency_hz: 4000.0,
        high_frequency_t60_ratio: 0.3,
        unison_count: 2,
        unison_detune_cents: 1.2,
        inharmonicity_b_at_55hz: 1e-4,
        inharmonicity_b_at_1760hz: 5e-3,
        dispersion_sections: 4, },
      exciter: ExciterParams {
        kind: ExciterKind::Strike,
        pick_energy: 0.5,
        brightness: 6.0,
        position: 0.125,
        strike_pulses: 2,
        hammer_contact_fraction: 0.12,
        knock_mix: 0.3,
        knock_ms: 60.0, },
      energy: EnergyParams {
        kind: EnergyKind::Ebow,
        ex_ante_energizing_ms: 300.0,
        ex_post_energizing_ms: 600.0,
        pedal_energy_exponent: 2.0,
        growth_rate_per_second_max: 8.0,
        drive_limit: 0.8,
        precharge_gain: 0.5,
        bow_velocity: 0.3,
        bow_force: 3.0,
        wind_speed_hz_max: 2000.0,
        wind_q: 8.0,
        wind_gain: 0.5, },
      bridge: BridgeParams {
        heaviness: 3000.0,
        damping_hz: 6000.0, },
      body: BodyParams {
        mix: 0.5,
        mode_count: 24,
        lowest_hz: 90.0,
        highest_hz: 6000.0,
        q: 30.0,
        seed: 1, },
      howl: HowlParams {
        gain: 0.0,
        level: 0.003,
        delay_ms: 5.0, },
      modal: ModalParams {
        crossover_hz: 2000.0,
        mode_count: 8, }, }}}

// Each table's default is its part of `StringsParams::default()`, so a
// rig that omits a key or a whole table gets the one copy of each
// number above.

impl Default for StringParams {
  fn default() -> StringParams {
    StringsParams::default().string }}

impl Default for ExciterParams {
  fn default() -> ExciterParams {
    StringsParams::default().exciter }}

impl Default for EnergyParams {
  fn default() -> EnergyParams {
    StringsParams::default().energy }}

impl Default for BridgeParams {
  fn default() -> BridgeParams {
    StringsParams::default().bridge }}

impl Default for BodyParams {
  fn default() -> BodyParams {
    StringsParams::default().body }}

impl Default for HowlParams {
  fn default() -> HowlParams {
    StringsParams::default().howl }}

impl Default for ModalParams {
  fn default() -> ModalParams {
    StringsParams::default().modal }}

impl StringsParams {
  /// Checks every range the doc comments state. The error names the
  /// offending key as the rig spells it (`string.unison_count`).
  pub fn validate(
    &self,
  ) -> Result<(), String> {
    validate_string(&self.string)?;
    validate_exciter(&self.exciter)?;
    validate_energy(&self.energy)?;
    validate_bridge(&self.bridge)?;
    validate_body(&self.body)?;
    validate_howl(&self.howl)?;
    validate_modal(&self.modal)?;
    Ok(( )) }}

fn validate_string(
  string: &StringParams,
) -> Result<(), String> {
  require_above("string.t60_seconds_at_55hz", string.t60_seconds_at_55hz, 0.0)?;
  require_above("string.t60_seconds_at_1760hz", string.t60_seconds_at_1760hz, 0.0)?;
  require_above("string.high_frequency_hz", string.high_frequency_hz, 0.0)?;
  require_above_and_at_most(
    "string.high_frequency_t60_ratio", string.high_frequency_t60_ratio, 0.0, 1.0)?;
  require_count_within("string.unison_count", string.unison_count, 1, 3)?;
  require_within("string.unison_detune_cents", string.unison_detune_cents, 0.0, 100.0)?;
  require_above_and_at_most(
    "string.inharmonicity_b_at_55hz", string.inharmonicity_b_at_55hz, 0.0, 0.1)?;
  require_above_and_at_most(
    "string.inharmonicity_b_at_1760hz", string.inharmonicity_b_at_1760hz, 0.0, 0.1)?;
  require_count_within(
    "string.dispersion_sections", string.dispersion_sections, 0, MAX_DISPERSION_SECTIONS)?;
  Ok(( )) }

fn validate_exciter(
  exciter: &ExciterParams,
) -> Result<(), String> {
  require_above_and_at_most("exciter.pick_energy", exciter.pick_energy, 0.0, 1.0)?;
  require_above("exciter.brightness", exciter.brightness, 0.0)?;
  require_above_and_at_most("exciter.position", exciter.position, 0.0, 0.5)?;
  require_count_within("exciter.strike_pulses", exciter.strike_pulses, 1, 8)?;
  require_above("exciter.hammer_contact_fraction", exciter.hammer_contact_fraction, 0.0)?;
  { // The pulses and the gaps between them must fit in one period.
    let span_in_periods: f32 =
      (2 * exciter.strike_pulses - 1) as f32 * exciter.hammer_contact_fraction;
    if span_in_periods > 1.0 {
      return Err(format!(
        "exciter.hammer_contact_fraction = {}: {} pulses and their gaps span {} periods; \
         they must fit in one (exciter.strike_pulses times 2, minus 1, times the fraction, \
         at most 1)",
        exciter.hammer_contact_fraction, exciter.strike_pulses, span_in_periods )); }}
  require_within("exciter.knock_mix", exciter.knock_mix, 0.0, 1.0)?;
  require_above_and_at_most("exciter.knock_ms", exciter.knock_ms, 0.0, 1000.0)?;
  Ok(( )) }

fn validate_energy(
  energy: &EnergyParams,
) -> Result<(), String> {
  require_within("energy.ex_ante_energizing_ms", energy.ex_ante_energizing_ms, 0.0, 10_000.0)?;
  require_within("energy.ex_post_energizing_ms", energy.ex_post_energizing_ms, 0.0, 60_000.0)?;
  require_above("energy.pedal_energy_exponent", energy.pedal_energy_exponent, 0.0)?;
  require_at_least(
    "energy.growth_rate_per_second_max", energy.growth_rate_per_second_max, 0.0)?;
  require_above("energy.drive_limit", energy.drive_limit, 0.0)?;
  require_at_least("energy.precharge_gain", energy.precharge_gain, 0.0)?;
  require_above_and_at_most("energy.bow_velocity", energy.bow_velocity, 0.0, 2.0)?;
  require_within("energy.bow_force", energy.bow_force, 1.0, 10.0)?;
  require_above("energy.wind_speed_hz_max", energy.wind_speed_hz_max, 0.0)?;
  require_above("energy.wind_q", energy.wind_q, 0.0)?;
  require_at_least("energy.wind_gain", energy.wind_gain, 0.0)?;
  Ok(( )) }

fn validate_bridge(
  bridge: &BridgeParams,
) -> Result<(), String> {
  // At heaviness 0 and one loop the junction writes back the negated
  // wave, which halves the pitch.
  require_above("bridge.heaviness", bridge.heaviness, 0.0)?;
  require_above("bridge.damping_hz", bridge.damping_hz, 0.0)?;
  Ok(( )) }

fn validate_body(
  body: &BodyParams,
) -> Result<(), String> {
  require_within("body.mix", body.mix, 0.0, 1.0)?;
  require_count_within("body.mode_count", body.mode_count, 1, 128)?;
  require_above("body.lowest_hz", body.lowest_hz, 0.0)?;
  require_above("body.highest_hz", body.highest_hz, body.lowest_hz)?;
  require_above("body.q", body.q, 0.0)?;
  Ok(( )) }

fn validate_howl(
  howl: &HowlParams,
) -> Result<(), String> {
  require_at_least("howl.gain", howl.gain, 0.0)?;
  require_within("howl.level", howl.level, 0.0, 1.0)?;
  require_within("howl.delay_ms", howl.delay_ms, 0.5, 20.0)?;
  Ok(( )) }

fn validate_modal(
  modal: &ModalParams,
) -> Result<(), String> {
  require_at_least("modal.crossover_hz", modal.crossover_hz, 0.0)?;
  require_count_within("modal.mode_count", modal.mode_count, 1, MAX_MODAL_MODES)?;
  Ok(( )) }

fn require_above(
  key: &str,
  value: f32,
  exclusive_minimum: f32,
) -> Result<(), String> {
  if value.is_finite() && value > exclusive_minimum {
    Ok(( ))
  } else {
    Err(format!("{key} = {value}: must be a finite number above {exclusive_minimum}")) }}

fn require_at_least(
  key: &str,
  value: f32,
  minimum: f32,
) -> Result<(), String> {
  if value.is_finite() && value >= minimum {
    Ok(( ))
  } else {
    Err(format!("{key} = {value}: must be a finite number at least {minimum}")) }}

fn require_within(
  key: &str,
  value: f32,
  minimum: f32,
  maximum: f32,
) -> Result<(), String> {
  if value.is_finite() && value >= minimum && value <= maximum {
    Ok(( ))
  } else {
    Err(format!("{key} = {value}: must be in [{minimum}, {maximum}]")) }}

fn require_above_and_at_most(
  key: &str,
  value: f32,
  exclusive_minimum: f32,
  maximum: f32,
) -> Result<(), String> {
  if value.is_finite() && value > exclusive_minimum && value <= maximum {
    Ok(( ))
  } else {
    Err(format!("{key} = {value}: must be in ({exclusive_minimum}, {maximum}]")) }}

fn require_count_within(
  key: &str,
  value: u32,
  minimum: u32,
  maximum: u32,
) -> Result<(), String> {
  if value >= minimum && value <= maximum {
    Ok(( ))
  } else {
    Err(format!("{key} = {value}: must be an integer in {minimum}..={maximum}")) }}

/// Seconds for the fundamental of a note at `f0` (Hz, > 0) to fall
/// 60 dB: the log-log curve through the two `t60_seconds_at_*`
/// anchors.
pub fn t60_seconds(
  string: &StringParams,
  f0: f32,
) -> f32 {
  interpolate_log_log_through_anchors(
    string.t60_seconds_at_55hz, string.t60_seconds_at_1760hz, f0) }

/// The stiffness coefficient `B` at `f0` (Hz, > 0): the log-log curve
/// through the two `inharmonicity_b_at_*` anchors.
pub fn inharmonicity_b(
  string: &StringParams,
  f0: f32,
) -> f32 {
  interpolate_log_log_through_anchors(
    string.inharmonicity_b_at_55hz, string.inharmonicity_b_at_1760hz, f0) }

/// The loop gain per round trip at `f0` that makes the fundamental
/// fall 60 dB (a factor 10^-3) over `T60(f0)` seconds, which is
/// `f0 * T60(f0)` round trips: `10 ^ (-3 / (f0 T60(f0)))`.
pub fn round_trip_gain(
  string: &StringParams,
  f0: f32,
) -> f32 {
  10.0_f32.powf(-3.0 / (f0 * t60_seconds(string, f0))) }

/// Whether a loop at `f0` carries a dispersion chain: it has sections
/// and `B(f0)` is above the perceptual threshold, below which the
/// stretch of the partials is inaudible.
pub fn dispersion_applies(
  string: &StringParams,
  f0: f32,
) -> bool {
  string.dispersion_sections > 0
    && inharmonicity_b(string, f0) > inharmonicity_perceptual_threshold(f0) }

/// The smallest audible `B` at `f0`, `exp(2.54 ln f0 - 24.6)`
/// (Järveläinen et al., cited in Smith, "Physical Audio Signal
/// Processing", "Dispersion Filter Design I").
pub fn inharmonicity_perceptual_threshold(
  f0: f32,
) -> f32 {
  (2.54 * f0.ln() - 24.6).exp() }

/// The value at `f0` of the curve whose logarithm is linear in
/// `log2(f0)` and passes through `at_low_anchor` at `LOW_ANCHOR_HZ`
/// and `at_high_anchor` at `HIGH_ANCHOR_HZ`. Both anchors must be
/// positive.
fn interpolate_log_log_through_anchors(
  at_low_anchor: f32,
  at_high_anchor: f32,
  f0: f32,
) -> f32 {
  let fraction_of_anchor_span: f32 =
    (f0 / LOW_ANCHOR_HZ).log2() / (HIGH_ANCHOR_HZ / LOW_ANCHOR_HZ).log2();
  let log_value: f32 =
    at_low_anchor.ln() + (at_high_anchor.ln() - at_low_anchor.ln()) * fraction_of_anchor_span;
  log_value.exp() }
