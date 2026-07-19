/// See the README.
/// Maybe tweak the 'CONST's defined below.

#[path = "edo12n_gui.rs"]
mod gui;

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use edo_surface::rig::Rig;
use edo_surface::midi;
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::{io, thread};

const SHIFT_IN_12_EDO : i8 = -5;  // Added to the MIDI note before processing.
const LOWEST_A        : u8 = 21;  // A0, lowest note on 88-key piano
const MIN_CHANNEL_OUT : u8 = 1;   // adjust for whatever the synth wants
const MIN_NOTE_OUT    : u8 = 28;  // could also be adjusted for the synth. I like to adjust the synth for this instead, though, because 28 = (128 - 72) / 2 puts the notes closest to the middle of the range [0,127], which makes future MIDI edits less constrained -- plenty of room to adjust up or down in either direction without switching channels.
const EDO_OVER_12     : u8 = 6;   // 72 / 12 = 6
const OFFSET_OCTAVE_START : u8 = 97;  // C#7 - first note of offset control octave (top 12 keys)
const OFFSET_ZERO_NOTE    : u8 = 102; // F#7 - this note means offset = 0
#[allow(dead_code)]
pub const FLASH_MILLIS : u64 = 500; // how long a note-on flash lasts
#[allow(dead_code)]
pub const TICK_MILLIS  : u64 = 50;  // redraw interval during flash animation
pub const GRID_ROWS : usize = 6;  // strings; vertical extent of grid
pub const GRID_COLS : usize = 7;  // frets; horizontal extent of grid
pub const GRID_ANCHOR   : usize = 4; // pitch class of bottom-left cell (4 = E)
pub const GRID_ROW_STEP : usize = 5; // semitones between adjacent strings (5 = perfect fourth)
pub const WHITE_KEYS : [bool; 12] = [
  true, false, true, // C, C#, D
  false, true, true, // D#, E, F, etc.
  false, true, false, true, false, true, ];

struct TransformedNote {
  output_channel: u8,
  output_note: u8, }

struct ShiftPress {
  shift_value: i8, }

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

pub fn pitch_class_shifts(
) -> &'static Mutex<HashMap<u8, i8>> {
  static SHIFTS: OnceLock<Mutex<HashMap<u8, i8>>> =
    OnceLock::new();
  SHIFTS.get_or_init(
    || Mutex::new(HashMap::new() )) }

fn current_total_shift() -> Option<i16> {
  let shifts: MutexGuard<'_, HashMap<u8, ShiftPress>> =
    ongoing_shifts() . lock() . unwrap();
  if shifts . is_empty()
  { None
  } else { Some( shifts . values() . map(
                   |s: &ShiftPress| s . shift_value as i16)
                 . sum( )) }}

pub fn run_from_rig(_rig: &Rig) -> Result<(), Box<dyn std::error::Error>> {
  run()
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
  let midi_in: MidiInput =
    MidiInput::new("edo12n_piano-in")?;
  let midi_out: MidiOutput =
    MidiOutput::new("edo12n_piano-out")?;
  let conn_out: MidiOutputConnection =
    midi_out.create_virtual("out")?;
  let (tx, rx): (mpsc::Sender<Vec<u8>>,
                 mpsc::Receiver<Vec<u8>>) = mpsc::channel();
  let _out_thread: thread::JoinHandle<()> =
    thread::spawn(move || {
      midi::run_output_thread(conn_out, rx); });
  let (display_tx, display_rx): (mpsc::Sender<(u8, bool)>,
                                 mpsc::Receiver<(u8, bool)>) = mpsc::channel();
  let _conn_in: MidiInputConnection<()> =
    midi_in.create_virtual(
      "in",
      move |_timestamp: u64, message: &[u8], _: &mut ()| {
        for msg in transform_message(message) {
          let _ = tx.send(msg); }
        if message.len() >= 3 && message[1] < OFFSET_OCTAVE_START {
          let is_note_on: bool = midi::is_note_on(message);
          let is_note_off: bool = midi::is_note_off(message);
          if is_note_on || is_note_off {
            let _ = display_tx.send(
              (message[1] % 12, is_note_on));
          }}},
      () )?;
  print_startup_message();
  let _display_thread: thread::JoinHandle<()> =
    thread::spawn(move || {
      gui::run_display_thread(display_rx); });
  let mut input: String = String::new();
  io::stdin().read_line(&mut input)?;
  Ok (( )) }

fn print_startup_message() {
  println!("72-EDO transformer started!");
  println!();
  println!("Virtual ports created:");
  println!("  - 'edo12n_piano-in:in' (input)");
  println!("  - 'edo12n_piano-out:out' (output)");
  println!();
  println!("Rig:");
  println!("  - min_channel: {}", MIN_CHANNEL_OUT);
  println!("  - min_midi_note: {}", MIN_NOTE_OUT);
  println!("  - offset control: notes {}-108 (F#7=0)",
           OFFSET_OCTAVE_START);
  println!();
  println!("Press Enter to exit...");
}

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
  let data: [u8; 3] = [status, input_note, velocity];
  let is_note_on: bool = midi::is_note_on(&data);
  let is_note_off: bool = midi::is_note_off(&data);
  let mut shifts: MutexGuard<'_, HashMap<u8, ShiftPress>> =
    ongoing_shifts().lock().unwrap();
  if is_note_on {
    let shift_value: i8 = input_note as i8
                          - OFFSET_ZERO_NOTE as i8;
    shifts.insert(input_note,
                  ShiftPress { shift_value });
  } else if is_note_off {
    shifts.remove(&input_note); }
  vec![] } // don't pass through offset control notes

fn handle_regular_note(
  status: u8,
  velocity: u8,
  original_note: u8
) -> Vec<Vec<u8>> {
  let data: [u8; 3] = [status, original_note, velocity];
  let is_note_on: bool = midi::is_note_on(&data);
  let is_note_off: bool = midi::is_note_off(&data);
  if is_note_on {
    // Update the persistent pitch class shift before transformation,
    // but only if shift keys are being held (we find a Some).
    if let Some(total_shift) = current_total_shift() {
      let pitch_class: u8 = original_note % 12;
      pitch_class_shifts().lock().unwrap()
        .insert(pitch_class, total_shift as i8); }}
  let (new_channel, new_note): (i16, i16) =
    edo12n_instruction(original_note);
  let output_in_range: bool = // what the MIDI standard allows
    new_channel >= 0 && new_channel <= 15 &&
    new_note >= 0 && new_note <= 127;
  let mut results: Vec<Vec<u8>> = vec![];
  let mut ongoing: MutexGuard<'_, HashMap<u8, TransformedNote>> =
    ongoing_notes().lock().unwrap();
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

fn edo12n_instruction(
  original_note: u8
) -> (i16, // channel
      i16) { // note
  let normalized: i16 = original_note as i16
                        - LOWEST_A as i16
                        + SHIFT_IN_12_EDO as i16;
  let channel_offset: i16 = normalized.div_euclid(12);
  let note_offset: i16 = normalized.rem_euclid(12);
  let channel: i16 = MIN_CHANNEL_OUT as i16 + channel_offset;
  let pitch_class: u8 = original_note % 12;
  let shift :  i16 =
    pitch_class_shifts() . lock() . unwrap()
    . get(&pitch_class) . copied()
    . unwrap_or(0) as i16;
  let note: i16 = MIN_NOTE_OUT as i16
                  + note_offset * EDO_OVER_12 as i16
                  + shift;
  (channel, note) }
