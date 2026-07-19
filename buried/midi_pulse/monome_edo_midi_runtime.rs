use midir::os::unix::VirtualOutput;
use midir::MidiOutput;
use edo_surface::rig::{Rig, MonomeWindowRig};
use edo_surface::{midi, monome};
use rosc::{decoder, OscPacket, OscType};
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const PREFIX: &str = "/256-1-cable";
const LISTEN_PORT: u16 = 9000;
const LISTEN_PORT_ENV: &str = "MONOME_EDO_MIDI_LISTEN_PORT";
const DEVICE_PORT_ENV: &str = "MONOME_EDO_MIDI_DEVICE_PORT";
const GRID_W: i32 = 16;
const GRID_H: i32 = 16;

const DEFAULT_EDO: i32 = 46;
const DEFAULT_X_STEP: i32 = 9;
const DEFAULT_Y_STEP: i32 = 1;
const DEFAULT_VELOCITY: u8 = 96;
const MIN_CHANNEL_OUT: i32 = 1;
const MIN_NOTE_OUT: i32 = 28;

const N_CHORDS: usize = 16;
const SILENCE_CHORD: usize = 0;
const INITIALLY_ACCRETING_CHORD: usize = 1;
const CELL_WIPE: Cell = (0, 14);
const CELL_ACCRETE_ON: Cell = (1, 14);
const CELL_EMIT_IS_TOGGLE: Cell = (2, 14);
const CELL_SET_ACCRETION_TARGET: Cell = (3, 14);
const CONTROLS_TOP_RECT: Rect = ((0, 14), (3, 14));
const CONTROLS_BOTTOM_RECT: Rect = ((0, 15), (15, 15));
const EDO_RECT: Rect = ((0, 0), (15, 15));
const FLASH_PHASE_MS: u128 = 150;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

type Cell = (i32, i32);
type Rect = (Cell, Cell);
type ChordId = usize;
type VoiceId = u64;
type Chord = HashMap<i32, HashSet<VoiceId>>;
type MidiNote = (u8, u8);
type LedCmd = (WindowId, Cell, Brightness);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowId {
  Edo,
  ControlsTop,
  ControlsBottom,
}

