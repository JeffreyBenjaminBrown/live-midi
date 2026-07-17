//! Where a monomekey sits in the tuning, and where the scroll pad moves it.
//!
//! Cannibalized from the old `edo_play`, with the arithmetic unchanged and the names
//! made to say what they mean. The old module called this a "step", which is a fine
//! word for an interval and a poor one for an absolute position; a pitch is what it is.

use crate::voice::{MonomeKey, Pitch};

/// A tuning: how many equal divisions the octave has, and how far one monomekey is
/// from its neighbours in each direction.
///
/// This rig runs 46-EDO with `x_step` 9 and `y_step` 1, so pitch rises down and to the
/// right, and the monomekey directly below one is exactly one microtone above it. The
/// gestures do not rely on that: they are pinned to position, so they survive a
/// retuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
  /// Equal divisions of the octave.
  pub edo: i32,
  /// How many divisions one monomekey to the right is worth.
  pub x_step: i32,
  /// How many divisions one monomekey down is worth.
  pub y_step: i32,
  /// The frequency of pitch 0.
  pub fundamental_hz: f64,
}

impl Tuning {
  /// The pitch a monomekey sounds, given the grid's current register.
  ///
  /// The register is what makes this not a property of the monomekey: scroll, and the
  /// same button means something else. That is why a monomekey cannot identify a voice
  /// and why `Voices` records what each one is actually sounding.
  pub fn pitch_at(&self, register: i32, monomekey: MonomeKey) -> Pitch {
    register + self.x_step * monomekey.0 + self.y_step * monomekey.1
  }

  /// The frequency of a pitch, in Hz.
  pub fn hz(&self, pitch: Pitch) -> f64 {
    self.fundamental_hz * 2f64.powf(pitch as f64 / self.edo as f64)
  }

  /// A pitch's class: where it sits within the octave, ignoring which octave.
  ///
  /// The LEDs work almost entirely in classes, because a sounding voice lights all its
  /// octave-equivalents. The diamond dance is the one thing that does not.
  pub fn pitch_class(&self, pitch: Pitch) -> i32 {
    pitch.rem_euclid(self.edo)
  }

  /// Every monomekey on `rect` that sounds `pitch` under `register`.
  ///
  /// Usually one, but a grid can sound one pitch at two monomekeys: with `x_step` 9,
  /// `(x, y)` and `(x + 1, y - 9)` land on the same pitch. Both are real buttons and
  /// both mark the voice, so callers take all of them.
  pub fn monomekeys_sounding(&self, register: i32, rect: [i32; 4], pitch: Pitch) -> Vec<MonomeKey> {
    let [x0, y0, x1, y1] = rect;
    let mut out = vec![];
    for y in y0..=y1 {
      for x in x0..=x1 {
        if self.pitch_at(register, (x, y)) == pitch {
          out.push((x, y));
        }
      }
    }
    out
  }

  /// The lowest and highest pitch `rect` can sound under `register`.
  ///
  /// The corners bound it because both steps are positive: pitch rises down and right,
  /// so the top-left monomekey is the lowest and the bottom-right the highest. A
  /// negative step would break that, and no tuning here has one.
  pub fn visible_range(&self, register: i32, rect: [i32; 4]) -> (Pitch, Pitch) {
    let [x0, y0, x1, y1] = rect;
    let a = self.pitch_at(register, (x0, y0));
    let b = self.pitch_at(register, (x1, y1));
    (a.min(b), a.max(b))
  }
}

/// What the scroll pad does to a grid's register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
  Up,
  Down,
  Left,
  Right,
  OctaveDown,
  OctaveUp,
}

