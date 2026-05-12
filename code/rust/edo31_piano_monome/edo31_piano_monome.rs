// VOCAB:
// The map maps the 'preimage' (all of 12-edo) to the 'image'
// (a 12-note subset of 31-edo).
// The terms 'image' and 'preimage'
// are sometimes used that way in the code.

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use midi_pulse::{midi, monome, piano_transform};
use rosc::{decoder, OscPacket, OscType};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use std::{io, thread};

const LOWEST_C        : u8 = 24;
const MIN_CHANNEL_OUT : u8 = 1;
const MIN_NOTE_OUT    : u8 = 28;

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
// Keep this no larger than the shortest on-window you want rendered.
const MONOME_REFRESH: Duration = Duration::from_millis(1);

const ANCHOR_PITCH_CLASSES: [usize; 3] = [0, 5, 7];
const SOUNDING_COLOR: Color = Color::AlwaysOn;
const ANCHOR_COLOR: Color = Color::Duty {
  period: Duration::from_micros(300_000),
  fraction_on: 0.3,
};
// A color for one of the 12 values of EDO that the keyboard maps to.
// This uses hardware level brightness instead of PWM; sub-millisecond
// PWM was visibly erratic through serialosc on the monome 256.
const IMAGE_COLOR: Color = Color::AlwaysOn;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static LED_TRACE_STARTED: OnceLock<Instant> = OnceLock::new();

#[derive(Clone)]
struct Edo31State {
  config: EdoConfig,
  map: [i16; 12],
  deltas: [i16; 12],
  loose: [LooseState; 12],
}

#[derive(Clone)]
struct EdoConfig {
  lowest_hz: f64,
  edo: i16,
  x_step: i16,
  y_step: i16,
  remap_idiom: RemapIdiom,
  grid_w: i32,
  grid_h: i32,
  initial_map: [i16; 12],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemapIdiom {
  Loose,
  Snap,
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

impl EdoConfig {
  fn default() -> Self {
    EdoConfig::new(
      DEFAULT_LOWEST_HZ,
      DEFAULT_EDO,
      DEFAULT_X_STEP,
      DEFAULT_Y_STEP,
      RemapIdiom::Snap,
      DEFAULT_GRID_W,
      DEFAULT_GRID_H,
    )
  }

  fn new(
    lowest_hz: f64,
    edo: i16,
    x_step: i16,
    y_step: i16,
    remap_idiom: RemapIdiom,
    grid_w: i32,
    grid_h: i32,
  ) -> Self {
    EdoConfig {
      lowest_hz,
      edo,
      x_step,
      y_step,
      remap_idiom,
      grid_w,
      grid_h,
      initial_map: evenly_spaced_map(edo),
    }
  }

  fn with_grid_size(&self, grid_w: i32, grid_h: i32) -> Self {
    EdoConfig {
      lowest_hz: self.lowest_hz,
      edo: self.edo,
      x_step: self.x_step,
      y_step: self.y_step,
      remap_idiom: self.remap_idiom,
      grid_w,
      grid_h,
      initial_map: self.initial_map,
    }
  }
}

impl Edo31State {
  fn new(config: EdoConfig) -> Self {
    Edo31State {
      map: config.initial_map,
      config,
      deltas: [0; 12],
      loose: [LooseState::Fixed; 12],
    }
  }
}

struct SoundingState {
  by_original_note: HashMap<u8, i16>,
  counts: Vec<u16>,
}

impl SoundingState {
  fn new(edo: i16) -> Self {
    SoundingState {
      by_original_note: HashMap::new(),
      counts: vec![0; edo as usize],
    }
  }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let config = parse_config()?;
  let listen_port = configured_listen_port()?;
  let state: Arc<Mutex<Edo31State>> =
    Arc::new(Mutex::new(Edo31State::new(config.clone())));
  let sounding: Arc<Mutex<SoundingState>> =
    Arc::new(Mutex::new(SoundingState::new(config.edo)));
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
        print_note_on_trace(message, &msg);
        let _ = tx.send(msg);
      }
    },
    (),
  )?;

  let state_for_monome = Arc::clone(&state);
  let sounding_for_monome = Arc::clone(&sounding);
  let monome_thread: thread::JoinHandle<()> =
    thread::spawn(move || {
      run_monome_thread(state_for_monome, sounding_for_monome, listen_port)
    });

  install_sigint_handler();
  print_startup_message(&config);
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

