//! What the on-screen window says: the pure formatting, so it can be tested without
//! an X server (`TODO/many/2_discussion.org` 3c).
//!
//! Jeff asked for exactly four things and explicitly declined the rest ("The rest
//! would be too much for me to read live"):
//! - the global tapped tempo as BPM to one decimal place;
//! - each monome's tempo factor as `2^x * 3^y` for integer x, y;
//! - whether each monome's factored pulse is on at all;
//! - a blinker at the *unfactored* global tempo.
//!
//! The window exists because the controls left the grid: the `=1` LED used to show
//! the switch and the tap cell used to blink the tempo, and both moved to the feet.

/// The tempo as BPM to one decimal place. `None` before any tempo is tapped -- the
/// tapped tempo is genuinely absent then, not zero.
pub fn bpm(hz: Option<f32>) -> Option<String> {
  hz.map(|hz| format!("{:.1}", hz * 60.0))
}

/// A grid's tempo factor as `2^x * 3^y`, Jeff's requested form. Exponents, not a
/// decimal: that is how the state is actually held (which is why x3-then-/3 is
/// *exactly* unity), and it is what tells you at a glance which pedals to press to
/// get back.
pub fn tempo_factor(two_exp: i32, three_exp: i32) -> String {
  format!("2^{two_exp} * 3^{three_exp}")
}

/// One grid's line: its name, whether its factored pulse is on, and its tempo factor.
pub fn grid_line(name: &str, factored_pulse_on: bool, two_exp: i32, three_exp: i32) -> String {
  let state = if factored_pulse_on { "ON " } else { "off" };
  format!("{name}  factored pulse {state}  {}", tempo_factor(two_exp, three_exp))
}

/// One grid's PEDAL SLIDE line: where its expression pedal is, and what the lowest
/// voice it can hear is actually sounding. Added to diagnose "there's something
/// discontinuous about it" -- both numbers change continuously when a slide is
/// behaving, so a jump in either is visible as it happens rather than reconstructed
/// afterwards from memory.
///
/// - `pedal` is the pedal's own scale, `[0, 120]`, one decimal (the EX-P's reliable
///   raw travel is 1..119, normalized and rescaled). `None` = this pedal has not sent
///   a CC yet, shown as `--` rather than a number that would be a guess -- and that is
///   a state worth seeing, because until it ends the engine has no idea which side of
///   the pedal is home.
/// - `pitch` is in EDO STEPS from the tuning fundamental, two decimals, so a slide
///   mid-flight (which sits between cells, on no note at all) reads as the fractional
///   thing it is. In 46-EDO, 0 is the fundamental and 46 is an octave above it.
pub fn pedal_slide_line(name: &str, pedal: Option<f32>, pitch: Option<f32>) -> String {
  // Both placeholders are padded to their number's exact width, so a value appearing
  // or vanishing never shifts the columns beside it.
  let pedal = match pedal {
    Some(p) => format!("{:05.1}", p.clamp(0.0, 120.0)),
    None => format!("{:^5}", "--"),
  };
  let pitch = match pitch {
    Some(p) => format!("{p:+08.2}"),
    None => format!("{:^8}", "--"),
  };
  format!("{name}  pedal {pedal}  low {pitch}")
}

