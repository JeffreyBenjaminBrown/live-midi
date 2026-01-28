//! Sampler - MIDI pass-through with recording and looping playback
//!
//! # How to run
//!
//! ```sh
//! cargo run --bin sampler
//! ```
//!
//! Creates two virtual MIDI output ports:
//! - "immediate-out": Pass-through for all normal notes
//! - "sample-out": Plays back recorded loop
//!
//! Special keys (not passed through):
//! - Bb7 (note 106): Stop - ends loop, silences hanging notes, stops recording if it's going
//! - B7 (note 107): Record - starts/stops recording
//! - C8 (note 108): Trigger - stops recording (if active) and starts looping

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use std::{io, thread};

const TOP_BFLAT: u8 = 106; // Bb7 - stop control
const TOP_B: u8 = 107; // B7 - record control
const TOP_C: u8 = 108; // C8 - trigger control
const LOOKBACK_MS: u64 = 50;
const TRIGGER_SLEEP_MS: u64 = 3;

struct TimestampedMessage {
  data: Vec<u8>,
  offset: Duration,
}

struct SamplerState {
  recording: bool,
  clip: Vec<TimestampedMessage>,
  record_start: Option<Instant>,
  last_normal_note: Option<(Instant, Vec<u8>)>,
}

impl SamplerState {
  fn new() -> Self {
    SamplerState {
      recording: false,
      clip: Vec::new(),
      record_start: None,
      last_normal_note: None,
    }
  }
}

enum Command {
  StartLoop,
  Stop,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let midi_in: MidiInput = MidiInput::new("sampler-in")?;
  let midi_out_immediate: MidiOutput = MidiOutput::new("sampler-immediate")?;
  let midi_out_sample: MidiOutput = MidiOutput::new("sampler-sample")?;

  let conn_immediate: MidiOutputConnection =
    midi_out_immediate.create_virtual("immediate-out")?;
  let conn_sample: MidiOutputConnection = midi_out_sample.create_virtual("sample-out")?;

  let state: Arc<Mutex<SamplerState>> = Arc::new(Mutex::new(SamplerState::new()));

  let (tx_immediate, rx_immediate): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) =
    mpsc::channel();
  let (tx_sample, rx_sample): (mpsc::Sender<Command>, mpsc::Receiver<Command>) =
    mpsc::channel();

  let playback_gen: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

  let _immediate_thread: thread::JoinHandle<()> =
    thread::spawn(move || run_immediate_thread(conn_immediate, rx_immediate));

  let state_for_sample: Arc<Mutex<SamplerState>> = Arc::clone(&state);
  let gen_for_sample: Arc<AtomicU64> = Arc::clone(&playback_gen);
  let _sample_thread: thread::JoinHandle<()> = thread::spawn(move || {
    run_sample_thread(conn_sample, rx_sample, state_for_sample, gen_for_sample)
  });

  let state_for_callback: Arc<Mutex<SamplerState>> = Arc::clone(&state);
  let gen_for_callback: Arc<AtomicU64> = Arc::clone(&playback_gen);

  let _conn_in: MidiInputConnection<()> = midi_in.create_virtual(
    "midi-in",
    move |_timestamp: u64, message: &[u8], _: &mut ()| {
      let data: Vec<u8> = message.to_vec();
      let note: Option<u8> = get_note(&data);
      let is_on: bool = is_note_on(&data);

      if let Some(n) = note {
        if n == TOP_BFLAT && is_on {
          handle_stop(&state_for_callback, &gen_for_callback, &tx_sample);
          return;
        }

        if n == TOP_B && is_on {
          let mut state: MutexGuard<SamplerState> = state_for_callback.lock().unwrap();
          handle_record_toggle(&mut state);
          return;
        }

        if n == TOP_C && is_on {
          handle_trigger(&state_for_callback, &gen_for_callback, &tx_sample);
          return;
        }
      }

      let mut state: MutexGuard<SamplerState> = state_for_callback.lock().unwrap();
      handle_normal_event(data, &mut state, &tx_immediate);
    },
    (),
  )?;

  print_startup_message();

  let mut input: String = String::new();
  io::stdin().read_line(&mut input)?;

  Ok(())
}