fn parse_config() -> Result<EdoConfig, Box<dyn std::error::Error>> {
  let args: Vec<String> = std::env::args().skip(1).collect();
  if args.len() > 1 {
    return Err("usage: edo31_piano_monome [CONFIGS_FILE]".into());
  }
  let name = args.first().map(String::as_str).unwrap_or("default");
  load_config(name)
}

#[derive(Deserialize)]
struct ConfigToml {
  lowest_hz: f64,
  edo: i16,
  between_columns: i16,
  between_rows: i16,
  #[serde(default)]
  remap_idiom: ConfigRemapIdiom,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConfigRemapIdiom {
  Snap,
  Loose,
  Loosen,
}

impl Default for ConfigRemapIdiom {
  fn default() -> Self {
    ConfigRemapIdiom::Snap
  }
}

impl ConfigRemapIdiom {
  fn into_runtime(self) -> RemapIdiom {
    match self {
      ConfigRemapIdiom::Snap => RemapIdiom::Snap,
      ConfigRemapIdiom::Loose | ConfigRemapIdiom::Loosen => RemapIdiom::Loose,
    }
  }
}

fn load_config(name: &str) -> Result<EdoConfig, Box<dyn std::error::Error>> {
  if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
    return Err(format!("config file name must not be a path, got {name:?}").into());
  }
  let path = config_path(name);
  let source = std::fs::read_to_string(&path)
    .or_else(|original_error| {
      if path.extension().is_none() {
        let toml_path = path.with_extension("toml");
        std::fs::read_to_string(&toml_path).map_err(|_| original_error)
      } else {
        Err(original_error)
      }
    })
    .map_err(|e| format!("read config {name:?}: {e}"))?;
  let parsed: ConfigToml = toml::from_str(&source)
    .map_err(|e| format!("parse config {name:?}: {e}"))?;
  validate_config_toml(&parsed)?;
  Ok(EdoConfig::new(
    parsed.lowest_hz,
    parsed.edo,
    parsed.between_columns,
    parsed.between_rows,
    parsed.remap_idiom.into_runtime(),
    DEFAULT_GRID_W,
    DEFAULT_GRID_H,
  ))
}

fn config_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join(CONFIGS_DIR)
    .join(name)
}

fn validate_config_toml(config: &ConfigToml) -> Result<(), Box<dyn std::error::Error>> {
  if !config.lowest_hz.is_finite() || config.lowest_hz <= 0.0 {
    return Err(format!("lowest_hz must be positive, got {}", config.lowest_hz).into());
  }
  if config.edo <= 0 {
    return Err(format!("edo must be positive, got {}", config.edo).into());
  }
  Ok(())
}

fn configured_listen_port() -> Result<u16, Box<dyn std::error::Error>> {
  let Some(port) = std::env::var_os(LISTEN_PORT_ENV) else {
    return Ok(LISTEN_PORT);
  };
  let port = port.to_string_lossy();
  let value = port.parse::<u16>()
    .map_err(|_| format!("{LISTEN_PORT_ENV} must be a UDP port, got {port:?}"))?;
  if value == 0 {
    return Err(format!("{LISTEN_PORT_ENV} must be nonzero").into());
  }
  Ok(value)
}

fn evenly_spaced_map(edo: i16) -> [i16; 12] {
  let mut map = [0; 12];
  for (i, slot) in map.iter_mut().enumerate() {
    *slot = ((i as f64 * edo as f64 / 12.0).round() as i16).rem_euclid(edo);
  }
  map
}

fn install_sigint_handler() {
  extern "C" fn handler(_: i32) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
  }
  unsafe {
    libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
  }
}

