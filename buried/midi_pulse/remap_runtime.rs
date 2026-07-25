use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiOutput};
use edo_surface::rig::{
  Rig, InitialMapRig, MonomeRig, MonomeWindowRig, PianoMappingRig,
  RecordControlKind, RemapIdiomRig, ScaleControlKind,
};
use edo_surface::midi;
use midi_pulse::piano_transform;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use std::{io, thread};

mod rig;
mod layout;
mod midi_runtime;
mod monome_runtime;
mod record;
mod remap;
mod render;
mod routing;
mod scale;
mod state;
mod sustain;
mod window_behavior;

const LOWEST_C: u8 = 24;
const MIN_CHANNEL_OUT: u8 = 1;
const MIN_NOTE_OUT: u8 = 28;

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

// Two keyboards, two port pairs. Pair 0 is "58-edo 1" in Reaper (the LEFT keyboard
// and the sustain pedal); pair 1 is "58-edo 2". One keyboard uses pair 0 alone --
// the second pair sits idle, which is harmless. Hardcoded like the original pair:
// the un-12 runtime does not read the rig's virtual_name (see the family readme).
const INPUT_CLIENT_NAMES: [&str; routing::NUM_KEYBOARDS] =
  ["edo_un12_piano_monome-in", "edo_un12_piano_monome-in-2"];
const OUTPUT_CLIENT_NAMES: [&str; routing::NUM_KEYBOARDS] =
  ["edo_un12_piano_monome-out", "edo_un12_piano_monome-out-2"];

/// Input i routes to output i normally, crossed while this is true. Set only at
/// startup (false) and by the 'l' identification gesture.
static SWAPPED: AtomicBool = AtomicBool::new(false);
/// True between 'l' Enter and the identifying note: the next note-on names the
/// LEFT keyboard's input port.
static IDENTIFYING: AtomicBool = AtomicBool::new(false);

pub fn run_from_rig(rig: &Rig) -> Result<(), Box<dyn std::error::Error>> {
  if rig.monome_windows.is_empty() {
    return Err("remap runtime requires at least one [[monome_windows]]".into());
  }
  let runtime_rig = runtime_rig(rig)?;
  let monome = remap_monome(rig)?;
  let listen_port = monome.map(|monome| monome.listen_port).unwrap_or(LISTEN_PORT);
  let prefix = monome
    .map(|monome| monome.prefix.clone())
    .unwrap_or_else(|| "/256-1-cable".to_string());
  let select_size = monome.and_then(|monome| monome.select.size);
  run(runtime_rig, listen_port, prefix, select_size)
}

fn remap_monome(rig: &Rig) -> Result<Option<&MonomeRig>, Box<dyn std::error::Error>> {
  let Some(monome_id) = rig.monome_windows.iter().find_map(|window| {
    if let MonomeWindowRig::RemappableUn12Grid { monome, .. } = window {
      Some(monome)
    } else {
      None
    }
  }) else {
    return Ok(None);
  };
  Ok(Some(
    rig
      .monomes
      .iter()
      .find(|monome| monome.id == *monome_id)
      .ok_or("remappable_un12_grid references unknown monome")?,
  ))
}