#[derive(Clone, Copy)]
struct Window {
  id: WindowId,
  rect: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Brightness {
  Off,
  Dim,
  Bright,
}

#[derive(Clone, Copy, Debug)]
enum Button {
  Toggle { state: bool, on: ButtonAction, off: ButtonAction },
  Fire { fire: ButtonAction },
}

#[derive(Clone, Copy, Debug)]
enum ButtonAction {
  AccreteOn,
  AccreteOff,
  WipeFire,
  EmitIsToggleOn,
  EmitIsToggleOff,
  SetTargetFire,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PitchLedReason {
  PitchEquivalent { source_xy: Cell },
  Chord { chord: ChordId, pitch: i32 },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Source {
  Fingered { xy: Cell },
  Accreted { chord: ChordId, pitch: i32 },
}

#[derive(Clone, Copy, Debug)]
struct FingerState {
  id: VoiceId,
  pitch: i32,
}

struct PitchClass {
  key_to_pitch: HashMap<Cell, i32>,
  key_to_pitchclass: HashMap<Cell, i32>,
  pitchclass_to_keys: HashMap<i32, Vec<Cell>>,
}

struct AppState {
  chords: Vec<Chord>,
  accretion_target: ChordId,
  emitting_chord: Option<ChordId>,
  pressed_chords: Vec<ChordId>,
  target_select_mode: bool,
  accrete_on: bool,
  emit_is_toggle: bool,
  next_voice_id: VoiceId,
  pitchled_reasons: HashMap<Cell, HashSet<PitchLedReason>>,
  control_buttons: HashMap<Cell, Button>,
  pitch_class: PitchClass,
  edo: i32,
  active_fingers: HashMap<Cell, FingerState>,
  active_accretions: HashMap<(ChordId, i32), VoiceId>,
  source_outputs: HashMap<Source, MidiNote>,
  output_counts: HashMap<MidiNote, usize>,
  midi_tx: mpsc::Sender<Vec<u8>>,
  velocity: u8,
  min_channel: i32,
  min_note: i32,
}

pub fn run_from_rig(rig: &Rig) -> Result<(), Box<dyn std::error::Error>> {
  let edo_window = rig.monome_windows.iter().find_map(|window| {
    if let MonomeWindowRig::EdoNoteGrid { monome, tuning, sink, .. } = window {
      Some((monome, tuning, sink))
    } else {
      None
    }
  }).ok_or("monome MIDI rig requires an edo_note_grid window")?;
  let monome_rig = rig.monomes.iter()
    .find(|monome| monome.id == *edo_window.0)
    .ok_or("edo_note_grid references an unknown monome")?;
  let tuning = rig.tunings.iter()
    .find(|tuning| tuning.id == *edo_window.1)
    .ok_or("edo_note_grid references an unknown tuning")?;
  if !rig.sinks.iter().any(|sink| sink.id() == edo_window.2.as_str()) {
    return Err("edo_note_grid references an unknown sink".into());
  }
  let midi_output = rig.midi.as_ref()
    .ok_or("monome MIDI rig requires [midi.output]")?
    .output
    .clone();
  let grid_size = monome_rig.select.size.unwrap_or([GRID_W, GRID_H]);
  run(RuntimeSettings {
    prefix: monome_rig.prefix.clone(),
    listen_port: monome_rig.listen_port,
    grid_w: grid_size[0],
    grid_h: grid_size[1],
    edo: tuning.edo as i32,
    x_step: tuning.x_step as i32,
    y_step: tuning.y_step as i32,
    velocity: DEFAULT_VELOCITY,
    min_channel: midi_output.min_channel as i32,
    min_note: midi_output.min_note as i32,
    midi_output_name: midi_output.virtual_name,
  })
}

#[allow(dead_code)]
pub fn run_default() -> Result<(), Box<dyn std::error::Error>> {
  let args: Vec<String> = std::env::args().collect();
  let edo = parse_i32_arg(&args, 1, "edo", DEFAULT_EDO);
  let x_step = parse_i32_arg(&args, 2, "x_step", DEFAULT_X_STEP);
  let y_step = parse_i32_arg(&args, 3, "y_step", DEFAULT_Y_STEP);
  let listen_port = std::env::var(LISTEN_PORT_ENV)
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(LISTEN_PORT);
  run(RuntimeSettings {
    prefix: PREFIX.to_string(),
    listen_port,
    grid_w: GRID_W,
    grid_h: GRID_H,
    edo,
    x_step,
    y_step,
    velocity: DEFAULT_VELOCITY,
    min_channel: MIN_CHANNEL_OUT,
    min_note: MIN_NOTE_OUT,
    midi_output_name: "monome_edo_midi-out".to_string(),
  })
}

struct RuntimeSettings {
  prefix: String,
  listen_port: u16,
  grid_w: i32,
  grid_h: i32,
  edo: i32,
  x_step: i32,
  y_step: i32,
  velocity: u8,
  min_channel: i32,
  min_note: i32,
  midi_output_name: String,
}

fn run(settings: RuntimeSettings) -> Result<(), Box<dyn std::error::Error>> {
  let RuntimeSettings {
    prefix,
    listen_port,
    grid_w,
    grid_h,
    edo,
    x_step,
    y_step,
    velocity,
    min_channel,
    min_note,
    midi_output_name,
  } = settings;
  assert!(edo > 0, "edo must be positive, got {edo}");

  let midi_out = MidiOutput::new(&midi_output_name)?;
  let conn_out = midi_out.create_virtual("out")?;
  let (midi_tx, midi_rx) = mpsc::channel();
  let _out_thread = thread::spawn(move || midi::run_output_thread(conn_out, midi_rx));

  let sock = UdpSocket::bind(("0.0.0.0", listen_port))
    .unwrap_or_else(|e| panic!("bind UDP :{listen_port}: {e}"));
  sock.set_read_timeout(Some(Duration::from_millis(50)))?;

  let mut device_port = match std::env::var(DEVICE_PORT_ENV).ok().and_then(|s| s.parse().ok()) {
    Some(port) => port,
    None => discover_big_monome(&sock, listen_port)
      .expect("no 16x16 monome found; is the big grid plugged in and serialoscd running?"),
  };
  let mut device: SocketAddr = format!("127.0.0.1:{device_port}").parse()?;
  monome::register(&sock, device, &prefix, listen_port);

  let windows = vec![
    Window { id: WindowId::ControlsTop, rect: CONTROLS_TOP_RECT },
    Window { id: WindowId::ControlsBottom, rect: CONTROLS_BOTTOM_RECT },
    Window { id: WindowId::Edo, rect: EDO_RECT },
  ];
  let mut state = AppState::new(
    build_pitch_class(x_step, y_step, edo, grid_w, grid_h),
    edo,
    velocity,
    min_channel,
    min_note,
    midi_tx,
  );
  repaint(&sock, device, &prefix, &windows, &state);

  install_sigint_handler();
  print_startup_message(edo, x_step, y_step, &midi_output_name);

  let key_addr = format!("{prefix}/grid/key");
  let mut buf = [0u8; 2048];
  let start = Instant::now();
  let mut last_flash_phase = 2u8;
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    if state.target_select_mode {
      let phase = ((start.elapsed().as_millis() / FLASH_PHASE_MS) & 1) as u8;
      if phase != last_flash_phase {
        last_flash_phase = phase;
        let brightness = if phase == 0 { Brightness::Bright } else { Brightness::Off };
        set_led(
          &sock,
          device,
          &prefix,
          &windows,
          WindowId::ControlsTop,
          CELL_SET_ACCRETION_TARGET,
          brightness,
        );
      }
    } else {
      last_flash_phase = 2;
    }

    let pkt = match sock.recv_from(&mut buf) {
      Ok((n, _)) => match decoder::decode_udp(&buf[..n]) {
        Ok((_, p)) => p,
        Err(e) => {
          eprintln!("OSC decode error: {e:?}");
          continue;
        }
      },
      Err(_) => continue,
    };
    let OscPacket::Message(message) = pkt else { continue; };
    if message.addr == "/serialosc/device" && message.args.len() >= 3 {
      if let Some((port, is_big)) = serialosc_device_port_and_size(&message.args) {
        let port = *port as u16;
        if is_big && port != device_port {
          device_port = port;
          device = format!("127.0.0.1:{port}").parse()?;
          monome::register(&sock, device, &prefix, listen_port);
          repaint(&sock, device, &prefix, &windows, &state);
        }
      }
      continue;
    }
    if message.addr != key_addr || message.args.len() != 3 {
      continue;
    }
    let (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(s))) =
      (message.args.first(), message.args.get(1), message.args.get(2))
    else {
      continue;
    };
    let cell = (*x, *y);
    let Some(window) = window_for_cell(&windows, cell) else { continue; };
    let press = *s == 1;
    let diffs = match window {
      WindowId::Edo => {
        if press { edo_press(&mut state, cell) } else { edo_release(&mut state, cell) }
      }
      WindowId::ControlsTop => {
        if press {
          control_press(&mut state, cell, window)
        } else {
          control_release(&mut state, cell, window)
        }
      }
      WindowId::ControlsBottom => {
        let chord = *x as usize;
        if press { chord_press(&mut state, chord) } else { chord_release(&mut state, chord) }
      }
    };
    for (from, cell, brightness) in diffs {
      set_led(&sock, device, &prefix, &windows, from, cell, brightness);
    }
  }

