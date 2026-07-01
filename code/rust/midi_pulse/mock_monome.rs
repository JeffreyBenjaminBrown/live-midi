//! A virtual monome rig: an in-process fake serialoscd plus one or more fake
//! grids, speaking the exact UDP/OSC protocol the real hardware does. It lets you
//! exercise the device layer -- discovery, registration, LED output, key input --
//! and even drive the whole looper, *without any physical grid* and *without
//! disturbing a live session*, because it lives on its own localhost ports.
//!
//! Protocol emulated (verified against the real serialosc and `monome.rs`):
//!
//! - Detector (fake serialoscd):
//!     recv `/serialosc/list   ,si <host> <port>`
//!     -> for each grid, send to <host>:<port>:
//!        `/serialosc/device ,ssi <id> <type> <device_port>`
//! - Each grid (on its own UDP port):
//!     recv `/sys/host   ,s <host>`        (where to send key events)
//!     recv `/sys/port   ,i <port>`
//!     recv `/sys/prefix ,s <prefix>`      (OSC namespace for this grid)
//!     recv `<prefix>/grid/led/all       ,i   <state>`
//!     recv `<prefix>/grid/led/level/set ,iii <x> <y> <level>`
//!     recv `<prefix>/grid/led/col       ,iii <x> <y> <mask>`   (monochrome)
//!     send `<prefix>/grid/key ,iii <x> <y> <s>`  on press/release (s in {0,1})
//!
//! The grid keeps an in-memory LED buffer you can read (`levels`, `level_at`,
//! `render`) and accepts injected key presses (`press`, `release`, `tap`). Point
//! the looper at it by setting `MIDI_PULSE_DETECTOR_PORT` to the rig's detector
//! port (see the `mock_monome` binary).

use crate::monome::send_osc;
use rosc::{decoder, OscPacket, OscType};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// What kind of grid to pretend to be. `type_name` is what the real serialosc
/// reports (e.g. "monome 256"); the looper derives grid size from it, so it must
/// match the dimensions for the runtime to assign it correctly.
#[derive(Clone, Debug)]
pub struct GridSpec {
  pub id: String,
  pub type_name: String,
  pub grid_w: i32,
  pub grid_h: i32,
}

impl GridSpec {
  /// A 16x16 "monome 256" -- what both of Jeff's grids report.
  pub fn grid_256(id: &str) -> Self {
    GridSpec { id: id.into(), type_name: "monome 256".into(), grid_w: 16, grid_h: 16 }
  }
}

/// Mutable grid state shared between the grid's receive thread and its handle.
struct GridState {
  grid_w: i32,
  grid_h: i32,
  levels: Vec<i32>,
  prefix: Option<String>,
  dest_host: Option<String>,
  dest_port: Option<u16>,
}

impl GridState {
  fn dest(&self) -> Option<SocketAddr> {
    let host = self.dest_host.as_ref()?;
    let port = self.dest_port?;
    format!("{host}:{port}").parse().ok()
  }
}

/// A handle to one virtual grid. Reads its LED buffer and injects key events.
pub struct MockGrid {
  spec: GridSpec,
  port: u16,
  state: Arc<Mutex<GridState>>,
  /// Bumped on every LED change, so a watcher can cheaply detect repaints.
  generation: Arc<AtomicU64>,
  /// Source socket for injected key events (the looper ignores the source addr).
  send_sock: UdpSocket,
}

impl MockGrid {
  pub fn id(&self) -> &str {
    &self.spec.id
  }

  /// The UDP port this grid listens on (the "device port" discovery reports).
  pub fn port(&self) -> u16 {
    self.port
  }

  pub fn dims(&self) -> (i32, i32) {
    (self.spec.grid_w, self.spec.grid_h)
  }

  /// True once the looper has registered (`/sys/port`) -- i.e. key events have
  /// somewhere to go.
  pub fn registered(&self) -> bool {
    self.lock().dest_port.is_some()
  }

  /// A snapshot of the LED levels, row-major, length `grid_w * grid_h`.
  pub fn levels(&self) -> Vec<i32> {
    self.lock().levels.clone()
  }

  pub fn level_at(&self, x: i32, y: i32) -> i32 {
    let st = self.lock();
    st.levels.get((y * st.grid_w + x) as usize).copied().unwrap_or(0)
  }