fn print_startup_message() {
  println!("Sampler started!");
  println!();
  println!("Virtual ports created:");
  println!("  - 'sampler-in:midi-in' (input)");
  println!("  - 'sampler-immediate:immediate-out' (pass-through)");
  println!("  - 'sampler-sample:sample-out' (loop playback)");
  println!();
  println!("Controls:");
  println!("  - Bb7 (note 106): Stop loop");
  println!("  - B7 (note 107): Start/stop recording");
  println!("  - C8 (note 108): Start loop (restarts if already playing)");
  println!();
  println!("Use 'aconnect -l' to see ports, 'aconnect <src> <dst>' to connect.");
  println!("Press Enter to exit...");
}

fn run_immediate_thread(
  mut conn: MidiOutputConnection,
  rx: mpsc::Receiver<Vec<u8>>)
  { while let Ok(data) = rx.recv()
      { let _ = conn.send(&data); }}

fn run_sample_thread(
  mut conn: MidiOutputConnection,
  rx: mpsc::Receiver<Command>,
  state: Arc<Mutex<SamplerState>>,
  gen: Arc<AtomicU64>,
) {
  while let Ok(cmd) = rx.recv() {
    match cmd {
      Command::StartLoop => {
        let my_gen: u64 = gen.load(Ordering::SeqCst);
        let clip: Vec<TimestampedMessage> = {
          let state: MutexGuard<SamplerState> = state.lock().unwrap();
          copy_clip(&state)
        };

        if clip.is_empty() {
          println!("[Sampler] No clip to play");
          continue;
        }

        play_loop(&clip, &mut conn, &gen, my_gen);
        println!("[Sampler] Loop stopped");
      }
      Command::Stop => {
        // Generation already incremented, loop will stop on its own
      }
    }
  }
}

fn play_loop(
  clip: &[TimestampedMessage],
  conn: &mut MidiOutputConnection,
  gen: &AtomicU64,
  my_gen: u64,
) {
  if clip.is_empty() {
    return;
  }

  // Calculate loop duration from last event
  let loop_duration: Duration = clip.last().map(|m| m.offset).unwrap_or(Duration::ZERO);

  let mut active_notes: HashSet<(u8, u8)> = HashSet::new();

  println!("[Sampler] Looping {} events (duration: {:?})", clip.len(), loop_duration);

  loop {
    let loop_start: Instant = Instant::now();

    for msg in clip.iter() {
      if gen.load(Ordering::SeqCst) != my_gen {
        send_all_notes_off(conn, &active_notes);
        return;
      }

      let target_time: Instant = loop_start + msg.offset;
      let now: Instant = Instant::now();
      if target_time > now {
        if interruptible_sleep(target_time - now, gen, my_gen) {
          send_all_notes_off(conn, &active_notes);
          return;
        }
      }

      // Track active notes
      if let (Some(note), Some(channel))
        = (get_note(&msg.data), get_channel(&msg.data))
        { if is_note_on(&msg.data) {
            active_notes.insert((channel, note));
          } else if is_note_off(&msg.data) {
            active_notes.remove(&(channel, note));
          }
        }

      let _ = conn.send(&msg.data);
    }

    // Wait for loop duration before repeating (if clip ends before loop_duration)
    let elapsed: Duration = loop_start.elapsed();
    if elapsed < loop_duration {
      if interruptible_sleep(loop_duration - elapsed, gen, my_gen) {
        send_all_notes_off(conn, &active_notes);
        return;
      }
    }
  }
}

fn copy_clip(state: &MutexGuard<SamplerState>) -> Vec<TimestampedMessage> {
  state
    .clip
    .iter()
    .map(|m| TimestampedMessage {
      data: m.data.clone(),
      offset: m.offset,
    })
    .collect()
}

fn interruptible_sleep(duration: Duration, gen: &AtomicU64, my_gen: u64) -> bool {
  let chunk: Duration = Duration::from_millis(TRIGGER_SLEEP_MS);
  let mut remaining: Duration = duration;
  while remaining > Duration::ZERO {
    if gen.load(Ordering::SeqCst) != my_gen {
      return true;
    }
    let to_sleep: Duration = remaining.min(chunk);
    thread::sleep(to_sleep);
    remaining = remaining.saturating_sub(to_sleep);
  }
  false
}