  state.all_notes_off();
  monome::send_led_all(&sock, device, &prefix, 0);
  Ok(())
}

impl AppState {
  fn new(
    pitch_class: PitchClass,
    edo: i32,
    velocity: u8,
    min_channel: i32,
    min_note: i32,
    midi_tx: mpsc::Sender<Vec<u8>>,
  ) -> Self {
    let mut control_buttons = HashMap::new();
    control_buttons.insert(CELL_WIPE, Button::Fire { fire: ButtonAction::WipeFire });
    control_buttons.insert(
      CELL_ACCRETE_ON,
      Button::Toggle { state: false, on: ButtonAction::AccreteOn, off: ButtonAction::AccreteOff },
    );
    control_buttons.insert(
      CELL_EMIT_IS_TOGGLE,
      Button::Toggle { state: true, on: ButtonAction::EmitIsToggleOn, off: ButtonAction::EmitIsToggleOff },
    );
    control_buttons.insert(
      CELL_SET_ACCRETION_TARGET,
      Button::Fire { fire: ButtonAction::SetTargetFire },
    );
    AppState {
      chords: vec![HashMap::new(); N_CHORDS],
      accretion_target: INITIALLY_ACCRETING_CHORD,
      emitting_chord: None,
      pressed_chords: vec![],
      target_select_mode: false,
      accrete_on: false,
      emit_is_toggle: true,
      next_voice_id: 0,
      pitchled_reasons: HashMap::new(),
      control_buttons,
      pitch_class,
      edo,
      active_fingers: HashMap::new(),
      active_accretions: HashMap::new(),
      source_outputs: HashMap::new(),
      output_counts: HashMap::new(),
      midi_tx,
      velocity,
      min_channel,
      min_note,
    }
  }