  /// Monotonic counter of LED changes; compare across calls to detect repaints.
  pub fn generation(&self) -> u64 {
    self.generation.load(Ordering::SeqCst)
  }

  /// An ASCII picture of the grid, brightness bucketed into ` .:+#`.
  pub fn render(&self) -> String {
    let st = self.lock();
    let glyph = |l: i32| match l {
      0 => ' ',
      1..=4 => '.',
      5..=8 => ':',
      9..=12 => '+',
      _ => '#',
    };
    let mut out = String::new();
    out.push_str(&format!("+{}+\n", "-".repeat(st.grid_w as usize)));
    for y in 0..st.grid_h {
      out.push('|');
      for x in 0..st.grid_w {
        out.push(glyph(st.levels[(y * st.grid_w + x) as usize]));
      }
      out.push_str("|\n");
    }
    out.push_str(&format!("+{}+", "-".repeat(st.grid_w as usize)));
    out
  }

  /// Inject a key press (`down=true`) or release (`down=false`). No-op until the
  /// looper has registered. Returns whether it was actually sent.
  pub fn key(&self, x: i32, y: i32, down: bool) -> bool {
    let st = self.lock();
    let (Some(prefix), Some(dest)) = (st.prefix.clone(), st.dest()) else {
      return false;
    };
    drop(st);
    send_osc(
      &self.send_sock,
      dest,
      &format!("{prefix}/grid/key"),
      vec![OscType::Int(x), OscType::Int(y), OscType::Int(i32::from(down))],
    );
    true
  }

  pub fn press(&self, x: i32, y: i32) -> bool {
    self.key(x, y, true)
  }

  pub fn release(&self, x: i32, y: i32) -> bool {
    self.key(x, y, false)
  }

  /// Press then release -- the natural unit for a button. (The looper debounces
  /// repeated presses, so press-without-release would swallow the next press.)
  pub fn tap(&self, x: i32, y: i32) -> bool {
    let a = self.press(x, y);
    let b = self.release(x, y);
    a && b
  }

  fn lock(&self) -> std::sync::MutexGuard<'_, GridState> {
    self.state.lock().unwrap_or_else(|e| e.into_inner())
  }
}

/// A running virtual rig: the detector thread plus one receive thread per grid.
/// Threads stop when the rig is dropped (or `shutdown` is called).
pub struct MockRig {
  detector_port: u16,
  grids: Vec<MockGrid>,
  stop: Arc<AtomicBool>,
  threads: Vec<JoinHandle<()>>,
}

impl MockRig {
  /// Start a rig. `detector_port` 0 lets the OS pick a free port (read it back
  /// with `detector_port()`); pass a fixed port to hand the looper a known
  /// `MIDI_PULSE_DETECTOR_PORT`. Each grid binds its own ephemeral device port.
  pub fn start(detector_port: u16, specs: &[GridSpec]) -> io::Result<MockRig> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut grids = Vec::new();
    let mut threads = Vec::new();
    let mut announce: Vec<(GridSpec, u16)> = Vec::new();

    for spec in specs {
      let sock = UdpSocket::bind(("127.0.0.1", 0))?;
      sock.set_read_timeout(Some(Duration::from_millis(50)))?;
      let port = sock.local_addr()?.port();
      let send_sock = sock.try_clone()?;
      let state = Arc::new(Mutex::new(GridState {
        grid_w: spec.grid_w,
        grid_h: spec.grid_h,
        levels: vec![0; (spec.grid_w * spec.grid_h) as usize],
        prefix: None,
        dest_host: None,
        dest_port: None,
      }));
      let generation = Arc::new(AtomicU64::new(0));

      let th_state = Arc::clone(&state);
      let th_gen = Arc::clone(&generation);
      let th_stop = Arc::clone(&stop);
      threads.push(thread::spawn(move || grid_loop(sock, th_state, th_gen, th_stop)));

      announce.push((spec.clone(), port));
      grids.push(MockGrid { spec: spec.clone(), port, state, generation, send_sock });
    }

    let det = UdpSocket::bind(("127.0.0.1", detector_port))?;
    det.set_read_timeout(Some(Duration::from_millis(50)))?;
    let detector_port = det.local_addr()?.port();
    let det_stop = Arc::clone(&stop);
    threads.push(thread::spawn(move || detector_loop(det, announce, det_stop)));

