// grid_synth: press a button on the monome, hear a triangle wave.
//
// Pitch layout (parameterised):
//     f(x, y) = fundamental * 2^((x_step * x + y_step * y) / edo)
//   (0, 0) = fundamental Hz.
//
// Defaults: fundamental=220, edo=46, x_step=9, y_step=1  (so (0,0) is
// 220 Hz, +x = +9/46 oct, +y = +1/46 oct). Override via CLI args:
//     grid_synth [FUND [EDO [X_STEP [Y_STEP]]]]
// e.g. grid_synth 440 72 7 1  for 440 Hz root in a 72-EDO layout.
//
// Architecture:
//   - Main thread: discovers the grid via serialoscd's detector on
//     :12002, registers as OSC receiver, then loops reading OSC
//     messages. Grid presses/releases update a shared voices map;
//     presses also light the LED at (x, y), releases darken it.
//   - Audio thread (owned by cpal): each callback locks the voices
//     map briefly, advances phases, sums triangles, writes samples.
//
// Per-voice shape: linear 5 ms attack, hold, linear 50 ms release.
// Mixing: 0.15 amplitude per voice, sum clamped to ±0.95. Plenty of
// headroom up to ~6 simultaneous keys.
//
// Exit: Ctrl-C. (LEDs aren't cleared on exit; run it again and they'll
// update as you press.)

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PREFIX: &str = "/256-1-cable";
const DETECTOR_PORT: u16 = 12002;
const LISTEN_PORT: u16 = 9000;
const AMPLITUDE: f32 = 0.15;
const ATTACK_SECS: f32 = 0.005;
const RELEASE_SECS: f32 = 0.050;

fn freq_for(x: i32, y: i32, fund: f64, edo: f64, x_step: f64, y_step: f64) -> f32 {
  (fund * 2.0_f64.powf((x_step * x as f64 + y_step * y as f64) / edo)) as f32
}

fn triangle(phase: f32) -> f32 {
  // phase is in [0, 1)
  if phase < 0.5 {
    4.0 * phase - 1.0
  } else {
    3.0 - 4.0 * phase
  }
}

#[derive(Debug, Clone, Copy)]
struct Voice {
  freq: f32,
  phase: f32,
  env: f32,
  releasing: bool,
}

type VoiceMap = HashMap<(i32, i32), Voice>;