  fn start_source(&mut self, source: Source, pitch: i32) {
    let Some((channel, note)) =
      midi_note_for_pitch(pitch, self.edo, self.min_channel, self.min_note)
    else {
      eprintln!("pitch {pitch} is outside MIDI channel/note range; not sounding");
      return;
    };
    if self.source_outputs.contains_key(&source) {
      self.stop_source(source);
    }
    self.source_outputs.insert(source, (channel, note));
    let count = self.output_counts.entry((channel, note)).or_insert(0);
    if *count == 0 {
      let _ = self.midi_tx.send(vec![0x90 | channel, note, self.velocity]);
    }
    *count += 1;
  }

  fn stop_source(&mut self, source: Source) {
    let Some(note) = self.source_outputs.remove(&source) else { return; };
    let Some(count) = self.output_counts.get_mut(&note) else { return; };
    *count = count.saturating_sub(1);
    if *count == 0 {
      self.output_counts.remove(&note);
      let (channel, midi_note) = note;
      let _ = self.midi_tx.send(vec![0x80 | channel, midi_note, 0]);
    }
  }

  fn move_source(&mut self, from: Source, to: Source) {
    if let Some(note) = self.source_outputs.remove(&from) {
      self.source_outputs.insert(to, note);
    }
  }

  fn all_notes_off(&mut self) {
    let active: HashSet<MidiNote> = self.output_counts.keys().copied().collect();
    self.output_counts.clear();
    self.source_outputs.clear();
    for (channel, note) in active {
      let _ = self.midi_tx.send(vec![0x80 | channel, note, 0]);
    }
  }
}

fn parse_i32_arg(args: &[String], index: usize, name: &str, default: i32) -> i32 {
  args.get(index)
    .map(|s| s.parse().unwrap_or_else(|_| panic!("{name} must be an integer, got {s:?}")))
    .unwrap_or(default)
}

fn discover_big_monome(sock: &UdpSocket, listen_port: u16) -> Option<u16> {
  let devices = monome::discover_devices(sock, listen_port);
  if devices.len() > 1 {
    eprintln!("monome devices reported by serialoscd:");
    for device in &devices {
      eprintln!(
        "  id={} type={} port={} size={}x{}",
        device.id, device.type_name, device.port, device.grid_w, device.grid_h,
      );
    }
  }
  let selected = devices
    .iter()
    .rev()
    .find(|device| device.grid_w == GRID_W && device.grid_h == GRID_H);
  if let Some(device) = selected {
    eprintln!(
      "using 16x16 monome id={} type={} port={}",
      device.id, device.type_name, device.port,
    );
  }
  selected.map(|device| device.port)
}

fn serialosc_device_port_and_size(args: &[OscType]) -> Option<(&i32, bool)> {
  let (Some(OscType::String(type_name)), Some(OscType::Int(port))) =
    (args.get(1), args.get(2))
  else {
    return None;
  };
  Some((port, type_name.contains("256")))
}

fn print_startup_message(edo: i32, x_step: i32, y_step: i32, midi_output_name: &str) {
  println!("monome_edo_midi started");
  println!();
  println!("Virtual port created:");
  println!("  - '{midi_output_name}:out' (output)");
  println!();
  println!("Grid tuning:");
  println!("  - edo={edo}, x_step={x_step}, y_step={y_step}");
  println!("  - MIDI output uses edo_un12-style channel/note spreading");
  println!("  - controls match monome_edo_sawwave: wipe, accrete, emit-toggle, target-select");
  println!();
  println!("Press Ctrl-C to exit...");
}

fn install_sigint_handler() {
  extern "C" fn handler(_: i32) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
  }
  unsafe {
    libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
  }
}

