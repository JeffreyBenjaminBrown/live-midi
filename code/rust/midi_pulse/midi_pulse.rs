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
#[path = "edo12n_piano_monome_runtime.rs"]
mod edo12n_piano_monome_runtime;
#[path = "edo12n_piano_runtime.rs"]
mod edo12n_piano_runtime;

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
mod looper_runtime;
mod drumkit_runtime;
mod surfaces_runtime;

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let config_name = std::env::args().nth(1).ok_or_else(|| {
    "usage: cargo run --bin midi_pulse -- CONFIG_NAME".to_string()
  })?;
  let config = config::load_named_config(&config_name)?;
  print_startup(&config);
  if is_surfaces_config(&config) {
    // Two-plus EDO play grids and/or a KMSS drumkit composed in one config. Leads the
    // dispatch: it is a superset of both the drumkit and sawwave predicates (it can
    // carry softstep windows AND edo grids), so it must be checked before them.
    surfaces_runtime::run_from_config(&config)?;
  } else if is_drumkit_config(&config) {
    // A KMSS drumkit config is unambiguous (it has softstep_windows and no
    // monome windows), so it can lead the remaining dispatch.
    drumkit_runtime::run_from_config(&config)?;
  } else if is_remap_config(&config) {
    remap_runtime::run_from_config(&config)?;
  } else if is_edo12n_monome_config(&config) {
    edo12n_piano_monome_runtime::run_from_config(&config)?;
  } else if is_edo12n_display_config(&config) {
    edo12n_piano_runtime::run_from_config(&config)?;
  } else if config.piano.is_some() {
    run_piano_runtime(&config)?;
  } else if is_looper_config(&config) {
    // Must precede the sawwave arm: a looper config is a superset of the sawwave
    // predicate (it also has an edo_note_grid + cpal_synth sink).
    looper_runtime::run_from_config(&config)?;
  } else if is_monome_sawwave_config(&config) {
    sawwave_runtime::run_from_config(&config)?;
  } else if is_monome_midi_config(&config) {
    monome_edo_midi_runtime::run_from_config(&config)?;
  } else {
    println!("No runnable runtime path is implemented for this config yet; config validation complete.");
  }
  Ok(())
}

fn is_edo12n_display_config(config: &Config) -> bool {
  config.piano.as_ref().is_some_and(|piano| {
    matches!(
      piano.mapping,
      midi_pulse::config::PianoMappingConfig::TwelveN { .. }
    )
  }) && config.display.as_ref().is_some_and(|display| {
    matches!(
      display,
      midi_pulse::config::DisplayConfig::PitchClassGrid { enabled: true, .. }
    )
  })
}

fn is_edo12n_monome_config(config: &Config) -> bool {
  config.piano.as_ref().is_some_and(|piano| {
    matches!(
      piano.mapping,
      midi_pulse::config::PianoMappingConfig::TwelveN { .. }
    )
  }) && config.monome_windows.iter().any(|window| {
    matches!(
      window,
      midi_pulse::config::MonomeWindowConfig::TwelveEdoOffsetBoard { .. }
    )
  })
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
      .any(|sink| matches!(sink, midi_pulse::config::SinkConfig::CpalSynth { .. }))
}

/// A standalone KMSS drumkit config declares at least one `softstep_window` and NO
/// monome windows. The "no monome windows" clause keeps a grids+drums config (which
/// the surfaces predicate claims first) from falling into the drums-only arm.
fn is_drumkit_config(config: &Config) -> bool {
  !config.softstep_windows.is_empty() && config.monome_windows.is_empty()
}

