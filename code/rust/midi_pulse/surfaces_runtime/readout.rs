//! What the on-screen window says: the pure formatting, so it can be tested without
//! an X server (`TODO/many/2_discussion.org` 3c).
//!
//! Jeff asked for exactly four things and explicitly declined the rest ("The rest
//! would be too much for me to read live"):
//! - the global pulse as BPM to one decimal place;
//! - each monome's factor as `2^x * 3^y` for integer x, y;
//! - whether each monome's pulse is on at all;
//! - a blinker at the *unfactored* global tempo.
//!
//! The window exists because the controls left the grid: the `=1` LED used to show
//! the switch and the tap cell used to blink the tempo, and both moved to the feet.

/// The tempo as BPM to one decimal place. `None` before any tempo is tapped -- the
/// tapped tempo is genuinely absent then, not zero.
pub fn bpm(hz: Option<f32>) -> Option<String> {
  hz.map(|hz| format!("{:.1}", hz * 60.0))
}

/// A grid's factor as `2^x * 3^y`, Jeff's requested form. Exponents, not a decimal:
/// that is how the state is actually held (which is why x3-then-/3 is *exactly*
/// unity), and it is what tells you at a glance which pedals to press to get back.
pub fn factor(two_exp: i32, three_exp: i32) -> String {
  format!("2^{two_exp} * 3^{three_exp}")
}

/// One grid's line: its name, whether its pulse is on, and its factor.
pub fn grid_line(name: &str, pulse_on: bool, two_exp: i32, three_exp: i32) -> String {
  let state = if pulse_on { "ON " } else { "off" };
  format!("{name}  pulse {state}  {}", factor(two_exp, three_exp))
}

#[cfg(test)]
mod tests {
  use super::*;

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
  fn the_factor_reads_as_powers_of_two_and_three() {
    assert_eq!(factor(0, 0), "2^0 * 3^0", "unity is shown, not blanked");
    assert_eq!(factor(1, 0), "2^1 * 3^0");
    assert_eq!(factor(-1, 2), "2^-1 * 3^2");
  }

  #[test]
  fn a_grid_line_shows_the_switch_and_the_factor() {
    assert_eq!(grid_line("LOM", true, 1, -1), "LOM  pulse ON   2^1 * 3^-1");
    assert_eq!(grid_line("RNM", false, 0, 0), "RNM  pulse off  2^0 * 3^0");
  }

  /// The two grids' lines must be the same width whatever their state, or the column
  /// jitters while you read it.
  #[test]
  fn the_grid_lines_line_up_across_states() {
    let on = grid_line("LOM", true, 0, 0);
    let off = grid_line("RNM", false, 0, 0);
    assert_eq!(on.len(), off.len(), "ON/off must not shift the factor column");
  }
}
