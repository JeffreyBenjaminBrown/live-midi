use midir::MidiOutputConnection;
use std::collections::HashSet;
use std::sync::mpsc;

pub fn run_output_thread(
  mut conn: MidiOutputConnection,
  rx: mpsc::Receiver<Vec<u8>>,
) {
  while let Ok(data) = rx.recv() {
    let _ = conn.send(&data);
  }
}

/// A SoftStep exposes several input ports and only ONE of them carries the tether
/// sensor stream; the others are silent from our point of view. Which one it is
/// depends on the unit: the original (ALSA client "SSCOM") names it "SSCOM MIDI 1",
/// while the newer one ("SoftStep") names it "SoftStep Control Surface" and *also*
/// offers "SoftStep TRS MIDI Out" and "SoftStep CV Out". So the data port must be
/// picked by name preference, in this order. See
/// `learnings/keith-mcmillen-softstep.org` ("For the Rust synth").
pub const SOFTSTEP_DATA_PORT_PREFERENCE: [&str; 2] = ["MIDI 1", "Control Surface"];

/// Pick the data port among `names`, taking the first entry that contains the
/// earliest-listed `preference` substring. Returns the chosen index and whether the
/// pick was a *guess* -- true when no preference matched and there was more than one
/// candidate, so the caller can warn rather than bind the wrong port silently.
///
/// `None` only when `names` is empty.
pub fn preferred_port_index(names: &[String], preference: &[&str]) -> Option<(usize, bool)> {
  if names.is_empty() {
    return None;
  }
  for want in preference {
    if let Some(idx) = names.iter().position(|n| n.contains(want)) {
      return Some((idx, false));
    }
  }
  Some((0, names.len() > 1))
}

pub fn note(data: &[u8]) -> Option<u8> {
  if data.len() >= 2 && is_note_event(data) {
    Some(data[1])
  } else {
    None
  }
}

pub fn channel(data: &[u8]) -> Option<u8> {
  if data.is_empty() {
    None
  } else {
    Some(data[0] & 0x0F)
  }
}

pub fn is_note_on(data: &[u8]) -> bool {
  if data.len() >= 3 {
    let status: u8 = data[0] & 0xF0;
    status == 0x90 && data[2] > 0
  } else {
    false
  }
}

pub fn is_note_off(data: &[u8]) -> bool {
  if data.len() >= 3 {
    let status: u8 = data[0] & 0xF0;
    status == 0x80 || (status == 0x90 && data[2] == 0)
  } else {
    false
  }
}

pub fn is_note_event(data: &[u8]) -> bool {
  if data.is_empty() {
    return false;
  }
  let status: u8 = data[0] & 0xF0;
  status == 0x80 || status == 0x90
}

pub fn send_all_notes_off(
  conn: &mut MidiOutputConnection,
  active_notes: &HashSet<(u8, u8)>,
) {
  for &(channel, note) in active_notes.iter() {
    let note_off: [u8; 3] = [0x80 | channel, note, 0];
    let _ = conn.send(&note_off);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
  }

  fn pick(v: &[&str]) -> Option<(usize, bool)> {
    preferred_port_index(&names(v), &SOFTSTEP_DATA_PORT_PREFERENCE)
  }

  #[test]
  fn original_softstep_binds_midi_1() {
    assert_eq!(pick(&["SSCOM MIDI 1", "SSCOM MIDI 2"]), Some((0, false)));
  }

  #[test]
  fn original_softstep_binds_midi_1_whatever_the_order() {
    assert_eq!(pick(&["SSCOM MIDI 2", "SSCOM MIDI 1"]), Some((1, false)));
  }

  /// The regression this exists for: the newer unit's data port is "Control Surface",
  /// so a "MIDI 1"-only preference fell through to "take the first" and bound the CV
  /// or TRS port -- silently, and with no sensor stream on it.
  #[test]
  fn newer_softstep_binds_control_surface_not_cv_or_trs() {
    assert_eq!(
      pick(&["SoftStep CV Out", "SoftStep TRS MIDI Out", "SoftStep Control Surface"]),
      Some((2, false)),
    );
  }

  /// "SoftStep TRS MIDI Out" contains "MIDI" but must NOT satisfy the "MIDI 1" preference.
  #[test]
  fn trs_midi_out_does_not_count_as_midi_1() {
    assert_eq!(pick(&["SoftStep TRS MIDI Out", "SoftStep Control Surface"]), Some((1, false)));
  }

  #[test]
  fn sole_candidate_is_taken_without_a_guess_warning() {
    assert_eq!(pick(&["Some Other Board"]), Some((0, false)));
  }

  #[test]
  fn several_candidates_and_no_preference_match_is_flagged_as_a_guess() {
    assert_eq!(pick(&["Mystery Port A", "Mystery Port B"]), Some((0, true)));
  }

  #[test]
  fn no_candidates_is_none() {
    assert_eq!(pick(&[]), None);
  }

  #[test]
  fn preference_order_wins_over_port_order() {
    // Both preferences present: "MIDI 1" is listed first, so it takes precedence
    // even though "Control Surface" appears earlier in the port list.
    assert_eq!(pick(&["Control Surface", "MIDI 1"]), Some((1, false)));
  }
}
