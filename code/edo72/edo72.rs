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
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::{io, thread};

struct TransformedNote {
  output_channel: u8,
  output_note: u8,
}

struct ShiftPress {
  input_note: u8,
  shift_value: i8,
}

fn ongoing_notes(
) -> &'static Mutex<HashMap<u8, TransformedNote>> {
  static ONGOING: OnceLock<Mutex<HashMap<u8, TransformedNote>>> =
    OnceLock::new();
  ONGOING.get_or_init(
    || Mutex::new(HashMap::new() )) }

fn ongoing_shifts(
) -> &'static Mutex<HashMap<u8, ShiftPress>> {
  static ONGOING: OnceLock<Mutex<HashMap<u8, ShiftPress>>> =
    OnceLock::new();
  ONGOING.get_or_init(
    || Mutex::new(HashMap::new() )) }

fn pitch_class_shifts(
) -> &'static Mutex<HashMap<u8, i8>> {
  static SHIFTS: OnceLock<Mutex<HashMap<u8, i8>>> =
    OnceLock::new();
  SHIFTS.get_or_init(
    || Mutex::new(HashMap::new() )) }

fn current_total_shift() -> Option<i16> {
  let shifts = ongoing_shifts() . lock() . unwrap();
  if shifts . is_empty()
  { None
  } else { Some( shifts . values() . map(
                   |s| s . shift_value as i16)
                 . sum( )) }}

const SHIFT_IN_12_EDO : i8 = -5;  // Added to the MIDI note before processing.
const LOWEST_A        : u8 = 21;  // A0, lowest note on 88-key piano
const MIN_CHANNEL     : u8 = 1;   // adjust for whatever the synth wants
const MIN_NOTE        : u8 = 28;  // could also be adjusted for the synth. I like to adjust the synth for this instead, though, because 28 = (128 - 72) / 2 puts the notes closest to the middle of the range [0,127], which makes future MIDI edits less constrained -- plenty of room to adjust up or down in either direction without switching channels.
const EDO_OVER_12     : u8 = 6;   // 72 / 12 = 6
const OFFSET_OCTAVE_START: u8 = 97;  // C#7 - first note of offset control octave (top 12 keys)
const OFFSET_ZERO_NOTE   : u8 = 102; // F#7 - this note means offset = 0

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
        for msg in transform_message(message) {
          let _ = tx.send(msg); }},
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
) -> Vec<Vec<u8>> {
  if message.len() < 2 {
    return vec![message.to_vec()]; }
  let status: u8 = message[0] & 0xF0;
  if message.len() < 3 ||
    ! ( status == 0x80 || status == 0x90)
  { // Not a note event, so pass through unchanged.
    return vec![message.to_vec()]; }
  let original_note: u8 = message[1];
  let velocity: u8 = message[2];
  if original_note >= OFFSET_OCTAVE_START {
    handle_offset_control(
      status, velocity, original_note)
  } else {
    handle_regular_note(
      status, velocity, original_note) }}

/// Modifies the set of shifts.
fn handle_offset_control(
  status: u8,
  velocity: u8,
  input_note: u8
) -> Vec<Vec<u8>> {
  // Top octave controls the offset (F#7 = 0, G7 = +1, F7 = -1, etc.)
  // Total shift = sum of all held shift notes.
  let is_note_on: bool =
    status == 0x90 && velocity > 0;
  let is_note_off: bool =
    status == 0x80 || (status == 0x90 && velocity == 0);
  let mut shifts = ongoing_shifts().lock().unwrap();
  if is_note_on {
    let shift_value: i8 = input_note as i8
                          - OFFSET_ZERO_NOTE as i8;
    shifts.insert(input_note,
                  ShiftPress { input_note, shift_value });
  } else if is_note_off {
    shifts.remove(&input_note); }
  vec![] } // don't pass through offset control notes

fn handle_regular_note(
  status: u8,
  velocity: u8,
  original_note: u8
) -> Vec<Vec<u8>> {
  let is_note_on: bool =
    status == 0x90 && velocity > 0;
  let is_note_off: bool =
    status == 0x80 || (status == 0x90 && velocity == 0);
  if is_note_on {
    // Update the persistent pitch class shift before transformation,
    // but only if shift keys are being held (we find a Some).
    if let Some(total_shift) = current_total_shift() {
      let pitch_class: u8 = original_note % 12;
      pitch_class_shifts().lock().unwrap()
        .insert(pitch_class, total_shift as i8); }}
  let (new_channel, new_note): (i16, i16) =
    edo72_instruction(original_note);
  let output_in_range: bool = // what the MIDI standard allows
    new_channel >= 0 && new_channel <= 15 &&
    new_note >= 0 && new_note <= 127;
  let mut results: Vec<Vec<u8>> = vec![];
  let mut ongoing = ongoing_notes().lock().unwrap();
  if is_note_on {
    if let Some(old) = ongoing.get(&original_note) {
      // The input note is already playing.
      if !output_in_range ||
         old.output_channel != new_channel as u8 ||
         old.output_note != new_note as u8
      { // The old note is somehow different. Silence it.
        let off_status: u8 = 0x80 | old.output_channel;
        results.push(vec![off_status, old.output_note, 0]); }}
    if output_in_range {
      // Send the new note.
      ongoing.insert(original_note, TransformedNote {
        output_channel: new_channel as u8,
        output_note: new_note as u8 });
      let on_status: u8 = 0x90 | new_channel as u8;
      results.push(vec![on_status, new_note as u8, velocity]); }
  } else if is_note_off {
    if let Some(old) = ongoing.remove(&original_note) {
      // Look up what output the earlier note-on produced.
      let off_status: u8 = 0x80 | old.output_channel;
      results.push(vec![off_status, old.output_note, velocity]);
    } else if output_in_range {
      // Somehow there is no record of the earlier note-on.
      // Send a note-off anyway, using current settings.
      let off_status: u8 = 0x80 | new_channel as u8;
      results.push(vec![off_status, new_note as u8, velocity]); }}
  results }

fn edo72_instruction(
  original_note: u8
) -> (i16, // channel
      i16) { // note
  let normalized: i16 = original_note as i16
                        - LOWEST_A as i16
                        + SHIFT_IN_12_EDO as i16;
  let channel_offset: i16 = normalized.div_euclid(12);
  let note_offset: i16 = normalized.rem_euclid(12);
  let channel: i16 = MIN_CHANNEL as i16 + channel_offset;
  let pitch_class: u8 = original_note % 12;
  let shift :  i16 =
    pitch_class_shifts() . lock() . unwrap()
    . get(&pitch_class) . copied()
    . unwrap_or(0) as i16;
  let note: i16 = MIN_NOTE as i16
                  + note_offset * EDO_OVER_12 as i16
                  + shift;
  (channel, note) }
