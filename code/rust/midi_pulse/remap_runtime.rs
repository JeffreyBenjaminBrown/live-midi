use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiOutput};
use midi_pulse::config::{
  Config, InitialMapConfig, MonomeWindowConfig, PianoMappingConfig, RemapIdiomConfig,
};
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

const LOWEST_C: u8 = 24;
const MIN_CHANNEL_OUT: u8 = 1;
const MIN_NOTE_OUT: u8 = 28;

const PREFIX: &str = "/128-1-cable";
const LISTEN_PORT: u16 = 9000;
const LED_TRACE_ENV: &str = "MIDI_PULSE_REMAP_LED_TRACE";
const DEFAULT_GRID_W: i32 = 16;
const DEFAULT_GRID_H: i32 = 8;
const MAP_W: i32 = 10;

const PREIMAGE_ROW_Y: i32 = 0;
const LED_LEVEL_OFF: u8 = 0;
const LED_LEVEL_IMAGE: u8 = 4;
const LED_LEVEL_FULL: u8 = 15;
const LED_LEVEL_UNDO: u8 = 8;
const MONOME_REFRESH: Duration = Duration::from_millis(1);
const PREIMAGE_ROW_FLASH_MIN: Duration = Duration::from_millis(300);
const PREIMAGE_ROW_FLASH_WAVELENGTH: Duration = Duration::from_millis(120);
const PREIMAGE_ROW_FLASH_FRACTION_ON: f64 = 0.5;

const ANCHOR_PITCH_CLASSES: [usize; 3] = [0, 5, 7];
const WHITE_KEYS: [bool; 12] = [
  true,
  false,
  true,
  false,
  true,
  true,
  false,
  true,
  false,
  true,
  false,
  true,
];

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn run_from_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  let runtime_config = runtime_config(config)?;
  let listen_port = config
    .monomes
    .first()
    .map(|monome| monome.listen_port)
    .unwrap_or(LISTEN_PORT);
  run(runtime_config, listen_port)
}

fn runtime_config(config: &Config) -> Result<config::RemapConfig, Box<dyn std::error::Error>> {
  let Some(piano) = &config.piano else {
    return Err("remappable_un12 runtime requires [piano.mapping]".into());
  };
  let PianoMappingConfig::RemappableUn12 {
    lowest_note,
    tuning,
    remap_idiom,
    initial_map,
  } = &piano.mapping
  else {
    return Err("remap runtime requires kind = \"remappable_un12\"".into());
  };
  if *lowest_note != LOWEST_C {
    return Err(format!(
      "remap runtime currently requires lowest_note = {LOWEST_C}, got {lowest_note}"
    )
    .into());
  }
  if *initial_map != InitialMapConfig::Even {
    return Err("remap runtime currently supports initial_map = \"even\"".into());
  }
  let tuning = config
    .tunings
    .iter()
    .find(|candidate| candidate.id == *tuning)
    .ok_or("remappable_un12 references an unknown tuning")?;
  let remap_idiom = match remap_idiom {
    RemapIdiomConfig::Loose => config::RemapIdiom::Loose,
    RemapIdiomConfig::Snap => config::RemapIdiom::Snap,
  };
  let mut result = config::RemapConfig::new(
    tuning.fundamental_hz,
    tuning.edo,
    tuning.x_step,
    tuning.y_step,
    remap_idiom,
    DEFAULT_GRID_W,
    DEFAULT_GRID_H,
  );
  if let Some(grid_rect) = config.monome_windows.iter().find_map(|window| {
    if let MonomeWindowConfig::RemappableUn12Grid { rect, .. } = window {
      Some(rect)
    } else {
      None
    }
  }) {
    result = result.with_grid_size(grid_rect[2] - grid_rect[0] + 1, grid_rect[3] + 1);
  }
  Ok(result)
}

fn run(
  runtime_config: config::RemapConfig,
  listen_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
  STOP_REQUESTED.store(false, Ordering::Relaxed);
  let state: Arc<Mutex<state::RemappableEdoState>> =
    Arc::new(Mutex::new(state::RemappableEdoState::new(runtime_config.clone())));
  let sounding: Arc<Mutex<state::SoundingPitchCounts>> =
    Arc::new(Mutex::new(state::SoundingPitchCounts::new(runtime_config.edo)));
  let ongoing: Arc<Mutex<HashMap<u8, piano_transform::TransformedNote>>> =
    Arc::new(Mutex::new(HashMap::new()));

  let midi_in = MidiInput::new("edo_un12_piano_monome-in")?;
  let midi_out = MidiOutput::new("edo_un12_piano_monome-out")?;
  let conn_out = midi_out.create_virtual("out")?;
  let (tx, rx): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) = mpsc::channel();
  let _out_thread = thread::spawn(move || midi::run_output_thread(conn_out, rx));

  let state_for_midi = Arc::clone(&state);
  let sounding_for_midi = Arc::clone(&sounding);
  let ongoing_for_midi = Arc::clone(&ongoing);
  let _conn_in = midi_in.create_virtual(
    "in",
    move |_timestamp, message, _| {
      midi_runtime::update_sounding(message, &state_for_midi, &sounding_for_midi);
      for msg in piano_transform::transform_message(
        message,
        &ongoing_for_midi,
        |original_note| midi_runtime::edo_un12_instruction(original_note, &state_for_midi),
      ) {
        midi_runtime::print_note_on_trace(message, &msg);
        let _ = tx.send(msg);
      }
    },
    (),
  )?;

  let state_for_monome = Arc::clone(&state);
  let sounding_for_monome = Arc::clone(&sounding);
  let monome_thread = thread::spawn(move || {
    monome_runtime::run_monome_thread(state_for_monome, sounding_for_monome, listen_port)
  });

  install_sigint_handler();
  print_startup_message(&runtime_config);
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

fn print_startup_message(config: &config::RemapConfig) {
  println!("{}-EDO remappable un-12 runtime started", config.edo);
  println!("Press Enter to exit...");
}
