//! The polyrhythm interface's pure state: tap tempo + the applied-tempo factors
//! (see TODO/misc.org "polyrhythm interface" and "=1 pulse switch"). One shared
//! instance; the TAPPED tempo is global, but each monome carries its own factor
//! exponents and its own pulse on/off switch, so the two grids can cycle at
//! different (2^a x 3^b-related) rates or not at all.
//!
//! *Tap tempo.* Taps pair up: an ARMING tap, then -- within `window` (rolling; a
//! stale arm simply re-arms) -- a SETTING tap whose distance from its partner is
//! the new tapped period. A third tap inside the window arms again ("does nothing
//! without a fourth"); its partner overrides the tempo taps 1+2 set. Tapping a
//! tempo only DEFINES it (displayed by the tap cell's blink); it does not start
//! any grid's amplitude cycling.
//!
//! *Factor.* A grid's applied tempo = tapped tempo x 2^a x 3^b, adjusted by its
//! own x2 / /2 / x3 / /3 buttons. Exponents (not a float factor) keep
//! x3-then-/3 exactly unity.
//!
//! *The =1 button* (per grid): a single tap zeroes this grid's exponents and
//! turns its amplitude cycling ON; a fast second tap (within
//! `UNITY_DOUBLE_TAP`) turns the cycling OFF. Cycling starts off. Its LED shows
//! the switch: bright while this grid's cycling is on.
//!
//! *Effect.* Each note struck while its grid's cycling is on (and a tempo
//! exists) gets a unipolar-triangle amplitude pulse in [0,1] at that grid's
//! applied tempo *at its onset* (fixed for the note's life; the engine's
//! `tempo_am_freq`/`tempo_am_phase`).

use std::time::{Duration, Instant};

/// The tap-tempo blink's on fraction of each applied-tempo cycle.
const BLINK_DUTY: f32 = 0.1;

/// Two =1 presses within this window count as the pulse-off double-tap.
const UNITY_DOUBLE_TAP: Duration = Duration::from_millis(400);

/// One grid's slice of the polyrhythm state: its factor exponents, its pulse
/// switch, and the =1 button's double-tap bookkeeping.
struct GridPulse {
  two_exp: i32,
  three_exp: i32,
  pulse_on: bool,
  last_unity: Option<Instant>,
}

impl GridPulse {
  fn new() -> Self {
    GridPulse { two_exp: 0, three_exp: 0, pulse_on: false, last_unity: None }
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
  /// The per-monome factor + pulse switches.
  grids: Vec<GridPulse>,
}

/// What a factor button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactorButton {
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
      grids: (0..num_grids).map(|_| GridPulse::new()).collect(),
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

  /// A factor-button press on `grid`'s pad at `now`. The four direction buttons
  /// nudge that grid's exponents; `=1` zeroes them AND drives the grid's pulse
  /// switch -- on for a lone tap, off for a fast double-tap.
  pub fn press(&mut self, grid: usize, button: FactorButton, now: Instant) {
    let Some(g) = self.grids.get_mut(grid) else {
      return;
    };
    match button {
      FactorButton::Times2 => g.two_exp += 1,
      FactorButton::Div2 => g.two_exp -= 1,
      FactorButton::Times3 => g.three_exp += 1,
      FactorButton::Div3 => g.three_exp -= 1,
      FactorButton::Unity => {
        g.two_exp = 0;
        g.three_exp = 0;
        let doubled = g
          .last_unity
          .is_some_and(|prev| now.duration_since(prev) <= UNITY_DOUBLE_TAP);
        // A lone tap turns cycling on; the fast second tap of an on-grid turns it
        // off (and a fast third tap turns it back on -- "tapping once" always
        // means on when it is off).
        g.pulse_on = !(doubled && g.pulse_on);
        g.last_unity = Some(now);
      }
    }
  }

  /// `grid`'s applied tempo in Hz (the global tapped tempo times this grid's
  /// factor), or None before any tempo has been tapped. This is what the tap
  /// cell DISPLAYS, whether or not the grid's cycling is on.
  pub fn applied_hz(&self, grid: usize) -> Option<f32> {
    let period = self.tapped_period?.as_secs_f32();
    if period <= 0.0 {
      return None; // two taps in the same instant: not a tempo
    }
    let g = self.grids.get(grid)?;
    Some((1.0 / period) * 2f32.powi(g.two_exp) * 3f32.powi(g.three_exp))
  }

  /// The pulse rate a note struck on `grid` right now should carry: its applied
  /// tempo, but only while this grid's amplitude cycling is ON (the =1 switch).
  pub fn pulse_hz(&self, grid: usize) -> Option<f32> {
    if self.grids.get(grid).is_some_and(|g| g.pulse_on) {
      self.applied_hz(grid)
    } else {
      None
    }
  }

  /// Is `grid`'s tap-cell blink ON at `now`? Lit for the first `BLINK_DUTY` of
  /// each of that grid's applied-tempo cycles, phase-anchored to the tap that set
  /// the tempo -- the tempo DISPLAY, so it blinks whether or not the grid's
  /// cycling is on (the painter renders ON as fully lit, OFF as black). Always
  /// off before a tempo exists (the button rests dim instead).
  pub fn tap_blink(&self, grid: usize, now: Instant) -> bool {
    let (Some(hz), Some(anchor)) = (self.applied_hz(grid), self.anchor) else {
      return false;
    };
    let period = 1.0 / hz;
    let phase = (now.duration_since(anchor).as_secs_f32() / period).fract();
    phase < BLINK_DUTY
  }

