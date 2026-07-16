use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiOutput};
use midi_pulse::rig::{self, Rig, PianoMappingRig};
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
  let rig_name = std::env::args().nth(1).ok_or_else(|| {
    "usage: cargo run --bin midi_pulse -- RIG_NAME".to_string()
  })?;
  let rig = rig::load_named_rig(&rig_name)?;
  print_startup(&rig);
  if is_surfaces_rig(&rig) {
    // Two-plus EDO play grids and/or a KMSS drumkit composed in one rig. Leads the
    // dispatch: it is a superset of both the drumkit and sawwave predicates (it can
    // carry softstep windows AND edo grids), so it must be checked before them.
    surfaces_runtime::run_from_rig(&rig, Some(&rig_name))?;
  } else if is_drumkit_rig(&rig) {
    // A KMSS drumkit rig is unambiguous (it has softstep_windows and no
    // monome windows), so it can lead the remaining dispatch.
    drumkit_runtime::run_from_rig(&rig)?;
  } else if is_remap_rig(&rig) {
    remap_runtime::run_from_rig(&rig)?;
  } else if is_edo12n_monome_config(&rig) {
    edo12n_piano_monome_runtime::run_from_rig(&rig)?;
  } else if is_edo12n_display_config(&rig) {
    edo12n_piano_runtime::run_from_rig(&rig)?;
  } else if rig.piano.is_some() {
    run_piano_runtime(&rig)?;
  } else if is_looper_rig(&rig) {
    // Must precede the sawwave arm: a looper rig is a superset of the sawwave
    // predicate (it also has an edo_note_grid + cpal_synth sink).
    looper_runtime::run_from_rig(&rig)?;
  } else if is_monome_sawwave_rig(&rig) {
    sawwave_runtime::run_from_rig(&rig)?;
  } else if is_monome_midi_rig(&rig) {
    monome_edo_midi_runtime::run_from_rig(&rig)?;
  } else {
    println!("No runnable runtime path is implemented for this rig yet; rig validation complete.");
  }
  Ok(())
}

fn is_edo12n_display_config(rig: &Rig) -> bool {
  rig.piano.as_ref().is_some_and(|piano| {
    matches!(
      piano.mapping,
      midi_pulse::rig::PianoMappingRig::TwelveN { .. }
    )
  }) && rig.display.as_ref().is_some_and(|display| {
    matches!(
      display,
      midi_pulse::rig::DisplayRig::PitchClassGrid { enabled: true, .. }
    )
  })
}

fn is_edo12n_monome_config(rig: &Rig) -> bool {
  rig.piano.as_ref().is_some_and(|piano| {
    matches!(
      piano.mapping,
      midi_pulse::rig::PianoMappingRig::TwelveN { .. }
    )
  }) && rig.monome_windows.iter().any(|window| {
    matches!(
      window,
      midi_pulse::rig::MonomeWindowRig::TwelveEdoOffsetBoard { .. }
    )
  })
}

fn is_remap_rig(rig: &Rig) -> bool {
  rig.piano.as_ref().is_some_and(|piano| {
    matches!(
      piano.mapping,
      midi_pulse::rig::PianoMappingRig::RemappableUn12 { .. }
    )
  }) && rig.monome_windows.iter().any(|window| {
    matches!(
      window,
      midi_pulse::rig::MonomeWindowRig::RemappableUn12Grid { .. }
    )
  })
}

fn is_monome_sawwave_rig(rig: &Rig) -> bool {
  rig
    .monome_windows
    .iter()
    .any(|window| matches!(window, midi_pulse::rig::MonomeWindowRig::EdoNoteGrid { .. }))
    && rig
      .sinks
      .iter()
      .any(|sink| matches!(sink, midi_pulse::rig::SinkRig::CpalSynth { .. }))
}

/// A standalone KMSS drumkit rig declares at least one `softstep_window` and NO
/// monome windows. The "no monome windows" clause keeps a grids+drums rig (which
/// the surfaces predicate claims first) from falling into the drums-only arm.
fn is_drumkit_rig(rig: &Rig) -> bool {
  !rig.softstep_windows.is_empty() && rig.monome_windows.is_empty()
}

/// The surfaces runtime: at least one `edo_note_grid`, no `loop_display` (that is the
/// looper), and something that only surfaces composes -- a softstep window (drums), an
/// edo grid on more than one monome (the two play grids), or a `waveform_selector`.
/// Precise enough that every existing rig keeps its current runtime.
fn is_surfaces_rig(rig: &Rig) -> bool {
  use midi_pulse::rig::MonomeWindowRig;
  let has_edo_grid = rig
    .monome_windows
    .iter()
    .any(|w| matches!(w, MonomeWindowRig::EdoNoteGrid { .. }));
  if !has_edo_grid {
    return false;
  }
  let has_loop_display = rig
    .monome_windows
    .iter()
    .any(|w| matches!(w, MonomeWindowRig::LoopDisplay { .. }));
  if has_loop_display {
    return false;
  }
  let distinct_grid_monomes: std::collections::HashSet<&str> = rig
    .monome_windows
    .iter()
    .filter_map(|w| match w {
      MonomeWindowRig::EdoNoteGrid { monome, .. } => Some(monome.as_str()),
      _ => None,
    })
    .collect();
  let has_selector = rig
    .monome_windows
    .iter()
    .any(|w| matches!(w, MonomeWindowRig::WaveformSelector { .. }));
  !rig.softstep_windows.is_empty() || distinct_grid_monomes.len() > 1 || has_selector
}