fn runtime_rig(rig: &Rig) -> Result<rig::RemapRig, Box<dyn std::error::Error>> {
  let Some(piano) = &rig.piano else {
    return Err("remappable_un12 runtime requires [piano.mapping]".into());
  };
  let PianoMappingRig::RemappableUn12 {
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
  if *initial_map != InitialMapRig::Even {
    return Err("remap runtime currently supports initial_map = \"even\"".into());
  }
  let tuning = rig
    .tunings
    .iter()
    .find(|candidate| candidate.id == *tuning)
    .ok_or("remappable_un12 references an unknown tuning")?;
  let remap_idiom = match remap_idiom {
    RemapIdiomRig::Loose => rig::RemapIdiom::Loose,
    RemapIdiomRig::Snap => rig::RemapIdiom::Snap,
  };
  let mut result = rig::RemapRig::new(
    tuning.fundamental_hz,
    tuning.edo,
    tuning.x_step,
    tuning.y_step,
    remap_idiom,
    DEFAULT_GRID_W,
    DEFAULT_GRID_H,
  );
  if let Some(grid_rect) = rig.monome_windows.iter().find_map(|window| {
    if let MonomeWindowRig::RemappableUn12Grid { rect, .. } = window {
      Some(rect)
    } else {
      None
    }
  }) {
    result = result.with_grid_size(grid_rect[2] - grid_rect[0] + 1, grid_rect[3] + 1);
  }
  result = result.with_record_controls(record_control_cells_from_rig(rig));
  result = result.with_scale_slots(scale_slots_rect_from_rig(rig));
  result = result.with_scale_controls(scale_controls_from_rig(rig));
  Ok(result)
}

fn record_control_cells_from_rig(
  rig: &Rig,
) -> Vec<(record::RecordControl, (i32, i32))> {
  rig
    .monome_windows
    .iter()
    .filter_map(|window| match window {
      MonomeWindowRig::RecordControl { rect, control, .. } => {
        Some((to_record_control(*control), (rect[0], rect[1])))
      }
      _ => None,
    })
    .collect()
}

fn scale_slots_rect_from_rig(rig: &Rig) -> Option<[i32; 4]> {
  rig.monome_windows.iter().find_map(|window| match window {
    MonomeWindowRig::ScaleSlots { rect, .. } => Some(*rect),
    _ => None,
  })
}

fn scale_controls_from_rig(
  rig: &Rig,
) -> Vec<(scale::ScaleControl, (i32, i32))> {
  rig
    .monome_windows
    .iter()
    .filter_map(|window| match window {
      MonomeWindowRig::ScaleControl { rect, control, .. } => {
        Some((to_scale_control(*control), (rect[0], rect[1])))
      }
      _ => None,
    })
    .collect()
}

fn to_scale_control(kind: ScaleControlKind) -> scale::ScaleControl {
  match kind {
    ScaleControlKind::Store => scale::ScaleControl::Store,
    ScaleControlKind::Empty => scale::ScaleControl::Empty,
  }
}

fn to_record_control(kind: RecordControlKind) -> record::RecordControl {
  match kind {
    RecordControlKind::Start => record::RecordControl::Start,
    RecordControlKind::Stop => record::RecordControl::Stop,
    RecordControlKind::Loop => record::RecordControl::Loop,
    RecordControlKind::Arm => record::RecordControl::Arm,
    RecordControlKind::EraseOns => record::RecordControl::EraseOns,
    RecordControlKind::EndAll => record::RecordControl::EndAll,
    RecordControlKind::Rscm => record::RecordControl::Rscm,
  }
}

fn run(
  runtime_rig: rig::RemapRig,
  listen_port: u16,
  prefix: String,
  select_size: Option<[i32; 2]>,
) -> Result<(), Box<dyn std::error::Error>> {
  STOP_REQUESTED.store(false, Ordering::Relaxed);
  SWAPPED.store(false, Ordering::Relaxed);
  IDENTIFYING.store(false, Ordering::Relaxed);
  let softstep_params = edo_surface::rig::load_softstep_params()?;
  let state: Arc<Mutex<state::RemappableEdoState>> =
    Arc::new(Mutex::new(state::RemappableEdoState::new(runtime_rig.clone())));
  let sounding: Arc<Mutex<state::SoundingPitchCounts>> =
    Arc::new(Mutex::new(state::SoundingPitchCounts::new(runtime_rig.edo)));
  let recorder: Arc<Mutex<record::RecordRuntime>> =
    Arc::new(Mutex::new(record::RecordRuntime::new()));

  // One output thread + gate per Reaper input. Gate 0 ("58-edo 1") is also the one
  // the recorder, the monome runtime, and the sustain pedal drive.
  let mut gates: Vec<record::SharedOutputGate> = Vec::new();
  for name in OUTPUT_CLIENT_NAMES {
    let midi_out = MidiOutput::new(name)?;
    let conn_out = midi_out.create_virtual("out")?;
    let (tx, rx): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) = mpsc::channel();
    let _out_thread = thread::spawn(move || midi::run_output_thread(conn_out, rx));
    gates.push(record::SharedOutputGate::new(tx));
  }
  let output_gate = gates[0].clone();

  let mut input_connections = Vec::new();
  for (source, name) in INPUT_CLIENT_NAMES.iter().enumerate() {
    let midi_in = MidiInput::new(name)?;
    let state_for_midi = Arc::clone(&state);
    let sounding_for_midi = Arc::clone(&sounding);
    let recorder_for_midi = Arc::clone(&recorder);
    let gates_for_midi = gates.clone();
    // Per input: its own held-note transform memory and note-off destination
    // memory. Two keyboards can hold the same note number at once; sharing either
    // map would cross one keyboard's note-offs onto the other's notes.
    let ongoing: Arc<Mutex<HashMap<u8, piano_transform::TransformedNote>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let mut router = routing::DestRouter::new();
    let conn_in = midi_in.create_virtual(
      "in",
      move |_timestamp, message, _| {
        if midi::is_note_on(message)
          && IDENTIFYING
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
          let swapped = routing::swapped_after_identify(source);
          SWAPPED.store(swapped, Ordering::Relaxed);
          println!(
            "left keyboard identified: {} -> 58-edo 1; the other -> 58-edo 2{}",
            INPUT_CLIENT_NAMES[source],
            if swapped { " (routing swapped)" } else { " (routing unchanged)" },
          );
        }
        let current = routing::dest_of_source(source, SWAPPED.load(Ordering::Relaxed));
        let dest = router.dest_for(message, current);
        midi_runtime::update_sounding(message, &state_for_midi, &sounding_for_midi);
        if dest == 0 {
          let recorder = recorder_for_midi.lock().unwrap();
          record::trace_midi_event("midi-input live", message, &recorder, std::time::Instant::now());
        }
        let original_note = if message.len() >= 3 && midi::is_note_event(message) {
          Some(message[1])
        } else {
          None
        };
        let transformed = piano_transform::transform_message(
          message,
          &ongoing,
          |original_note| midi_runtime::edo_un12_instruction(original_note, &state_for_midi),
        );
        // The recorder hears only the 58-edo 1 stream: playback replays through
        // gate 0, so recording the other keyboard would move its notes across.
        if dest == 0 {
          if let Some(original_note) = original_note {
            if let Some(event) = record::recorded_event_from_live_message(
              message,
              original_note,
              &transformed,
              &recorder_for_midi,
            ) {
              recorder_for_midi.lock().unwrap().record_live_event(event);
            }
          }
        }
        for msg in transformed {
          midi_runtime::print_note_on_trace(message, &msg, source, dest);
          if dest == 0 {
            let recorder = recorder_for_midi.lock().unwrap();
            record::trace_midi_event(
              "midi-output live",
              &msg,
              &recorder,
              std::time::Instant::now(),
            );
          }
          gates_for_midi[dest].send_raw(msg);
        }
      },
      (),
    )?;
    input_connections.push(conn_in);
  }

  let state_for_monome = Arc::clone(&state);
  let sounding_for_monome = Arc::clone(&sounding);
  let recorder_for_monome = Arc::clone(&recorder);
  let output_gate_for_monome = output_gate.clone();
  let monome_thread = thread::spawn(move || {
    monome_runtime::run_monome_thread(
      state_for_monome,
      sounding_for_monome,
      recorder_for_monome,
      output_gate_for_monome,
      listen_port,
      prefix,
      select_size,
    )
  });
  let recorder_for_playback = Arc::clone(&recorder);
  let state_for_playback = Arc::clone(&state);
  let output_gate_for_playback = output_gate.clone();
  let playback_thread = thread::spawn(move || {
    record::run_playback_thread(recorder_for_playback, state_for_playback, output_gate_for_playback)
  });

  install_sigint_handler();
  print_startup_message(&runtime_rig);

  // The question comes AFTER every port exists: an unanswered question must
  // never block the bring-up -- connect-midi.sh needs the ports, and only the
  // sustain thread waits on the answer. (Bitten for real 2026-07-25: with the
  // question first, the script ran against a portless runtime and connected
  // nothing.)
  let sustain_hardware = ask_sustain_hardware();
  let sustain_gates = gates.clone();
  let sustain_thread = thread::spawn(move || {
    sustain::run_sustain_thread(sustain_hardware, softstep_params, sustain_gates)
  });

  println!("Press Enter to exit...");
  let (quit_tx, quit_rx): (mpsc::Sender<()>, mpsc::Receiver<()>) = mpsc::channel();
  thread::spawn(move || {
    // Bare Enter (or EOF, or any other line) exits, as before; 'l' Enter starts
    // the left-keyboard identification instead of exiting.
    loop {
      let mut input = String::new();
      match io::stdin().read_line(&mut input) {
        Ok(n) if n > 0 && input.trim().eq_ignore_ascii_case("l") => {
          IDENTIFYING.store(true, Ordering::Relaxed);
          println!("play a note from the left keyboard to identify it");
        }
        _ => {
          let _ = quit_tx.send(());
          return;
        }
      }
    }
  });
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    if quit_rx.try_recv().is_ok() {
      break;
    }
    thread::sleep(Duration::from_millis(50));
  }
  STOP_REQUESTED.store(true, Ordering::Relaxed);
  let _ = monome_thread.join();
  let _ = playback_thread.join();
  let _ = sustain_thread.join();
  Ok(())
}