  /// LED state for `grid`'s factor buttons: each direction lights while that
  /// grid's exponent leans its way; `=1` shows the grid's pulse switch -- bright
  /// while its amplitude cycling is on.
  pub fn factor_lit(&self, grid: usize, button: FactorButton) -> bool {
    let Some(g) = self.grids.get(grid) else {
      return false;
    };
    match button {
      FactorButton::Times2 => g.two_exp > 0,
      FactorButton::Div2 => g.two_exp < 0,
      FactorButton::Times3 => g.three_exp > 0,
      FactorButton::Div3 => g.three_exp < 0,
      FactorButton::Unity => g.pulse_on,
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
  fn factors_are_per_grid_over_one_tapped_tempo() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(1000), WINDOW); // 1 Hz, globally
    p.press(0, FactorButton::Times3, t0);
    p.press(0, FactorButton::Times2, t0);
    p.press(1, FactorButton::Div2, t0);
    assert!((p.applied_hz(0).unwrap() - 6.0).abs() < 1e-5, "grid 0: x3 then x2 = 6x");
    assert!((p.applied_hz(1).unwrap() - 0.5).abs() < 1e-6, "grid 1: /2 halves, untouched by grid 0");
    assert!(p.factor_lit(0, FactorButton::Times2), "grid 0's lean shows on grid 0");
    assert!(!p.factor_lit(1, FactorButton::Times2), "grid 1 shows its own lean only");
    p.press(0, FactorButton::Div3, t0);
    p.press(0, FactorButton::Div2, t0);
    assert!((p.applied_hz(0).unwrap() - 1.0).abs() < 1e-6, "exactly back to unity");
  }

  #[test]
  fn tapping_a_tempo_does_not_start_the_pulse() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(500), WINDOW);
    assert!(p.applied_hz(0).is_some(), "the tempo is defined...");
    assert_eq!(p.pulse_hz(0), None, "...but cycling starts OFF: notes are un-pulsed");
    assert_eq!(p.pulse_hz(1), None);
    assert!(p.tap_blink(0, t0 + MS(520)), "the blink still displays the tempo");
  }

  #[test]
  fn a_unity_tap_turns_cycling_on_and_a_fast_double_tap_off() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(500), WINDOW); // 2 Hz
    p.press(0, FactorButton::Unity, t0 + MS(1000));
    let hz = p.pulse_hz(0).expect("a lone =1 tap turns grid 0's cycling on");
    assert!((hz - 2.0).abs() < 1e-4);
    assert!(p.factor_lit(0, FactorButton::Unity), "=1 lights while cycling is on");
    assert_eq!(p.pulse_hz(1), None, "grid 1's switch is its own");
    // The fast second tap: off.
    p.press(0, FactorButton::Unity, t0 + MS(1300));
    assert_eq!(p.pulse_hz(0), None, "a fast double-tap turns cycling off");
    assert!(!p.factor_lit(0, FactorButton::Unity));
    assert!(p.applied_hz(0).is_some(), "the tempo itself survives (still displayed)");
    // A fast THIRD tap: back on ("tapping it once turns it back on", however fast).
    p.press(0, FactorButton::Unity, t0 + MS(1500));
    assert!(p.pulse_hz(0).is_some(), "a tap with cycling off always turns it on");
    // A slow pair: on stays on (each lone tap re-asserts on), no accidental off.
    p.press(0, FactorButton::Unity, t0 + MS(3000));
    assert!(p.pulse_hz(0).is_some(), "a SLOW second tap is a lone tap: still on");
  }

  #[test]
  fn a_unity_tap_still_resets_the_factor() {
    let t0 = Instant::now();
    let mut p = two_grids();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(1000), WINDOW); // 1 Hz
    p.press(0, FactorButton::Times3, t0);
    p.press(0, FactorButton::Unity, t0 + MS(1500));
    assert!((p.applied_hz(0).unwrap() - 1.0).abs() < 1e-6, "=1 zeroes the exponents");
    assert!(p.pulse_hz(0).is_some(), "and turns cycling on");
    // The off double-tap resets too (its first tap already did).
    p.press(0, FactorButton::Times2, t0 + MS(2000));
    p.press(0, FactorButton::Unity, t0 + MS(2100));
    p.press(0, FactorButton::Unity, t0 + MS(2200));
    assert_eq!(p.pulse_hz(0), None, "double-tap: off");
    assert!((p.applied_hz(0).unwrap() - 1.0).abs() < 1e-6, "factor is unity after =1");
  }

  #[test]
  fn the_tap_blink_pulses_at_ten_percent_duty_from_the_set_anchor() {
    let t0 = Instant::now();
    let mut p = two_grids();
    assert!(!p.tap_blink(0, t0), "no tempo, no blink");
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(1000), WINDOW); // 1 Hz, anchored at t0+1s
    let anchor = t0 + MS(1000);
    assert!(p.tap_blink(0, anchor + MS(50)), "on during the first 10% of the cycle");
    assert!(!p.tap_blink(0, anchor + MS(500)), "off mid-cycle");
    assert!(p.tap_blink(0, anchor + MS(2050)), "on again two cycles later");
    // The display follows each grid's own applied tempo.
    p.press(1, FactorButton::Times2, anchor); // grid 1 blinks at 2 Hz
    assert!(p.tap_blink(1, anchor + MS(510)), "grid 1's second (half-second) cycle starts");
    assert!(!p.tap_blink(0, anchor + MS(510)), "grid 0 still mid-cycle at 1 Hz");
  }
}
