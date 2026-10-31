//! Pedal targets: an expression pedal's (parameter, min, max)
//! triples, each moving one float of the strings engine linearly
//! with the pedal's raw position, 'min' at the heel and 'max' at the
//! toe. The rig writes them as 'ELEM targets' under an
//! 'ELEM expression_pedals' ('rig::PedalTargetRig'); this module
//! resolves each against the sink's parameters into a
//! 'StringsPedalTarget' and computes the effective parameters a block
//! renders with ('apply_pedal_targets'). 'resolve_strings_pedals' is
//! the one resolution of a rig's pedals for the engine, shared by the
//! runtime's settings and the harness.
//!
//! A parameter is targetable when it is a float whose change the
//! engine applies without rebuilding a table. 'TARGETABLE_PARAMETERS'
//! is the one list of them; the rig README quotes it.

use crate::expression_pedals::NUM_PEDALS;
use crate::rig::{PedalRole, PedalTargetRig, Rig};
use crate::strings::StringsParams;

/// The most targets one pedal may carry. Validation refuses more, so
/// the live parameters can hold every pedal's targets in a fixed
/// array and stay 'Copy'.
pub const MAX_PEDAL_TARGETS: usize = 16;

/// Writes a value into the one field of 'StringsParams' a target
/// parameter names.
pub type PedalTargetSetter = fn(&mut StringsParams, f32);

/// Every pedal's resolved targets, indexed by pedal (the MPC-20
/// channel minus 1), filled from the front.
pub type PedalTargetsPerChannel = [[Option<StringsPedalTarget>; MAX_PEDAL_TARGETS]; NUM_PEDALS];

/// No pedal carries a target.
pub const NO_PEDAL_TARGETS: PedalTargetsPerChannel = [[None; MAX_PEDAL_TARGETS]; NUM_PEDALS];

/// Every targetable parameter, spelled as the rig and the sink's
/// validation messages spell it, with the setter that writes it.
pub const TARGETABLE_PARAMETERS: &[(&str, PedalTargetSetter)] = &[
  ("string.t60_seconds_at_55hz",
   |params: &mut StringsParams, value: f32| params.string.t60_seconds_at_55hz = value),
  ("string.t60_seconds_at_1760hz",
   |params: &mut StringsParams, value: f32| params.string.t60_seconds_at_1760hz = value),
  ("string.high_frequency_hz",
   |params: &mut StringsParams, value: f32| params.string.high_frequency_hz = value),
  ("string.high_frequency_t60_ratio",
   |params: &mut StringsParams, value: f32| params.string.high_frequency_t60_ratio = value),
  ("string.unison_detune_cents",
   |params: &mut StringsParams, value: f32| params.string.unison_detune_cents = value),
  ("string.inharmonicity_b_at_55hz",
   |params: &mut StringsParams, value: f32| params.string.inharmonicity_b_at_55hz = value),
  ("string.inharmonicity_b_at_1760hz",
   |params: &mut StringsParams, value: f32| params.string.inharmonicity_b_at_1760hz = value),
  ("exciter.pick_energy",
   |params: &mut StringsParams, value: f32| params.exciter.pick_energy = value),
  ("exciter.position",
   |params: &mut StringsParams, value: f32| params.exciter.position = value),
  ("exciter.hammer_contact_fraction",
   |params: &mut StringsParams, value: f32| params.exciter.hammer_contact_fraction = value),
  ("exciter.knock_mix",
   |params: &mut StringsParams, value: f32| params.exciter.knock_mix = value),
  ("energy.ex_ante_energizing_ms",
   |params: &mut StringsParams, value: f32| params.energy.ex_ante_energizing_ms = value),
  ("energy.ex_post_energizing_ms",
   |params: &mut StringsParams, value: f32| params.energy.ex_post_energizing_ms = value),
  ("energy.pedal_energy_exponent",
   |params: &mut StringsParams, value: f32| params.energy.pedal_energy_exponent = value),
  ("energy.growth_rate_per_second_max",
   |params: &mut StringsParams, value: f32| params.energy.growth_rate_per_second_max = value),
  ("energy.drive_limit",
   |params: &mut StringsParams, value: f32| params.energy.drive_limit = value),
  ("energy.precharge_gain",
   |params: &mut StringsParams, value: f32| params.energy.precharge_gain = value),
  ("energy.bow_velocity",
   |params: &mut StringsParams, value: f32| params.energy.bow_velocity = value),
  ("energy.bow_slipperiness",
   |params: &mut StringsParams, value: f32| params.energy.bow_slipperiness = value),
  ("energy.wind_speed_hz_max",
   |params: &mut StringsParams, value: f32| params.energy.wind_speed_hz_max = value),
  ("energy.wind_q",
   |params: &mut StringsParams, value: f32| params.energy.wind_q = value),
  ("energy.wind_gain",
   |params: &mut StringsParams, value: f32| params.energy.wind_gain = value),
  ("bridge.heaviness",
   |params: &mut StringsParams, value: f32| params.bridge.heaviness = value),
  ("bridge.damping_hz",
   |params: &mut StringsParams, value: f32| params.bridge.damping_hz = value),
  ("body.mix",
   |params: &mut StringsParams, value: f32| params.body.mix = value),
  ("howl.gain",
   |params: &mut StringsParams, value: f32| params.howl.gain = value),
  ("howl.level",
   |params: &mut StringsParams, value: f32| params.howl.level = value),
  ("howl.delay_ms",
   |params: &mut StringsParams, value: f32| params.howl.delay_ms = value),
  ("modal.crossover_hz",
   |params: &mut StringsParams, value: f32| params.modal.crossover_hz = value), ];

