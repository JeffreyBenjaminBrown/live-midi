use crate::midi;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

pub struct TransformedNote {
  pub output_channel: u8,
  pub output_note: u8,
}

pub fn transform_message<F>(
  message: &[u8],
  ongoing: &Arc<Mutex<HashMap<u8, TransformedNote>>>,
  instruction: F,
) -> Vec<Vec<u8>>
where
  F: FnOnce(u8) -> (i16, i16),
{
  if message.len() < 3 || !midi::is_note_event(message) {
    return vec![message.to_vec()];
  }
  handle_note_event(message[0] & 0xF0, message[2], message[1], ongoing, instruction)
}

fn handle_note_event<F>(
  status: u8,
  velocity: u8,
  original_note: u8,
  ongoing: &Arc<Mutex<HashMap<u8, TransformedNote>>>,
  instruction: F,
) -> Vec<Vec<u8>>
where
  F: FnOnce(u8) -> (i16, i16),
{
  let data: [u8; 3] = [status, original_note, velocity];
  let is_note_on = midi::is_note_on(&data);
  let is_note_off = midi::is_note_off(&data);
  let (new_channel, new_note) = instruction(original_note);
  let output_in_range =
    new_channel >= 0 && new_channel <= 15 &&
    new_note >= 0 && new_note <= 127;
  let mut results: Vec<Vec<u8>> = vec![];
  let mut ongoing: MutexGuard<'_, HashMap<u8, TransformedNote>> =
    ongoing.lock().unwrap();

  if is_note_on {
    if let Some(old) = ongoing.get(&original_note) {
      if !output_in_range ||
         old.output_channel != new_channel as u8 ||
         old.output_note != new_note as u8
      {
        let off_status = 0x80 | old.output_channel;
        results.push(vec![off_status, old.output_note, 0]);
      }
    }
    if output_in_range {
      ongoing.insert(original_note, TransformedNote {
        output_channel: new_channel as u8,
        output_note: new_note as u8,
      });
      let on_status = 0x90 | new_channel as u8;
      results.push(vec![on_status, new_note as u8, velocity]);
    }
  } else if is_note_off {
    if let Some(old) = ongoing.remove(&original_note) {
      let off_status = 0x80 | old.output_channel;
      results.push(vec![off_status, old.output_note, velocity]);
    } else if output_in_range {
      let off_status = 0x80 | new_channel as u8;
      results.push(vec![off_status, new_note as u8, velocity]);
    }
  }
  results
}
