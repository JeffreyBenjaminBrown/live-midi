// VOCAB:
// The map maps the 'preimage' (all of 12-edo) to the 'image'
// (a 12-note subset of 31-edo).
// The terms 'image' and 'preimage'
// are sometimes used that way in the code.

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use midi_pulse::{midi, monome, piano_transform};
use rosc::{decoder, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{io, thread};

const SHIFT_IN_12_EDO : i8 = -5;
const LOWEST_A        : u8 = 21;
const MIN_CHANNEL_OUT : u8 = 1;
const MIN_NOTE_OUT    : u8 = 28;

const PREFIX: &str = "/128-1-cable";
const LISTEN_PORT: u16 = 9000;
const LED_TRACE_ENV: &str = "EDO31_LED_TRACE";
const GRID_W: i32 = 16;
const GRID_H: i32 = 8;
const EDO: i16 = 31;
const INITIAL_MAP: [u8; 12] = [0, 3, 5, 8, 10, 13, 16, 18, 21, 23, 26, 28]; // In GHCI, the following expression yields this list:[ round $ i * 31/12 | i <- [0..12]]
// Keep this no larger than the shortest on-window you want rendered.
const MONOME_REFRESH: Duration = Duration::from_millis(1);

const ANCHOR_PITCH_CLASSES: [usize; 3] = [0, 5, 7];
const SOUNDING_COLOR: Color = Color::AlwaysOn;
const ANCHOR_COLOR: Color = Color::Duty {
  period: Duration::from_micros(300_000),
  fraction_on: 0.3,
};
const IMAGE_COLOR: Color = Color::Duty {
  // A color for one of the 12 values of 31-edo that the keyboard maps to.
  period: Duration::from_micros(100_000),
  fraction_on: 0.001,
};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct Edo31State {
  map: [u8; 12],
  deltas: [i16; 12],
  loose: [LooseState; 12],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LooseState {
  Loose,
  Fixed,
}

#[derive(Clone, Copy)]
enum Color {
  AlwaysOn,
  Duty {
    period: Duration,
    fraction_on: f64,
  },
}

impl Color {
  fn is_initially_on(self) -> bool {
    match self {
      Color::AlwaysOn => true,
      Color::Duty { .. } =>
        self.duty_durations()
          .map(|(on, _)| !on.is_zero())
          .unwrap_or(false),
    }
  }

  fn phase_duration(self, on: bool) -> Option<Duration> {
    let (on_duration, off_duration) = self.duty_durations()?;
    Some(if on { on_duration } else { off_duration })
  }

  fn duty_durations(self) -> Option<(Duration, Duration)> {
    let (period_micros, on_micros) = self.duty_micros()?;
    Some((
      Duration::from_micros(on_micros as u64),
      Duration::from_micros((period_micros - on_micros) as u64),
    ))
  }

  fn duty_micros(self) -> Option<(u128, u128)> {
    match self {
      Color::AlwaysOn => None,
      Color::Duty { period, fraction_on } => {
        let period_micros = period.as_micros();
        if period_micros == 0 {
          return None;
        }
        let fraction_on = fraction_on.clamp(0.0, 1.0);
        let on_micros = (period_micros as f64 * fraction_on) as u128;
        Some((period_micros, on_micros))
      }
    }
  }
}

#[derive(Clone, Copy)]
struct ColorClock {
  color: Color,
  on: bool,
  next_transition: Option<Instant>,
}

impl ColorClock {
  fn new(color: Color, now: Instant) -> Self {
    let Some((on_duration, off_duration)) = color.duty_durations() else {
      return ColorClock { color, on: true, next_transition: None };
    };
    if on_duration.is_zero() {
      return ColorClock { color, on: false, next_transition: None };
    }
    if off_duration.is_zero() {
      return ColorClock { color, on: true, next_transition: None };
    }
    let on = color.is_initially_on();
    let next_transition = Some(now + on_duration);
    ColorClock { color, on, next_transition }
  }

  fn is_on(self) -> bool {
    match self.color {
      Color::AlwaysOn => true,
      Color::Duty { .. } => self.on,
    }
  }

  fn advance_if_due(&mut self, now: Instant) -> bool {
    let Some(next_transition) = self.next_transition else {
      return false;
    };
    if now < next_transition {
      return false;
    }
    self.on = !self.on;
    self.next_transition =
      self.color
        .phase_duration(self.on)
        .filter(|duration| !duration.is_zero())
        .map(|duration| now + duration);
    true
  }

  fn wait(self, now: Instant) -> Option<Duration> {
    self.next_transition.map(|transition| {
      if transition > now {
        transition.duration_since(now)
      } else {
        Duration::ZERO
      }
    })
  }
}

#[derive(Clone, Copy)]
struct LedPhases {
  sounding_on: bool,
  anchor_on: bool,
  image_on: bool,
}

impl Edo31State {
  fn new() -> Self {
    Edo31State {
      map: INITIAL_MAP,
      deltas: [0; 12],
      loose: [LooseState::Fixed; 12],
    }
  }
}

struct SoundingState {
  by_original_note: HashMap<u8, u8>,
  counts: [u16; 31],
}

impl SoundingState {
  fn new() -> Self {
    SoundingState {
      by_original_note: HashMap::new(),
      counts: [0; 31],
    }
  }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let state: Arc<Mutex<Edo31State>> =
    Arc::new(Mutex::new(Edo31State::new()));
  let sounding: Arc<Mutex<SoundingState>> =
    Arc::new(Mutex::new(SoundingState::new()));
  let ongoing: Arc<Mutex<HashMap<u8, piano_transform::TransformedNote>>> =
    Arc::new(Mutex::new(HashMap::new()));

  let midi_in: MidiInput = MidiInput::new("edo31_piano_monome-in")?;
  let midi_out: MidiOutput = MidiOutput::new("edo31_piano_monome-out")?;
  let conn_out: MidiOutputConnection = midi_out.create_virtual("out")?;
  let (tx, rx): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) =
    mpsc::channel();
  let _out_thread: thread::JoinHandle<()> =
    thread::spawn(move || midi::run_output_thread(conn_out, rx));

  let state_for_midi = Arc::clone(&state);
  let sounding_for_midi = Arc::clone(&sounding);
  let ongoing_for_midi = Arc::clone(&ongoing);
  let _conn_in: MidiInputConnection<()> = midi_in.create_virtual(
    "in",
    move |_timestamp: u64, message: &[u8], _: &mut ()| {
      update_sounding(message, &state_for_midi, &sounding_for_midi);
      for msg in piano_transform::transform_message(
        message,
        &ongoing_for_midi,
        |original_note| edo31_instruction(original_note, &state_for_midi),
      ) {
        let _ = tx.send(msg);
      }
    },
    (),
  )?;

  let state_for_monome = Arc::clone(&state);
  let sounding_for_monome = Arc::clone(&sounding);
  let monome_thread: thread::JoinHandle<()> =
    thread::spawn(move || run_monome_thread(state_for_monome, sounding_for_monome));

  install_sigint_handler();
  print_startup_message();
  let (quit_tx, quit_rx): (mpsc::Sender<()>, mpsc::Receiver<()>) =
    mpsc::channel();
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

fn print_startup_message() {
  println!("31-EDO piano transformer with monome mapping started!");
  println!();
  println!("Virtual ports created:");
  println!("  - 'edo31_piano_monome-in:in' (input)");
  println!("  - 'edo31_piano_monome-out:out' (output)");
  println!();
  println!("Monome 31-EDO map:");
  println!("  - each key's pitch is 6*x + y mod 31");
  println!("  - sounding pitches stay lit");
  println!("  - C, F and G flash 50/50 every 50 ms");
  println!("  - other image pitches flash 10/90 every 10 ms");
  println!("  - tap lit pitches to loosen them");
  println!("  - tap a dark pitch to move a neighboring loose pitch");
  println!();
  println!("Press Enter to exit...");
}

fn edo31_instruction(
  original_note: u8,
  state: &Arc<Mutex<Edo31State>>,
) -> (i16, i16) {
  let absolute_step = edo31_absolute_step(original_note, state);
  let channel = MIN_CHANNEL_OUT as i16 + absolute_step.div_euclid(EDO);
  let note = MIN_NOTE_OUT as i16 + absolute_step.rem_euclid(EDO);
  (channel, note)
}

fn edo31_absolute_step(
  original_note: u8,
  state: &Arc<Mutex<Edo31State>>,
) -> i16 {
  let normalized = original_note as i16
                   - LOWEST_A as i16
                   + SHIFT_IN_12_EDO as i16;
  let channel_offset = normalized.div_euclid(12);
  let note_offset = normalized.rem_euclid(12) as usize;
  let pitch_class = original_note % 12;
  let state = state.lock().unwrap();
  let pc = pitch_class as usize;
  channel_offset * EDO
    + INITIAL_MAP[note_offset] as i16
    + state.deltas[pc]
}

fn update_sounding(
  message: &[u8],
  state: &Arc<Mutex<Edo31State>>,
  sounding: &Arc<Mutex<SoundingState>>,
) {
  if message.len() < 3 || !midi::is_note_event(message) {
    return;
  }
  let original_note = message[1];
  if midi::is_note_on(message) {
    let step = sounding_step(original_note, state);
    let mut sounding = sounding.lock().unwrap();
    if let Some(old_step) = sounding.by_original_note.insert(original_note, step) {
      decrement_sounding_count(&mut sounding, old_step);
    }
    sounding.counts[step as usize] += 1;
  } else if midi::is_note_off(message) {
    let mut sounding = sounding.lock().unwrap();
    if let Some(old_step) = sounding.by_original_note.remove(&original_note) {
      decrement_sounding_count(&mut sounding, old_step);
    }
  }
}

fn sounding_step(
  original_note: u8,
  state: &Arc<Mutex<Edo31State>>,
) -> u8 {
  let pitch_class = (original_note % 12) as usize;
  state.lock().unwrap().map[pitch_class]
}

fn decrement_sounding_count(sounding: &mut SoundingState, step: u8) {
  let count = &mut sounding.counts[step as usize];
  if *count > 0 {
    *count -= 1;
  }
}

fn run_monome_thread(
  state: Arc<Mutex<Edo31State>>,
  sounding: Arc<Mutex<SoundingState>>,
) {
  let sock = UdpSocket::bind(("0.0.0.0", LISTEN_PORT))
    .unwrap_or_else(|e| panic!("bind UDP :{LISTEN_PORT}: {e}"));
  sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
  let mut device_port =
    monome::discover_device(&sock, LISTEN_PORT)
      .expect("no monome found; is serialoscd running?");
  let mut device: SocketAddr = format!("127.0.0.1:{device_port}").parse().unwrap();
  monome::register(&sock, device, PREFIX, LISTEN_PORT);
  let mut rendered_cols = [0u8; 16];
  let mut sounding_clock = ColorClock::new(SOUNDING_COLOR, Instant::now());
  let mut anchor_clock = ColorClock::new(ANCHOR_COLOR, Instant::now());
  let mut image_clock = ColorClock::new(IMAGE_COLOR, Instant::now());
  render_to_monome(
    &sock,
    device,
    &state.lock().unwrap(),
    &sounding.lock().unwrap().counts,
    led_phases(sounding_clock, anchor_clock, image_clock),
    &mut rendered_cols,
  );
  let key_addr = format!("{PREFIX}/grid/key");
  let mut buf = [0u8; 2048];
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    let now = Instant::now();
    let mut dirty = false;
    dirty |= sounding_clock.advance_if_due(now);
    dirty |= anchor_clock.advance_if_due(now);
    dirty |= image_clock.advance_if_due(now);
    sock.set_read_timeout(Some(next_render_wait(
      now,
      sounding_clock,
      anchor_clock,
      image_clock,
    ))).unwrap();
    render_to_monome(
      &sock,
      device,
      &state.lock().unwrap(),
      &sounding.lock().unwrap().counts,
      led_phases(sounding_clock, anchor_clock, image_clock),
      &mut rendered_cols,
    );
    let pkt = match sock.recv_from(&mut buf) {
      Ok((n, _)) => match decoder::decode_udp(&buf[..n]) {
        Ok((_, p)) => p,
        Err(_) => continue,
      },
      Err(_) => continue,
    };
    let OscPacket::Message(m) = pkt else { continue; };
    if m.addr == "/serialosc/device" && m.args.len() >= 3 {
      if let Some(OscType::Int(p)) = m.args.get(2) {
        let p = *p as u16;
        if p != device_port {
          device_port = p;
          device = format!("127.0.0.1:{p}").parse().unwrap();
          monome::register(&sock, device, PREFIX, LISTEN_PORT);
          rendered_cols = [0; 16];
          render_to_monome(
            &sock,
            device,
            &state.lock().unwrap(),
            &sounding.lock().unwrap().counts,
            led_phases(sounding_clock, anchor_clock, image_clock),
            &mut rendered_cols,
          );
        }
      }
      continue;
    }
    if m.addr != key_addr || m.args.len() != 3 {
      continue;
    }
    let (x, y, s) = match (m.args.first(), m.args.get(1), m.args.get(2)) {
      (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(s))) =>
        (*x, *y, *s),
      _ => continue,
    };
    if s != 1 || !is_grid_cell(x, y) {
      continue;
    }
    let mut state = state.lock().unwrap();
    if apply_grid_press(&mut state, x, y) {
      dirty = true;
    }
    if dirty {
      render_to_monome(
        &sock,
        device,
        &state,
        &sounding.lock().unwrap().counts,
        led_phases(sounding_clock, anchor_clock, image_clock),
        &mut rendered_cols,
      );
    }
  }
  monome::send_led_all(&sock, device, PREFIX, 0);
}