fn send_all_notes_off(conn: &mut MidiOutputConnection, active_notes: &HashSet<(u8, u8)>) {
  for &(channel, note) in active_notes.iter() {
    let note_off: [u8; 3] = [0x80 | channel, note, 0];
    let _ = conn.send(&note_off);
  }
}

fn handle_stop(
  state: &Arc<Mutex<SamplerState>>,
  gen: &AtomicU64,
  tx: &mpsc::Sender<Command>,
) {{ let mut state: MutexGuard<SamplerState> = state.lock().unwrap();
     if state.recording {
     stop_recording(&mut state);
     }}
  gen.fetch_add(1, Ordering::SeqCst);
  let _ = tx.send(Command::Stop);
  println!("[Sampler] Stop requested"); }

fn handle_record_toggle(state: &mut MutexGuard<SamplerState>) {
  if state.recording
  { stop_recording(state);
  } else { start_recording(state); }}

fn handle_trigger(
  state: &Arc<Mutex<SamplerState>>,
  gen: &AtomicU64,
  tx: &mpsc::Sender<Command>,
) {{ let mut state: MutexGuard<SamplerState> = state.lock().unwrap();
     if state.recording {
     stop_recording(&mut state);
     }}
  gen.fetch_add(1, Ordering::SeqCst);
  let _ = tx.send(Command::StartLoop); }

fn handle_normal_event(
  data: Vec<u8>,
  state: &mut MutexGuard<SamplerState>,
  tx_immediate: &mpsc::Sender<Vec<u8>>,
) {
  let _ = tx_immediate.send(data.clone());
  let now: Instant = Instant::now();
  if is_note_event(&data)
  { state.last_normal_note = Some((now,
                                   data.clone() )); }
  if state.recording {
    if let Some(start) = state.record_start {
      let offset: Duration = now.duration_since(start);
      state.clip.push(TimestampedMessage { data, offset }); }} }

fn stop_recording(state: &mut MutexGuard<SamplerState>) {
  state.recording = false;
  state.record_start = None;
  println!(
    "[Sampler] Recording stopped. {} events captured.",
    state.clip.len() ); }

fn start_recording(state: &mut MutexGuard<SamplerState>) {
  state.recording = true;
  state.clip.clear();
  let now: Instant = Instant::now();
  let last_note: Option<(Instant, Vec<u8>)> =
    state.last_normal_note.clone();
  if let Some((event_time, event_data)) = last_note {
    let elapsed: Duration = now.duration_since(event_time);
    if elapsed <= Duration::from_millis(LOOKBACK_MS) {
      state.record_start = Some(event_time);
      state.clip.push(TimestampedMessage {
        data: event_data,
        offset: Duration::ZERO, });
      println!(
        "[Sampler] Recording started (included note from {:?} ago)...",
        elapsed );
      return; }}
  state.record_start = Some(now);
  println!("[Sampler] Recording started..."); }

fn get_note(data: &[u8]) -> Option<u8> {
  if data.len() >= 2 && is_note_event(data) {
    Some(data[1])
  } else {
    None
  }
}

fn get_channel(data: &[u8]) -> Option<u8> {
  if !data.is_empty() {
    Some(data[0] & 0x0F)
  } else {
    None
  }
}

fn is_note_on(data: &[u8]) -> bool {
  if data.len() >= 3 {
    let status: u8 = data[0] & 0xF0;
    status == 0x90 && data[2] > 0
  } else {
    false
  }
}

fn is_note_off(data: &[u8]) -> bool {
  if data.len() >= 3 {
    let status: u8 = data[0] & 0xF0;
    // Note off, or note on with velocity 0
    status == 0x80 || (status == 0x90 && data[2] == 0)
  } else {
    false
  }
}

fn is_note_event(data: &[u8]) -> bool {
  if data.is_empty() {
    return false;
  }
  let status: u8 = data[0] & 0xF0;
  status == 0x80 || status == 0x90
}