impl Scroll {
  /// How much this moves the register.
  ///
  /// Raising the register raises every visible pitch, so a voice ABOVE the window is
  /// reached by `OctaveUp`. That is the rule the off-screen indicator flashes by, and
  /// it is easy to get backwards.
  pub fn register_delta(&self, tuning: &Tuning) -> i32 {
    match self {
      Scroll::Up => -tuning.y_step,
      Scroll::Down => tuning.y_step,
      Scroll::Left => -tuning.x_step,
      Scroll::Right => tuning.x_step,
      Scroll::OctaveDown => -tuning.edo,
      Scroll::OctaveUp => tuning.edo,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The rig's own tuning, so the numbers below are the ones Jeff plays.
  fn rig() -> Tuning {
    Tuning { edo: 46, x_step: 9, y_step: 1, fundamental_hz: 80.0 }
  }

  #[test]
  fn a_monomekey_sounds_its_offset_from_the_register() {
    let t = rig();
    assert_eq!(t.pitch_at(0, (0, 0)), 0);
    assert_eq!(t.pitch_at(0, (1, 0)), 9, "one to the right is x_step");
    assert_eq!(t.pitch_at(0, (0, 1)), 1, "one down is y_step");
    assert_eq!(t.pitch_at(100, (0, 0)), 100, "the register offsets everything");
  }

  /// Pitch rises down and to the right, which is the fact every gesture's geometry
  /// leans on.
  #[test]
  fn pitch_rises_down_and_to_the_right() {
    let t = rig();
    assert!(t.pitch_at(0, (0, 1)) > t.pitch_at(0, (0, 0)), "down is higher");
    assert!(t.pitch_at(0, (1, 0)) > t.pitch_at(0, (0, 0)), "right is higher");
  }

  /// With y_step 1, the monomekey below a voice is one microtone above it. The edit
  /// gesture is pinned to that position rather than to this arithmetic, so a retuning
  /// moves the pitch without moving the gesture.
  #[test]
  fn the_monomekey_below_is_one_microtone_up_in_this_tuning() {
    let t = rig();
    assert_eq!(t.pitch_at(0, (3, 4)) - t.pitch_at(0, (3, 3)), 1);
  }

  #[test]
  fn an_octave_up_doubles_the_frequency() {
    let t = rig();
    assert!((t.hz(0) - 80.0).abs() < 1e-9);
    assert!((t.hz(46) - 160.0).abs() < 1e-6, "46 divisions is one octave");
    assert!((t.hz(-46) - 40.0).abs() < 1e-6);
  }

  #[test]
  fn a_pitch_class_wraps_the_octave_in_both_directions() {
    let t = rig();
    assert_eq!(t.pitch_class(0), 0);
    assert_eq!(t.pitch_class(46), 0);
    assert_eq!(t.pitch_class(47), 1);
    assert_eq!(t.pitch_class(-1), 45, "negative pitches wrap, they do not go negative");
  }

  /// A grid really can sound one pitch at two monomekeys, and both mark the voice.
  /// With x_step 9, (x, y) and (x + 1, y - 9) collide.
  #[test]
  fn one_pitch_can_sit_at_two_monomekeys() {
    let t = rig();
    let full = [0, 0, 15, 15];
    let pitch = t.pitch_at(0, (2, 10));
    let found = t.monomekeys_sounding(0, full, pitch);
    assert!(found.contains(&(2, 10)));
    assert!(found.contains(&(3, 1)), "9 * 3 + 1 == 9 * 2 + 10");
    assert_eq!(found.len(), 2);
  }

  #[test]
  fn a_pitch_off_the_grid_sits_at_no_monomekey() {
    let t = rig();
    assert!(t.monomekeys_sounding(0, [0, 0, 15, 15], 9999).is_empty());
  }

  #[test]
  fn the_visible_range_runs_from_the_top_left_to_the_bottom_right() {
    let t = rig();
    let (lo, hi) = t.visible_range(0, [0, 0, 15, 15]);
    assert_eq!(lo, 0);
    assert_eq!(hi, 9 * 15 + 15);
    let (lo, hi) = t.visible_range(100, [0, 0, 15, 15]);
    assert_eq!((lo, hi), (100, 100 + 9 * 15 + 15), "the register moves the window");
  }

  /// The rule the off-screen flash depends on, and the one that is easy to invert:
  /// raising the register raises the visible pitches, so a voice above the window is
  /// reached by scrolling up an octave.
  #[test]
  fn octave_up_raises_the_window_so_it_reaches_a_voice_above_it() {
    let t = rig();
    let (_, hi) = t.visible_range(0, [0, 0, 15, 15]);
    let after = t.visible_range(Scroll::OctaveUp.register_delta(&t), [0, 0, 15, 15]);
    assert!(after.1 > hi, "the window now reaches higher");
    assert_eq!(Scroll::OctaveUp.register_delta(&t), 46);
    assert_eq!(Scroll::OctaveDown.register_delta(&t), -46);
  }

  #[test]
  fn the_arrows_move_by_one_monomekey_in_each_direction() {
    let t = rig();
    assert_eq!(Scroll::Down.register_delta(&t), 1);
    assert_eq!(Scroll::Up.register_delta(&t), -1);
    assert_eq!(Scroll::Right.register_delta(&t), 9);
    assert_eq!(Scroll::Left.register_delta(&t), -9);
  }

  /// Scrolling and un-scrolling returns every monomekey to the pitch it started at.
  #[test]
  fn opposite_scrolls_cancel_exactly() {
    let t = rig();
    for (a, b) in [
      (Scroll::Up, Scroll::Down),
      (Scroll::Left, Scroll::Right),
      (Scroll::OctaveUp, Scroll::OctaveDown),
    ] {
      assert_eq!(a.register_delta(&t) + b.register_delta(&t), 0);
    }
  }
}