fn led_phases(
  sounding_clock: ColorClock,
  anchor_clock: ColorClock,
  image_clock: ColorClock,
) -> LedPhases {
  LedPhases {
    sounding_on: sounding_clock.is_on(),
    anchor_on: anchor_clock.is_on(),
    image_on: image_clock.is_on(),
  }
}

fn next_render_wait(
  now: Instant,
  sounding_clock: ColorClock,
  anchor_clock: ColorClock,
  image_clock: ColorClock,
) -> Duration {
  let next_transition = [
    sounding_clock.wait(now),
    anchor_clock.wait(now),
    image_clock.wait(now),
  ]
    .into_iter()
    .flatten()
    .min();
  next_transition
    .map(|transition| transition.min(MONOME_REFRESH))
    .unwrap_or(MONOME_REFRESH)
}

fn render_to_monome(
  sock: &UdpSocket,
  device: SocketAddr,
  state: &Edo31State,
  sounding_counts: &[u16; 31],
  phases: LedPhases,
  rendered_cols: &mut [u8; 16],
) {
  let cols = render_led_cols(state, sounding_counts, phases);
  let trace_leds = led_trace_enabled();
  for (x, col) in cols.iter().enumerate() {
    if rendered_cols[x] != *col {
      if trace_leds {
        eprintln!(
          "led x={x:02} mask=0b{col:08b} old=0b{:08b}",
          rendered_cols[x],
        );
      }
      monome::send_led_col(sock, device, PREFIX, x as i32, 0, *col as i32);
      rendered_cols[x] = *col;
    }
  }
}

