//! The polyrhythm interface's pure state: tap tempo + the applied-tempo tempo
//! factors (see TODO/misc.org "polyrhythm interface" and "=1 factored-pulse
//! switch"). One shared instance; the TAPPED tempo is global, but each monome
//! carries its own tempo-factor exponents and its own factored-pulse on/off
//! switch, so the two grids can cycle at different (2^a x 3^b-related) rates or
//! not at all.
//!
//! *Tap tempo.* Taps pair up: an ARMING tap, then -- within `window` (rolling; a
//! stale arm simply re-arms) -- a SETTING tap whose distance from its partner is
//! the new tapped period. A third tap inside the window arms again ("does nothing
//! without a fourth"); its partner overrides the tempo taps 1+2 set. Tapping a
//! tempo only DEFINES it (displayed by the tap cell's blink); it does not start
//! any grid's amplitude cycling.
//!
//! *Tempo factor.* A grid's applied tempo = tapped tempo x 2^a x 3^b, adjusted by
//! its own x2 / /2 / x3 / /3 buttons. Exponents (not a float multiplier) keep
//! x3-then-/3 exactly unity.
//!
//! *The =1 button* (per grid). What it does depends on whether this grid's
//! amplitude cycling is already on:
//!
//! - Cycling OFF: the press only turns cycling ON. The tempo factor is left
//!   alone, so you can start a grid cycling at whatever rate you had dialled in.
//!   This press does NOT arm the double-tap detector -- otherwise the very tap
//!   that switched cycling on could pair with the next one and switch it straight
//!   back off.
//! - Cycling ON: a lone press zeroes this grid's exponents (back to the tapped
//!   tempo); a second press within `UNITY_DOUBLE_TAP` of that one turns cycling
//!   OFF instead.
//!
//! So from cold: tap once to start cycling (tempo factor kept), again to snap to
//! unity -- however fast, since the first press did not arm the detector -- and a
//! third, fast, to stop. Cycling starts off. The LED shows the switch: bright
//! while this grid's cycling is on.
//!
//! *Effect.* Each note struck while its grid's cycling is on (and a tempo
//! exists) gets a unipolar-triangle amplitude factored pulse in [0,1] at that
//! grid's applied tempo *at its onset* (the engine's
//! `factored_pulse_freq`/`factored_pulse_phase`). That onset rate is the note's
//! rate for life UNLESS it is put into per-voice edit mode, whose multipliers
//! retune sounding notes in place (`synth::scale_factored_pulse_rate`). Turning a
//! grid's cycling off never retroactively touches sounding notes -- only the
//! multipliers do (Jeff, `2_discussion` 2e), which also dodges the amplitude step
//! that snapping a live factored pulse to none would make.

use std::time::{Duration, Instant};

/// The tap-tempo blink's on fraction of each TAPPED-tempo cycle.
const BLINK_DUTY: f32 = 0.1;

/// Two =1 presses within this window count as the factored-pulse-off double-tap.
const UNITY_DOUBLE_TAP: Duration = Duration::from_millis(400);

/// One grid's slice of the polyrhythm state: its tempo-factor exponents, its
/// factored-pulse switch, and the =1 button's double-tap bookkeeping.
struct GridFactoredPulse {
  two_exp: i32,
  three_exp: i32,
  factored_pulse_on: bool,
  /// The last =1 press made *while cycling was already on* -- the only kind that
  /// can be the first half of the turn-off double-tap. `None` right after cycling
  /// is switched on or off, so the next press is always a lone tap.
  last_unity: Option<Instant>,
}

impl GridFactoredPulse {
  fn new() -> Self {
    GridFactoredPulse { two_exp: 0, three_exp: 0, factored_pulse_on: false, last_unity: None }
  }
}

pub struct PolyrhythmState {
  /// The tapped period, once a pair of taps has set one (GLOBAL: both grids).
  tapped_period: Option<Duration>,
  /// The moment the tempo was last set -- the blink's phase anchor, so the light
  /// pulses in time with the taps that defined it.
  anchor: Option<Instant>,
  /// The previous tap and whether it is ARMING (awaiting its partner).
  last_tap: Option<(Instant, bool)>,
  /// The per-monome tempo factor + factored-pulse switches.
  grids: Vec<GridFactoredPulse>,
}

/// What a tempo-factor button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TempoFactorButton {
  Times2,
  Div2,
  Times3,
  Div3,
  Unity,
}

impl PolyrhythmState {
  pub fn new(num_grids: usize) -> Self {
    PolyrhythmState {
      tapped_period: None,
      anchor: None,
      last_tap: None,
      grids: (0..num_grids).map(|_| GridFactoredPulse::new()).collect(),
    }
  }