fn send_osc(sock: &UdpSocket, dst: SocketAddr, addr: &str, args: Vec<OscType>) {
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
fn register(sock: &UdpSocket, device: SocketAddr) {
  send_osc(sock, device, "/sys/host", vec![OscType::String("127.0.0.1".into())]);
  send_osc(sock, device, "/sys/port", vec![OscType::Int(LISTEN_PORT as i32)]);
  send_osc(sock, device, "/sys/prefix", vec![OscType::String(PREFIX.into())]);
  send_osc(sock, device, &format!("{PREFIX}/grid/led/all"), vec![OscType::Int(0)]);
}

fn discover_device(sock: &UdpSocket) -> Option<u16> {
  let detector: SocketAddr = format!("127.0.0.1:{DETECTOR_PORT}").parse().ok()?;
  send_osc(
    sock,
    detector,
    "/serialosc/list",
    vec![
      OscType::String("127.0.0.1".into()),
      OscType::Int(LISTEN_PORT as i32),
    ],
  );
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut buf = [0u8; 2048];
  while Instant::now() < deadline {
    if let Ok((n, _)) = sock.recv_from(&mut buf) {
      if let Ok((_, OscPacket::Message(m))) = decoder::decode_udp(&buf[..n]) {
        if m.addr == "/serialosc/device" && m.args.len() >= 3 {
          if let Some(OscType::Int(p)) = m.args.get(2) {
            return Some(*p as u16); }}} }}
  None }

fn main() {
  let args: Vec<String> = std::env::args().collect();
  let fund: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(220.0);
  let edo: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(46.0);
  let x_step: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(9.0);
  let y_step: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1.0);
  eprintln!("tuning: fund={fund} Hz  edo={edo}  x_step={x_step}  y_step={y_step}");

  let sock = UdpSocket::bind(("0.0.0.0", LISTEN_PORT)).expect("bind UDP :9000");
  sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();

  let mut device_port = discover_device(&sock)
    .expect("no monome device found; is serialoscd running and a grid plugged in?");
  let mut device: SocketAddr = format!("127.0.0.1:{device_port}").parse().unwrap();
  eprintln!("device port: {device_port}");

  register(&sock, device);

  // Shared state: map (x, y) -> Voice.
  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));

  // --- Audio stream setup ---
  let host = cpal::default_host();
  let device_audio = host
    .default_output_device()
    .expect("no default output device");
  let supported = device_audio
    .default_output_config()
    .expect("no default output config");
  let sample_format = supported.sample_format();
  let sample_rate = supported.sample_rate().0 as f32;
  let channels = supported.channels() as usize;
  let default_buf = supported.buffer_size().clone();
  // BUF env var picks buffer_size: "default" or a positive integer.
  // Smaller → lower press-to-sound latency, but too small can silence
  // the stream on some backends. Default is cpal's default.
  let buffer_size = match std::env::var("BUF").ok().as_deref() {
    Some("default") | None => cpal::BufferSize::Default,
    Some(s) => match s.parse::<u32>() {
      Ok(n) => cpal::BufferSize::Fixed(n),
      Err(_) => {
        eprintln!("BUF={s:?} is neither 'default' nor an integer; using Default");
        cpal::BufferSize::Default
      }
    },
  };
  let config = cpal::StreamConfig {
    channels: supported.channels(),
    sample_rate: supported.sample_rate(),
    buffer_size,
  };
  eprintln!(
    "audio: {} Hz, {} channels, format={:?}, default_buf={:?}, requested_buf={:?}",
    sample_rate as u32, channels, sample_format, default_buf, config.buffer_size
  );
  assert!(
    sample_format == SampleFormat::F32,
    "this program assumes F32 samples; got {sample_format:?}"
  );

  let voices_audio = Arc::clone(&voices);
  let attack_per_sample = 1.0 / (ATTACK_SECS * sample_rate);
  let release_per_sample = 1.0 / (RELEASE_SECS * sample_rate);
  let mut promoted = false;

  let stream = device_audio
    .build_output_stream(
      &config,
      move |data: &mut [f32], _| {
        // On the very first callback, bump this thread to SCHED_FIFO
        // (realtime) via a direct syscall. Takes ~1 µs, no D-Bus.
        // Requires --cap-add=SYS_NICE --ulimit rtprio=99 on the
        // container (see docker.sh).
        if !promoted {
          promoted = true;
          unsafe {
            let param = libc::sched_param { sched_priority: 50 };
            let ret = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
            if ret == 0 {
              eprintln!("audio thread: RT priority acquired (SCHED_FIFO 50)");
            } else {
              eprintln!(
                "audio thread: sched_setscheduler failed: {}; expect glitches at small buffers",
                std::io::Error::last_os_error()
              );
            }
          }
        }
        let mut voices = voices_audio.lock().unwrap();
        for frame in data.chunks_mut(channels) {
          let mut mix = 0.0_f32;
          voices.retain(|_, v| {
            if v.releasing {
              v.env -= release_per_sample;
              if v.env <= 0.0 {
                return false;
              }
            } else if v.env < 1.0 {
              v.env = (v.env + attack_per_sample).min(1.0);
            }
            v.phase += v.freq / sample_rate;
            if v.phase >= 1.0 {
              v.phase -= 1.0;
            }
            mix += triangle(v.phase) * v.env * AMPLITUDE;
            true
          });
          let s = mix.clamp(-0.95, 0.95);
          for out in frame.iter_mut() {
            *out = s; }}},
      |e| eprintln!("audio stream error: {e}"),
      None, )
    .expect("build output stream");
  stream.play().expect("start stream");

  eprintln!("ready. press keys on the grid. Ctrl-C to quit.");

  // --- Main event loop: OSC from grid ---
  let key_addr = format!("{PREFIX}/grid/key");
  let led_set = format!("{PREFIX}/grid/led/set");
  let mut buf = [0u8; 2048];
  loop {
    match sock.recv_from(&mut buf) {
      Ok((n, _)) => {
        let pkt = match decoder::decode_udp(&buf[..n]) {
          Ok((_, p)) => p,
          Err(e) => { eprintln!("  <- decode error: {e:?}"); continue; }
        };
        // Debug-log every packet (message or bundle) before filtering,
        // so we can tell arriving-but-wrong-address apart from nothing-at-all.
        match &pkt {
          OscPacket::Message(m) => eprintln!("  <- msg {} {:?}", m.addr, m.args),
          OscPacket::Bundle(b)  => eprintln!("  <- bundle ({} items)", b.content.len()),
        }
        if let OscPacket::Message(m) = pkt {
          // Device port may change under us when serialoscd restarts.
          // Any /serialosc/device announcement with a new port: re-register.
          if m.addr == "/serialosc/device" && m.args.len() >= 3 {
            if let Some(OscType::Int(p)) = m.args.get(2) {
              let p = *p as u16;
              if p != device_port {
                eprintln!("device port changed {device_port} -> {p}; re-registering");
                device_port = p;
                device = format!("127.0.0.1:{p}").parse().unwrap();
                register(&sock, device);
              }
            }
            continue;
          }
          if m.addr != key_addr || m.args.len() != 3 {
            continue;
          }
          let (x, y, s) = match (m.args.first(), m.args.get(1), m.args.get(2)) {
            (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(s))) => {
              (*x, *y, *s)
            }
            _ => continue,
          };
          let mut vs = voices.lock().unwrap();
          let key = (x, y);
          if s == 1 {
            let freq = freq_for(x, y, fund, edo, x_step, y_step);
            eprintln!("press   x={x:>2} y={y:>2}  f={freq:.2} Hz");
            vs.entry(key)
              .and_modify(|v| {
                // Retrigger while still sounding: cancel release, keep env.
                v.releasing = false;
                v.freq = freq;
              })
              .or_insert(Voice {
                freq,
                phase: 0.0,
                env: 0.0,
                releasing: false,
              });
            drop(vs);
            send_osc(
              &sock,
              device,
              &led_set,
              vec![OscType::Int(x), OscType::Int(y), OscType::Int(1)],
            );
          } else {
            eprintln!("release x={x:>2} y={y:>2}");
            if let Some(v) = vs.get_mut(&key) {
              v.releasing = true;
            }
            drop(vs);
            send_osc(
              &sock,
              device,
              &led_set,
              vec![OscType::Int(x), OscType::Int(y), OscType::Int(0)],
            ); }}}
      Err(_) => { /* timeout, loop again */ }}} }
