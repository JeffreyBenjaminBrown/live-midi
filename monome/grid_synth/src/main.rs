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
// A short noise burst at press-onset, to compare ear-time of audio
// against the physical click of the button.
const CLICK_SECS: f32 = 0.003;
const CLICK_AMPLITUDE: f32 = 0.4;

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

// Render one cpal callback's worth of audio into `data` from `voices`.
// Pulled out of the closure so unit tests can exercise it without cpal
// or PipeWire.
fn render_block(
  voices: &mut VoiceMap,
  data: &mut [f32],
  channels: usize,
  sample_rate: f32,
  attack_per_sample: f32,
  release_per_sample: f32,
  click_samples_total: u32,
  rng: &mut u32,
) {
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
      if v.click_samples > 0 {
        // xorshift32
        *rng ^= *rng << 13;
        *rng ^= *rng >> 17;
        *rng ^= *rng << 5;
        let n = (*rng as i32 as f32) / (i32::MAX as f32);
        let click_env = v.click_samples as f32 / click_samples_total as f32;
        mix += n * click_env * CLICK_AMPLITUDE;
        v.click_samples -= 1;
      }
      true
    });
    let s = mix.clamp(-0.95, 0.95);
    for out in frame.iter_mut() {
      *out = s;
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct Voice {
  freq: f32,
  phase: f32,
  env: f32,
  releasing: bool,
  click_samples: u32,
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

  // GRID_LISTEN_PORT overrides the default :9000 — useful for running
  // a second instance alongside the live one for testing.
  let listen_port: u16 = std::env::var("GRID_LISTEN_PORT").ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(LISTEN_PORT);
  let sock = UdpSocket::bind(("0.0.0.0", listen_port))
    .unwrap_or_else(|e| panic!("bind UDP :{listen_port}: {e}"));
  sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();

  // GRID_DEVICE_PORT skips serialoscd discovery — useful for running
  // the audio path without a monome plugged in (testing, etc.). LED
  // commands still get sent but go nowhere.
  let mut device_port = match std::env::var("GRID_DEVICE_PORT").ok()
    .and_then(|s| s.parse::<u16>().ok())
  {
    Some(p) => { eprintln!("GRID_DEVICE_PORT={p}; skipping discovery"); p }
    None => discover_device(&sock)
      .expect("no monome device found; is serialoscd running and a grid plugged in?"),
  };
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
  // Prefer a 48 kHz F32 config: PipeWire runs at 48 k natively, so
  // asking for 44.1 k forces an extra resampler in the chain. Match
  // the default config's channel count — picking the wrong one
  // (e.g. a mono variant PipeWire exposes but doesn't actually route)
  // results in a stream that opens but never fires its callback.
  let default_cfg = device_audio
    .default_output_config()
    .expect("no default output config");
  let default_channels = default_cfg.channels();
  let supported = device_audio
    .supported_output_configs()
    .expect("query output configs")
    .filter(|c| c.sample_format() == SampleFormat::F32
                && c.min_sample_rate().0 <= 48000
                && c.max_sample_rate().0 >= 48000)
    .max_by_key(|c| (c.channels() == default_channels, c.channels()))
    .map(|c| c.with_sample_rate(cpal::SampleRate(48000)))
    .unwrap_or_else(|| {
      eprintln!("no 48 kHz F32 config; falling back to default");
      default_cfg
    });
  let sample_format = supported.sample_format();
  let sample_rate = supported.sample_rate().0 as f32;
  let channels = supported.channels() as usize;
  let default_buf = supported.buffer_size().clone();
  // BUF env var picks buffer_size. Default is 128 frames (≈2.7 ms at
  // 48 kHz). Smaller → lower press-to-sound latency, but too small
  // can xrun. "default" → cpal's default; integer → that frame count.
  let buffer_size = match std::env::var("BUF").ok().as_deref() {
    Some("default") => cpal::BufferSize::Default,
    None => cpal::BufferSize::Fixed(128),
    Some(s) => match s.parse::<u32>() {
      Ok(n) => cpal::BufferSize::Fixed(n),
      Err(_) => {
        eprintln!("BUF={s:?} is neither 'default' nor an integer; using Fixed(128)");
        cpal::BufferSize::Fixed(128)
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
  let click_samples_total: u32 = (CLICK_SECS * sample_rate) as u32;
  let mut promoted = false;
  let mut rng: u32 = 0xCAFEBABE;

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
        render_block(
          &mut voices, data, channels, sample_rate,
          attack_per_sample, release_per_sample,
          click_samples_total, &mut rng,
        );
      },
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
                v.click_samples = click_samples_total;
              })
              .or_insert(Voice {
                freq,
                phase: 0.0,
                env: 0.0,
                releasing: false,
                click_samples: click_samples_total,
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

#[cfg(test)]
mod tests {
  use super::*;

  fn fresh_rng() -> u32 { 0xCAFEBABE }

  // Render N frames (mono) for a single voice with the given initial state.
  fn render_voice(v: Voice, n: usize, sample_rate: f32) -> Vec<f32> {
    let mut voices: VoiceMap = HashMap::new();
    voices.insert((0, 0), v);
    let mut data = vec![0.0_f32; n];
    let mut rng = fresh_rng();
    let click_total = (CLICK_SECS * sample_rate) as u32;
    render_block(
      &mut voices, &mut data, 1, sample_rate,
      1.0 / (ATTACK_SECS * sample_rate),
      1.0 / (RELEASE_SECS * sample_rate),
      click_total, &mut rng,
    );
    data
  }

  #[test]
  fn empty_voices_produce_silence() {
    let mut voices: VoiceMap = HashMap::new();
    let mut data = vec![0.123_f32; 64]; // pre-fill nonzero
    let mut rng = fresh_rng();
    render_block(&mut voices, &mut data, 1, 48000.0, 0.01, 0.01, 144, &mut rng);
    assert!(data.iter().all(|&s| s == 0.0), "expected all zeros");
  }

  #[test]
  fn click_produces_nonzero_output_in_first_3ms() {
    // 48 kHz, 3 ms click → 144 samples. Voice with env=1, click_samples=144.
    let sr = 48000.0;
    let click_total = (CLICK_SECS * sr) as u32;
    let v = Voice {
      freq: 220.0, phase: 0.0, env: 1.0,
      releasing: false, click_samples: click_total,
    };
    // Render past the click window so we can check both regions.
    let data = render_voice(v, 1024, sr);

    let click_region = &data[..click_total as usize];
    let post_click = &data[click_total as usize..];

    let click_peak = click_region.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    let post_peak = post_click.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));

    // Click region should be louder than just the triangle (AMPLITUDE=0.15).
    assert!(click_peak > 0.20,
      "click region peak too quiet: {click_peak} (want > 0.20)");
    // Post-click region should still have triangle audio (env decays slowly).
    assert!(post_peak > 0.05,
      "post-click triangle too quiet: {post_peak} (want > 0.05)");
  }

  #[test]
  fn no_click_just_triangle() {
    let sr = 48000.0;
    let v = Voice {
      freq: 440.0, phase: 0.0, env: 1.0,
      releasing: false, click_samples: 0,
    };
    let data = render_voice(v, 1024, sr);
    let peak = data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    // Only the triangle, capped by AMPLITUDE=0.15.
    assert!(peak > 0.10 && peak < 0.20,
      "triangle-only peak out of range: {peak} (want 0.10..0.20)");
  }

  #[test]
  fn release_decays_to_zero_and_drops_voice() {
    let sr = 48000.0;
    let release_samples = (RELEASE_SECS * sr) as usize; // 2400
    let v = Voice {
      freq: 220.0, phase: 0.0, env: 1.0,
      releasing: true, click_samples: 0,
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert((0, 0), v);
    let mut data = vec![0.0_f32; release_samples + 200];
    let mut rng = fresh_rng();
    render_block(
      &mut voices, &mut data, 1, sr,
      1.0 / (ATTACK_SECS * sr),
      1.0 / (RELEASE_SECS * sr),
      144, &mut rng,
    );
    assert!(voices.is_empty(), "voice should have been dropped after release");
    let tail = &data[release_samples + 100..];
    assert!(tail.iter().all(|&s| s == 0.0),
      "tail after release should be silent");
  }
}