/// Keyed on the looper-only `loop_display` kind, so it is strictly more specific
/// than the sawwave predicate. Dispatch order makes this matter (see `main`).
fn is_looper_rig(rig: &Rig) -> bool {
  rig
    .monome_windows
    .iter()
    .any(|window| matches!(window, midi_pulse::rig::MonomeWindowRig::LoopDisplay { .. }))
}

fn print_startup(rig: &Rig) {
  println!("Loaded midi_pulse rig: {} ({})", rig.id, rig.title);
  println!("  tunings: {}", rig.tunings.len());
  println!("  monomes: {}", rig.monomes.len());
  println!("  sinks: {}", rig.sinks.len());
  println!("  monome windows: {}", rig.monome_windows.len());
  if !rig.softsteps.is_empty() || !rig.softstep_windows.is_empty() {
    println!("  softsteps: {}", rig.softsteps.len());
    println!("  softstep windows: {}", rig.softstep_windows.len());
  }
  if let Some(piano) = &rig.piano {
    match &piano.mapping {
      PianoMappingRig::TwelveN { edo_per_12, .. } => {
        println!("  piano mapping: twelve_n ({edo_per_12} EDO steps per 12-EDO semitone)");
      }
      PianoMappingRig::RemappableUn12 { tuning, remap_idiom, .. } => {
        println!("  piano mapping: remappable_un12 using tuning {tuning:?} ({remap_idiom:?})");
      }
    }
  }
}

fn is_monome_midi_rig(rig: &Rig) -> bool {
  rig
    .monome_windows
    .iter()
    .any(|window| matches!(window, midi_pulse::rig::MonomeWindowRig::EdoNoteGrid { .. }))
    && rig.sinks.iter().any(|sink| matches!(sink, midi_pulse::rig::SinkRig::Midi { .. }))
}

fn run_piano_runtime(rig: &Rig) -> Result<(), Box<dyn std::error::Error>> {
  if !rig.monomes.is_empty() || !rig.monome_windows.is_empty() {
    eprintln!(
      "WARN: monome runtime wiring is still in progress; starting configured piano MIDI path only."
    );
  }
  let Some(midi_rig) = &rig.midi else {
    return Err("piano rig requires [midi.output]".into());
  };
  let Some(input_rig) = &midi_rig.input else {
    return Err("piano rig requires [midi.input]".into());
  };
  let Some(piano_rig) = &rig.piano else {
    return Ok(());
  };

  let mapper = PianoMapper::from_rig(
    &piano_rig.mapping,
    &rig.tunings,
    midi_rig.output.min_channel,
    midi_rig.output.min_note,
  )?;
  let mut piano_runtime = PianoRuntime::new(mapper, piano_rig.regions.clone());

  let midi_in = MidiInput::new(&input_rig.virtual_name)?;
  let midi_out = MidiOutput::new(&midi_rig.output.virtual_name)?;
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
  println!("  - '{}:in' (input)", input_rig.virtual_name);
  println!("  - '{}:out' (output)", midi_rig.output.virtual_name);
  println!("Press Enter to exit...");
  let mut input = String::new();
  io::stdin().read_line(&mut input)?;
  Ok(())
}

#[cfg(test)]
mod dispatch_tests {
  //! The dispatch predicates decide which runtime a rig gets. Adding the surfaces
  //! arm must not change any existing rig's runtime, so we assert the routing of
  //! each real rig family by name.
  use super::*;
  use midi_pulse::rig::load_named_rig;

  #[test]
  fn surfaces_rig_routes_to_surfaces_only() {
    let c = load_named_rig("2-monomes_kmss-drums").expect("loads");
    assert!(is_surfaces_rig(&c), "grids+drums is a surfaces rig");
    // It must NOT fall into the drums-only arm (it has monome windows) or the looper /
    // sawwave arms (surfaces is checked first, but assert the tightened predicate too).
    assert!(!is_drumkit_rig(&c), "grids+drums is not the drums-only arm");
    assert!(!is_looper_rig(&c), "no loop_display");
  }

  #[test]
  fn drumkit_rig_still_routes_to_drumkit() {
    let c = load_named_rig("kmss-drumkit").expect("loads");
    assert!(is_drumkit_rig(&c), "a pure drumkit still matches the drums arm");
    assert!(!is_surfaces_rig(&c), "no edo_note_grid -> not surfaces");
  }

  #[test]
  fn looper_rig_is_not_surfaces() {
    let c = load_named_rig("monome-looper-58-8-1").expect("loads");
    assert!(is_looper_rig(&c), "loop_display -> looper");
    assert!(!is_surfaces_rig(&c), "a looper is excluded by its loop_display");
  }

  #[test]
  fn sawwave_rig_is_not_surfaces() {
    let c = load_named_rig("monome-edo-sawwave").expect("loads");
    assert!(is_monome_sawwave_rig(&c), "single grid + cpal_synth -> sawwave");
    assert!(!is_surfaces_rig(&c), "one grid, no drums, no selector -> not surfaces");
    assert!(!is_drumkit_rig(&c), "no softstep windows");
  }
}