  /// Seed the base tempo at bring-up (the surfaces runtime seeds 1 Hz for every
  /// rig). Without a seed the base would stay `None` until the first tap, and the
  /// tempo-factor controls would have nothing to multiply -- fatal in a rig with no
  /// tap source at all (2-monomes_2-softsteps: Jeff never taps), and a pointless
  /// wait in the rest. The phase anchor is `now`, so blinkers pulse at this rate
  /// from bring-up. Tapping, where a rig offers it, still overrides this later.
  pub fn set_fixed_tempo(&mut self, hz: f32, now: Instant) {
    if hz > 0.0 {
      self.tapped_period = Some(Duration::from_secs_f32(1.0 / hz));
      self.anchor = Some(now);
    }
  }

  /// A press of the tap button at `now`, under the rolling `window`. Defines the
  /// global tapped tempo only -- no grid's cycling turns on.
  pub fn tap(&mut self, now: Instant, window: Duration) {
    match self.last_tap {
      Some((prev, true)) if now.duration_since(prev) <= window => {
        // The arming tap's partner: set the tempo.
        self.tapped_period = Some(now.duration_since(prev));
        self.anchor = Some(now);
        self.last_tap = Some((now, false));
      }
      // First tap ever, a stale arm, or the tap right after a set: (re-)arm.
      _ => self.last_tap = Some((now, true)),
    }
  }

  /// A tempo-factor-button press on `grid`'s pad at `now`. The four direction
  /// buttons nudge that grid's exponents; `=1` drives the grid's factored-pulse
  /// switch and, once cycling is on, zeroes the exponents. See the module docs.
  pub fn press(&mut self, grid: usize, button: TempoFactorButton, now: Instant) {
    let Some(g) = self.grids.get_mut(grid) else {
      return;
    };
    match button {
      TempoFactorButton::Times2 => g.two_exp += 1,
      TempoFactorButton::Div2 => g.two_exp -= 1,
      TempoFactorButton::Times3 => g.three_exp += 1,
      TempoFactorButton::Div3 => g.three_exp -= 1,
      TempoFactorButton::Unity if !g.factored_pulse_on => {
        // Cycling was off: this press ONLY switches it on, keeping the tempo
        // factor you dialled in. `last_unity = None` so the next press -- however
        // soon -- is a lone tap (which snaps to unity) rather than the double-tap
        // that would switch cycling straight back off.
        g.factored_pulse_on = true;
        g.last_unity = None;
      }
      TempoFactorButton::Unity => {
        let doubled = g
          .last_unity
          .is_some_and(|prev| now.duration_since(prev) <= UNITY_DOUBLE_TAP);
        if doubled {
          // The fast second tap of an already-cycling grid: stop. The tempo
          // factor keeps whatever the first tap set (unity), and the next press
          // just starts again.
          g.factored_pulse_on = false;
          g.last_unity = None;
        } else {
          g.two_exp = 0;
          g.three_exp = 0;
          g.last_unity = Some(now);
        }
      }
    }
  }

  /// The global tapped tempo in Hz, before any grid's tempo factor -- None before
  /// a pair of taps has set one. This is what the tap cell DISPLAYS, on both
  /// grids alike: the blink is the metronome you tapped, not what either grid
  /// made of it.
  /// One grid's tempo-factor exponents, for the on-screen readout: Jeff asked for
  /// the tempo factor as `2^x * 3^y`, which is also exactly how it is stored --
  /// the reason x3-then-/3 is exactly unity rather than nearly.
  pub fn tempo_factor_exponents(&self, grid: usize) -> (i32, i32) {
    self.grids.get(grid).map(|g| (g.two_exp, g.three_exp)).unwrap_or((0, 0))
  }

  /// Whether this grid's amplitude cycling is switched on (the `=1` LED's state, now
  /// also a line in the window).
  pub fn factored_pulse_on(&self, grid: usize) -> bool {
    self.grids.get(grid).map(|g| g.factored_pulse_on).unwrap_or(false)
  }

  pub fn tapped_hz(&self) -> Option<f32> {
    let period = self.tapped_period?.as_secs_f32();
    if period <= 0.0 {
      return None; // two taps in the same instant: not a tempo
    }
    Some(1.0 / period)
  }

