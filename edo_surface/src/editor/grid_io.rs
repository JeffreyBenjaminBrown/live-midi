//! The editor's monome threads: one per connected grid, turning serialosc
//! key messages into `Session::key` calls and sending the session's LED
//! levels back, a quad at a time and only when they change.
//!
//! Grids are found once, at startup, from the startup rig's `monomes`
//! (their `select`, `listen_port` and `prefix`); a missing grid is skipped
//! without complaint. A later rig change re-reads what the keys mean but
//! keeps these connections.

use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rosc::{decoder, OscPacket, OscType};

use crate::device_assign::assign_selected_devices;
use crate::monome::{self, DeviceInfo};
use crate::monome_brightness::PulseBrightness;
use crate::rig::{MonomeRig, MonomeSelect};

use super::session::Session;

/// How often the LEDs are repainted.
const PAINT_EVERY: Duration = Duration::from_millis(20);

/// A monobright grid's fake dim: on for 1/32 of every 15 ms, the surfaces
/// runtime's figure.
const DIM_PULSE: PulseBrightness = PulseBrightness::one_thirty_second(15_000);

/// The level from which a monobright grid shows a cell steadily lit; below it
/// (but above 0) the cell is dim, so it flashes.
const MONOBRIGHT_ON: u8 = 8;

/// Find the grids (asking the serialosc detector at `detector_port`) and
/// start a thread for each one present. Returns the threads and one line per
/// grid saying what became of it.
pub fn start_grids(
  session: &Arc<Mutex<Session>>,
  stop: &Arc<AtomicBool>,
  detector_port: u16,
) -> Result<(Vec<JoinHandle<()>>, Vec<String>), String> {
  let monomes: Vec<MonomeRig> = session.lock().unwrap_or_else(|e| e.into_inner())
    .live.rig.grids.iter().map(|grid| grid.monome.clone()).collect();
  let first_port: u16 = monomes[0].listen_port;
  let discovery: UdpSocket = UdpSocket::bind(("0.0.0.0", first_port))
    .map_err(|e| format!("bind UDP :{first_port}: {e}"))?;
  discovery.set_read_timeout(Some(Duration::from_millis(50))).map_err(|e| e.to_string())?;
  let devices: Vec<DeviceInfo> =
    monome::discover_devices_via(&discovery, first_port, detector_port);
  let selects: Vec<MonomeSelect> = monomes.iter().map(|m| m.select.clone()).collect();
  let assigned: Vec<Option<DeviceInfo>> = assign_selected_devices(&devices, &selects);
  let mut drain = [0u8; 2048];
  while discovery.recv_from(&mut drain).is_ok() {}
  let mut discovery: Option<UdpSocket> = Some(discovery);
  let mut threads: Vec<JoinHandle<()>> = vec![];
  let mut report: Vec<String> = vec![];
  for (grid, (monome_rig, device)) in monomes.iter().zip(assigned).enumerate() {
    let Some(device) = device else {
      report.push(format!("grid {}: not connected", monome_rig.id));
      continue; };
    let sock: UdpSocket = match (grid, discovery.take()) {
      (0, Some(sock)) => sock,
      _ => UdpSocket::bind(("0.0.0.0", monome_rig.listen_port))
        .map_err(|e| format!("bind UDP :{}: {e}", monome_rig.listen_port))?, };
    sock.set_read_timeout(Some(Duration::from_millis(2))).map_err(|e| e.to_string())?;
    report.push(format!("grid {}: {} on port {}", monome_rig.id, device.id, device.port));
    let session: Arc<Mutex<Session>> = Arc::clone(session);
    let stop: Arc<AtomicBool> = Arc::clone(stop);
    let monome_rig: MonomeRig = monome_rig.clone();
    threads.push(thread::spawn(move ||
      grid_thread(grid, sock, device, monome_rig, session, stop))); }
  Ok((threads, report)) }