/// The float parameters that are not targetable: a change to any of
/// them rebuilds a table (the knock, or the body's bank, whose
/// rebuild also measures its normalization on a second of noise),
/// far too costly at pedal rate. Every integer and enum key is not
/// targetable either.
pub const TABLE_REBUILDING_PARAMETERS: &[&str] = &[
  "exciter.brightness",
  "exciter.knock_ms",
  "body.lowest_hz",
  "body.highest_hz",
  "body.q", ];

/// One pedal target, resolved: the parameter's key (for messages and
/// tests), the setter that writes it, and its values at the heel
/// ('min') and at the toe ('max'). 'min' above 'max' inverts the
/// pedal; equal pins the parameter.
#[derive(Clone, Copy, Debug)]
pub struct StringsPedalTarget {
  pub parameter: &'static str,
  pub setter: PedalTargetSetter,
  pub min: f32,
  pub max: f32, }

// By key and range: the key determines the setter, and comparing
// function pointers is unreliable.
impl PartialEq for StringsPedalTarget {
  fn eq(
    &self,
    other: &StringsPedalTarget,
  ) -> bool {
    self.parameter == other.parameter && self.min == other.min && self.max == other.max }}

impl StringsPedalTarget {
  /// The parameter's value at a pedal position in [0, 1]: linear in
  /// the raw position, 'min' at the heel, 'max' at the toe.
  pub fn value_at(
    &self,
    position: f32,
  ) -> f32 {
    self.min + (self.max - self.min) * position }}

/// Resolves one rig target against the sink's parameters: the
/// parameter must be targetable, and the whole 'StringsParams' with
/// it set to 'min' must pass 'StringsParams::validate', and so with
/// 'max'. Every validated range is an interval, and the one coupled
/// constraint (the strike's span in periods) is monotone in its
/// float, so every position between the two ends is legal too.
pub fn resolve_pedal_target(
  target: &PedalTargetRig,
  sink_params: &StringsParams,
) -> Result<StringsPedalTarget, String> {
  let (parameter, setter): (&'static str, PedalTargetSetter) =
    targetable_parameter(&target.parameter)
    .ok_or_else(|| not_targetable_message(&target.parameter))?;
  for (end, value) in [("min", target.min), ("max", target.max)] {
    let mut params_at_end: StringsParams = *sink_params;
    setter(&mut params_at_end, value);
    params_at_end.validate().map_err(|error| format!(
      "pedal target {parameter}: its {end} {value} is not a legal value ({error})"))?; }
  Ok(StringsPedalTarget { parameter, setter, min: target.min, max: target.max }) }

/// A rig's pedals as the strings engine reads them: every pedal's
/// resolved targets, and the energy pedal's index (its MPC-20 channel
/// minus 1), 'None' without one. The default is a rig with neither.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResolvedStringsPedals {
  pub pedal_targets: PedalTargetsPerChannel,
  pub string_energy_pedal: Option<usize>, }