  /// `grid`'s applied tempo in Hz (the global tapped tempo times this grid's
  /// tempo factor), or None before any tempo has been tapped. This is the rate
  /// notes' factored pulse runs at; it is deliberately not what the tap cell
  /// displays.
  pub fn applied_hz(&self, grid: usize) -> Option<f32> {
    let hz = self.tapped_hz()?;
    let g = self.grids.get(grid)?;
    Some(hz * 2f32.powi(g.two_exp) * 3f32.powi(g.three_exp))
  }

  /// The factored-pulse rate a note struck on `grid` right now should carry: its
  /// applied tempo, but only while this grid's amplitude cycling is ON (the =1
  /// switch).
  pub fn factored_pulse_hz(&self, grid: usize) -> Option<f32> {
    if self.grids.get(grid).is_some_and(|g| g.factored_pulse_on) {
      self.applied_hz(grid)
    } else {
      None
    }
  }

  /// Is the tap-cell blink ON at `now`? Lit for the first `BLINK_DUTY` of each
  /// TAPPED-tempo cycle, phase-anchored to the tap that set the tempo -- so it
  /// shows the tempo you tapped, unfactored, as if every grid were at unity. It is
  /// therefore the same on both grids, and neither grid's tempo-factor buttons
  /// move it. It blinks whether or not cycling is on (the painter renders ON as
  /// fully lit, OFF as black), and is always off before a tempo exists (the
  /// button rests dim).
  /// The tap blink's phase at `now`, 0.0..1.0 through one cycle of the *tapped*
  /// (unfactored) tempo. `None` before any tempo exists.
  ///
  /// Exposed because the LED and the screen cannot share a duty cycle. `BLINK_DUTY`
  /// (10%) suits an LED, which is sampled far faster than it flashes. A window
  /// redrawing at tens of Hz cannot render a 10% flash at a brisk tempo -- at 120 bpm
  /// that is 50 ms, one frame, so it lands or misses on the frame boundary. The
  /// caller picks a duty it can actually draw; the phase is the shared part.
  pub fn tap_phase(&self, now: Instant) -> Option<f32> {
    let (hz, anchor) = (self.tapped_hz()?, self.anchor?);
    let period = 1.0 / hz;
    Some((now.duration_since(anchor).as_secs_f32() / period).fract())
  }

  pub fn tap_blink(&self, now: Instant) -> bool {
    let (Some(hz), Some(anchor)) = (self.tapped_hz(), self.anchor) else {
      return false;
    };
    let period = 1.0 / hz;
    let phase = (now.duration_since(anchor).as_secs_f32() / period).fract();
    phase < BLINK_DUTY
  }