fn grid_thread(
  grid: usize,
  sock: UdpSocket,
  device: DeviceInfo,
  monome_rig: MonomeRig,
  session: Arc<Mutex<Session>>,
  stop: Arc<AtomicBool>,
) {
  let Ok(address) = format!("127.0.0.1:{}", device.port).parse::<SocketAddr>() else { return; };
  let prefix: &str = &monome_rig.prefix;
  monome::register(&sock, address, prefix, monome_rig.listen_port);
  let key_address: String = format!("{prefix}/grid/key");
  let (width, height): (i32, i32) = (device.grid_w, device.grid_h);
  let mut pressed: HashSet<(i32, i32)> = HashSet::new();
  let mut sent: Vec<u8> = vec![];
  let mut levels: Vec<u8> = vec![];
  let mut next_paint: Instant = Instant::now();
  // A monobright grid can't dim an LED, so it shows a dim cell by flashing it on
  // briefly every DIM_PULSE period, as the surfaces runtime does.
  let monobright: bool = monome::is_monobright(&device.id);
  let mut sent_quads: Vec<[u8; 8]> = vec![];
  let mut next_pulse: Instant = Instant::now();
  let mut buf = [0u8; 2048];
  while !stop.load(Ordering::SeqCst) {
    if let Ok((n, _)) = sock.recv_from(&mut buf) {
      if let Some((cell, down)) = key_message(&buf[..n], &key_address) {
        // a duplicate press or release from a bouncing device changes nothing
        let changed: bool = if down { pressed.insert(cell) } else { pressed.remove(&cell) };
        if changed {
          session.lock().unwrap_or_else(|e| e.into_inner())
            .key(Instant::now(), grid, cell, down); } } }
    let now: Instant = Instant::now();
    if now >= next_paint {
      next_paint = now + PAINT_EVERY;
      levels = session.lock().unwrap_or_else(|e| e.into_inner()).leds(now, grid, width, height);
      if !monobright {
        send_changed_quads(&sock, address, prefix, &levels, &sent, width, height);
        sent = levels.clone(); } }
    if monobright {
      let bright = |index: usize| levels.get(index).is_some_and(|level| *level >= MONOBRIGHT_ON);
      monome::send_binary_frame(&sock, address, prefix, width, height, &mut sent_quads, bright);
      if now >= next_pulse {
        next_pulse = now + DIM_PULSE.period;
        let any_level = |index: usize| levels.get(index).is_some_and(|level| *level > 0);
        monome::send_binary_frame(
          &sock, address, prefix, width, height, &mut sent_quads, any_level);
        thread::sleep(DIM_PULSE.on_time);
        monome::send_binary_frame(&sock, address, prefix, width, height, &mut sent_quads, bright);
      } } }
  monome::send_led_all(&sock, address, prefix, 0); }

/// A key message's cell and whether it went down.
fn key_message(
  bytes: &[u8],
  key_address: &str,
) -> Option<((i32, i32), bool)> {
  let Ok((_, OscPacket::Message(message))) = decoder::decode_udp(bytes) else { return None; };
  if message.addr != key_address { return None; }
  match message.args.as_slice() {
    [OscType::Int(x), OscType::Int(y), OscType::Int(state)] => Some(((*x, *y), *state != 0)),
    _ => None, } }