fn midi_note_for_pitch(pitch: i32, edo: i32, min_channel: i32, min_note: i32) -> Option<MidiNote> {
  let channel = min_channel + pitch.div_euclid(edo);
  let note = min_note + pitch.rem_euclid(edo);
  if (0..=15).contains(&channel) && (0..=127).contains(&note) {
    Some((channel as u8, note as u8))
  } else {
    None
  }
}

fn build_pitch_class(x_step: i32, y_step: i32, edo: i32, w: i32, h: i32) -> PitchClass {
  let mut key_to_pitch = HashMap::new();
  let mut key_to_pitchclass = HashMap::new();
  let mut pitchclass_to_keys: HashMap<i32, Vec<Cell>> = HashMap::new();
  for x in 0..w {
    for y in 0..h {
      let pitch = x_step * x + y_step * y;
      let pitch_class = pitch.rem_euclid(edo);
      key_to_pitch.insert((x, y), pitch);
      key_to_pitchclass.insert((x, y), pitch_class);
      pitchclass_to_keys.entry(pitch_class).or_default().push((x, y));
    }
  }
  PitchClass { key_to_pitch, key_to_pitchclass, pitchclass_to_keys }
}

fn cells_for_pitch_of(pc: &PitchClass, cell: Cell) -> Vec<Cell> {
  pc.key_to_pitchclass
    .get(&cell)
    .and_then(|pitch_class| pc.pitchclass_to_keys.get(pitch_class))
    .cloned()
    .unwrap_or_else(|| vec![cell])
}

fn cells_for_pitch(pc: &PitchClass, pitch: i32, edo: i32) -> Vec<Cell> {
  pc.pitchclass_to_keys
    .get(&pitch.rem_euclid(edo))
    .cloned()
    .unwrap_or_default()
}

fn edo_press(state: &mut AppState, cell: Cell) -> Vec<LedCmd> {
  let Some(&pitch) = state.pitch_class.key_to_pitch.get(&cell) else { return vec![]; };
  let id = state.next_voice_id;
  state.next_voice_id += 1;
  state.active_fingers.insert(cell, FingerState { id, pitch });
  state.start_source(Source::Fingered { xy: cell }, pitch);

  let mut diffs = vec![];
  let reason = PitchLedReason::PitchEquivalent { source_xy: cell };
  for equivalent in cells_for_pitch_of(&state.pitch_class, cell) {
    if add_reason(&mut state.pitchled_reasons, equivalent, reason) == Some(true) {
      diffs.push((WindowId::Edo, equivalent, Brightness::Bright));
    }
  }

  let target = state.accretion_target;
  if state.accrete_on && !state.chords[target].contains_key(&pitch) {
    state.chords[target].insert(pitch, HashSet::from([id]));
    if state.emitting_chord == Some(target) {
      diffs.extend(add_chord_pitchled_reasons(state, target, &[pitch]));
    }
  }
  diffs
}