    Ok(MockRig { detector_port, grids, stop, threads })
  }

  pub fn detector_port(&self) -> u16 {
    self.detector_port
  }

  pub fn grids(&self) -> &[MockGrid] {
    &self.grids
  }

  pub fn grid(&self, i: usize) -> &MockGrid {
    &self.grids[i]
  }

  /// Stop all threads and join them. (Drop does the same; this is for tests that
  /// want it deterministic.)
  pub fn shutdown(mut self) {
    self.stop_and_join();
  }

  fn stop_and_join(&mut self) {
    self.stop.store(true, Ordering::SeqCst);
    for th in self.threads.drain(..) {
      let _ = th.join();
    }
  }
}

impl Drop for MockRig {
  fn drop(&mut self) {
    self.stop_and_join();
  }
}

/// Fake serialoscd: answer every `/serialosc/list` with one `/serialosc/device`
/// per grid, sent to the host:port the caller asked replies be sent to.
fn detector_loop(sock: UdpSocket, grids: Vec<(GridSpec, u16)>, stop: Arc<AtomicBool>) {
  let mut buf = [0u8; 2048];
  while !stop.load(Ordering::SeqCst) {
    let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
    let Ok((_, OscPacket::Message(m))) = decoder::decode_udp(&buf[..n]) else { continue };
    if m.addr != "/serialosc/list" {
      continue;
    }
    let (Some(OscType::String(host)), Some(OscType::Int(port))) = (m.args.first(), m.args.get(1))
    else {
      continue;
    };
    let Ok(dst) = format!("{host}:{port}").parse::<SocketAddr>() else { continue };
    for (spec, device_port) in &grids {
      send_osc(
        &sock,
        dst,
        "/serialosc/device",
        vec![
          OscType::String(spec.id.clone()),
          OscType::String(spec.type_name.clone()),
          OscType::Int(i32::from(*device_port)),
        ],
      );
    }
  }
}

/// One grid's receive loop: track registration, apply LED writes to the buffer.
fn grid_loop(
  sock: UdpSocket,
  state: Arc<Mutex<GridState>>,
  generation: Arc<AtomicU64>,
  stop: Arc<AtomicBool>,
) {
  let mut buf = [0u8; 2048];
  while !stop.load(Ordering::SeqCst) {
    let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
    let Ok((_, OscPacket::Message(m))) = decoder::decode_udp(&buf[..n]) else { continue };
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    let mut changed = false;
    let addr = m.addr.as_str();
    if addr == "/sys/host" {
      if let Some(OscType::String(h)) = m.args.first() {
        st.dest_host = Some(h.clone());
      }
    } else if addr == "/sys/port" {
      if let Some(OscType::Int(p)) = m.args.first() {
        st.dest_port = Some(*p as u16);
      }
    } else if addr == "/sys/prefix" {
      if let Some(OscType::String(p)) = m.args.first() {
        st.prefix = Some(p.clone());
      }
    } else if addr.ends_with("/grid/led/all") {
      if let Some(OscType::Int(s)) = m.args.first() {
        let v = *s;
        st.levels.iter_mut().for_each(|l| *l = v);
        changed = true;
      }
    } else if addr.ends_with("/grid/led/level/set") {
      if let (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(level))) =
        (m.args.first(), m.args.get(1), m.args.get(2))
      {
        let (w, h) = (st.grid_w, st.grid_h);
        if (0..w).contains(x) && (0..h).contains(y) {
          st.levels[(y * w + x) as usize] = *level;
          changed = true;
        }
      }
    } else if addr.ends_with("/grid/led/col") {
      // Monochrome column: bit i of the mask -> row y+i, on=15 / off=0.
      if let (Some(OscType::Int(x)), Some(OscType::Int(y0)), Some(OscType::Int(mask))) =
        (m.args.first(), m.args.get(1), m.args.get(2))
      {
        let (w, h) = (st.grid_w, st.grid_h);
        for bit in 0..8 {
          let y = y0 + bit;
          if (0..w).contains(x) && (0..h).contains(&y) {
            st.levels[(y * w + x) as usize] = if mask & (1 << bit) != 0 { 15 } else { 0 };
            changed = true;
          }
        }
      }
    } else if addr.ends_with("/grid/led/map") {
      // Binary 8x8 quad: args = x_off, y_off, then 8 row-bytes. Bit c of row r -> cell
      // (x_off+c, y_off+r), on = 15 / off = 0. The surfaces runtime uses this to flash a
      // monobright grid's fake-dim frames.
      if let (Some(OscType::Int(x0)), Some(OscType::Int(y0))) = (m.args.first(), m.args.get(1)) {
        let (w, h) = (st.grid_w, st.grid_h);
        let (x0, y0) = (*x0, *y0);
        for r in 0..8i32 {
          let Some(OscType::Int(byte)) = m.args.get(2 + r as usize) else { break };
          let byte = *byte;
          for c in 0..8i32 {
            let (x, y) = (x0 + c, y0 + r);
            if (0..w).contains(&x) && (0..h).contains(&y) {
              st.levels[(y * w + x) as usize] = if byte & (1 << c) != 0 { 15 } else { 0 };
              changed = true;
            }
          }
        }
      }
    }
    drop(st);
    if changed {
      generation.fetch_add(1, Ordering::SeqCst);
    }
  }
}

