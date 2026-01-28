//! 72-EDO MIDI transformer
//!
//! # USAGE
//! ```sh
//! cargo run --bin edo72
//! ```
//! Be sure the 'const' definitions in the code make sense --
//! they depend on the synth being used.
//!
//! # PURPOSE
//! Transforms piano notes into multi-channel output for 72-EDO tuning.
//! For this first pass, uses every 6th note (so really 12-EDO).
//! For each piano note (21-108):
//! - Subtract lowest A (21) to get 0-87
//! - divmod by 12: quotient -> channel offset,
//!                 remainder -> note offset
//! - Add those offsets to min_channel and min_note
//!   (The earlier channel value is discarded.)

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use std::sync::mpsc;
use std::{io, thread};

const SHIFT_IN_12_EDO : i8 = -5; // Added to the MIDI note before processing.
const LOWEST_A        : u8 = 21; // A0, lowest note on 88-key piano
const MIN_CHANNEL     : u8 = 0;  // adjust for whatever the synth wants
const MIN_NOTE        : u8 = 28; // could also be adjusted for the synth. I like to adjust the synth for this instead, though, because 28 = (128 - 72) / 2 puts the notes closest to the middle of the range [0,127], which makes future MIDI edits less constrained -- plenty of room to adjust up or down in either direction without switching channels.
const EDO_OVER_12     : u8 = 6;  // 72 / 12 = 6

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let midi_in: MidiInput =
    MidiInput::new("edo72-in")?;
  let midi_out: MidiOutput =
    MidiOutput::new("edo72-out")?;
  let conn_out: MidiOutputConnection =
    midi_out.create_virtual("out")?;
  let (tx, rx): (mpsc::Sender<Vec<u8>>,
                 mpsc::Receiver<Vec<u8>>) = mpsc::channel();
  let _out_thread: thread::JoinHandle<()> =
    thread::spawn(move || {
      run_output_thread(conn_out, rx); });
  let _conn_in: MidiInputConnection<()> =
    midi_in.create_virtual(
      "in",
      move |_timestamp: u64, message: &[u8], _: &mut ()| {
        if let Some(transformed) = transform_message(message) {
          let _ = tx.send(transformed); }},
      () )?;
  print_startup_message();
  let mut input: String = String::new();
  io::stdin().read_line(&mut input)?;
  Ok (( )) }

fn print_startup_message() {
  println!("72-EDO transformer started!");
  println!();
  println!("Virtual ports created:");
  println!("  - 'edo72-in:in' (input)");
  println!("  - 'edo72-out:out' (output)");
  println!();
  println!("Config:");
  println!("  - min_channel: {}", MIN_CHANNEL);
  println!("  - min_midi_note: {}", MIN_NOTE);
  println!();
  println!("Press Enter to exit...");
}

fn run_output_thread(
  mut conn: MidiOutputConnection,
  rx: mpsc::Receiver<Vec<u8>>)
{ while let Ok(data) = rx.recv() {
    let _ = conn.send(&data); }}

fn transform_message(
  message: &[u8]
) -> Option<Vec<u8>> {
  if message.len() < 2 {
    return Some(message.to_vec()); }
  let status: u8 = message[0] & 0xF0;
  if message.len() < 3 ||
    ! ( status == 0x80 || status == 0x90)
  { // Not a note event, so pass through unchanged.
    return Some(message.to_vec()); }
  let original_note: u8 = message[1];
  let normalized: i16 = original_note as i16
                        - LOWEST_A as i16
                        + SHIFT_IN_12_EDO as i16;
  let channel_offset: i16 = normalized / 12;
  let note_offset: i16 = normalized % 12;
  let new_channel: i16 = MIN_CHANNEL as i16 + channel_offset;
  let new_note: i16 = MIN_NOTE as i16
                      + note_offset * EDO_OVER_12 as i16;
  if ( new_channel < 0 || new_channel > 15 ||
       new_note < 0 || new_note > 127 )
  { // the MIDI standard does not allow such messages
    return None; }
  let new_status: u8 = status | (new_channel as u8);
  let velocity: u8 = message[2];
  Some(vec![new_status,
            new_note as u8,
            velocity])
}
