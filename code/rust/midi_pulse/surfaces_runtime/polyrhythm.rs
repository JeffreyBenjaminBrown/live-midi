//! The polyrhythm interface's pure state: tap tempo + the applied-tempo factor
//! (see TODO/misc.org "polyrhythm interface"). One shared instance drives both
//! grids' pads.
//!
//! *Tap tempo.* Taps pair up: an ARMING tap, then -- within `window` (rolling; a
//! stale arm simply re-arms) -- a SETTING tap whose distance from its partner is
//! the new tapped period. A third tap inside the window arms again ("does nothing
//! without a fourth"); its partner overrides the tempo taps 1+2 set.
//!
//! *Factor.* The applied tempo = tapped tempo x 2^a x 3^b, adjusted by the x2 / /2
//! / x3 / /3 buttons; `=1` zeroes both exponents. Exponents (not a float factor)
//! keep x3-then-/3 exactly unity.
//!
//! *Effect.* Each note struck while a tempo is applied gets a descending-sawtooth
//! amplitude pulse at the applied tempo *at its onset* (fixed for the note's life;
//! the engine's `tempo_am_freq`/`tempo_am_phase`). No tempo tapped yet = no pulse.

use std::time::{Duration, Instant};

/// The tap-tempo blink's on fraction of each applied-tempo cycle.
const BLINK_DUTY: f32 = 0.1;

pub struct PolyrhythmState {
  /// The tapped period, once a pair of taps has set one.
  tapped_period: Option<Duration>,
  /// The moment the tempo was last set -- the blink's phase anchor, so the light
  /// pulses in time with the taps that defined it.
  anchor: Option<Instant>,
  /// The previous tap and whether it is ARMING (awaiting its partner).
  last_tap: Option<(Instant, bool)>,
  /// Applied tempo = tapped x 2^two_exp x 3^three_exp.
  two_exp: i32,
  three_exp: i32,
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
  pub fn new() -> Self {
    PolyrhythmState {
      tapped_period: None,
      anchor: None,
      last_tap: None,
      two_exp: 0,
      three_exp: 0,
    }
  }

  /// A press of the tap button at `now`, under the rolling `window`.
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

  pub fn press(&mut self, button: FactorButton) {
    match button {
      FactorButton::Times2 => self.two_exp += 1,
      FactorButton::Div2 => self.two_exp -= 1,
      FactorButton::Times3 => self.three_exp += 1,
      FactorButton::Div3 => self.three_exp -= 1,
      FactorButton::Unity => {
        self.two_exp = 0;
        self.three_exp = 0;
      }
    }
  }

  /// The applied tempo in Hz (the tapped tempo times the factor), or None before
  /// any tempo has been tapped.
  pub fn applied_hz(&self) -> Option<f32> {
    let period = self.tapped_period?.as_secs_f32();
    if period <= 0.0 {
      return None; // two taps in the same instant: not a tempo
    }
    Some((1.0 / period) * 2f32.powi(self.two_exp) * 3f32.powi(self.three_exp))
  }

  /// Is the tap button's blink ON at `now`? Lit for the first `BLINK_DUTY` of each
  /// applied-tempo cycle, phase-anchored to the tap that set the tempo. The painter
  /// renders ON as fully lit and OFF as black (misc.org "tap blink between black and
  /// fully lit"). Always off before a tempo exists (the button rests dim instead).
  pub fn tap_blink(&self, now: Instant) -> bool {
    let (Some(hz), Some(anchor)) = (self.applied_hz(), self.anchor) else {
      return false;
    };
    let period = 1.0 / hz;
    let phase = (now.duration_since(anchor).as_secs_f32() / period).fract();
    phase < BLINK_DUTY
  }

  /// LED state for the factor buttons: each direction lights while its exponent
  /// leans that way, and `=1` lights exactly when pressing it would do something
  /// (the factor is not unity).
  pub fn factor_lit(&self, button: FactorButton) -> bool {
    match button {
      FactorButton::Times2 => self.two_exp > 0,
      FactorButton::Div2 => self.two_exp < 0,
      FactorButton::Times3 => self.three_exp > 0,
      FactorButton::Div3 => self.three_exp < 0,
      FactorButton::Unity => self.two_exp != 0 || self.three_exp != 0,
    }
  }
}