fn edo_release(state: &mut AppState, cell: Cell) -> Vec<LedCmd> {
  let Some(finger) = state.active_fingers.get(&cell).copied() else { return vec![]; };
  let mut diffs = vec![];
  let source = Source::Fingered { xy: cell };
  let emitting = state.emitting_chord;
  let originvoices = emitting.and_then(|chord| state.chords[chord].get(&finger.pitch));
  let is_originvoice = originvoices.is_some_and(|ids| ids.contains(&finger.id));
  let last_alive_originvoice = originvoices.is_some_and(|ids| {
    ids.iter().all(|id| {
      *id == finger.id || !state.active_fingers.values().any(|active| active.id == *id)
    })
  });
  let should_transform = emitting.is_some()
    && is_originvoice
    && last_alive_originvoice
    && !state.active_accretions.contains_key(&(emitting.unwrap(), finger.pitch));

  state.active_fingers.remove(&cell);
  if should_transform {
    let chord = emitting.unwrap();
    state.active_accretions.insert((chord, finger.pitch), finger.id);
    state.move_source(source, Source::Accreted { chord, pitch: finger.pitch });
  } else {
    state.stop_source(source);
  }

  let reason = PitchLedReason::PitchEquivalent { source_xy: cell };
  for equivalent in cells_for_pitch_of(&state.pitch_class, cell) {
    if remove_reason(&mut state.pitchled_reasons, equivalent, reason) == Some(false) {
      diffs.push((WindowId::Edo, equivalent, Brightness::Off));
    }
  }
  diffs
}

fn control_press(state: &mut AppState, cell: Cell, window: WindowId) -> Vec<LedCmd> {
  if state.target_select_mode {
    state.target_select_mode = false;
    return vec![repaint_set_target_button()];
  }
  let Some(button) = state.control_buttons.get_mut(&cell) else { return vec![]; };
  let (action, new_state) = match button {
    Button::Toggle { state, on, off } => {
      *state = !*state;
      (if *state { *on } else { *off }, Some(*state))
    }
    Button::Fire { fire } => (*fire, None),
  };
  let mut diffs = vec![];
  if let Some(lit) = new_state {
    diffs.push((window, cell, if lit { Brightness::Bright } else { Brightness::Off }));
  }
  diffs.extend(do_action(state, action));
  diffs
}

fn control_release(_state: &mut AppState, _cell: Cell, _window: WindowId) -> Vec<LedCmd> {
  vec![]
}

fn chord_press(state: &mut AppState, chord: ChordId) -> Vec<LedCmd> {
  if chord >= N_CHORDS {
    return vec![];
  }
  if state.target_select_mode {
    state.target_select_mode = false;
    let mut diffs = vec![repaint_set_target_button()];
    if chord != SILENCE_CHORD {
      let old_target = state.accretion_target;
      state.accretion_target = chord;
      diffs.push(repaint_chord_button(state, old_target));
      diffs.push(repaint_chord_button(state, chord));
    }
    return diffs;
  }
  state.pressed_chords.retain(|&c| c != chord);
  state.pressed_chords.push(chord);
  if state.emit_is_toggle {
    if state.emitting_chord == Some(chord) {
      switch_emitter_to(state, None)
    } else {
      switch_emitter_to(state, Some(chord))
    }
  } else {
    switch_emitter_to(state, Some(chord))
  }
}

fn chord_release(state: &mut AppState, chord: ChordId) -> Vec<LedCmd> {
  state.pressed_chords.retain(|&c| c != chord);
  if state.emit_is_toggle {
    vec![]
  } else if state.emitting_chord == Some(chord) {
    switch_emitter_to(state, state.pressed_chords.last().copied())
  } else {
    vec![]
  }
}

