use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use edo_surface::monome;
use edo_surface::monome_brightness::PulseBrightness;
use edo_surface::rig::Rig;
use edo_surface::midi;
use midi_pulse::piano_transform;
use rosc::{decoder, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use std::{io, thread};

const SHIFT_IN_12_EDO : i8 = -5;
const LOWEST_A        : u8 = 21;
const MIN_CHANNEL_OUT : u8 = 1;
const MIN_NOTE_OUT    : u8 = 28;
const EDO_OVER_12     : u8 = 6;

const PREFIX: &str = "/128-1-cable";
const LISTEN_PORT: u16 = 9000;

const OFFSET_COLS: i32 = 12;
const OFFSET_Y_MIN: i32 = 1;
const OFFSET_Y_MAX: i32 = 7;
const OFFSET_Y_ZERO: i32 = 4;
const ROOT_Y: i32 = 0;
const GROUP_COLS: [i32; 3] = [13, 14, 15];
const GROUP_INTERVALS: [&[usize]; 3] = [
  &[0, 2, 5, 7],
  &[1, 3, 8, 10],
  &[4, 6, 9, 11],
];
const NOTE_FLASH: Duration = Duration::from_millis(30);
const DIM_PULSE: PulseBrightness = PulseBrightness::one_thirty_second(15_000);
const WHITE_KEYS: [bool; 12] = [
  true, false, true,
  false, true, true,
  false, true, false, true, false, true,
];

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

struct MonomeLedState {
  shifts: [i8; 12],
  root: usize,
  group_shifts: [i8; 3],
  note_flash_until: [Option<Instant>; 12],
  dim_columns_on: bool,
}

impl MonomeLedState {
  fn new(shifts: [i8; 12]) -> Self {
    MonomeLedState {
      shifts,
      root: 0,
      group_shifts: [0; 3],
      note_flash_until: [None; 12],
      dim_columns_on: false,
    }
  }

  fn flash_note(&mut self, pitch_class: usize, now: Instant) {
    self.note_flash_until[pitch_class] = Some(now + NOTE_FLASH);
  }

  fn expire_flashes(&mut self, now: Instant) {
    for flash_until in &mut self.note_flash_until {
      if flash_until.is_some_and(|deadline| now >= deadline) {
        *flash_until = None;
      }
    }
  }
}

pub fn run_from_rig(_rig: &Rig) -> Result<(), Box<dyn std::error::Error>> {
  run()
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
  STOP_REQUESTED.store(false, Ordering::Relaxed);
  let shifts: Arc<Mutex<[i8; 12]>> = Arc::new(Mutex::new([0; 12]));
  let ongoing: Arc<Mutex<HashMap<u8, piano_transform::TransformedNote>>> =
    Arc::new(Mutex::new(HashMap::new()));

  let midi_in: MidiInput = MidiInput::new("edo12n_piano_monome-in")?;
  let midi_out: MidiOutput = MidiOutput::new("edo12n_piano_monome-out")?;
  let conn_out: MidiOutputConnection = midi_out.create_virtual("out")?;
  let (tx, rx): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) =
    mpsc::channel();
  let (flash_tx, flash_rx) : (mpsc::Sender<usize>,
                              mpsc::Receiver<usize>)
    = mpsc::channel();
  let _out_thread: thread::JoinHandle<()> =
    thread::spawn(move || midi::run_output_thread(conn_out, rx));

  let shifts_for_midi: Arc<Mutex<[i8; 12]>> = Arc::clone(&shifts);
  let ongoing_for_midi: Arc<Mutex<HashMap<u8, piano_transform::TransformedNote>>> =
    Arc::clone(&ongoing);
  let _conn_in: MidiInputConnection<()> = midi_in.create_virtual(
    "in",
    move |_timestamp: u64, message: &[u8], _: &mut ()| {
      if let Some(pitch_class) = note_on_pitch_class(message) {
        let _ = flash_tx.send(pitch_class);
      }
      for msg in piano_transform::transform_message(
        message,
        &ongoing_for_midi,
        |original_note| edo12n_instruction(original_note, &shifts_for_midi),
      ) {
        let _ = tx.send(msg); }},
    (), )?;

  let shifts_for_monome: Arc<Mutex<[i8; 12]>> = Arc::clone(&shifts);
  let monome_thread: thread::JoinHandle<()> =
    thread::spawn(move || run_monome_thread(shifts_for_monome, flash_rx));

  install_sigint_handler();
  print_startup_message();
  let (quit_tx, quit_rx): (mpsc::Sender<()>, mpsc::Receiver<()>) = mpsc::channel();
  thread::spawn(move || {
    let mut input: String = String::new();
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
  black_monome();
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
  println!("72-EDO piano transformer with monome offsets started!");
  println!();
  println!("Virtual ports created:");
  println!("  - 'edo12n_piano_monome-in:in' (input)");
  println!("  - 'edo12n_piano_monome-out:out' (output)");
  println!();
  println!("Monome offset columns:");
  println!("  - x=0..11 maps C..B");
  println!("  - y=0 selects the root");
  println!("  - y=4 is 0; y=3 is +1; y=5 is -1");
  println!("  - x=13..15 set root-relative group offsets");
  println!("  - black-key columns are flashed at 1/32 duty cycle");
  println!();
  println!("Press Enter to exit...");
}

fn note_on_pitch_class(message: &[u8]) -> Option<usize> {
  if message.len() >= 3 && midi::is_note_on(message) {
    Some((message[1] % 12) as usize)
  } else {
    None
  }
}

fn edo12n_instruction(
  original_note: u8,
  shifts: &Arc<Mutex<[i8; 12]>>,
) -> (i16, i16) {
  let normalized: i16 = original_note as i16
                        - LOWEST_A as i16
                        + SHIFT_IN_12_EDO as i16;
  let channel_offset: i16 = normalized.div_euclid(12);
  let note_offset: i16 = normalized.rem_euclid(12);
  let channel: i16 = MIN_CHANNEL_OUT as i16 + channel_offset;
  let pitch_class: u8 = original_note % 12;
  let shift: i16 = shifts.lock().unwrap()[pitch_class as usize] as i16;
  let note: i16 = MIN_NOTE_OUT as i16
                  + note_offset * EDO_OVER_12 as i16
                  + shift;
  (channel, note)
}

fn run_monome_thread(
  shifts: Arc<Mutex<[i8; 12]>>,
  flash_rx: mpsc::Receiver<usize>,
) {
  let sock = UdpSocket::bind(("0.0.0.0", LISTEN_PORT))
    .unwrap_or_else(|e| panic!("bind UDP :{LISTEN_PORT}: {e}"));
  sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
  let mut device_port: u16 =
    monome::discover_device(&sock, LISTEN_PORT)
      .expect("no monome found; is serialoscd running?");
  let mut device: SocketAddr = format!("127.0.0.1:{device_port}").parse().unwrap();
  monome::register(&sock, device, PREFIX, LISTEN_PORT);
  let mut led_state = MonomeLedState::new(*shifts.lock().unwrap());
  let mut rendered_cols: [u8; 16] = [0; 16];
  render_to_monome(&sock, device, &led_state, Instant::now(), &mut rendered_cols);
  sock.set_read_timeout(Some(Duration::from_millis(1))).unwrap();

  let key_addr: String = format!("{PREFIX}/grid/key");
  let mut buf = [0u8; 2048];
  let mut next_dim_pulse: Instant = Instant::now() + DIM_PULSE.period;
  while !STOP_REQUESTED.load(Ordering::Relaxed) {
    drain_note_flashes(&flash_rx, &mut led_state);
    let now = Instant::now();
    render_to_monome(&sock, device, &led_state, now, &mut rendered_cols);
    if now >= next_dim_pulse {
      led_state.dim_columns_on = true;
      render_to_monome(&sock, device, &led_state, now, &mut rendered_cols);
      thread::sleep(DIM_PULSE.on_time);
      led_state.dim_columns_on = false;
      render_to_monome(
        &sock,
        device,
        &led_state,
        Instant::now(),
        &mut rendered_cols,
      );
      next_dim_pulse = now + DIM_PULSE.period;
    }
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
            &led_state,
            Instant::now(),
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
    if let Some(root) = apply_root_press(x, y, s) {
      led_state.root = root;
      eprintln!("root -> {}", pitch_name(root));
      render_to_monome(&sock, device, &led_state, Instant::now(), &mut rendered_cols);
      continue;
    }
    if let Some((group, shift, changed)) =
      apply_group_press(x, y, s, led_state.root, &shifts)
    {
      led_state.group_shifts[group] = shift;
      for pitch_class in changed {
        led_state.shifts[pitch_class] = shift;
      }
      eprintln!("group {} -> {shift}", group + 1);
      render_to_monome(&sock, device, &led_state, Instant::now(), &mut rendered_cols);
      continue;
    }
    if let Some((pitch_class, shift)) = apply_offset_press(x, y, s, &shifts) {
      led_state.shifts[pitch_class] = shift;
      eprintln!("offset {} -> {shift}", pitch_name(pitch_class));
      render_to_monome(&sock, device, &led_state, Instant::now(), &mut rendered_cols);
    }
  }
  monome::send_led_all(&sock, device, PREFIX, 0);
}

fn black_monome() {
  monome::black(PREFIX);
}

fn send_led_col(sock: &UdpSocket, device: SocketAddr, x: i32, mask: i32) {
  monome::send_led_col(
    sock,
    device,
    PREFIX,
    x,
    0,
    mask,
  );
}

fn drain_note_flashes(flash_rx: &mpsc::Receiver<usize>, led_state: &mut MonomeLedState) {
  let now = Instant::now();
  while let Ok(pitch_class) = flash_rx.try_recv() {
    led_state.flash_note(pitch_class, now);
  }
}

fn render_to_monome(
  sock: &UdpSocket,
  device: SocketAddr,
  led_state: &MonomeLedState,
  now: Instant,
  rendered_cols: &mut [u8; 16],
) {
  let cols = render_led_cols(led_state, now);
  for (x, col) in cols.iter().enumerate() {
    if rendered_cols[x] != *col {
      send_led_col(sock, device, x as i32, *col as i32);
      rendered_cols[x] = *col;
    }
  }
}

fn render_led_cols(led_state: &MonomeLedState, now: Instant) -> [u8; 16] {
  let mut state = MonomeLedState {
    shifts: led_state.shifts,
    root: led_state.root,
    group_shifts: led_state.group_shifts,
    note_flash_until: led_state.note_flash_until,
    dim_columns_on: led_state.dim_columns_on,
  };
  state.expire_flashes(now);

  let mut cols: [u8; 16] = [0; 16];
  for x in 0..OFFSET_COLS as usize {
    if !WHITE_KEYS[x] && state.dim_columns_on {
      cols[x] = bottom_seven_mask();
    }
    let active_y: i32 = OFFSET_Y_ZERO - state.shifts[x] as i32;
    if active_y >= 0 && active_y < 8 {
      cols[x] |= 1 << active_y;
    }
    if state.note_flash_until[x].is_some_and(|deadline| now < deadline) {
      cols[x] = 0xff;
    }
  }
  cols[state.root] |= 1 << ROOT_Y;
  for (group, x) in GROUP_COLS.iter().enumerate() {
    let active_y: i32 = OFFSET_Y_ZERO - state.group_shifts[group] as i32;
    if active_y >= 0 && active_y < 8 {
      cols[*x as usize] |= 1 << active_y;
    }
  }
  cols
}

fn bottom_seven_mask() -> u8 {
  let mut mask: u8 = 0;
  for y in OFFSET_Y_MIN..=OFFSET_Y_MAX {
    mask |= 1 << y;
  }
  mask
}

fn is_offset_cell(x: i32, y: i32) -> bool {
  x >= 0 && x < OFFSET_COLS && y >= OFFSET_Y_MIN && y <= OFFSET_Y_MAX
}

fn is_root_cell(x: i32, y: i32) -> bool {
  x >= 0 && x < OFFSET_COLS && y == ROOT_Y
}

fn is_group_cell(x: i32, y: i32) -> Option<usize> {
  if y < OFFSET_Y_MIN || y > OFFSET_Y_MAX {
    return None;
  }
  GROUP_COLS.iter().position(|col| *col == x)
}

fn pitch_class_from_root(root: usize, interval: usize) -> usize {
  (root + interval) % 12
}

fn group_pitch_classes(root: usize, group: usize) -> Vec<usize> {
  GROUP_INTERVALS[group]
    .iter()
    .map(|interval| pitch_class_from_root(root, *interval))
    .collect()
}

fn shift_from_y(y: i32) -> i8 {
  (OFFSET_Y_ZERO - y) as i8
}

fn apply_root_press(x: i32, y: i32, s: i32) -> Option<usize> {
  if s == 1 && is_root_cell(x, y) {
    Some(x as usize)
  } else {
    None
  }
}

fn apply_group_press(
  x: i32,
  y: i32,
  s: i32,
  root: usize,
  shifts: &Arc<Mutex<[i8; 12]>>,
) -> Option<(usize, i8, Vec<usize>)> {
  if s != 1 {
    return None;
  }
  let group = is_group_cell(x, y)?;
  let shift = shift_from_y(y);
  let changed = group_pitch_classes(root, group);
  {
    let mut shifts = shifts.lock().unwrap();
    for pitch_class in &changed {
      shifts[*pitch_class] = shift;
    }
  }
  Some((group, shift, changed))
}

fn apply_offset_press(
  x: i32,
  y: i32,
  s: i32,
  shifts: &Arc<Mutex<[i8; 12]>>,
) -> Option<(usize, i8)> {
  if s != 1 || !is_offset_cell(x, y) {
    return None;
  }
  let pitch_class: usize = x as usize;
  let shift: i8 = shift_from_y(y);
  shifts.lock().unwrap()[pitch_class] = shift;
  Some((pitch_class, shift))
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

  fn shared_shifts() -> Arc<Mutex<[i8; 12]>> {
    Arc::new(Mutex::new([0; 12]))
  }

  #[test]
  fn offset_key_down_updates_pitch_class_shift() {
    let shifts = shared_shifts();

    assert_eq!(apply_offset_press(1, 2, 1, &shifts), Some((1, 2)));
    assert_eq!(shifts.lock().unwrap()[1], 2);
  }

  #[test]
  fn offset_key_release_does_not_change_shift() {
    let shifts = shared_shifts();

    assert_eq!(apply_offset_press(1, 2, 0, &shifts), None);
    assert_eq!(shifts.lock().unwrap()[1], 0);
  }

  #[test]
  fn top_row_does_not_change_offset_yet() {
    let shifts = shared_shifts();

    assert_eq!(apply_offset_press(1, 0, 1, &shifts), None);
    assert_eq!(shifts.lock().unwrap()[1], 0);
  }

  #[test]
  fn top_row_selects_root() {
    assert_eq!(apply_root_press(6, 0, 1), Some(6));
    assert_eq!(apply_root_press(6, 0, 0), None);
    assert_eq!(apply_root_press(13, 0, 1), None);
  }

  #[test]
  fn first_group_press_includes_root_and_uses_third_and_fifth_like_interval_set() {
    let shifts = shared_shifts();

    assert_eq!(apply_group_press(13, 3, 1, 6, &shifts), Some((0, 1, vec![6, 8, 11, 1])));
    let shifts = shifts.lock().unwrap();

    assert_eq!(shifts[6], 1);
    assert_eq!(shifts[8], 1);
    assert_eq!(shifts[11], 1);
    assert_eq!(shifts[1], 1);
  }

  #[test]
  fn second_group_press_uses_four_note_interval_set() {
    let shifts = shared_shifts();

    assert_eq!(apply_group_press(14, 3, 1, 6, &shifts), Some((1, 1, vec![7, 9, 2, 4])));
    let shifts = shifts.lock().unwrap();

    assert_eq!(shifts[7], 1);
    assert_eq!(shifts[9], 1);
    assert_eq!(shifts[2], 1);
    assert_eq!(shifts[4], 1);
    assert_eq!(shifts[6], 0);
  }

  #[test]
  fn third_group_press_uses_other_four_note_interval_set() {
    let shifts = shared_shifts();

    assert_eq!(apply_group_press(15, 5, 1, 6, &shifts), Some((2, -1, vec![10, 0, 3, 5])));
    let shifts = shifts.lock().unwrap();

    assert_eq!(shifts[10], -1);
    assert_eq!(shifts[0], -1);
    assert_eq!(shifts[3], -1);
    assert_eq!(shifts[5], -1);
    assert_eq!(shifts[6], 0);
  }

  #[test]
  fn piano_note_uses_current_pitch_class_shift() {
    let shifts = shared_shifts();
    let (_baseline_channel, baseline_note) = edo12n_instruction(60, &shifts);
    apply_offset_press(0, 3, 1, &shifts);

    let (_channel, note) = edo12n_instruction(60, &shifts);

    assert_eq!(note, baseline_note + 1);
  }

  #[test]
  fn note_on_pitch_class_detects_keyboard_note_ons() {
    assert_eq!(note_on_pitch_class(&[0x90, 61, 100]), Some(1));
    assert_eq!(note_on_pitch_class(&[0x90, 61, 0]), None);
    assert_eq!(note_on_pitch_class(&[0x80, 61, 64]), None);
  }

  #[test]
  fn render_flashes_entire_pitch_class_column() {
    let now = Instant::now();
    let mut led_state = MonomeLedState::new([0; 12]);
    led_state.flash_note(0, now);

    let cols = render_led_cols(&led_state, now);

    assert_eq!(cols[0], 0xff);
  }

  #[test]
  fn render_restores_offset_after_flash_expires() {
    let now = Instant::now();
    let mut led_state = MonomeLedState::new([0; 12]);
    led_state.flash_note(0, now);

    let cols = render_led_cols(&led_state, now + NOTE_FLASH);

    assert_eq!(cols[0], (1 << ROOT_Y) | (1 << OFFSET_Y_ZERO));
  }

  #[test]
  fn dim_black_column_preserves_selected_offset_bit() {
    let mut led_state = MonomeLedState::new([0; 12]);
    led_state.dim_columns_on = false;

    let cols = render_led_cols(&led_state, Instant::now());

    assert_eq!(cols[1], 1 << OFFSET_Y_ZERO);
  }

  #[test]
  fn render_root_selector_on_top_row_only() {
    let mut led_state = MonomeLedState::new([0; 12]);
    led_state.root = 6;
    led_state.dim_columns_on = true;

    let cols = render_led_cols(&led_state, Instant::now());

    for (x, col) in cols.iter().take(12).enumerate() {
      let root_bit = (col & (1 << ROOT_Y)) != 0;
      assert_eq!(root_bit, x == 6);
    }
  }

  #[test]
  fn render_group_columns_use_their_own_offsets() {
    let mut led_state = MonomeLedState::new([0; 12]);
    led_state.group_shifts = [1, 0, -2];

    let cols = render_led_cols(&led_state, Instant::now());

    assert_eq!(cols[13], 1 << 3);
    assert_eq!(cols[14], 1 << 4);
    assert_eq!(cols[15], 1 << 6);
  }
}