/// A frequency as (fractional) EDO steps from the tuning fundamental -- the inverse of
/// the sink's `freq_for_pitch`. `None` for a frequency that cannot be a pitch, so a
/// dead or nonsense voice shows `--` instead of `-inf` or `NaN`.
pub fn steps_from_hz(hz: f32, fund: f64, edo: i32) -> Option<f32> {
  if !hz.is_finite() || hz <= 0.0 || fund <= 0.0 || edo <= 0 {
    return None;
  }
  Some(((hz as f64 / fund).log2() * edo as f64) as f32)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The two numbers Jeff asked to watch while hunting the discontinuity.
  #[test]
  fn the_pedal_slide_line_shows_the_pedal_in_its_own_scale_and_the_pitch_in_steps() {
    let line = pedal_slide_line("LOM", Some(72.4), Some(13.45));
    assert!(line.contains("072.4"), "the pedal on its own 0..120 scale: {line}");
    assert!(line.contains("+0013.45"), "the pitch in EDO steps, two decimals: {line}");
  }

  /// A pedal that has not been touched yet must not READ as one parked at the heel:
  /// that difference is exactly what decides which side is home.
  #[test]
  fn an_untouched_pedal_shows_a_dash_rather_than_a_zero() {
    let untouched = pedal_slide_line("LOM", None, None);
    assert!(untouched.contains("--"), "{untouched}");
    assert!(!untouched.contains("000.0"), "an absent pedal is not a pedal at 0: {untouched}");
    assert!(pedal_slide_line("LOM", Some(0.0), None).contains("000.0"), "but a real heel is");
  }

  /// The columns are fixed-width, so a number changing does not shuffle the ones beside
  /// it -- a jittering line is unreadable at a glance, which is the only way this gets
  /// read.
  #[test]
  fn the_columns_do_not_move_as_the_numbers_change() {
    let a = pedal_slide_line("LOM", Some(0.0), Some(-5.0));
    let b = pedal_slide_line("LOM", Some(120.0), Some(120.25));
    let c = pedal_slide_line("LOM", None, None);
    assert_eq!(a.len(), b.len(), "{a} vs {b}");
    assert_eq!(a.len(), c.len(), "{a} vs {c}");
  }

  #[test]
  fn steps_from_hz_inverts_the_sinks_pitch_to_frequency_map() {
    let (fund, edo) = (80.0, 46);
    assert!((steps_from_hz(80.0, fund, edo).unwrap() - 0.0).abs() < 1e-3, "the fundamental is 0");
    assert!((steps_from_hz(160.0, fund, edo).unwrap() - 46.0).abs() < 1e-3, "an octave is edo");
    assert!((steps_from_hz(40.0, fund, edo).unwrap() + 46.0).abs() < 1e-3, "and down is negative");
    // Round-trips against the real forward map, at a fractional step (a slide in flight).
    let hz = crate::pitch::freq_for_pitch(27, fund, edo);
    assert!((steps_from_hz(hz, fund, edo).unwrap() - 27.0).abs() < 1e-2);
  }

  #[test]
  fn a_nonsense_frequency_is_absent_rather_than_infinite() {
    assert_eq!(steps_from_hz(0.0, 80.0, 46), None);
    assert_eq!(steps_from_hz(-10.0, 80.0, 46), None);
    assert_eq!(steps_from_hz(f32::NAN, 80.0, 46), None);
  }

  #[test]
  fn bpm_is_one_decimal_place() {
    assert_eq!(bpm(Some(2.0)).as_deref(), Some("120.0"));
    assert_eq!(bpm(Some(1.0)).as_deref(), Some("60.0"));
    // 2.5 Hz = 150 bpm; a rate that is not a whole bpm still shows one place.
    assert_eq!(bpm(Some(2.505)).as_deref(), Some("150.3"));
  }

  /// Before any tap there is no tempo -- not a tempo of zero. The window has to say
  /// so rather than print "0.0", which would read as a stopped clock.
  #[test]
  fn bpm_is_absent_until_a_tempo_is_tapped() {
    assert_eq!(bpm(None), None);
  }

  #[test]
  fn the_tempo_factor_reads_as_powers_of_two_and_three() {
    assert_eq!(tempo_factor(0, 0), "2^0 * 3^0", "unity is shown, not blanked");
    assert_eq!(tempo_factor(1, 0), "2^1 * 3^0");
    assert_eq!(tempo_factor(-1, 2), "2^-1 * 3^2");
  }

  #[test]
  fn a_grid_line_shows_the_switch_and_the_tempo_factor() {
    assert_eq!(grid_line("LOM", true, 1, -1), "LOM  factored pulse ON   2^1 * 3^-1");
    assert_eq!(grid_line("RNM", false, 0, 0), "RNM  factored pulse off  2^0 * 3^0");
  }

  /// The two grids' lines must be the same width whatever their state, or the column
  /// jitters while you read it.
  #[test]
  fn the_grid_lines_line_up_across_states() {
    let on = grid_line("LOM", true, 0, 0);
    let off = grid_line("RNM", false, 0, 0);
    assert_eq!(on.len(), off.len(), "ON/off must not shift the tempo-factor column");
  }
}