/// The surfaces runtime: at least one `edo_note_grid`, no `loop_display` (that is the
/// looper), and something that only surfaces composes -- a softstep window (drums), an
/// edo grid on more than one monome (the two play grids), or a `waveform_selector`.
/// Precise enough that every existing config keeps its current runtime.
fn is_surfaces_config(config: &Config) -> bool {
  use midi_pulse::config::MonomeWindowConfig;
  let has_edo_grid = config
    .monome_windows
    .iter()
    .any(|w| matches!(w, MonomeWindowConfig::EdoNoteGrid { .. }));
  if !has_edo_grid {
    return false;
  }
  let has_loop_display = config
    .monome_windows
    .iter()
    .any(|w| matches!(w, MonomeWindowConfig::LoopDisplay { .. }));
  if has_loop_display {
    return false;
  }
  let distinct_grid_monomes: std::collections::HashSet<&str> = config
    .monome_windows
    .iter()
    .filter_map(|w| match w {
      MonomeWindowConfig::EdoNoteGrid { monome, .. } => Some(monome.as_str()),
      _ => None,
    })
    .collect();
  let has_selector = config
    .monome_windows
    .iter()
    .any(|w| matches!(w, MonomeWindowConfig::WaveformSelector { .. }));
  !config.softstep_windows.is_empty() || distinct_grid_monomes.len() > 1 || has_selector
}

/// Keyed on the looper-only `loop_display` kind, so it is strictly more specific
/// than the sawwave predicate. Dispatch order makes this matter (see `main`).
fn is_looper_config(config: &Config) -> bool {
  config
    .monome_windows
    .iter()
    .any(|window| matches!(window, midi_pulse::config::MonomeWindowConfig::LoopDisplay { .. }))
}

fn print_startup(config: &Config) {
  println!("Loaded midi_pulse config: {} ({})", config.id, config.title);
  println!("  tunings: {}", config.tunings.len());
  println!("  monomes: {}", config.monomes.len());
  println!("  sinks: {}", config.sinks.len());
  println!("  monome windows: {}", config.monome_windows.len());
  if !config.softsteps.is_empty() || !config.softstep_windows.is_empty() {
    println!("  softsteps: {}", config.softsteps.len());
    println!("  softstep windows: {}", config.softstep_windows.len());
  }
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

#[cfg(test)]
mod dispatch_tests {
  //! The dispatch predicates decide which runtime a config gets. Adding the surfaces
  //! arm must not change any existing config's runtime, so we assert the routing of
  //! each real config family by name.
  use super::*;
  use midi_pulse::config::load_named_config;

  #[test]
  fn surfaces_config_routes_to_surfaces_only() {
    let c = load_named_config("2-monomes_58-8-1_kmss-drums").expect("loads");
    assert!(is_surfaces_config(&c), "grids+drums is a surfaces config");
    // It must NOT fall into the drums-only arm (it has monome windows) or the looper /
    // sawwave arms (surfaces is checked first, but assert the tightened predicate too).
    assert!(!is_drumkit_config(&c), "grids+drums is not the drums-only arm");
    assert!(!is_looper_config(&c), "no loop_display");
  }

  #[test]
  fn drumkit_config_still_routes_to_drumkit() {
    let c = load_named_config("kmss-drumkit").expect("loads");
    assert!(is_drumkit_config(&c), "a pure drumkit still matches the drums arm");
    assert!(!is_surfaces_config(&c), "no edo_note_grid -> not surfaces");
  }

  #[test]
  fn looper_config_is_not_surfaces() {
    let c = load_named_config("monome-looper-58-8-1").expect("loads");
    assert!(is_looper_config(&c), "loop_display -> looper");
    assert!(!is_surfaces_config(&c), "a looper is excluded by its loop_display");
  }

  #[test]
  fn sawwave_config_is_not_surfaces() {
    let c = load_named_config("monome-edo-sawwave").expect("loads");
    assert!(is_monome_sawwave_config(&c), "single grid + cpal_synth -> sawwave");
    assert!(!is_surfaces_config(&c), "one grid, no drums, no selector -> not surfaces");
    assert!(!is_drumkit_config(&c), "no softstep windows");
  }
}
