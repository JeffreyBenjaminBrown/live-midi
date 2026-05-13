// VOCAB:
// The map maps the 'preimage' (all of 12-edo) to the 'image'
// (a 12-note subset of 31-edo).
// The terms 'image' and 'preimage'
// are sometimes used that way in the code.

use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midi_pulse::{midi, monome, piano_transform};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use std::{io, thread};

mod config;
mod layout;
mod midi_runtime;
mod monome_runtime;
mod remap;
mod render;
mod state;

#[cfg(test)]
mod tests;

const LOWEST_C: u8 = 24;
const MIN_CHANNEL_OUT: u8 = 1;
const MIN_NOTE_OUT: u8 = 28;

const PREFIX: &str = "/128-1-cable";
const LISTEN_PORT: u16 = 9000;
const LISTEN_PORT_ENV: &str = "EDO31_LISTEN_PORT";
const LED_TRACE_ENV: &str = "EDO31_LED_TRACE";
const DEFAULT_EDO: i16 = 31;
const DEFAULT_X_STEP: i16 = 6;
const DEFAULT_Y_STEP: i16 = 1;
const DEFAULT_LOWEST_HZ: f64 = 80.0;
const DEFAULT_GRID_W: i32 = 16;
const DEFAULT_GRID_H: i32 = 8;
const CONFIGS_DIR: &str = "code/rust/edo31_piano_monome/configs";
const MAP_W: i32 = 10;
const LED_LEVEL_OFF: u8 = 0;
const LED_LEVEL_IMAGE: u8 = 4;
const LED_LEVEL_FULL: u8 = 15;
const LED_LEVEL_UNDO: u8 = 8;
const MONOME_REFRESH: Duration = Duration::from_millis(1);

const ANCHOR_PITCH_CLASSES: [usize; 3] = [0, 5, 7];

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let config = config::parse_config()?;
  let listen_port = config::configured_listen_port()?;
  let state: Arc<Mutex<state::Edo31State>> =
    Arc::new(Mutex::new(state::Edo31State::new(config.clone())));
  let sounding: Arc<Mutex<state::SoundingState>> =
    Arc::new(Mutex::new(state::SoundingState::new(config.edo)));
  let ongoing: Arc<Mutex<HashMap<u8, piano_transform::TransformedNote>>> =
    Arc::new(Mutex::new(HashMap::new()));

  let midi_in: MidiInput = MidiInput::new("edo31_piano_monome-in")?;
  let midi_out: MidiOutput = MidiOutput::new("edo31_piano_monome-out")?;
  let conn_out: MidiOutputConnection = midi_out.create_virtual("out")?;
  let (tx, rx): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) = mpsc::channel();
  let _out_thread: thread::JoinHandle<()> =
    thread::spawn(move || midi::run_output_thread(conn_out, rx));

  let state_for_midi = Arc::clone(&state);
  let sounding_for_midi = Arc::clone(&sounding);
  let ongoing_for_midi = Arc::clone(&ongoing);
  let _conn_in: MidiInputConnection<()> = midi_in.create_virtual(
    "in",
    move |_timestamp: u64, message: &[u8], _: &mut ()| {
      midi_runtime::update_sounding(message, &state_for_midi, &sounding_for_midi);
      for msg in piano_transform::transform_message(
        message,
        &ongoing_for_midi,
        |original_note| midi_runtime::edo31_instruction(original_note, &state_for_midi),
      ) {
        midi_runtime::print_note_on_trace(message, &msg);
        let _ = tx.send(msg);
      }
    },
    (),
  )?;

  let state_for_monome = Arc::clone(&state);
  let sounding_for_monome = Arc::clone(&sounding);
  let monome_thread: thread::JoinHandle<()> = thread::spawn(move || {
    monome_runtime::run_monome_thread(state_for_monome, sounding_for_monome, listen_port)
  });

  install_sigint_handler();
  print_startup_message(&config);
  let (quit_tx, quit_rx): (mpsc::Sender<()>, mpsc::Receiver<()>) = mpsc::channel();
  thread::spawn(move || {
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
    let _ = quit_tx.send(());
  });
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    if quit_rx.try_recv().is_ok() {
      break;
    }
    thread::sleep(Duration::from_millis(50));
  }
  STOP_REQUESTED.store(true, Ordering::Relaxed);
  let _ = monome_thread.join();
  monome::black(PREFIX);
  Ok(())
}

fn install_sigint_handler() {
  extern "C" fn handler(_: i32) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
  }
  unsafe {
    libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
  }
}

fn print_startup_message(config: &config::EdoConfig) {
  println!("{}-EDO piano transformer with monome mapping started!", config.edo);
  println!();
  println!("Virtual ports created:");
  println!("  - 'edo31_piano_monome-in:in' (input)");
  println!("  - 'edo31_piano_monome-out:out' (output)");
  println!();
  println!("Monome {}-EDO map:", config.edo);
  println!("  - lowest Hz: {}", config.lowest_hz);
  println!("  - remap idiom: {}", config::remap_idiom_name(config.remap_idiom));
  println!(
    "  - each key's pitch is {}*x + {}*y mod {}",
    config.x_step, config.y_step, config.edo,
  );
  println!("  - initial map: {:?}", config.initial_map);
  println!("  - sounding pitches stay lit");
  println!("  - C, F and G flash as anchors");
  println!("  - other image pitches are steady dim");
  match config.remap_idiom {
    config::RemapIdiom::Loose => {
      println!("  - tap lit pitches to loosen them");
      println!("  - tap a dark pitch to move a neighboring loose pitch");
    }
    config::RemapIdiom::Snap => {
      println!("  - tap a dark pitch to snap the nearest image to it");
    }
  }
  println!();
  println!("Press Enter to exit...");
}