fn switch_emitter_to(state: &mut AppState, new: Option<ChordId>) -> Vec<LedCmd> {
  let previous = state.emitting_chord;
  if previous == new {
    return vec![];
  }
  state.emitting_chord = new;
  let mut diffs = vec![];
  if let Some(chord) = previous {
    let pitches: Vec<i32> = state.chords[chord].keys().copied().collect();
    for pitch in &pitches {
      state.active_accretions.remove(&(chord, *pitch));
      state.stop_source(Source::Accreted { chord, pitch: *pitch });
    }
    diffs.extend(remove_chord_pitchled_reasons(state, chord, &pitches));
    diffs.push(repaint_chord_button(state, chord));
  }
  if let Some(chord) = new {
    let pitches: Vec<i32> = state.chords[chord].keys().copied().collect();
    for pitch in &pitches {
      let originvoices = &state.chords[chord][pitch];
      let any_alive = originvoices.iter()
        .any(|id| state.active_fingers.values().any(|finger| finger.id == *id));
      if !any_alive {
        let id = state.next_voice_id;
        state.next_voice_id += 1;
        state.active_accretions.insert((chord, *pitch), id);
        state.start_source(Source::Accreted { chord, pitch: *pitch }, *pitch);
      }
    }
    diffs.extend(add_chord_pitchled_reasons(state, chord, &pitches));
    diffs.push(repaint_chord_button(state, chord));
  }
  diffs
}

fn do_action(state: &mut AppState, action: ButtonAction) -> Vec<LedCmd> {
  match action {
    ButtonAction::AccreteOn => {
      state.accrete_on = true;
      let target = state.accretion_target;
      let mut newly_introduced = vec![];
      for finger in state.active_fingers.values() {
        if !state.chords[target].contains_key(&finger.pitch) {
          state.chords[target].insert(finger.pitch, HashSet::from([finger.id]));
          newly_introduced.push(finger.pitch);
        }
      }
      if state.emitting_chord == Some(target) {
        add_chord_pitchled_reasons(state, target, &newly_introduced)
      } else {
        vec![]
      }
    }
    ButtonAction::AccreteOff => {
      state.accrete_on = false;
      vec![]
    }
    ButtonAction::WipeFire => {
      let target = state.accretion_target;
      let pitches: Vec<i32> = state.chords[target].keys().copied().collect();
      state.chords[target].clear();
      if state.emitting_chord == Some(target) {
        for pitch in &pitches {
          state.active_accretions.remove(&(target, *pitch));
          state.stop_source(Source::Accreted { chord: target, pitch: *pitch });
        }
        remove_chord_pitchled_reasons(state, target, &pitches)
      } else {
        vec![]
      }
    }
    ButtonAction::EmitIsToggleOn | ButtonAction::EmitIsToggleOff => {
      let new_mode_is_toggle = matches!(action, ButtonAction::EmitIsToggleOn);
      let was_toggle = state.emit_is_toggle;
      state.emit_is_toggle = new_mode_is_toggle;
      if was_toggle && !new_mode_is_toggle {
        let keep = state.emitting_chord.is_some_and(|chord| state.pressed_chords.contains(&chord));
        if !keep {
          return switch_emitter_to(state, None);
        }
      }
      vec![]
    }
    ButtonAction::SetTargetFire => {
      state.target_select_mode = true;
      vec![]
    }
  }
}

fn repaint_set_target_button() -> LedCmd {
  (WindowId::ControlsTop, CELL_SET_ACCRETION_TARGET, Brightness::Dim)
}

fn repaint_chord_button(state: &AppState, chord: ChordId) -> LedCmd {
  (WindowId::ControlsBottom, (chord as i32, 15), chord_button_brightness(state, chord))
}

fn chord_button_brightness(state: &AppState, chord: ChordId) -> Brightness {
  if state.emitting_chord == Some(chord) {
    Brightness::Bright
  } else if chord == state.accretion_target && chord != SILENCE_CHORD {
    Brightness::Dim
  } else {
    Brightness::Off
  }
}