fn led_trace_enabled() -> bool {
  std::env::var_os(LED_TRACE_ENV).is_some()
}

fn render_led_cols(
  state: &Edo31State,
  sounding_counts: &[u16; 31],
  phases: LedPhases,
) -> [u8; 16] {
  let mut cols = [0u8; 16];
  for y in 0..GRID_H {
    for x in 0..GRID_W {
      let step = grid_step(x, y);
      if is_rendered_on(state, sounding_counts, step, phases) {
        cols[x as usize] |= 1 << y;
      }
    }
  }
  cols
}

fn is_rendered_on(
  state: &Edo31State,
  sounding_counts: &[u16; 31],
  step: u8,
  phases: LedPhases,
) -> bool {
  if sounding_counts[step as usize] > 0 {
    return phases.sounding_on;
  }
  if let Some(preimage) = preimage_for_step(state, step) {
    if is_anchor_pitch_class(preimage) {
      phases.anchor_on
    } else {
      phases.image_on
    }
  } else {
    false
  }
}

fn is_anchor_pitch_class(preimage: usize) -> bool {
  ANCHOR_PITCH_CLASSES.contains(&preimage)
}

fn apply_grid_press(state: &mut Edo31State, x: i32, y: i32) -> bool {
  let step = grid_step(x, y);
  if let Some(preimage) = preimage_for_step(state, step) {
    state.loose[preimage] = LooseState::Loose;
    eprintln!("loose {} -> {step}", pitch_name(preimage));
    return true;
  }
  let Some(preimage) = loose_neighbor_for_dark_step(step, state) else { return false; };
  let current = state.map[preimage];
  let Some(delta) = move_delta(current, step, &state.map) else { return false; };
  state.map[preimage] = step;
  state.deltas[preimage] += delta;
  state.loose[preimage] = LooseState::Fixed;
  eprintln!("moved {}: {current} -> {step}", pitch_name(preimage));
  true
}