/// Resolves a rig's pedals against the play sink's parameters: each
/// pedal's targets, in rig order, into its channel's row, and the
/// channel of its 'string_energy' pedal. 'resolve_settings' calls this
/// for the live parameters and the harness for its render, so the two
/// cannot disagree. The rig's validation ('validate_pedal_targets'
/// in 'rig.rs') refuses everything that fails here first; the error
/// is for a rig built without it.
pub fn resolve_strings_pedals(
  rig: &Rig,
  sink_params: &StringsParams,
) -> Result<ResolvedStringsPedals, String> {
  let mut resolved: ResolvedStringsPedals = ResolvedStringsPedals::default();
  for pedal in &rig.expression_pedals {
    let channel: u8 = pedal.channel;
    let pedal_index: usize = usize::from(channel).checked_sub(1)
      .filter(|index| *index < NUM_PEDALS)
      .ok_or(format!("expression pedal channel {channel} is not 1..={NUM_PEDALS}"))?;
    if pedal.role == PedalRole::StringEnergy {
      resolved.string_energy_pedal = Some(pedal_index); }
    if pedal.targets.len() > MAX_PEDAL_TARGETS {
      return Err(format!(
        "expression pedal (channel {channel}) carries {} targets; a pedal carries at most \
         {MAX_PEDAL_TARGETS}",
        pedal.targets.len())); }
    for (slot, target) in resolved.pedal_targets[pedal_index].iter_mut().zip(&pedal.targets) {
      *slot = Some(resolve_pedal_target(target, sink_params)
        .map_err(|error| format!("expression pedal (channel {channel}): {error}"))?); }}
  Ok(resolved) }

/// What the strings engine renders one block with: the effective
/// parameters ('apply_pedal_targets'), and the energy pedal's position,
/// read from its slot of 'positions' (NaN without an energy pedal, or
/// before it reports). The audio callback calls this every block with
/// the positions the pedal thread last published.
pub fn effective_params_and_energy_position(
  live_params: &StringsParams,
  pedal_targets: &PedalTargetsPerChannel,
  string_energy_pedal: Option<usize>,
  positions: &[f32; NUM_PEDALS],
) -> (StringsParams, f32) {
  let effective_params: StringsParams =
    apply_pedal_targets(live_params, pedal_targets, positions);
  let energy_position: f32 =
    string_energy_pedal
    .and_then(|pedal_index| positions.get(pedal_index).copied())
    .unwrap_or(f32::NAN);
  (effective_params, energy_position) }

/// The effective parameters: 'live_params' with every target of
/// every pedal that has reported applied at that pedal's position.
/// 'positions' is indexed like 'targets'; a NaN position is a pedal
/// that has never reported, whose targets leave the live values.
pub fn apply_pedal_targets(
  live_params: &StringsParams,
  targets: &PedalTargetsPerChannel,
  positions: &[f32; NUM_PEDALS],
) -> StringsParams {
  let mut effective_params: StringsParams = *live_params;
  for (pedal_targets, position) in targets.iter().zip(positions) {
    if position.is_nan() {
      continue; }
    for target in pedal_targets.iter().flatten() {
      (target.setter)(&mut effective_params, target.value_at(*position)); }}
  effective_params }

/// The table entry for a key, if the key is targetable.
fn targetable_parameter(
  key: &str,
) -> Option<(&'static str, PedalTargetSetter)> {
  TARGETABLE_PARAMETERS.iter()
    .find(|(targetable_key, _)| *targetable_key == key)
    .copied() }

/// Names the refused key, says why when it is one of the
/// table-rebuilding floats, and lists every targetable parameter.
fn not_targetable_message(
  key: &str,
) -> String {
  let reason: &str =
    if TABLE_REBUILDING_PARAMETERS.contains(&key) {
      ": a change to it rebuilds a table, too costly at pedal rate"
    } else { "" };
  let targetable_keys: Vec<&str> =
    TARGETABLE_PARAMETERS.iter().map(|(targetable_key, _)| *targetable_key).collect();
  format!(
    "pedal target {key:?} is not a targetable parameter{reason}; the targetable \
     parameters are {}",
    targetable_keys.join(", ")) }