/// The startup question: which pedal hardware sustains this session. Asked after
/// the port bring-up (see the call site) so waiting on it blocks nothing but
/// sustain; EOF (running without a terminal) takes the EX-P default rather than
/// blocking.
fn ask_sustain_hardware() -> sustain::SustainHardware {
  println!("sustain pedals: EX-P via the MPC-20 bridge (e) or SoftStep pad 0 (s)? [e]");
  loop {
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
      Ok(n) if n > 0 => match sustain::parse_hardware_answer(&input) {
        Some(choice) => return choice,
        None => println!("type e (EX-P) or s (SoftStep), or bare Enter for EX-P"),
      },
      _ => return sustain::SustainHardware::ExP,
    }
  }
}

fn install_sigint_handler() {
  extern "C" fn handler(_: i32) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
  }
  unsafe {
    libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
  }
}

fn print_startup_message(rig: &rig::RemapRig) {
  println!("{}-EDO remappable un-12 runtime started", rig.edo);
  println!(
    "MIDI port pairs: {}/{} (58-edo 1, left) and {}/{} (58-edo 2, right)",
    INPUT_CLIENT_NAMES[0], OUTPUT_CLIENT_NAMES[0], INPUT_CLIENT_NAMES[1], OUTPUT_CLIENT_NAMES[1],
  );
  println!("Type 'l' then Enter to identify which keyboard is on the LEFT (-> 58-edo 1).");
}