impl Default for PolyrhythmState {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const WINDOW: Duration = Duration::from_secs(2);
  const MS: fn(u64) -> Duration = Duration::from_millis;

  #[test]
  fn two_taps_within_the_window_set_the_tempo() {
    let t0 = Instant::now();
    let mut p = PolyrhythmState::new();
    assert_eq!(p.applied_hz(), None, "no tempo before any taps");
    p.tap(t0, WINDOW);
    assert_eq!(p.applied_hz(), None, "one tap is only an arm");
    p.tap(t0 + MS(500), WINDOW);
    let hz = p.applied_hz().expect("a pair sets the tempo");
    assert!((hz - 2.0).abs() < 1e-4, "500 ms apart = 2 Hz: {hz}");
  }

  #[test]
  fn the_window_rolls_a_stale_arm_re_arms() {
    let t0 = Instant::now();
    let mut p = PolyrhythmState::new();
    p.tap(t0, WINDOW);
    // Too late to pair with tap 1 -- but it must itself arm, so a THIRD tap soon
    // after pairs with IT (no dead zone at window boundaries).
    p.tap(t0 + MS(3000), WINDOW);
    assert_eq!(p.applied_hz(), None, "stale pair sets nothing");
    p.tap(t0 + MS(3250), WINDOW);
    let hz = p.applied_hz().expect("the re-armed tap pairs with the next");
    assert!((hz - 4.0).abs() < 1e-4, "250 ms apart = 4 Hz: {hz}");
  }

  #[test]
  fn a_third_tap_does_nothing_until_its_fourth_overrides() {
    let t0 = Instant::now();
    let mut p = PolyrhythmState::new();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(500), WINDOW); // 2 Hz
    p.tap(t0 + MS(1000), WINDOW); // third tap: arms only
    let hz = p.applied_hz().unwrap();
    assert!((hz - 2.0).abs() < 1e-4, "third tap left the tempo alone: {hz}");
    p.tap(t0 + MS(1100), WINDOW); // fourth: 3+4 override 1+2
    let hz = p.applied_hz().unwrap();
    assert!((hz - 10.0).abs() < 1e-4, "taps 3+4 (100 ms) override: {hz}");
  }

  #[test]
  fn factor_exponents_are_exact_and_unity_resets() {
    let t0 = Instant::now();
    let mut p = PolyrhythmState::new();
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(1000), WINDOW); // 1 Hz
    p.press(FactorButton::Times3);
    p.press(FactorButton::Times2);
    assert!((p.applied_hz().unwrap() - 6.0).abs() < 1e-5, "x3 then x2 = 6x");
    p.press(FactorButton::Div3);
    p.press(FactorButton::Div2);
    assert!((p.applied_hz().unwrap() - 1.0).abs() < 1e-6, "exactly back to unity");
    p.press(FactorButton::Div2);
    assert!((p.applied_hz().unwrap() - 0.5).abs() < 1e-6, "/2 halves");
    assert!(p.factor_lit(FactorButton::Div2), "/2 lights while the factor leans down");
    assert!(p.factor_lit(FactorButton::Unity), "=1 lights while pressing it would act");
    p.press(FactorButton::Unity);
    assert!((p.applied_hz().unwrap() - 1.0).abs() < 1e-6, "=1 resets");
    assert!(!p.factor_lit(FactorButton::Unity), "=1 rests once the factor is unity");
  }

  #[test]
  fn the_tap_blink_pulses_at_ten_percent_duty_from_the_set_anchor() {
    let t0 = Instant::now();
    let mut p = PolyrhythmState::new();
    assert!(!p.tap_blink(t0), "no tempo, no blink");
    p.tap(t0, WINDOW);
    p.tap(t0 + MS(1000), WINDOW); // 1 Hz, anchored at t0+1s
    let anchor = t0 + MS(1000);
    assert!(p.tap_blink(anchor + MS(50)), "on during the first 10% of the cycle");
    assert!(!p.tap_blink(anchor + MS(500)), "off mid-cycle");
    assert!(p.tap_blink(anchor + MS(2050)), "on again two cycles later");
  }
}