fn print_startup_message(config: &EdoConfig) {
  println!("{}-EDO piano transformer with monome mapping started!", config.edo);
  println!();
  println!("Virtual ports created:");
  println!("  - 'edo31_piano_monome-in:in' (input)");
  println!("  - 'edo31_piano_monome-out:out' (output)");
  println!();
  println!("Monome {}-EDO map:", config.edo);
  println!("  - lowest Hz: {}", config.lowest_hz);
  println!("  - remap idiom: {}", remap_idiom_name(config.remap_idiom));
  println!(
    "  - each key's pitch is {}*x + {}*y mod {}",
    config.x_step,
    config.y_step,
    config.edo,
  );
  println!("  - initial map: {:?}", config.initial_map);
  println!("  - sounding pitches stay lit");
  println!("  - C, F and G flash as anchors");
  println!("  - other image pitches are steady dim");
  match config.remap_idiom {
    RemapIdiom::Loose => {
      println!("  - tap lit pitches to loosen them");
      println!("  - tap a dark pitch to move a neighboring loose pitch");
    }
    RemapIdiom::Snap => {
      println!("  - tap a dark pitch to snap the nearest image to it");
    }
  }
  println!();
  println!("Press Enter to exit...");
}

fn remap_idiom_name(remap_idiom: RemapIdiom) -> &'static str {
  match remap_idiom {
    RemapIdiom::Loose => "loose",
    RemapIdiom::Snap => "snap",
  }
}

fn edo31_instruction(
  original_note: u8,
  state: &Arc<Mutex<Edo31State>>,
) -> (i16, i16) {
  let absolute_step = edo31_absolute_step(original_note, state);
  let edo = state.lock().unwrap().config.edo;
  let channel = MIN_CHANNEL_OUT as i16 + absolute_step.div_euclid(edo);
  let note = MIN_NOTE_OUT as i16 + absolute_step.rem_euclid(edo);
  (channel, note)
}

fn edo31_absolute_step(
  original_note: u8,
  state: &Arc<Mutex<Edo31State>>,
) -> i16 {
  let normalized = original_note as i16 - LOWEST_C as i16;
  let channel_offset = normalized.div_euclid(12);
  let pitch_class = original_note % 12;
  let state = state.lock().unwrap();
  let pc = pitch_class as usize;
  channel_offset * state.config.edo
    + state.config.initial_map[pc]
    + state.deltas[pc]
}

fn print_note_on_trace(input: &[u8], output: &[u8]) {
  if input.len() < 3 || output.len() < 3 {
    return;
  }
  if !midi::is_note_on(input) || !midi::is_note_on(output) {
    return;
  }
  let input_note = input[1];
  let input_pc = input_note % 12;
  let output_channel = (output[0] & 0x0f) + 1;
  let output_note = output[1];
  println!(
    "note-on: input <{}, {}> -> output ({}, {})",
    input_note,
    input_pc,
    output_channel,
    output_note,
  );
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
) -> i16 {
  let pitch_class = (original_note % 12) as usize;
  state.lock().unwrap().map[pitch_class]
}

fn decrement_sounding_count(sounding: &mut SoundingState, step: i16) {
  let count = &mut sounding.counts[step as usize];
  if *count > 0 {
    *count -= 1;
  }
}