/// Block until `cond` is true or `timeout` elapses; returns whether it became
/// true. Useful around the async grid threads in tests and in the binary.
pub fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
  let deadline = Instant::now() + timeout;
  while Instant::now() < deadline {
    if cond() {
      return true;
    }
    thread::sleep(Duration::from_millis(2));
  }
  cond()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::monome;

  fn recv_key(sock: &UdpSocket, timeout: Duration) -> Option<(i32, i32, i32)> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
      if let Ok((n, _)) = sock.recv_from(&mut buf) {
        if let Ok((_, OscPacket::Message(m))) = decoder::decode_udp(&buf[..n]) {
          if m.addr.ends_with("/grid/key") {
            if let (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(s))) =
              (m.args.first(), m.args.get(1), m.args.get(2))
            {
              return Some((*x, *y, *s));
            }
          }
        }
      }
    }
    None
  }

  /// The whole device layer, end to end, against the mock: discover -> register
  /// -> LED write lands -> injected key arrives. This is the path that otherwise
  /// requires the real grids (and would interrupt a live session).
  #[test]
  fn looper_protocol_roundtrip_through_mock() {
    let rig = MockRig::start(0, &[GridSpec::grid_256("m-a"), GridSpec::grid_256("m-b")])
      .expect("start mock rig");
    let det = rig.detector_port();

    // Discover exactly as run() does, but aimed at the mock detector.
    let listen = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    listen.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let listen_port = listen.local_addr().unwrap().port();
    let devices = monome::discover_devices_via(&listen, listen_port, det);
    assert_eq!(devices.len(), 2, "both fake grids discovered");
    assert!(devices.iter().all(|d| (d.grid_w, d.grid_h) == (16, 16)));

    // Match the discovered device back to its grid handle by port.
    let dev = &devices[0];
    let g = rig.grids().iter().find(|g| g.port() == dev.port).expect("grid for device port");

    // Register like grid_thread does; the mock should learn where to send keys.
    let addr: SocketAddr = format!("127.0.0.1:{}", dev.port).parse().unwrap();
    monome::register(&listen, addr, "/looper-edo", listen_port);
    assert!(wait_until(Duration::from_secs(1), || g.registered()), "grid registered");

    // An LED write lands in the buffer.
    monome::send_led_level_set(&listen, addr, "/looper-edo", 3, 4, 15);
    assert!(
      wait_until(Duration::from_secs(1), || g.level_at(3, 4) == 15),
      "LED set reflected in mock buffer"
    );

    // led/all blanks everything.
    monome::send_led_all(&listen, addr, "/looper-edo", 0);
    assert!(
      wait_until(Duration::from_secs(1), || g.level_at(3, 4) == 0),
      "led/all 0 blanks the buffer"
    );

    // A key press on the mock arrives at the listen socket as <prefix>/grid/key.
    assert!(g.press(5, 6), "press sent (grid is registered)");
    assert_eq!(recv_key(&listen, Duration::from_secs(1)), Some((5, 6, 1)));
    assert!(g.release(5, 6));
    assert_eq!(recv_key(&listen, Duration::from_secs(1)), Some((5, 6, 0)));
  }
}