fn add_chord_pitchled_reasons(state: &mut AppState, chord: ChordId, pitches: &[i32]) -> Vec<LedCmd> {
  let mut diffs = vec![];
  for &pitch in pitches {
    let reason = PitchLedReason::Chord { chord, pitch };
    for cell in cells_for_pitch(&state.pitch_class, pitch, state.edo) {
      if add_reason(&mut state.pitchled_reasons, cell, reason) == Some(true) {
        diffs.push((WindowId::Edo, cell, Brightness::Bright));
      }
    }
  }
  diffs
}

fn remove_chord_pitchled_reasons(state: &mut AppState, chord: ChordId, pitches: &[i32]) -> Vec<LedCmd> {
  let mut diffs = vec![];
  for &pitch in pitches {
    let reason = PitchLedReason::Chord { chord, pitch };
    for cell in cells_for_pitch(&state.pitch_class, pitch, state.edo) {
      if remove_reason(&mut state.pitchled_reasons, cell, reason) == Some(false) {
        diffs.push((WindowId::Edo, cell, Brightness::Off));
      }
    }
  }
  diffs
}

fn add_reason(
  reasons: &mut HashMap<Cell, HashSet<PitchLedReason>>,
  cell: Cell,
  reason: PitchLedReason,
) -> Option<bool> {
  let entry = reasons.entry(cell).or_default();
  let was_empty = entry.is_empty();
  entry.insert(reason);
  if was_empty { Some(true) } else { None }
}

fn remove_reason(
  reasons: &mut HashMap<Cell, HashSet<PitchLedReason>>,
  cell: Cell,
  reason: PitchLedReason,
) -> Option<bool> {
  let entry = reasons.get_mut(&cell)?;
  entry.remove(&reason);
  if entry.is_empty() {
    reasons.remove(&cell);
    Some(false)
  } else {
    None
  }
}

fn repaint(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  windows: &[Window],
  state: &AppState,
) {
  for (&cell, button) in &state.control_buttons {
    let brightness = if cell == CELL_SET_ACCRETION_TARGET {
      Brightness::Dim
    } else {
      match button {
        Button::Toggle { state, .. } if *state => Brightness::Bright,
        _ => Brightness::Off,
      }
    };
    set_led(sock, device, prefix, windows, WindowId::ControlsTop, cell, brightness);
  }
  for chord in 0..N_CHORDS {
    set_led(
      sock,
      device,
      prefix,
      windows,
      WindowId::ControlsBottom,
      (chord as i32, 15),
      chord_button_brightness(state, chord),
    );
  }
  for &cell in state.pitchled_reasons.keys() {
    set_led(sock, device, prefix, windows, WindowId::Edo, cell, Brightness::Bright);
  }
}

fn set_led(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  windows: &[Window],
  from: WindowId,
  cell: Cell,
  brightness: Brightness,
) {
  if !visible(windows, from, cell) {
    return;
  }
  let level = match brightness {
    Brightness::Off => 0,
    Brightness::Dim => 4,
    Brightness::Bright => 15,
  };
  monome::send_led_level_set(sock, device, prefix, cell.0, cell.1, level);
}

fn window_for_cell(windows: &[Window], cell: Cell) -> Option<WindowId> {
  windows
    .iter()
    .find(|window| rect_contains(&window.rect, cell))
    .map(|window| window.id)
}

fn visible(windows: &[Window], from: WindowId, cell: Cell) -> bool {
  for window in windows {
    if window.id == from {
      return rect_contains(&window.rect, cell);
    }
    if rect_contains(&window.rect, cell) {
      return false;
    }
  }
  false
}

fn rect_contains(rect: &Rect, cell: Cell) -> bool {
  let ((x0, y0), (x1, y1)) = *rect;
  x0 <= cell.0 && cell.0 <= x1 && y0 <= cell.1 && cell.1 <= y1
}