fn run_monome_thread(
  state: Arc<Mutex<Edo31State>>,
  sounding: Arc<Mutex<SoundingState>>,
  listen_port: u16,
) {
  let sock = UdpSocket::bind(("0.0.0.0", listen_port))
    .unwrap_or_else(|e| panic!("bind UDP :{listen_port}: {e}"));
  sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
  let mut device_info =
    monome::discover_device_info(&sock, listen_port)
      .expect("no monome found; is serialoscd running?");
  {
    let mut state = state.lock().unwrap();
    state.config = state.config.with_grid_size(device_info.grid_w, device_info.grid_h);
  }
  eprintln!(
    "monome: id={} type={} port={} size={}x{}",
    device_info.id,
    device_info.type_name,
    device_info.port,
    device_info.grid_w,
    device_info.grid_h,
  );
  let mut device: SocketAddr = format!("127.0.0.1:{}", device_info.port).parse().unwrap();
  monome::register(&sock, device, PREFIX, listen_port);
  let mut rendered_cols = blank_rendered_cols(&state.lock().unwrap().config);
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
        if p != device_info.port {
          device_info.port = p;
          device = format!("127.0.0.1:{p}").parse().unwrap();
          monome::register(&sock, device, PREFIX, listen_port);
          rendered_cols = blank_rendered_cols(&state.lock().unwrap().config);
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
    if s != 1 || !is_grid_cell(&state.lock().unwrap().config, x, y) {
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

fn blank_rendered_cols(config: &EdoConfig) -> Vec<u8> {
  vec![0; (config.grid_w * config.grid_h) as usize]
}

#[cfg(test)]
fn col_bank_count(config: &EdoConfig) -> usize {
  ((config.grid_h + 7) / 8) as usize
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
  sounding_counts: &[u16],
  phases: LedPhases,
  rendered_cols: &mut Vec<u8>,
) {
  let levels = render_led_levels(state, sounding_counts, phases);
  let trace_leds = led_trace_enabled();
  if rendered_cols.len() != levels.len() {
    *rendered_cols = vec![0; levels.len()];
  }
  for (i, level) in levels.iter().enumerate() {
    if rendered_cols[i] != *level {
      let x = i as i32 % state.config.grid_w;
      let y = i as i32 / state.config.grid_w;
      if trace_leds {
        let trace_started = LED_TRACE_STARTED.get_or_init(Instant::now);
        eprintln!(
          "led {:.3}ms x={x:02} y={y:02} level={level:02} old={:02}",
          trace_started.elapsed().as_secs_f64() * 1000.0,
          rendered_cols[i],
        );
      }
      monome::send_led_level_set(sock, device, PREFIX, x, y, *level as i32);
      rendered_cols[i] = *level;
    }
  }
}

fn led_trace_enabled() -> bool {
  std::env::var_os(LED_TRACE_ENV).is_some()
}

#[cfg(test)]
fn render_led_cols(
  state: &Edo31State,
  sounding_counts: &[u16],
  phases: LedPhases,
) -> Vec<u8> {
  let banks = col_bank_count(&state.config);
  let mut cols = vec![0u8; state.config.grid_w as usize * banks];
  let levels = render_led_levels(state, sounding_counts, phases);
  for y in 0..state.config.grid_h {
    for x in 0..state.config.grid_w {
      if levels[(y * state.config.grid_w + x) as usize] > 0 {
        let i = x as usize * banks + (y as usize / 8);
        cols[i] |= 1u8 << (y % 8);
      }
    }
  }
  cols
}

fn render_led_levels(
  state: &Edo31State,
  sounding_counts: &[u16],
  phases: LedPhases,
) -> Vec<u8> {
  let mut levels = vec![LED_LEVEL_OFF; (state.config.grid_w * state.config.grid_h) as usize];
  let rect = map_rect(&state.config);
  for y in rect.y0..rect.y1 {
    for x in rect.x0..rect.x1 {
      let step = grid_step(&state.config, x - rect.x0, y - rect.y0);
      levels[(y * state.config.grid_w + x) as usize] =
        rendered_level(state, sounding_counts, step, phases);
    }
  }
  levels
}

fn rendered_level(
  state: &Edo31State,
  sounding_counts: &[u16],
  step: i16,
  phases: LedPhases,
) -> u8 {
  if sounding_counts[step as usize] > 0 {
    return if phases.sounding_on { LED_LEVEL_FULL } else { LED_LEVEL_OFF };
  }
  if let Some(preimage) = preimage_for_step(state, step) {
    if is_anchor_pitch_class(preimage) {
      if phases.anchor_on { LED_LEVEL_FULL } else { LED_LEVEL_OFF }
    } else {
      if phases.image_on { LED_LEVEL_IMAGE } else { LED_LEVEL_OFF }
    }
  } else {
    LED_LEVEL_OFF
  }
}

fn is_anchor_pitch_class(preimage: usize) -> bool {
  ANCHOR_PITCH_CLASSES.contains(&preimage)
}

fn apply_grid_press(state: &mut Edo31State, x: i32, y: i32) -> bool {
  let Some((local_x, local_y)) = map_local_cell(&state.config, x, y) else {
    return false;
  };
  let step = grid_step(&state.config, local_x, local_y);
  match state.config.remap_idiom {
    RemapIdiom::Loose => apply_loose_grid_press(state, step),
    RemapIdiom::Snap => apply_snap_grid_press(state, step),
  }
}

fn apply_loose_grid_press(state: &mut Edo31State, step: i16) -> bool {
  if let Some(preimage) = preimage_for_step(state, step) {
    state.loose[preimage] = LooseState::Loose;
    eprintln!("loose {} -> {step}", pitch_name(preimage));
    return true;
  }
  let Some(preimage) = loose_neighbor_for_dark_step(step, state) else {
    return false;
  };
  move_preimage_to_step(state, preimage, step)
}

fn apply_snap_grid_press(state: &mut Edo31State, step: i16) -> bool {
  if preimage_for_step(state, step).is_some() {
    return false;
  }
  let (lower, higher) = nearest_light_neighbors(step, &state.map, state.config.edo);
  let preimage = if lower.distance < higher.distance {
    lower.preimage
  } else {
    higher.preimage
  };
  move_preimage_to_step(state, preimage, step)
}

fn move_preimage_to_step(state: &mut Edo31State, preimage: usize, step: i16) -> bool {
  let current = state.map[preimage];
  let Some(delta) = move_delta(current, step, &state.map, state.config.edo) else {
    return false;
  };
  state.map[preimage] = step;
  state.deltas[preimage] += delta;
  state.loose[preimage] = LooseState::Fixed;
  eprintln!("moved {}: {current} -> {step}", pitch_name(preimage));
  true
}

fn preimage_for_step(state: &Edo31State, step: i16) -> Option<usize> {
  state.map.iter().position(|s| *s == step)
}

fn loose_neighbor_for_dark_step(step: i16, state: &Edo31State) -> Option<usize> {
  let (lower, higher) = nearest_light_neighbors(step, &state.map, state.config.edo);
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

fn nearest_light_neighbors(step: i16, lit_steps: &[i16; 12], edo: i16) -> (Neighbor, Neighbor) {
  let mut lower = Neighbor { preimage: 0, distance: edo };
  let mut higher = Neighbor { preimage: 0, distance: edo };
  for (preimage, lit) in lit_steps.iter().enumerate() {
    let lower_distance = (step - *lit).rem_euclid(edo);
    if lower_distance > 0 && lower_distance < lower.distance {
      lower = Neighbor { preimage, distance: lower_distance };
    }
    let higher_distance = (*lit - step).rem_euclid(edo);
    if higher_distance > 0 && higher_distance < higher.distance {
      higher = Neighbor { preimage, distance: higher_distance };
    }
  }
  (lower, higher)
}

fn move_delta(from: i16, to: i16, lit_steps: &[i16; 12], edo: i16) -> Option<i16> {
  if from == to {
    return Some(0);
  }
  let cw = (to - from).rem_euclid(edo);
  let ccw = (from - to).rem_euclid(edo);
  if cw < ccw {
    let blocked = lit_steps.iter().any(|step| {
      let d = (*step - from).rem_euclid(edo);
      d > 0 && d < cw
    });
    if blocked { None } else { Some(cw) }
  } else {
    let blocked = lit_steps.iter().any(|step| {
      let d = (from - *step).rem_euclid(edo);
      d > 0 && d < ccw
    });
    if blocked { None } else { Some(-ccw) }
  }
}

fn grid_step(config: &EdoConfig, x: i32, y: i32) -> i16 {
  ((config.x_step as i32 * x + config.y_step as i32 * y)
    .rem_euclid(config.edo as i32)) as i16
}

fn is_grid_cell(config: &EdoConfig, x: i32, y: i32) -> bool {
  map_local_cell(config, x, y).is_some()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GridRect {
  x0: i32,
  y0: i32,
  x1: i32,
  y1: i32,
}

fn map_rect(config: &EdoConfig) -> GridRect {
  let w = MAP_W.min(config.grid_w).max(0);
  GridRect {
    x0: 0,
    y0: 0,
    x1: w,
    y1: config.grid_h,
  }
}

fn map_local_cell(config: &EdoConfig, x: i32, y: i32) -> Option<(i32, i32)> {
  let rect = map_rect(config);
  if x >= rect.x0 && x < rect.x1 && y >= rect.y0 && y < rect.y1 {
    Some((x - rect.x0, y - rect.y0))
  } else {
    None
  }
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

  fn test_config() -> EdoConfig {
    EdoConfig::new(
      DEFAULT_LOWEST_HZ,
      DEFAULT_EDO,
      DEFAULT_X_STEP,
      DEFAULT_Y_STEP,
      RemapIdiom::Loose,
      DEFAULT_GRID_W,
      DEFAULT_GRID_H,
    )
  }

  fn test_state() -> Edo31State {
    Edo31State::new(test_config())
  }

  fn snap_state() -> Edo31State {
    Edo31State::new(EdoConfig::default())
  }

  fn test_state_arc() -> Arc<Mutex<Edo31State>> {
    Arc::new(Mutex::new(test_state()))
  }

  fn no_sounding() -> Vec<u16> {
    vec![0; test_config().edo as usize]
  }

  fn phases(sounding_on: bool, anchor_on: bool, image_on: bool) -> LedPhases {
    LedPhases { sounding_on, anchor_on, image_on }
  }

  fn encoded_output_step(channel: i16, note: i16, edo: i16) -> i16 {
    (channel - MIN_CHANNEL_OUT as i16) * edo
      + (note - MIN_NOTE_OUT as i16)
  }

  #[test]
  fn grid_geometry_matches_requested_axes() {
    let config = test_config();
    assert_eq!(grid_step(&config, 0, 0), 0);
    assert_eq!(grid_step(&config, 1, 0), 6);
    assert_eq!(grid_step(&config, 0, 1), 1);
  }

  #[test]
  fn map_rect_uses_all_rows_in_leftmost_ten_columns() {
    let config = EdoConfig::new(80.0, 58, 8, 1, RemapIdiom::Snap, 16, 16);

    assert_eq!(
      map_rect(&config),
      GridRect { x0: 0, y0: 0, x1: 10, y1: 16 },
    );
    assert_eq!(map_local_cell(&config, 0, 0), Some((0, 0)));
    assert_eq!(map_local_cell(&config, 9, 15), Some((9, 15)));
    assert_eq!(map_local_cell(&config, 10, 15), None);
  }

  #[test]
  fn initial_map_matches_even_31_edo_spacing() {
    assert_eq!(test_state().map, [0, 3, 5, 8, 10, 13, 16, 18, 21, 23, 26, 28]);
  }

  #[test]
  fn initial_map_generalizes_to_58_edo() {
    assert_eq!(
      evenly_spaced_map(58),
      [0, 5, 10, 15, 19, 24, 29, 34, 39, 44, 48, 53],
    );
  }

  #[test]
  fn default_config_load_58_8_5_snap() {
    let config = load_config("default").unwrap();

    assert_eq!(config.lowest_hz, 80.0);
    assert_eq!(config.edo, 58);
    assert_eq!(config.x_step, 8);
    assert_eq!(config.y_step, 5);
    assert_eq!(config.remap_idiom, RemapIdiom::Snap);
  }

  #[test]
  fn loose_config_accept_loose_alias() {
    let config = load_config("31-loose.toml").unwrap();

    assert_eq!(config.edo, 31);
    assert_eq!(config.x_step, 6);
    assert_eq!(config.y_step, 1);
    assert_eq!(config.remap_idiom, RemapIdiom::Loose);
  }

  #[test]
  fn lit_press_makes_preimage_loose_without_moving() {
    let mut state = test_state();
    let initial_map = state.config.initial_map;
    let y0 = map_rect(&state.config).y0;

    assert!(apply_grid_press(&mut state, 0, y0));

    assert_eq!(state.loose[0], LooseState::Loose);
    assert_eq!(state.map, initial_map);
  }

  #[test]
  fn dark_press_has_no_effect_when_no_preimage_is_loose() {
    let mut state = test_state();
    let y = map_rect(&state.config).y0 + 1;

    assert!(!apply_grid_press(&mut state, 0, y));

    assert_eq!(state.map[0], 0);
  }

  #[test]
  fn dark_press_moves_loose_neighbor_and_fixes_it() {
    let mut state = test_state();
    state.loose[0] = LooseState::Loose;
    let y = map_rect(&state.config).y0 + 1;

    assert!(apply_grid_press(&mut state, 0, y));

    assert_eq!(state.map[0], 1);
    assert_eq!(state.deltas[0], 1);
    assert_eq!(state.loose[0], LooseState::Fixed);
  }

  #[test]
  fn dark_press_moves_farther_neighbor_if_only_farther_neighbor_is_loose() {
    let mut state = test_state();
    state.loose[1] = LooseState::Loose;
    let y = map_rect(&state.config).y0 + 1;

    assert!(apply_grid_press(&mut state, 0, y));

    assert_eq!(state.map[0], 0);
    assert_eq!(state.map[1], 1);
    assert_eq!(state.deltas[1], -2);
    assert_eq!(state.loose[1], LooseState::Fixed);
  }

  #[test]
  fn dark_press_chooses_higher_neighbor_on_tie() {
    let mut state = test_state();
    state.loose[0] = LooseState::Loose;
    state.loose[1] = LooseState::Loose;
    let y = map_rect(&state.config).y0 + 2;

    assert!(apply_grid_press(&mut state, 0, y));

    assert_eq!(state.map[0], 0);
    assert_eq!(state.map[1], 2);
    assert_eq!(state.deltas[1], -1);
    assert_eq!(state.loose[0], LooseState::Loose);
    assert_eq!(state.loose[1], LooseState::Fixed);
  }

  #[test]
  fn snap_dark_press_moves_nearest_image_without_loose_state() {
    let mut state = snap_state();
    let y = map_rect(&state.config).y0 + 1;

    assert!(apply_grid_press(&mut state, 0, y));

    assert_eq!(state.map[0], 1);
    assert_eq!(state.deltas[0], 1);
    assert_eq!(state.loose[0], LooseState::Fixed);
  }

  #[test]
  fn snap_tie_moves_higher_image_down() {
    let mut state = snap_state();
    state.map[1] = 4;
    let y = map_rect(&state.config).y0 + 2;

    assert!(apply_grid_press(&mut state, 0, y));

    assert_eq!(state.map[0], 0);
    assert_eq!(state.map[1], 2);
    assert_eq!(state.deltas[1], -2);
  }

  #[test]
  fn edo31_note_uses_current_pitch_class_mapping() {
    let state = test_state_arc();
    {
      let mut state = state.lock().unwrap();
      state.map[0] = 1;
      state.deltas[0] = 1;
    }

    let (_channel, note) = edo31_instruction(60, &state);

    assert_eq!(note, MIN_NOTE_OUT as i16 + 1);
  }

  #[test]
  fn move_delta_uses_shorter_arc_before_checking_blockers() {
    let initial_map = test_config().initial_map;
    assert_eq!(move_delta(0, 30, &initial_map, 31), Some(-1));
    assert_eq!(move_delta(0, 4, &initial_map, 31), None);
  }

  #[test]
  fn lowering_c_across_display_boundary_lowers_one_31_edo_step() {
    let state = test_state_arc();
    let edo = test_config().edo;
    let (default_channel, default_note) = edo31_instruction(60, &state);
    {
      let mut state = state.lock().unwrap();
      state.map[0] = 30;
      state.deltas[0] = -1;
    }

    let (channel, note) = edo31_instruction(60, &state);

    assert_eq!(
      encoded_output_step(channel, note, edo),
      encoded_output_step(default_channel, default_note, edo) - 1,
    );
  }

  #[test]
  fn default_middle_octave_mapping_is_monotone_across_c_boundary() {
    let state = test_state_arc();
    let encoded: Vec<i16> = (60..72)
      .map(|note| {
        let (channel, midi_note) = edo31_instruction(note, &state);
        encoded_output_step(channel, midi_note, test_config().edo)
      })
      .collect();

    assert_eq!(encoded, vec![93, 96, 98, 101, 103, 106, 109, 111, 114, 116, 119, 121]);
  }

  #[test]
  fn output_residue_matches_displayed_pitch_class_mapping() {
    let config = EdoConfig::new(80.0, 58, 8, 1, RemapIdiom::Snap, 16, 16);
    let state = Arc::new(Mutex::new(Edo31State::new(config.clone())));

    for note in 48..60 {
      let pitch_class = (note % 12) as usize;
      let (channel, midi_note) = edo31_instruction(note, &state);
      let residue = encoded_output_step(channel, midi_note, config.edo)
        .rem_euclid(config.edo);

      assert_eq!(residue, config.initial_map[pitch_class]);
    }
  }

  #[test]
  fn eb_to_bb_output_interval_matches_58_edo_display_interval() {
    let config = EdoConfig::new(80.0, 58, 8, 1, RemapIdiom::Snap, 16, 16);
    let state = Arc::new(Mutex::new(Edo31State::new(config.clone())));
    let (eb_channel, eb_note) = edo31_instruction(51, &state);
    let (bb_channel, bb_note) = edo31_instruction(58, &state);
    let eb = encoded_output_step(eb_channel, eb_note, config.edo);
    let bb = encoded_output_step(bb_channel, bb_note, config.edo);
    let output_interval = (bb - eb).rem_euclid(config.edo);
    let display_interval = (config.initial_map[10] - config.initial_map[3])
      .rem_euclid(config.edo);

    assert_eq!(output_interval, display_interval);
  }

  #[test]
  fn render_hides_anchor_preimages_during_anchor_off_phase() {
    let state = test_state();
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
    let state = test_state();
    let sounding = no_sounding();

    let off_cols = render_led_cols(&state, &sounding, phases(true, true, false));

    assert_eq!(off_cols[0] & (1 << 3), 0);
  }

  #[test]
  fn render_keeps_non_anchor_image_during_image_on_phase() {
    let state = test_state();
    let sounding = no_sounding();

    let on_cols = render_led_cols(&state, &sounding, phases(true, true, true));

    assert_ne!(on_cols[0] & (1 << 3), 0);
  }

  #[test]
  fn render_uses_two_led_col_banks_for_16_high_grid() {
    let config = EdoConfig::new(80.0, 58, 8, 1, RemapIdiom::Snap, 16, 16);
    let state = Edo31State::new(config.clone());
    let sounding = vec![0; config.edo as usize];

    let cols = render_led_cols(&state, &sounding, phases(true, true, true));

    assert_eq!(cols.len(), 32);
    assert_ne!(cols[0], 0);
    assert_ne!(cols[1], 0);
  }

  #[test]
  fn duty_clock_schedules_off_from_actual_on_time() {
    let color = Color::Duty {
      period: Duration::from_micros(100_000),
      fraction_on: 0.02,
    };
    let start = Instant::now();
    let mut clock = ColorClock::new(color, start);
    let (on_duration, off_duration) = color.duty_durations().unwrap();

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
    let (anchor_on, _) = ANCHOR_COLOR.duty_durations().unwrap();

    assert_eq!(
      next_render_wait(start, sounding_clock, anchor_clock, image_clock),
      anchor_on.min(MONOME_REFRESH),
    );
    assert_eq!(
      next_render_wait(
        start + anchor_on,
        sounding_clock,
        anchor_clock,
        image_clock,
      ),
      Duration::ZERO,
    );
  }

  #[test]
  fn sounding_pitch_clobbers_anchor_and_image_off_phases() {
    let mut state = test_state();
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
    let state = test_state_arc();
    let sounding = Arc::new(Mutex::new(SoundingState::new(test_config().edo)));

    update_sounding(&[0x90, 60, 100], &state, &sounding);
    state.lock().unwrap().map[0] = 30;

    assert_eq!(sounding.lock().unwrap().counts[0], 1);
    assert_eq!(sounding.lock().unwrap().counts[30], 0);
    update_sounding(&[0x80, 60, 64], &state, &sounding);
    assert_eq!(sounding.lock().unwrap().counts[0], 0);
  }

  #[test]
  fn sounding_state_uses_displayed_pitch_class_not_output_residue() {
    let state = test_state_arc();
    let sounding = Arc::new(Mutex::new(SoundingState::new(test_config().edo)));

    update_sounding(&[0x90, 62, 100], &state, &sounding);

    assert_eq!(sounding.lock().unwrap().counts[test_config().initial_map[2] as usize], 1);
    assert_eq!(sounding.lock().unwrap().counts[0], 0);
  }
}
