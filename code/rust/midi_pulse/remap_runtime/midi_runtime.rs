use midi_pulse::midi;
use std::sync::{Arc, Mutex};

use super::state::{RemappableEdoState, SoundingPitchCounts};
use super::{LOWEST_C, MIN_CHANNEL_OUT, MIN_NOTE_OUT};

pub(crate) fn edo_un12_instruction(
  original_note: u8,
  state: &Arc<Mutex<RemappableEdoState>>,
) -> (i16, i16) {
  let absolute_step = edo_un12_absolute_step(original_note, state);
  let edo = state.lock().unwrap().config.edo;
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
  channel_offset * state.config.edo + state.config.initial_map[pc] + state.deltas[pc]
}

pub(crate) fn print_note_on_trace(input: &[u8], output: &[u8]) {
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
    "note-on: input <{}, {}> -> output ({}, {})",
    input_note, input_pc, output_channel, output_note,
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
    sounding.counts[step as usize] += 1;
  } else if midi::is_note_off(message) {
    let mut sounding = sounding.lock().unwrap();
    if let Some(old_step) = sounding.by_original_note.remove(&original_note) {
      decrement_sounding_count(&mut sounding, old_step);
    }
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
