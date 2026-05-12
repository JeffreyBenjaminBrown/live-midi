use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

pub const DETECTOR_PORT: u16 = 12002;

pub fn send_osc(sock: &UdpSocket, dst: SocketAddr, addr: &str, args: Vec<OscType>) {
  let buf = encoder::encode(&OscPacket::Message(OscMessage {
    addr: addr.to_string(),
    args,
  })).expect("encode OSC");
  let _ = sock.send_to(&buf, dst);
}

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
            if !ports.contains(&p) {
              ports.push(p);
            }
          }
        }
      }
    }
  }
  ports.last().copied()
}

pub fn register(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  listen_port: u16,
) {
  send_osc(sock, device, "/sys/host", vec![OscType::String("127.0.0.1".into())]);
  send_osc(sock, device, "/sys/port", vec![OscType::Int(listen_port as i32)]);
  send_osc(sock, device, "/sys/prefix", vec![OscType::String(prefix.into())]);
  send_led_all(sock, device, prefix, 0);
}

pub fn black(
  prefix: &str,
) {
  let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else { return; };
  let Ok(my_port) = sock.local_addr().map(|a| a.port()) else { return; };
  let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
  let Some(device_port) = discover_device(&sock, my_port) else { return; };
  let device: SocketAddr = format!("127.0.0.1:{device_port}").parse().unwrap();
  send_osc(&sock, device, "/sys/prefix", vec![OscType::String(prefix.into())]);
  send_led_all(&sock, device, prefix, 0);
}

pub fn send_led_all(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  state: i32,
) {
  send_osc(
    sock,
    device,
    &format!("{prefix}/grid/led/all"),
    vec![OscType::Int(state)],
  );
}

pub fn send_led_col(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  x: i32,
  y: i32,
  mask: i32,
) {
  send_osc(
    sock,
    device,
    &format!("{prefix}/grid/led/col"),
    vec![OscType::Int(x), OscType::Int(y), OscType::Int(mask)],
  );
}
