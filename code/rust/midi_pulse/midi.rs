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
