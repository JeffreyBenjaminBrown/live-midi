// Step 10: monome button-press test.
//
// Flow:
//   1. Bind a UDP socket on localhost:9000 (our listen port).
//   2. Ask serialosc-detector (on :12002) to list connected devices,
//      replying to our port. Response tells us each device's OSC port.
//   3. Pick the first device. Register this listener with it via
//      /sys/host, /sys/port, /sys/prefix.
//   4. Listen for 15 seconds; print each incoming OSC packet.
//
// During (4), press buttons on the grid. Each press should log
// something like:  /256-1-cable/grid/key x=3 y=5 s=1
//
// Build + run via ../10-button-test.sh.

use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const LISTEN_PORT: u16 = 9000;
const DETECTOR_PORT: u16 = 12002;
const WINDOW: Duration = Duration::from_secs(15);
const PREFIX: &str = "/256-1-cable";

fn send(sock: &UdpSocket, dst: SocketAddr, addr: &str, args: Vec<OscType>) {
    let buf = encoder::encode(&OscPacket::Message(OscMessage {
        addr: addr.to_string(),
        args,
    }))
    .expect("encode OSC");
    sock.send_to(&buf, dst).expect("send OSC");
}

fn main() {
    let listen = format!("0.0.0.0:{LISTEN_PORT}");
    let sock = UdpSocket::bind(&listen).expect("bind listen socket");
    println!("listening on {listen}");

    // Short timeout so the listen loop can check the overall deadline.
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set read timeout");

    // 1. Ask serialosc-detector for the device list.
    let detector: SocketAddr = format!("127.0.0.1:{DETECTOR_PORT}").parse().unwrap();
    send(
        &sock,
        detector,
        "/serialosc/list",
        vec![OscType::String("127.0.0.1".into()), OscType::Int(LISTEN_PORT as i32)],
    );
    println!("asked detector for device list");

    // 2. Wait briefly for the first /serialosc/device response.
    let mut device_port: Option<u16> = None;
    let mut device_id: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline && device_port.is_none() {
        match sock.recv_from(&mut buf) {
            Ok((n, _from)) => {
                if let Ok((_, pkt)) = decoder::decode_udp(&buf[..n]) {
                    if let OscPacket::Message(m) = pkt {
                        println!("  <- {} {:?}", m.addr, m.args);
                        if m.addr == "/serialosc/device" && m.args.len() >= 3 {
                            if let (Some(OscType::String(id)), Some(OscType::Int(port))) =
                                (m.args.first(), m.args.get(2))
                            {
                                device_id = Some(id.clone());
                                device_port = Some(*port as u16);
                            }
                        }
                    }
                }
            }
            Err(_) => { /* timeout; keep looping */ }
        }
    }

    let device_port = match device_port {
        Some(p) => p,
        None => {
            eprintln!("no device replied; is serialoscd running and a grid plugged in?");
            std::process::exit(1);
        }
    };
    println!(
        "device: id={} port={}",
        device_id.as_deref().unwrap_or("?"),
        device_port
    );

    // 3. Register our listener with the device.
    let device_addr: SocketAddr = format!("127.0.0.1:{device_port}").parse().unwrap();
    send(
        &sock,
        device_addr,
        "/sys/host",
        vec![OscType::String("127.0.0.1".into())],
    );
    send(
        &sock,
        device_addr,
        "/sys/port",
        vec![OscType::Int(LISTEN_PORT as i32)],
    );
    send(
        &sock,
        device_addr,
        "/sys/prefix",
        vec![OscType::String(PREFIX.into())],
    );
    println!(
        "registered as receiver; prefix={PREFIX}; now press buttons on the grid"
    );
    println!(
        "listening for {} seconds...",
        WINDOW.as_secs()
    );

    // 4. Listen for the test window.
    let deadline = Instant::now() + WINDOW;
    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, _from)) => {
                if let Ok((_, pkt)) = decoder::decode_udp(&buf[..n]) {
                    if let OscPacket::Message(m) = pkt {
                        // Pretty-print grid/key messages specially.
                        if m.addr.ends_with("/grid/key") && m.args.len() == 3 {
                            if let (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(s))) =
                                (m.args.first(), m.args.get(1), m.args.get(2))
                            {
                                let evt = if *s == 1 { "press  " } else { "release" };
                                println!("  {} x={:>2} y={:>2}  ({})", evt, x, y, m.addr);
                                continue;
                            }
                        }
                        println!("  <- {} {:?}", m.addr, m.args);
                    }
                }
            }
            Err(_) => { /* timeout */ }
        }
    }
    println!("window closed.");
}
