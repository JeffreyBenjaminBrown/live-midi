/// See the README.
/// Maybe tweak the 'CONST's defined below.

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::io::Write;
use std::time::{Duration, Instant};
use std::{io, thread};

const SHIFT_IN_12_EDO : i8 = -5;  // Added to the MIDI note before processing.
const LOWEST_A        : u8 = 21;  // A0, lowest note on 88-key piano
const MIN_CHANNEL_OUT : u8 = 1;   // adjust for whatever the synth wants
const MIN_NOTE_OUT    : u8 = 28;  // could also be adjusted for the synth. I like to adjust the synth for this instead, though, because 28 = (128 - 72) / 2 puts the notes closest to the middle of the range [0,127], which makes future MIDI edits less constrained -- plenty of room to adjust up or down in either direction without switching channels.
const EDO_OVER_12     : u8 = 6;   // 72 / 12 = 6
const OFFSET_OCTAVE_START : u8 = 97;  // C#7 - first note of offset control octave (top 12 keys)
const OFFSET_ZERO_NOTE    : u8 = 102; // F#7 - this note means offset = 0
const FLASH_MILLIS : u64 = 500; // how long a note-on flash lasts
const TICK_MILLIS  : u64 = 50;  // redraw interval during flash animation
const GRID_ROWS : usize = 6;  // strings; vertical extent of grid
const GRID_COLS : usize = 7;  // frets; horizontal extent of grid
const GRID_ANCHOR   : usize = 4; // pitch class of bottom-left cell (4 = E)
const GRID_ROW_STEP : usize = 5; // semitones between adjacent strings (5 = perfect fourth)
const WHITE_KEYS : [bool; 12] = [
  true, false, true, // C, C#, D
  false, true, true, // D#, E, F, etc.
  false, true, false, true, false, true, ];

struct TransformedNote {
  output_channel: u8,
  output_note: u8, }

struct ShiftPress {
  input_note: u8,
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

fn pitch_class_shifts(
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

fn draw_grid(
  flash_deadline: &[Option<Instant>; 12],
  phase_white: bool,
) {
  let now: Instant = Instant::now();
  let shifts: MutexGuard<'_, HashMap<u8, i8>> =
    pitch_class_shifts().lock().unwrap();
  let mut buf: String = String::new();
  for row in (0..GRID_ROWS).rev() {
    for col in 0..GRID_COLS {
      let pc: u8 =
        ((GRID_ANCHOR + GRID_ROW_STEP * row + col) % 12) as u8;
      let flashing: bool = flash_deadline [pc as usize]
                           . map_or(false, |d: Instant| d > now);
      let show_white: bool = if flashing { phase_white }
                             else { WHITE_KEYS[pc as usize] };
      let block: &str = if show_white { "██" } else { "  " };
      let shift: i8 = shifts.get(&pc).copied().unwrap_or(0);
      buf.push_str(&format!("{}{:2}", block, shift)); }
    buf.push('\n'); }
  buf.push_str(&format!("\x1b[{}A", GRID_ROWS));
  io::stdout().write_all(buf.as_bytes());
  io::stdout().flush(); }

fn run_display_thread(rx: mpsc::Receiver<u8>) {
  let mut flash_deadline: [Option<Instant>; 12] =
    [None; 12];
  let mut phase_white: bool = true;
  let flash_dur: Duration = Duration::from_millis(FLASH_MILLIS);
  let tick: Duration = Duration::from_millis(TICK_MILLIS);
  draw_grid(&flash_deadline, phase_white);
  loop { // Drain all pending events
    while let Ok(pc) = rx.try_recv() {
      flash_deadline[pc as usize] =
        Some(Instant::now() + flash_dur); }
    let any_active: bool = flash_deadline.iter().any(
      |d: &Option<Instant>| d.map_or(false,
        |d: Instant| d > Instant::now()));
    if any_active {
      phase_white = !phase_white;
      let now: Instant = Instant::now();
      for d in flash_deadline.iter_mut() {
        if d.map_or(false, |deadline: Instant| deadline <= now)
          { *d = None; }}
      draw_grid(&flash_deadline, phase_white);
      thread::sleep(tick);
    } else {
      draw_grid(&flash_deadline, true); // Final resting redraw
      match rx.recv() { // Block until next note (zero CPU)
        Ok(pc) => {
          flash_deadline[pc as usize] =
            Some(Instant::now() + flash_dur); }
        Err(_) => break, }} }}

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
  let (display_tx, display_rx): (mpsc::Sender<u8>,
                                 mpsc::Receiver<u8>) = mpsc::channel();
  let _conn_in: MidiInputConnection<()> =
    midi_in.create_virtual(
      "in",
      move |_timestamp: u64, message: &[u8], _: &mut ()| {
        for msg in transform_message(message) {
          let _ = tx.send(msg); }
        let status: u8 = message[0] & 0xF0;
        if message.len() >= 3 && status == 0x90
           && message[2] > 0 && message[1] < OFFSET_OCTAVE_START {
          let _ = display_tx.send(message[1] % 12);
        }},
      () )?;
  print_startup_message();
  let _display_thread: thread::JoinHandle<()> =
    thread::spawn(move || {
      run_display_thread(display_rx); });
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
  println!("  - min_channel: {}", MIN_CHANNEL_OUT);
  println!("  - min_midi_note: {}", MIN_NOTE_OUT);
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
  let mut shifts: MutexGuard<'_, HashMap<u8, ShiftPress>> =
    ongoing_shifts().lock().unwrap();
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

fn edo72_instruction(
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