fn preimage_for_step(state: &Edo31State, step: u8) -> Option<usize> {
  state.map.iter().position(|s| *s == step)
}

fn loose_neighbor_for_dark_step(step: u8, state: &Edo31State) -> Option<usize> {
  let (lower, higher) = nearest_light_neighbors(step, &state.map);
  match (
    state.loose[lower.preimage] == LooseState::Loose,
    state.loose[higher.preimage] == LooseState::Loose,
  ) {
    (false, false) => None,
    (true, false) => Some(lower.preimage),
    (false, true) => Some(higher.preimage),
    (true, true) =>
      if lower.distance < higher.distance {
        Some(lower.preimage)
      } else {
        Some(higher.preimage)
      },
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Neighbor {
  preimage: usize,
  distance: i16,
}

fn nearest_light_neighbors(step: u8, lit_steps: &[u8; 12]) -> (Neighbor, Neighbor) {
  let mut lower = Neighbor { preimage: 0, distance: EDO };
  let mut higher = Neighbor { preimage: 0, distance: EDO };
  for (preimage, lit) in lit_steps.iter().enumerate() {
    let lower_distance = (step as i16 - *lit as i16).rem_euclid(EDO);
    if lower_distance > 0 && lower_distance < lower.distance {
      lower = Neighbor { preimage, distance: lower_distance };
    }
    let higher_distance = (*lit as i16 - step as i16).rem_euclid(EDO);
    if higher_distance > 0 && higher_distance < higher.distance {
      higher = Neighbor { preimage, distance: higher_distance };
    }
  }
  (lower, higher)
}

fn move_delta(from: u8, to: u8, lit_steps: &[u8; 12]) -> Option<i16> {
  if from == to {
    return Some(0);
  }
  let cw = (to as i16 - from as i16).rem_euclid(EDO);
  let ccw = (from as i16 - to as i16).rem_euclid(EDO);
  if cw < ccw {
    let blocked = lit_steps.iter().any(|step| {
      let d = (*step as i16 - from as i16).rem_euclid(EDO);
      d > 0 && d < cw
    });
    if blocked { None } else { Some(cw) }
  } else {
    let blocked = lit_steps.iter().any(|step| {
      let d = (from as i16 - *step as i16).rem_euclid(EDO);
      d > 0 && d < ccw
    });
    if blocked { None } else { Some(-ccw) }
  }
}

fn grid_step(x: i32, y: i32) -> u8 {
  ((6 * x + y).rem_euclid(EDO as i32)) as u8
}

fn is_grid_cell(x: i32, y: i32) -> bool {
  x >= 0 && x < GRID_W && y >= 0 && y < GRID_H
}

fn pitch_name(pc: usize) -> &'static str {
  [
    "C", "C#", "D", "D#", "E", "F",
    "F#", "G", "G#", "A", "A#", "B",
  ][pc]
}

#[cfg(test)]
mod tests {
  use super::*;

  fn no_sounding() -> [u16; 31] {
    [0; 31]
  }

  fn phases(sounding_on: bool, anchor_on: bool, image_on: bool) -> LedPhases {
    LedPhases { sounding_on, anchor_on, image_on }
  }

  #[test]
  fn grid_geometry_matches_requested_axes() {
    assert_eq!(grid_step(0, 0), 0);
    assert_eq!(grid_step(1, 0), 6);
    assert_eq!(grid_step(0, 1), 1);
  }

  #[test]
  fn initial_map_matches_even_31_edo_spacing() {
    assert_eq!(Edo31State::new().map, INITIAL_MAP);
  }

  #[test]
  fn lit_press_makes_preimage_loose_without_moving() {
    let mut state = Edo31State::new();

    assert!(apply_grid_press(&mut state, 0, 0));

    assert_eq!(state.loose[0], LooseState::Loose);
    assert_eq!(state.map, INITIAL_MAP);
  }

  #[test]
  fn dark_press_has_no_effect_when_no_preimage_is_loose() {
    let mut state = Edo31State::new();

    assert!(!apply_grid_press(&mut state, 0, 1));

    assert_eq!(state.map[0], 0);
  }

  #[test]
  fn dark_press_moves_loose_neighbor_and_fixes_it() {
    let mut state = Edo31State::new();
    state.loose[0] = LooseState::Loose;

    assert!(apply_grid_press(&mut state, 0, 1));

    assert_eq!(state.map[0], 1);
    assert_eq!(state.deltas[0], 1);
    assert_eq!(state.loose[0], LooseState::Fixed);
  }

  #[test]
  fn dark_press_moves_farther_neighbor_if_only_farther_neighbor_is_loose() {
    let mut state = Edo31State::new();
    state.loose[1] = LooseState::Loose;

    assert!(apply_grid_press(&mut state, 0, 1));

    assert_eq!(state.map[0], 0);
    assert_eq!(state.map[1], 1);
    assert_eq!(state.deltas[1], -2);
    assert_eq!(state.loose[1], LooseState::Fixed);
  }

  #[test]
  fn dark_press_chooses_higher_neighbor_on_tie() {
    let mut state = Edo31State::new();
    state.loose[0] = LooseState::Loose;
    state.loose[1] = LooseState::Loose;

    assert!(apply_grid_press(&mut state, 0, 2));

    assert_eq!(state.map[0], 0);
    assert_eq!(state.map[1], 2);
    assert_eq!(state.deltas[1], -1);
    assert_eq!(state.loose[0], LooseState::Loose);
    assert_eq!(state.loose[1], LooseState::Fixed);
  }

  #[test]
  fn edo31_note_uses_current_pitch_class_mapping() {
    let state = Arc::new(Mutex::new(Edo31State::new()));
    {
      let mut state = state.lock().unwrap();
      state.map[0] = 1;
      state.deltas[0] = 1;
    }

    let (_channel, note) = edo31_instruction(60, &state);

    assert_eq!(note, MIN_NOTE_OUT as i16 + 27);
  }

  #[test]
  fn move_delta_uses_shorter_arc_before_checking_blockers() {
    assert_eq!(move_delta(0, 30, &INITIAL_MAP), Some(-1));
    assert_eq!(move_delta(0, 4, &INITIAL_MAP), None);
  }

  #[test]
  fn lowering_c_across_display_boundary_lowers_one_31_edo_step() {
    let state = Arc::new(Mutex::new(Edo31State::new()));
    let (default_channel, default_note) = edo31_instruction(60, &state);
    {
      let mut state = state.lock().unwrap();
      state.map[0] = 30;
      state.deltas[0] = -1;
    }

    let (channel, note) = edo31_instruction(60, &state);

    assert_eq!(channel, default_channel);
    assert_eq!(note, default_note - 1);
  }

  #[test]
  fn default_middle_octave_mapping_is_monotone_across_c_boundary() {
    let state = Arc::new(Mutex::new(Edo31State::new()));
    let encoded: Vec<i16> = (60..72)
      .map(|note| {
        let (channel, midi_note) = edo31_instruction(note, &state);
        (channel - MIN_CHANNEL_OUT as i16) * EDO
          + (midi_note - MIN_NOTE_OUT as i16)
      })
      .collect();

    assert_eq!(encoded, vec![88, 90, 93, 96, 98, 101, 103, 106, 109, 111, 114, 116]);
  }

  #[test]
  fn render_hides_anchor_preimages_during_anchor_off_phase() {
    let state = Edo31State::new();
    let sounding = no_sounding();

    let on_cols = render_led_cols(&state, &sounding, phases(true, true, true));
    let off_cols = render_led_cols(&state, &sounding, phases(true, false, true));

    assert_ne!(on_cols[0] & 1, 0);
    assert_eq!(off_cols[0] & 1, 0);
    assert_ne!(on_cols[1] & (1 << 7), 0);
    assert_eq!(off_cols[1] & (1 << 7), 0);
    assert_ne!(on_cols[3] & 1, 0);
    assert_eq!(off_cols[3] & 1, 0);
  }

  #[test]
  fn render_hides_non_anchor_image_during_image_off_phase() {
    let state = Edo31State::new();
    let sounding = no_sounding();

    let off_cols = render_led_cols(&state, &sounding, phases(true, true, false));

    assert_eq!(off_cols[0] & (1 << 3), 0);
  }

  #[test]
  fn render_keeps_non_anchor_image_during_image_on_phase() {
    let state = Edo31State::new();
    let sounding = no_sounding();

    let on_cols = render_led_cols(&state, &sounding, phases(true, true, true));

    assert_ne!(on_cols[0] & (1 << 3), 0);
  }

  #[test]
  fn duty_clock_schedules_off_from_actual_on_time() {
    let start = Instant::now();
    let mut clock = ColorClock::new(IMAGE_COLOR, start);
    let (on_duration, off_duration) = IMAGE_COLOR.duty_durations().unwrap();

    assert!(clock.is_on());
    assert_eq!(clock.wait(start), Some(on_duration));
    assert!(clock.advance_if_due(start + on_duration + Duration::from_micros(200)));
    assert!(!clock.is_on());
    assert_eq!(
      clock.wait(start + on_duration + Duration::from_micros(200)),
      Some(off_duration),
    );
  }

  #[test]
  fn refresh_wait_uses_next_scheduled_color_transition() {
    let start = Instant::now();
    let sounding_clock = ColorClock::new(SOUNDING_COLOR, start);
    let anchor_clock = ColorClock::new(ANCHOR_COLOR, start);
    let image_clock = ColorClock::new(IMAGE_COLOR, start);
    let (image_on, _) = IMAGE_COLOR.duty_durations().unwrap();

    assert_eq!(
      next_render_wait(start, sounding_clock, anchor_clock, image_clock),
      image_on.min(MONOME_REFRESH),
    );
    assert_eq!(
      next_render_wait(
        start + image_on,
        sounding_clock,
        anchor_clock,
        image_clock,
      ),
      Duration::ZERO,
    );
  }

  #[test]
  fn sounding_pitch_clobbers_anchor_and_image_off_phases() {
    let mut state = Edo31State::new();
    state.map[0] = 30;
    let mut sounding = no_sounding();
    sounding[30] = 1;
    sounding[3] = 1;

    let off_cols = render_led_cols(&state, &sounding, phases(true, false, false));

    assert_ne!(off_cols[5] & 1, 0);
    assert_ne!(off_cols[0] & (1 << 3), 0);
  }

  #[test]
  fn sounding_state_survives_map_change_until_note_off() {
    let state = Arc::new(Mutex::new(Edo31State::new()));
    let sounding = Arc::new(Mutex::new(SoundingState::new()));

    update_sounding(&[0x90, 60, 100], &state, &sounding);
    state.lock().unwrap().map[0] = 30;

    assert_eq!(sounding.lock().unwrap().counts[0], 1);
    assert_eq!(sounding.lock().unwrap().counts[30], 0);
    update_sounding(&[0x80, 60, 64], &state, &sounding);
    assert_eq!(sounding.lock().unwrap().counts[0], 0);
  }

  #[test]
  fn sounding_state_uses_displayed_pitch_class_not_output_residue() {
    let state = Arc::new(Mutex::new(Edo31State::new()));
    let sounding = Arc::new(Mutex::new(SoundingState::new()));

    update_sounding(&[0x90, 62, 100], &state, &sounding);

    assert_eq!(sounding.lock().unwrap().counts[INITIAL_MAP[2] as usize], 1);
    assert_eq!(sounding.lock().unwrap().counts[0], 0);
  }
}
