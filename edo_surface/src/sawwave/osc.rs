//! OSC plumbing: send / register / discover.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};

use crate::consts::{DETECTOR_PORT, PREFIX};

pub fn send_osc(sock: &UdpSocket, dst: SocketAddr, addr: &str, args: Vec<OscType>) {
  let buf = encoder::encode(&OscPacket::Message(OscMessage {
    addr: addr.to_string(),
    args,
  }))
    .expect("encode OSC");
  // send_to can fail if the destination isn't up; ignore errors so a
  // transient hiccup doesn't crash the synth.
  let _ = sock.send_to(&buf, dst);
}

// Send everything a fresh device needs: route events here, set prefix,
// clear LEDs. Called on startup and again whenever serialoscd tells us
// the device has moved to a new port (which happens when serialoscd
// or the USB link restarts).
pub fn register(sock: &UdpSocket, device: SocketAddr, listen_port: u16) {
  send_osc(sock, device, "/sys/host", vec![OscType::String("127.0.0.1".into())]);
  send_osc(sock, device, "/sys/port", vec![OscType::Int(listen_port as i32)]);
  send_osc(sock, device, "/sys/prefix", vec![OscType::String(PREFIX.into())]);
  send_osc(sock, device, &format!("{PREFIX}/grid/led/all"), vec![OscType::Int(0)]);
}

// Listen for the full 2-second discovery window — DON'T return on
// the first response. Multiple /serialosc/device replies are the
// classic ghost-tty symptom (see README): a real serialosc-device
// plus zombie ones tied to dead ttyUSBs. The zombies typically pin
// a CPU at 100% spinning on EIO, and host-side audio scheduling
// gets unhappy. We can't fix that from the container, but we can
// be loud about it and at least pick the most-recently-spawned
// device port (= last response), which heuristically maps to the
// real grid per the README.
pub fn discover_device(sock: &UdpSocket, listen_port: u16) -> Option<u16> {
  let detector: SocketAddr = format!("127.0.0.1:{DETECTOR_PORT}").parse().ok()?;
  send_osc(sock, detector, "/serialosc/list", vec![
    OscType::String("127.0.0.1".into()),
    OscType::Int(listen_port as i32),
  ]);
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut buf = [0u8; 2048];
  let mut ports: Vec<u16> = vec![];
  while Instant::now() < deadline {
    if let Ok((n, _)) = sock.recv_from(&mut buf) {
      if let Ok((_, OscPacket::Message(m))) = decoder::decode_udp(&buf[..n]) {
        if m.addr == "/serialosc/device" && m.args.len() >= 3 {
          if let Some(OscType::Int(p)) = m.args.get(2) {
            let p = *p as u16;
            if !ports.contains(&p) { ports.push(p); }
          }
        }
      }
    }
  }
  if ports.len() > 1 {
    eprintln!(
      "WARN: serialoscd reported {} device ports: {:?}. \
       This is the ghost-tty bug from the README. The dead one(s) \
       are probably pinning a CPU at 100% on the host and may \
       starve PipeWire enough to drop audio. Picking the last \
       (heuristically newest) port = {}. Fix on the host:\n\
       \n\
       \tsystemctl --user restart serialoscd\n\
       \t# unplug the monome, wait ~3s, replug\n",
      ports.len(), ports, ports.last().unwrap(),
    );
  }
  ports.last().copied()
}
