use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiOutput};
use midi_pulse::config::{self, Config, PianoMappingConfig};
use midi_pulse::mapping::PianoMapper;
use midi_pulse::piano_runtime::PianoRuntime;
use midi_pulse::midi;
use std::sync::mpsc;
use std::{io, thread};

#[path = "monome_edo_midi_runtime.rs"]
mod monome_edo_midi_runtime;

#[path = "sawwave/consts.rs"]
#[allow(dead_code)]
mod consts;
#[path = "sawwave/diagnostics.rs"]
#[allow(dead_code)]
mod diagnostics;
#[path = "sawwave/leds.rs"]
#[allow(dead_code)]
mod leds;
#[path = "sawwave/osc.rs"]
#[allow(dead_code)]
mod osc;
#[path = "sawwave/pitch.rs"]
#[allow(dead_code)]
mod pitch;
#[path = "sawwave/state.rs"]
#[allow(dead_code)]
mod state;
#[path = "sawwave/types.rs"]
#[allow(dead_code)]
mod types;
#[path = "sawwave/voices.rs"]
#[allow(dead_code)]
mod voices;
#[path = "sawwave/windows.rs"]
#[allow(dead_code)]
mod windows;
mod sawwave_runtime;
#[allow(dead_code)]
mod remap_runtime;

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let config_name = std::env::args().nth(1).ok_or_else(|| {
    "usage: cargo run --bin midi_pulse -- CONFIG_NAME".to_string()
  })?;
  let config = config::load_named_config(&config_name)?;
  print_startup(&config);
  if is_remap_config(&config) {
    remap_runtime::run_from_config(&config)?;
  } else if config.piano.is_some() {
    run_piano_runtime(&config)?;
  } else if is_monome_sawwave_config(&config) {
    sawwave_runtime::run_from_config(&config)?;
  } else if is_monome_midi_config(&config) {
    monome_edo_midi_runtime::run_from_config(&config)?;
  } else {
    println!("No runnable runtime path is implemented for this config yet; config validation complete.");
  }
  Ok(())
}

fn is_remap_config(config: &Config) -> bool {
  config.piano.as_ref().is_some_and(|piano| {
    matches!(
      piano.mapping,
      midi_pulse::config::PianoMappingConfig::RemappableUn12 { .. }
    )
  }) && config.monome_windows.iter().any(|window| {
    matches!(
      window,
      midi_pulse::config::MonomeWindowConfig::RemappableUn12Grid { .. }
    )
  })
}

fn is_monome_sawwave_config(config: &Config) -> bool {
  config
    .monome_windows
    .iter()
    .any(|window| matches!(window, midi_pulse::config::MonomeWindowConfig::EdoNoteGrid { .. }))
    && config
      .sinks
      .iter()
      .any(|sink| matches!(sink, midi_pulse::config::SinkConfig::CpalSawwave { .. }))
}

fn print_startup(config: &Config) {
  println!("Loaded midi_pulse config: {} ({})", config.id, config.title);
  println!("  tunings: {}", config.tunings.len());
  println!("  monomes: {}", config.monomes.len());
  println!("  sinks: {}", config.sinks.len());
  println!("  monome windows: {}", config.monome_windows.len());
  if let Some(piano) = &config.piano {
    match &piano.mapping {
      PianoMappingConfig::TwelveN { edo_per_12, .. } => {
        println!("  piano mapping: twelve_n ({edo_per_12} EDO steps per 12-EDO semitone)");
      }
      PianoMappingConfig::RemappableUn12 { tuning, remap_idiom, .. } => {
        println!("  piano mapping: remappable_un12 using tuning {tuning:?} ({remap_idiom:?})");
      }
    }
  }
}

fn is_monome_midi_config(config: &Config) -> bool {
  config
    .monome_windows
    .iter()
    .any(|window| matches!(window, midi_pulse::config::MonomeWindowConfig::EdoNoteGrid { .. }))
    && config.sinks.iter().any(|sink| matches!(sink, midi_pulse::config::SinkConfig::Midi { .. }))
}

fn run_piano_runtime(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  if !config.monomes.is_empty() || !config.monome_windows.is_empty() {
    eprintln!(
      "WARN: monome runtime wiring is still in progress; starting configured piano MIDI path only."
    );
  }
  let Some(midi_config) = &config.midi else {
    return Err("piano config requires [midi.output]".into());
  };
  let Some(input_config) = &midi_config.input else {
    return Err("piano config requires [midi.input]".into());
  };
  let Some(piano_config) = &config.piano else {
    return Ok(());
  };

  let mapper = PianoMapper::from_config(
    &piano_config.mapping,
    &config.tunings,
    midi_config.output.min_channel,
    midi_config.output.min_note,
  )?;
  let mut piano_runtime = PianoRuntime::new(mapper, piano_config.regions.clone());

  let midi_in = MidiInput::new(&input_config.virtual_name)?;
  let midi_out = MidiOutput::new(&midi_config.output.virtual_name)?;
  let conn_out = midi_out.create_virtual("out")?;
  let (tx, rx) = mpsc::channel();
  let _out_thread = thread::spawn(move || midi::run_output_thread(conn_out, rx));

  let _conn_in = midi_in.create_virtual(
    "in",
    move |_timestamp, message, _| {
      for message in piano_runtime.transform_message(message) {
        let _ = tx.send(message);
      }
    },
    (),
  )?;

  println!("Virtual ports created:");
  println!("  - '{}:in' (input)", input_config.virtual_name);
  println!("  - '{}:out' (output)", midi_config.output.virtual_name);
  println!("Press Enter to exit...");
  let mut input = String::new();
  io::stdin().read_line(&mut input)?;
  Ok(())
}