#[cfg(test)]
mod tests {
  use super::*;
  use crate::strings::params::{
    BodyParams, BridgeParams, EnergyParams, ExciterParams, HowlParams, ModalParams, StringParams,
  };
  use std::cell::RefCell;
  use std::collections::HashSet;

  /// No pedal's targets, every pedal unreported.
  fn no_targets(
  ) -> PedalTargetsPerChannel {
    NO_PEDAL_TARGETS }

  fn resolved(
    parameter: &str,
    min: f32,
    max: f32,
  ) -> StringsPedalTarget {
    resolve_pedal_target(
      &PedalTargetRig { parameter: parameter.to_string(), min, max },
      &StringsParams::default())
      .expect("the target resolves") }

  /// Every float field of 'StringsParams' is placed: targetable, or
  /// refused as table-rebuilding. Each targetable key's setter is
  /// applied with a value of its own, and the result must equal a
  /// struct written out by hand, field by field with no '..': a
  /// field added later fails to compile here until it is placed.
  #[test]
  fn every_float_field_is_targetable_or_refused() {
    let defaults: StringsParams = StringsParams::default();
    let keys_written_by_hand: RefCell<HashSet<&str>> = RefCell::new(HashSet::new());
    let distinct_value_for = |key: &'static str| -> f32 {
      keys_written_by_hand.borrow_mut().insert(key);
      let index: usize = TARGETABLE_PARAMETERS.iter()
        .position(|(targetable_key, _)| *targetable_key == key)
        .unwrap_or_else(|| panic!("{key} is not in the targetable list"));
      1000.0 + index as f32 };
    let refused = |key: &str, default_value: f32| -> f32 {
      assert!(TABLE_REBUILDING_PARAMETERS.contains(&key), "{key} is not in the refused list");
      default_value };
    let by_hand: StringsParams = StringsParams {
      string: StringParams {
        t60_seconds_at_55hz: distinct_value_for("string.t60_seconds_at_55hz"),
        t60_seconds_at_1760hz: distinct_value_for("string.t60_seconds_at_1760hz"),
        high_frequency_hz: distinct_value_for("string.high_frequency_hz"),
        high_frequency_t60_ratio: distinct_value_for("string.high_frequency_t60_ratio"),
        unison_count: defaults.string.unison_count,
        unison_detune_cents: distinct_value_for("string.unison_detune_cents"),
        inharmonicity_b_at_55hz: distinct_value_for("string.inharmonicity_b_at_55hz"),
        inharmonicity_b_at_1760hz: distinct_value_for("string.inharmonicity_b_at_1760hz"),
        dispersion_sections: defaults.string.dispersion_sections, },
      exciter: ExciterParams {
        kind: defaults.exciter.kind,
        pick_energy: distinct_value_for("exciter.pick_energy"),
        brightness: refused("exciter.brightness", defaults.exciter.brightness),
        position: distinct_value_for("exciter.position"),
        strike_pulses: defaults.exciter.strike_pulses,
        hammer_contact_fraction: distinct_value_for("exciter.hammer_contact_fraction"),
        knock_mix: distinct_value_for("exciter.knock_mix"),
        knock_ms: refused("exciter.knock_ms", defaults.exciter.knock_ms), },
      energy: EnergyParams {
        kind: defaults.energy.kind,
        ex_ante_energizing_ms: distinct_value_for("energy.ex_ante_energizing_ms"),
        ex_post_energizing_ms: distinct_value_for("energy.ex_post_energizing_ms"),
        pedal_energy_exponent: distinct_value_for("energy.pedal_energy_exponent"),
        growth_rate_per_second_max: distinct_value_for("energy.growth_rate_per_second_max"),
        drive_limit: distinct_value_for("energy.drive_limit"),
        precharge_gain: distinct_value_for("energy.precharge_gain"),
        bow_velocity: distinct_value_for("energy.bow_velocity"),
        bow_slipperiness: distinct_value_for("energy.bow_slipperiness"),
        wind_speed_hz_max: distinct_value_for("energy.wind_speed_hz_max"),
        wind_q: distinct_value_for("energy.wind_q"),
        wind_gain: distinct_value_for("energy.wind_gain"), },
      bridge: BridgeParams {
        heaviness: distinct_value_for("bridge.heaviness"),
        damping_hz: distinct_value_for("bridge.damping_hz"), },
      body: BodyParams {
        mix: distinct_value_for("body.mix"),
        mode_count: defaults.body.mode_count,
        lowest_hz: refused("body.lowest_hz", defaults.body.lowest_hz),
        highest_hz: refused("body.highest_hz", defaults.body.highest_hz),
        q: refused("body.q", defaults.body.q),
        seed: defaults.body.seed, },
      howl: HowlParams {
        gain: distinct_value_for("howl.gain"),
        level: distinct_value_for("howl.level"),
        delay_ms: distinct_value_for("howl.delay_ms"), },
      modal: ModalParams {
        crossover_hz: distinct_value_for("modal.crossover_hz"),
        mode_count: defaults.modal.mode_count, }, };
    let mut through_the_list: StringsParams = defaults;
    for (index, (_, setter)) in TARGETABLE_PARAMETERS.iter().enumerate() {
      setter(&mut through_the_list, 1000.0 + index as f32); }
    assert_eq!(through_the_list, by_hand, "each setter writes the field its key names");
    let listed_keys: HashSet<&str> =
      TARGETABLE_PARAMETERS.iter().map(|(key, _)| *key).collect();
    assert_eq!(listed_keys.len(), TARGETABLE_PARAMETERS.len(), "no key is listed twice");
    assert_eq!(*keys_written_by_hand.borrow(), listed_keys, "every listed key is a field"); }

  /// A pedal at a quarter of its travel puts a 0..20 target at 5.
  #[test]
  fn a_pedal_at_a_quarter_puts_a_zero_to_twenty_target_at_five() {
    let mut targets: PedalTargetsPerChannel = no_targets();
    targets[1][0] = Some(resolved("howl.gain", 0.0, 20.0));
    let effective: StringsParams =
      apply_pedal_targets(&StringsParams::default(), &targets, &[f32::NAN, 0.25]);
    assert_eq!(effective.howl.gain, 5.0);
    let mut expected: StringsParams = StringsParams::default();
    expected.howl.gain = 5.0;
    assert_eq!(effective, expected, "nothing but the target moved"); }

  /// A pedal that has never reported (NaN) leaves its targets'
  /// parameters at the live values.
  #[test]
  fn an_unreported_pedal_leaves_the_live_value() {
    let mut live: StringsParams = StringsParams::default();
    live.howl.gain = 2.5;
    let mut targets: PedalTargetsPerChannel = no_targets();
    targets[0][0] = Some(resolved("howl.gain", 0.0, 20.0));
    let effective: StringsParams =
      apply_pedal_targets(&live, &targets, &[f32::NAN, f32::NAN]);
    assert_eq!(effective, live); }

  /// 'min' above 'max' inverts the pedal: the heel gives 'min', the
  /// toe 'max', and a quarter of the travel a quarter of the way down.
  #[test]
  fn a_min_above_max_inverts_the_pedal() {
    let mut targets: PedalTargetsPerChannel = no_targets();
    targets[0][0] = Some(resolved("bridge.heaviness", 3000.0, 300.0));
    let at = |position: f32| -> f32 {
      apply_pedal_targets(&StringsParams::default(), &targets, &[position, f32::NAN])
        .bridge.heaviness };
    assert_eq!(at(0.0), 3000.0);
    assert_eq!(at(1.0), 300.0);
    assert_eq!(at(0.25), 2325.0); }

  /// The error for a key off the list names it, gives the reason for
  /// a table-rebuilding float, and lists every targetable key.
  #[test]
  fn a_key_off_the_list_is_refused_listing_the_targetable_ones() {
    let error: String = resolve_pedal_target(
      &PedalTargetRig { parameter: "body.q".to_string(), min: 10.0, max: 20.0 },
      &StringsParams::default())
      .expect_err("body.q is not targetable");
    assert!(error.contains("body.q") && error.contains("rebuilds a table"), "{error}");
    for (key, _) in TARGETABLE_PARAMETERS {
      assert!(error.contains(key), "{error}"); }}
}
