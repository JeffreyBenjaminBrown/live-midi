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
//! For each piano note (21-96):
//! - Subtract lowest A (21) to get 0-75
//! - divmod by 12: quotient -> channel offset,
//!                 remainder -> note offset
//! - Add those offsets to min_channel and min_note
//!   (The earlier channel value is discarded.)
//!
//! # OFFSET CONTROL
//! The top octave (notes 97-108, C#7 to C8) controls microtonal offset:
//! - F#7 (102) = 0 offset (12-EDO)
//! - G7 (103) = +1, G#7 = +2, ... C8 (108) = +6
//! - F7 (101) = -1, E7 = -2, ... C#7 (97) = -5
//! This offset is added to the output note, shifting all played notes.

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::mpsc;
use std::{io, thread};

const SHIFT_IN_12_EDO : i8 = -5;  // Added to the MIDI note before processing.
const LOWEST_A        : u8 = 21;  // A0, lowest note on 88-key piano
const MIN_CHANNEL     : u8 = 1;   // adjust for whatever the synth wants
const MIN_NOTE        : u8 = 28;  // could also be adjusted for the synth. I like to adjust the synth for this instead, though, because 28 = (128 - 72) / 2 puts the notes closest to the middle of the range [0,127], which makes future MIDI edits less constrained -- plenty of room to adjust up or down in either direction without switching channels.
const EDO_OVER_12     : u8 = 6;   // 72 / 12 = 6
const OFFSET_OCTAVE_START: u8 = 97;  // C#7 - first note of offset control octave (top 12 keys)
const OFFSET_ZERO_NOTE   : u8 = 102; // F#7 - this note means offset = 0

static CURRENT_OFFSET: AtomicI8 = AtomicI8::new(0);

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
  println!("  - offset control: notes {}-108 (F#7=0)",
           OFFSET_OCTAVE_START);
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
  let velocity: u8 = message[2];
  // Top octave controls the offset (F#7 = 0, G7 = +1, F7 = -1, etc.)
  if original_note >= OFFSET_OCTAVE_START {
    if status == 0x90 && velocity > 0 { // note-on
      let offset: i8 = original_note as i8 - OFFSET_ZERO_NOTE as i8;
      CURRENT_OFFSET.store(offset, Ordering::Relaxed); }
    return None; } // don't pass through offset control notes
  let normalized: i16 = original_note as i16
                        - LOWEST_A as i16
                        + SHIFT_IN_12_EDO as i16;
  let channel_offset: i16 = normalized.div_euclid(12);
  let note_offset: i16 = normalized.rem_euclid(12); // Whereas (%) preserves sign, rem_euclid returns in range [0,divisor-1].
  let new_channel: i16 = MIN_CHANNEL as i16 + channel_offset;
  let offset: i8 = CURRENT_OFFSET.load(Ordering::Relaxed);
  let new_note: i16 = MIN_NOTE as i16
                      + note_offset * EDO_OVER_12 as i16
                      + offset as i16;
  if ( new_channel < 0 || new_channel > 15 ||
       new_note    < 0 || new_note    > 127 )
  { // the MIDI standard does not allow such messages
    return None; }
  let new_status: u8 = status | (new_channel as u8);
  Some(vec![new_status,
            new_note as u8,
            velocity])
}
