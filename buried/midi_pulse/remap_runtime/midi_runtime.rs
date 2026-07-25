use edo_surface::midi;
use std::sync::{Arc, Mutex};

use super::state::{RemappableEdoState, SoundingPitchCounts};
use super::{LOWEST_C, MIN_CHANNEL_OUT, MIN_NOTE_OUT};

pub(crate) fn edo_un12_instruction(
  original_note: u8,
  state: &Arc<Mutex<RemappableEdoState>>,
) -> (i16, i16) {
  let absolute_step = edo_un12_absolute_step(original_note, state);
  let edo = state.lock().unwrap().rig.edo;
  let channel = MIN_CHANNEL_OUT as i16 + absolute_step.div_euclid(edo);
  let note = MIN_NOTE_OUT as i16 + absolute_step.rem_euclid(edo);
  (channel, note)
}

fn edo_un12_absolute_step(original_note: u8, state: &Arc<Mutex<RemappableEdoState>>) -> i16 {
  let normalized = original_note as i16 - LOWEST_C as i16;
  let channel_offset = normalized.div_euclid(12);
  let pitch_class = original_note % 12;
  let state = state.lock().unwrap();
  let pc = pitch_class as usize;
  channel_offset * state.rig.edo + state.rig.initial_map[pc] + state.deltas[pc]
}

pub(crate) fn print_note_on_trace(input: &[u8], output: &[u8], source: usize, dest: usize) {
  if input.len() < 3 || output.len() < 3 {
    return;
  }
  if !midi::is_note_on(input) || !midi::is_note_on(output) {
    return;
  }
  let input_note = input[1];
  let input_pc = input_note % 12;
  let output_channel = (output[0] & 0x0f) + 1;
  let output_note = output[1];
  println!(
    "note-on: input <{}, {}> -> output ({}, {}) [kb {} -> 58-edo {}]",
    input_note, input_pc, output_channel, output_note, source + 1, dest + 1,
  );
}

pub(crate) fn update_sounding(
  message: &[u8],
  state: &Arc<Mutex<RemappableEdoState>>,
  sounding: &Arc<Mutex<SoundingPitchCounts>>,
) {
  if message.len() < 3 || !midi::is_note_event(message) {
    return;
  }
  let original_note = message[1];
  if midi::is_note_on(message) {
    let step = sounding_step(original_note, state);
    let mut sounding = sounding.lock().unwrap();
    if let Some(old_step) = sounding.by_original_note.insert(original_note, step) {
      decrement_sounding_count(&mut sounding, old_step);
    }
    remove_held_note(&mut sounding, original_note);
    sounding.held_order.push(original_note);
    sounding.counts[step as usize] += 1;
  } else if midi::is_note_off(message) {
    let mut sounding = sounding.lock().unwrap();
    if let Some(old_step) = sounding.by_original_note.remove(&original_note) {
      decrement_sounding_count(&mut sounding, old_step);
    }
    remove_held_note(&mut sounding, original_note);
  }
}

fn sounding_step(original_note: u8, state: &Arc<Mutex<RemappableEdoState>>) -> i16 {
  let pitch_class = (original_note % 12) as usize;
  state.lock().unwrap().map[pitch_class]
}

fn decrement_sounding_count(sounding: &mut SoundingPitchCounts, step: i16) {
  let count = &mut sounding.counts[step as usize];
  if *count > 0 {
    *count -= 1;
  }
}

fn remove_held_note(sounding: &mut SoundingPitchCounts, original_note: u8) {
  if let Some(index) = sounding
    .held_order
    .iter()
    .position(|held_note| *held_note == original_note)
  {
    sounding.held_order.remove(index);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use super::super::rig::{RemapRig, RemapIdiom};

  fn test_state() -> Arc<Mutex<RemappableEdoState>> {
    Arc::new(Mutex::new(RemappableEdoState::new(RemapRig::new(
      80.0,
      12,
      1,
      0,
      RemapIdiom::Snap,
      12,
      8,
    ))))
  }

  #[test]
  fn releasing_latest_note_restores_previous_held_note_as_most_recent() {
    let state = test_state();
    let sounding = Arc::new(Mutex::new(SoundingPitchCounts::new(12)));

    update_sounding(&[0x90, 28, 64], &state, &sounding);
    update_sounding(&[0x90, 31, 64], &state, &sounding);
    update_sounding(&[0x80, 31, 0], &state, &sounding);

    let sounding = sounding.lock().unwrap();
    assert_eq!(sounding.held_order, vec![28]);
    assert_eq!(sounding.most_recent_step(), Some(4));
  }
}
