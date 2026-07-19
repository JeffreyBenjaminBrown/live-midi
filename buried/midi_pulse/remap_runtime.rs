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
mod scale;
mod state;
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
  let state: Arc<Mutex<state::RemappableEdoState>> =
    Arc::new(Mutex::new(state::RemappableEdoState::new(runtime_rig.clone())));
  let sounding: Arc<Mutex<state::SoundingPitchCounts>> =
    Arc::new(Mutex::new(state::SoundingPitchCounts::new(runtime_rig.edo)));
  let ongoing: Arc<Mutex<HashMap<u8, piano_transform::TransformedNote>>> =
    Arc::new(Mutex::new(HashMap::new()));
  let recorder: Arc<Mutex<record::RecordRuntime>> =
    Arc::new(Mutex::new(record::RecordRuntime::new()));

  let midi_in = MidiInput::new("edo_un12_piano_monome-in")?;
  let midi_out = MidiOutput::new("edo_un12_piano_monome-out")?;
  let conn_out = midi_out.create_virtual("out")?;
  let (tx, rx): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) = mpsc::channel();
  let _out_thread = thread::spawn(move || midi::run_output_thread(conn_out, rx));
  let output_gate = record::SharedOutputGate::new(tx.clone());

  let state_for_midi = Arc::clone(&state);
  let sounding_for_midi = Arc::clone(&sounding);
  let ongoing_for_midi = Arc::clone(&ongoing);
  let recorder_for_midi = Arc::clone(&recorder);
  let output_gate_for_midi = output_gate.clone();
  let _conn_in = midi_in.create_virtual(
    "in",
    move |_timestamp, message, _| {
      midi_runtime::update_sounding(message, &state_for_midi, &sounding_for_midi);
      {
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
        &ongoing_for_midi,
        |original_note| midi_runtime::edo_un12_instruction(original_note, &state_for_midi),
      );
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
      for msg in transformed {
        midi_runtime::print_note_on_trace(message, &msg);
        {
          let recorder = recorder_for_midi.lock().unwrap();
          record::trace_midi_event(
            "midi-output live",
            &msg,
            &recorder,
            std::time::Instant::now(),
          );
        }
        output_gate_for_midi.send_raw(msg);
      }
    },
    (),
  )?;

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
  let _ = playback_thread.join();
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

fn print_startup_message(rig: &rig::RemapRig) {
  println!("{}-EDO remappable un-12 runtime started", rig.edo);
  println!("Press Enter to exit...");
}