fn send_changed_quads(
  sock: &UdpSocket,
  address: SocketAddr,
  prefix: &str,
  levels: &[u8],
  sent: &[u8],
  width: i32,
  height: i32,
) {
  for y_off in (0..height).step_by(8) {
    for x_off in (0..width).step_by(8) {
      let mut quad = [0u8; 64];
      for row in 0..8 {
        for col in 0..8 {
          let index: usize = ((y_off + row) * width + x_off + col) as usize;
          quad[(row * 8 + col) as usize] = levels.get(index).copied().unwrap_or(0); } }
      let unchanged: bool = !sent.is_empty() && (0..64).all(|i| {
        let index: usize = ((y_off + i / 8) * width + x_off + i % 8) as usize;
        sent.get(index) == levels.get(index) });
      if !unchanged {
        monome::send_led_level_map(sock, address, prefix, x_off, y_off, &quad); } } } }

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;
  use crate::editor::live_grid::BRIGHT;
  use crate::editor::recorder::RecordMode;
  use crate::editor::session::BLINK_MILLIS;
  use crate::editor::rig_layout::{editor_rig, EditorRig, PieceKind};
  use crate::editor::song::Song;
  use crate::editor::sound::Sound;
  use crate::mock_monome::{wait_until, GridSpec, MockRig};
  use crate::rig::load_named_rig;
  use crate::types::VoiceMap;

  const PLAIN: &str = "timbres-volume-scroll_41edo";

  /// A port nothing is listening on, so the test never meets a running editor.
  fn free_port() -> u16 {
    UdpSocket::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port() }

  #[test]
  fn keys_on_a_mock_grid_record_and_light_and_playback_blinks() {
    let mock: MockRig = MockRig::start(0, &[GridSpec::grid_256("m-editor-test")]).unwrap();
    let mut rig: EditorRig = editor_rig(PLAIN, &load_named_rig(PLAIN).unwrap()).unwrap();
    for grid in rig.grids.iter_mut() { grid.monome.listen_port = free_port(); }
    let dir: PathBuf = std::env::temp_dir()
      .join(format!("edo-editor-grid-io-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sound: Sound = Sound::new(Arc::new(Mutex::new(VoiceMap::new())), 48000.0);
    let session: Arc<Mutex<Session>> =
      Arc::new(Mutex::new(Session::new(Song::load(&dir).unwrap(), rig, sound)));
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let (threads, report) = start_grids(&session, &stop, mock.detector_port()).unwrap();
    assert_eq!(threads.len(), 1, "{report:?}");
    let cell: (i32, i32) = {
      let session = session.lock().unwrap();
      (0..16).flat_map(|y| (0..16).map(move |x| (x, y)))
        .find(|c| session.live.rig.grids[0].piece_at(*c).unwrap().kind == PieceKind::Notes)
        .unwrap() };
    let grid = mock.grid(0);
    assert!(wait_until(Duration::from_secs(3), || grid.registered()));
    session.lock().unwrap().record(Instant::now(), RecordMode::Notes);
    grid.press(cell.0, cell.1);
    assert!(wait_until(Duration::from_secs(2), || grid.level_at(cell.0, cell.1) == BRIGHT as i32),
      "a held note lights");
    // longer than a whole blink cycle, so playback must catch it lit
    std::thread::sleep(Duration::from_millis(2 * BLINK_MILLIS as u64 + 100));
    grid.release(cell.0, cell.1);
    assert!(wait_until(Duration::from_secs(2), || grid.level_at(cell.0, cell.1) == 0));
    session.lock().unwrap().stop(Instant::now());
    assert_eq!(session.lock().unwrap().song.tracks[0].track.notes.len(), 1);
    session.lock().unwrap().play(Instant::now());
    let mut seen: Vec<i32> = vec![];
    let until: Instant = Instant::now() + Duration::from_millis(800);
    while Instant::now() < until {
      session.lock().unwrap().tick(Instant::now());
      seen.push(grid.level_at(cell.0, cell.1));
      std::thread::sleep(Duration::from_millis(5)); }
    assert!(seen.contains(&(BRIGHT as i32)), "the played-back note blinks on");
    stop.store(true, Ordering::SeqCst);
    for thread in threads { thread.join().unwrap(); }
    let _ = std::fs::remove_dir_all(&dir); }

  #[test]
  fn a_monobright_grid_shows_bright_cells_steady_and_flashes_dim_ones() {
    let mock: MockRig = MockRig::start(0, &[GridSpec::grid_256("m256-editor-test")]).unwrap();
    let mut rig: EditorRig = editor_rig(PLAIN, &load_named_rig(PLAIN).unwrap()).unwrap();
    for grid in rig.grids.iter_mut() { grid.monome.listen_port = free_port(); }
    let dir: PathBuf = std::env::temp_dir()
      .join(format!("edo-editor-monobright-{}", std::process::id()));
    let sound: Sound = Sound::new(Arc::new(Mutex::new(VoiceMap::new())), 48000.0);
    let session: Arc<Mutex<Session>> =
      Arc::new(Mutex::new(Session::new(Song::load(&dir).unwrap(), rig, sound)));
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let (threads, _) = start_grids(&session, &stop, mock.detector_port()).unwrap();
    let (selected, other): ((i32, i32), (i32, i32)) = {
      let session = session.lock().unwrap();
      let piece = session.live.rig.grids[0].pieces.iter()
        .find(|p| matches!(p.kind, PieceKind::Timbre { .. })).unwrap().clone();
      let selected_slot: i32 = session.live.tones[0].timbre as i32;
      let other_slot: i32 = if selected_slot == 0 { 1 } else { 0 };
      ((piece.rect[0] + selected_slot, piece.rect[1]), (piece.rect[0] + other_slot, piece.rect[1])) };
    let grid = mock.grid(0);
    assert!(wait_until(Duration::from_secs(3), || grid.level_at(selected.0, selected.1) == 15),
      "the selected timbre key lights steadily");
    let mut other_levels: Vec<i32> = vec![];
    for _ in 0..50 {
      other_levels.push(grid.level_at(other.0, other.1));
      std::thread::sleep(Duration::from_millis(2)); }
    assert!(other_levels.iter().filter(|level| **level == 0).count() > 40,
      "a dim key is dark nearly all the time: {other_levels:?}");
    stop.store(true, Ordering::SeqCst);
    for thread in threads { thread.join().unwrap(); }
    let _ = std::fs::remove_dir_all(&dir); }
}
