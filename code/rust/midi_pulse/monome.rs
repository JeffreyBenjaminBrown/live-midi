use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// The serialosc detector (serialoscd) UDP port. Defaults to the well-known
/// 12002. Override with the `MIDI_PULSE_DETECTOR_PORT` env var to aim discovery
/// at a mock serialosc (see the `mock_monome` module/binary) instead of the real
/// one -- the only way to test the device layer without grabbing live grids,
/// since 12002 is the host's serialoscd.
pub const DETECTOR_PORT: u16 = 12002;

/// The detector port to use: the env override if set and parseable, else 12002.
pub fn detector_port() -> u16 {
  std::env::var("MIDI_PULSE_DETECTOR_PORT")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(DETECTOR_PORT)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
  pub id: String,
  pub type_name: String,
  pub port: u16,
  pub grid_w: i32,
  pub grid_h: i32,
}

pub fn send_osc(sock: &UdpSocket, dst: SocketAddr, addr: &str, args: Vec<OscType>) {
  let buf = encoder::encode(&OscPacket::Message(OscMessage {
    addr: addr.to_string(),
    args,
  })).expect("encode OSC");
  let _ = sock.send_to(&buf, dst);
}

pub fn discover_device(sock: &UdpSocket, listen_port: u16) -> Option<u16> {
  discover_device_info(sock, listen_port).map(|device| device.port)
}

pub fn discover_device_info(sock: &UdpSocket, listen_port: u16) -> Option<DeviceInfo> {
  discover_devices(sock, listen_port).last().cloned()
}

pub fn discover_devices(sock: &UdpSocket, listen_port: u16) -> Vec<DeviceInfo> {
  discover_devices_via(sock, listen_port, detector_port())
}

/// Like `discover_devices`, but queries the detector at an explicit port. Lets a
/// test point discovery at an in-process mock without setting the process-wide
/// env var (which would race other tests running in the same process).
pub fn discover_devices_via(sock: &UdpSocket, listen_port: u16, detector_port: u16) -> Vec<DeviceInfo> {
  let Ok(detector) = format!("127.0.0.1:{detector_port}").parse::<SocketAddr>() else {
    return vec![];
  };
  send_osc(sock, detector, "/serialosc/list", vec![
    OscType::String("127.0.0.1".into()),
    OscType::Int(listen_port as i32),
  ]);
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut buf = [0u8; 2048];
  let mut devices: Vec<DeviceInfo> = vec![];
  while Instant::now() < deadline {
    if let Ok((n, _)) = sock.recv_from(&mut buf) {
      if let Ok((_, OscPacket::Message(m))) = decoder::decode_udp(&buf[..n]) {
        if m.addr == "/serialosc/device" && m.args.len() >= 3 {
          if let (
            Some(OscType::String(id)),
            Some(OscType::String(type_name)),
            Some(OscType::Int(port)),
          ) = (m.args.first(), m.args.get(1), m.args.get(2)) {
            let port = *port as u16;
            if !devices.iter().any(|device| device.port == port) {
              let (grid_w, grid_h) = grid_size_for_type(type_name);
              devices.push(DeviceInfo {
                id: id.clone(),
                type_name: type_name.clone(),
                port,
                grid_w,
                grid_h,
              });
            }
          }
        }
      }
    }
  }
  if devices.len() > 1 {
    let ports: Vec<u16> = devices.iter().map(|device| device.port).collect();
    eprintln!(
      "WARN: serialoscd reported {} device ports: {:?}. \
       discover_device_info() will pick the last reply, which is usually the newest live device.",
      devices.len(),
      ports,
    );
  }
  devices
}

fn grid_size_for_type(type_name: &str) -> (i32, i32) {
  if type_name.contains("256") {
    (16, 16)
  } else if type_name.contains("128") {
    (16, 8)
  } else if type_name.contains("64") {
    (8, 8)
  } else {
    (16, 8)
  }
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

pub fn send_led_level_set(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  x: i32,
  y: i32,
  level: i32,
) {
  send_osc(
    sock,
    device,
    &format!("{prefix}/grid/led/level/set"),
    vec![OscType::Int(x), OscType::Int(y), OscType::Int(level)],
  );
}

/// Binary quad map: set a whole 8x8 block in one message (`/grid/led/map x y d[8]`).
/// `x_off`/`y_off` are the block's top-left (multiples of 8); `rows[r]` is a bitmask of
/// the 8 columns of row `y_off + r`, LSB = leftmost (`x_off`). Cheap enough to flash the
/// whole grid at LED-fusion rates (4 messages for a 16x16), which per-cell writes are not
/// -- see the surfaces runtime's monobright fake-dim path.
pub fn send_led_map(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  x_off: i32,
  y_off: i32,
  rows: &[u8; 8],
) {
  let mut args = Vec::with_capacity(10);
  args.push(OscType::Int(x_off));
  args.push(OscType::Int(y_off));
  args.extend(rows.iter().map(|r| OscType::Int(*r as i32)));
  send_osc(sock, device, &format!("{prefix}/grid/led/map"), args);
}