  /// LED state for `grid`'s tempo-factor buttons: each direction lights while
  /// that grid's exponent leans its way; `=1` shows the grid's factored-pulse
  /// switch -- bright while its amplitude cycling is on.
  pub fn tempo_factor_lit(&self, grid: usize, button: TempoFactorButton) -> bool {
    let Some(g) = self.grids.get(grid) else {
      return false;
    };
    match button {
      TempoFactorButton::Times2 => g.two_exp > 0,
      TempoFactorButton::Div2 => g.two_exp < 0,
      TempoFactorButton::Times3 => g.three_exp > 0,
      TempoFactorButton::Div3 => g.three_exp < 0,
      TempoFactorButton::Unity => g.factored_pulse_on,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const WINDOW: Duration = Duration::from_secs(2);
  const MS: fn(u64) -> Duration = Duration::from_millis;

  fn two_grids() -> PolyrhythmState {
    PolyrhythmState::new(2)
  }

  #[test]
  fn a_fixed_tempo_gives_the_factor_pedals_a_base_with_no_tapping() {
    // The runtime seeds a fixed 1 Hz base at bring-up, so the tempo-factor controls
    // have something to multiply and the blinker still pulses.
    let t0 = Instant::now();
    let mut p = two_grids();
    p.set_fixed_tempo(1.0, t0);
    assert!((p.tapped_hz().unwrap() - 1.0).abs() < 1e-4, "base is a fixed 1 Hz");
    // x2 on grid 0 -> its applied tempo is 2 Hz; grid 1 is independent at 1 Hz.
    p.press(0, TempoFactorButton::Times2, t0);
    assert!((p.applied_hz(0).unwrap() - 2.0).abs() < 1e-4, "x2 over the fixed base = 2 Hz");
    assert!((p.applied_hz(1).unwrap() - 1.0).abs() < 1e-4, "the other grid stays at the base");
    assert!(p.tap_blink(t0 + MS(10)), "the blinker runs off the fixed base");
  }

  #[test]
  fn two_taps_within_the_window_set_the_tempo_for_both_grids() {
    let t0 = Instant::now();
    let mut p = two_grids();
    assert_eq!(p.applied_hz(0), None, "no tempo before any taps");
    p.tap(t0, WINDOW);
    assert_eq!(p.applied_hz(0), None, "one tap is only an arm");
    p.tap(t0 + MS(500), WINDOW);
    for grid in 0..2 {
      let hz = p.applied_hz(grid).expect("a pair sets the global tempo");
      assert!((hz - 2.0).abs() < 1e-4, "500 ms apart = 2 Hz on grid {grid}: {hz}");
    }
  }

  #[test]
  fn the_window_rolls_a_stale_arm_re_arms() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    // Too late to pair with tap 1 -- but it must itself arm, so a THIRD tap soon
    // after pairs with IT (no dead zone at window boundaries).
    p.tap(t0 + MS(3000), WINDOW);
    assert_eq!(p.applied_hz(0), None, "stale pair sets nothing");
    p.tap(t0 + MS(3250), WINDOW);
    let hz = p.applied_hz(0).expect("the re-armed tap pairs with the next");
    assert!((hz - 4.0).abs() < 1e-4, "250 ms apart = 4 Hz: {hz}");
  }

  #[test]
  fn a_third_tap_does_nothing_until_its_fourth_overrides() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(500), WINDOW); // 2 Hz
    p.tap(t0 + MS(1000), WINDOW); // third tap: arms only
    let hz = p.applied_hz(0).unwrap();
    assert!((hz - 2.0).abs() < 1e-4, "third tap left the tempo alone: {hz}");
    p.tap(t0 + MS(1100), WINDOW); // fourth: 3+4 override 1+2
    let hz = p.applied_hz(0).unwrap();
    assert!((hz - 10.0).abs() < 1e-4, "taps 3+4 (100 ms) override: {hz}");
  }

  #[test]
  fn tempo_factors_are_per_grid_over_one_tapped_tempo() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(1000), WINDOW); // 1 Hz, globally
    p.press(0, TempoFactorButton::Times3, t0);
    p.press(0, TempoFactorButton::Times2, t0);
    p.press(1, TempoFactorButton::Div2, t0);
    assert!((p.applied_hz(0).unwrap() - 6.0).abs() < 1e-5, "grid 0: x3 then x2 = 6x");
    assert!((p.applied_hz(1).unwrap() - 0.5).abs() < 1e-6, "grid 1: /2 halves, untouched by grid 0");
    assert!(p.tempo_factor_lit(0, TempoFactorButton::Times2), "grid 0's lean shows on grid 0");
    assert!(!p.tempo_factor_lit(1, TempoFactorButton::Times2), "grid 1 shows its own lean only");
    p.press(0, TempoFactorButton::Div3, t0);
    p.press(0, TempoFactorButton::Div2, t0);
    assert!((p.applied_hz(0).unwrap() - 1.0).abs() < 1e-6, "exactly back to unity");
  }

  #[test]
  fn tapping_a_tempo_does_not_start_the_factored_pulse() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(500), WINDOW);
    assert!(p.applied_hz(0).is_some(), "the tempo is defined...");
    assert_eq!(p.factored_pulse_hz(0), None, "...but cycling starts OFF: notes are un-pulsed");
    assert_eq!(p.factored_pulse_hz(1), None);
    assert!(p.tap_blink(t0 + MS(520)), "the blink still displays the tempo");
  }

  /// The =1 button's three-press story from cold: on (tempo factor kept) -> unity -> off.
  #[test]
  fn unity_starts_cycling_then_snaps_to_unity_then_stops() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(500), WINDOW); // 2 Hz tapped
    p.press(0, TempoFactorButton::Times3, t0 + MS(600)); // grid 0 applies 6 Hz
    assert_eq!(p.factored_pulse_hz(0), None, "cycling starts off");

    // Press 1: switch cycling on, WITHOUT snapping the tempo factor to unity.
    p.press(0, TempoFactorButton::Unity, t0 + MS(1000));
    let hz = p.factored_pulse_hz(0).expect("the first press turns cycling on");
    assert!((hz - 6.0).abs() < 1e-4, "the x3 tempo factor survives the switch-on: {hz}");
    assert!(p.tempo_factor_lit(0, TempoFactorButton::Unity), "=1 lights while cycling is on");
    assert!(p.tempo_factor_lit(0, TempoFactorButton::Times3), "and x3 still leans");
    assert_eq!(p.factored_pulse_hz(1), None, "grid 1's switch is its own");

    // Press 2, only 100 ms later: snaps to unity and KEEPS cycling. The switch-on
    // press must not have armed the double-tap detector.
    p.press(0, TempoFactorButton::Unity, t0 + MS(1100));
    let hz = p.factored_pulse_hz(0).expect("still cycling after the second press");
    assert!((hz - 2.0).abs() < 1e-4, "the second press snaps to unity: {hz}");
    assert!(!p.tempo_factor_lit(0, TempoFactorButton::Times3), "exponents zeroed");

    // Press 3, 100 ms after THAT: now both presses were made while cycling, so the
    // 400 ms double-tap applies and cycling stops.
    p.press(0, TempoFactorButton::Unity, t0 + MS(1200));
    assert_eq!(p.factored_pulse_hz(0), None, "a fast press after a unity press stops cycling");
    assert!(!p.tempo_factor_lit(0, TempoFactorButton::Unity));
    assert!(p.applied_hz(0).is_some(), "the tempo itself survives (still displayed)");
  }

  /// The 400 ms test is applied only between two presses made while cycling was ON.
  /// A press that switches cycling on -- or one that follows a switch-off -- can
  /// never be half of the turn-off double-tap, however fast it lands.
  #[test]
  fn the_double_tap_window_applies_only_while_cycling_is_on() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(500), WINDOW);

    // From cold, two presses 10 ms apart: on, then unity. NOT off.
    p.press(0, TempoFactorButton::Unity, t0 + MS(1000));
    p.press(0, TempoFactorButton::Unity, t0 + MS(1010));
    assert!(p.factored_pulse_hz(0).is_some(), "a fast pair from cold leaves cycling ON");

    // A third, also 10 ms on: now its partner was an on-grid press, so it stops.
    p.press(0, TempoFactorButton::Unity, t0 + MS(1020));
    assert_eq!(p.factored_pulse_hz(0), None, "two on-grid presses inside 400 ms stop cycling");

    // A press right after the stop restarts it -- it cannot pair with the stop.
    p.press(0, TempoFactorButton::Unity, t0 + MS(1030));
    assert!(p.factored_pulse_hz(0).is_some(), "a press while off always turns cycling on");

    // Slow presses while cycling never stop it; each is a lone tap.
    p.press(0, TempoFactorButton::Unity, t0 + MS(3000));
    p.press(0, TempoFactorButton::Unity, t0 + MS(5000));
    assert!(p.factored_pulse_hz(0).is_some(), "SLOW presses are lone taps: still cycling");
  }

  #[test]
  fn a_unity_press_while_cycling_zeroes_the_exponents() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(1000), WINDOW); // 1 Hz
    p.press(0, TempoFactorButton::Unity, t0 + MS(1100)); // cycling on, tempo factor untouched (unity)
    p.press(0, TempoFactorButton::Times3, t0 + MS(1200));
    p.press(0, TempoFactorButton::Times2, t0 + MS(1300));
    assert!((p.applied_hz(0).unwrap() - 6.0).abs() < 1e-5, "x3 then x2");
    // A lone (slow) press while cycling: back to the tapped tempo, still cycling.
    p.press(0, TempoFactorButton::Unity, t0 + MS(3000));
    assert!((p.applied_hz(0).unwrap() - 1.0).abs() < 1e-6, "=1 zeroes the exponents");
    assert!(p.factored_pulse_hz(0).is_some(), "and leaves cycling on");
  }

  #[test]
  fn the_tap_blink_shows_the_unfactored_tempo_and_ignores_every_tempo_factor() {
    let t0 = Instant::now();
    let mut p = two_grids();
    assert!(!p.tap_blink(t0), "no tempo, no blink");
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(1000), WINDOW); // 1 Hz, anchored at t0+1s
    let anchor = t0 + MS(1000);
    assert!(p.tap_blink(anchor + MS(50)), "on during the first 10% of the cycle");
    assert!(!p.tap_blink(anchor + MS(500)), "off mid-cycle");
    assert!(p.tap_blink(anchor + MS(2050)), "on again two cycles later");

    // A grid's tempo factor moves what its NOTES do, and never the display: the
    // blink is the metronome you tapped, as if every grid were at unity.
    p.press(1, TempoFactorButton::Times2, anchor);
    assert!((p.applied_hz(1).unwrap() - 2.0).abs() < 1e-5, "grid 1's factored pulse now runs at 2 Hz");
    assert!(
      !p.tap_blink(anchor + MS(510)),
      "the blink still runs at the tapped 1 Hz (a 2 Hz blink would be lit here)",
    );
    assert!(p.tap_blink(anchor + MS(1050)), "on at the next TAPPED-tempo cycle");
    assert!((p.tapped_hz().unwrap() - 1.0).abs() < 1e-6, "the display's rate is the tapped one");
  }
}
