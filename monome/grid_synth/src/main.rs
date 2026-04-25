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
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PREFIX: &str = "/256-1-cable";
const DETECTOR_PORT: u16 = 12002;
const LISTEN_PORT: u16 = 9000;
const AMPLITUDE: f32 = 0.15;
const ATTACK_SECS: f32 = 0.003;
const RELEASE_SECS: f32 = 0.050;
// Accretion-born voices play at this fraction of full volume.
// Currently unused — wired up by the accretion state machine in a
// later commit. Kept here so the VoiceState envelope code can refer
// to it.
#[allow(dead_code)]
const ACCRETION_TARGET: f32 = 0.5;
// Hardcoded for the 256 (16×16) grid. If smaller/larger grids appear,
// query /sys/size from serialoscd at startup and replace these.
const GRID_W: i32 = 16;
const GRID_H: i32 = 16;

fn freq_for(x: i32, y: i32, fund: f64, edo: f64, x_step: f64, y_step: f64) -> f32 {
  (fund * 2.0_f64.powf((x_step * x as f64 + y_step * y as f64) / edo)) as f32
}

// === Windows ============================================================

type Cell = (i32, i32);
// Inclusive corners.
type Rect = (Cell, Cell);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowId { Edo, Accretion2x2, EmitToggle1x1 }

#[derive(Debug, Clone, Copy)]
struct Window {
  id:   WindowId,
  rect: Rect,
}

fn rect_contains(rect: &Rect, cell: Cell) -> bool {
  let ((x0, y0), (x1, y1)) = *rect;
  let (x, y) = cell;
  x0 <= x && x <= x1 && y0 <= y && y <= y1
}

// Front-to-back: returns the first window whose rect contains `cell`.
fn window_for_cell(windows: &[Window], cell: Cell) -> Option<WindowId> {
  windows.iter().find(|w| rect_contains(&w.rect, cell)).map(|w| w.id)
}

// True iff window `from` "owns" `cell` — i.e. `from`'s rect contains
// `cell` AND no earlier (front-er) window's rect does. This is the
// compositor's only decision; pulled out as a pure fn for testing.
//
// PITFALL: the LedReasons map (later commit) is global state that
// every window writes into. Without this filter the EDO grid would
// quietly stomp the LEDs that control windows are managing. If you
// move windows around, also update what cells the EDO grid claims.
fn visible(windows: &[Window], from: WindowId, cell: Cell) -> bool {
  for w in windows {
    if w.id == from {
      return rect_contains(&w.rect, cell);
    }
    if rect_contains(&w.rect, cell) {
      return false;
    }
  }
  false
}

// LED-command compositor wrapper around send_osc. Drops writes for
// cells the calling window doesn't own.
fn set_led(
  windows: &[Window], from: WindowId, cell: Cell, on: bool,
  sock: &UdpSocket, device: SocketAddr, led_set: &str,
) {
  if !visible(windows, from, cell) { return; }
  send_osc(sock, device, led_set, vec![
    OscType::Int(cell.0), OscType::Int(cell.1),
    OscType::Int(if on { 1 } else { 0 }),
  ]);
}

// === Buttons ============================================================

// What a control-window cell does. Stored in per-window button maps
// on World; dispatched by ButtonRole, never by closure (closures
// fight Rust's borrow checker for &mut World).
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum Button {
  Toggle { state: bool, on: ButtonRole, off: ButtonRole },
  Nursed { state: bool, on: ButtonRole, off: ButtonRole },
  Fire   { fire: ButtonRole },
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum ButtonRole {
  AccreteOn, AccreteOff,
  EmitOn,    EmitOff,
  SilentFire,
  WipeFire,
  EmitIsToggleOn, EmitIsToggleOff,
}

// Per-grid pitch-class index. Two keys with the same EdoPitch sound
// the same note modulo octave. Built once at startup; lookup is O(1).
// Only meaningful when edo / x_step / y_step are integers.
struct PitchClass {
  key_to_pitch: HashMap<(i32, i32), i32>,
  pitch_to_keys: HashMap<i32, Vec<(i32, i32)>>,
}

fn build_pitch_class(x_step: i32, y_step: i32, edo: i32, w: i32, h: i32) -> PitchClass {
  let mut k2p = HashMap::new();
  let mut p2k: HashMap<i32, Vec<(i32, i32)>> = HashMap::new();
  for x in 0..w {
    for y in 0..h {
      let p = (x_step * x + y_step * y).rem_euclid(edo);
      k2p.insert((x, y), p);
      p2k.entry(p).or_default().push((x, y));
    }
  }
  PitchClass { key_to_pitch: k2p, pitch_to_keys: p2k }
}

// All cells whose LEDs reflect the same pitch class as `cell` —
// the pressed cell itself plus its enharmonic equivalents.
// Falls back to just `cell` when no pitch-class index exists
// (non-integer tuning; gone once startup validation lands).
fn cells_for_pitch_of(pc: Option<&PitchClass>, cell: Cell) -> Vec<Cell> {
  match pc.and_then(|pc| pc.key_to_pitch.get(&cell)
                          .and_then(|p| pc.pitch_to_keys.get(p))) {
    Some(keys) => keys.clone(),
    None => vec![cell],
  }
}

// === LedReasons =========================================================

// Why a cell's LED is currently lit. A cell stays lit as long as it
// has ≥1 reason and goes dark when its reason set empties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)] // Accretion variant lights up in step 5.
enum LedReason {
  PitchEquivalent { source_xy: Cell },   // a fingered key at source_xy is held
  Accretion       { pitch: i32 },        // pitch is in PitchAccretion AND emit_on
}

// Sparse: only cells with ≥1 reason appear here.
type LedReasons = HashMap<Cell, HashSet<LedReason>>;

// Mutate `reasons`; return Some(true) iff `cell` newly lit (was empty
// or absent, now has this reason). Returns None when the cell already
// had reasons, or when the reason was already present (no transition).
fn add_reason(reasons: &mut LedReasons, cell: Cell, r: LedReason) -> Option<bool> {
  let entry = reasons.entry(cell).or_default();
  let was_empty = entry.is_empty();
  let inserted = entry.insert(r);
  if was_empty && inserted { Some(true) } else { None }
}

// Mutate `reasons`; return Some(false) iff `cell` newly dark (had
// this reason, now has none). Returns None when no transition (cell
// wasn't lit, reason wasn't there, or other reasons remain).
fn remove_reason(reasons: &mut LedReasons, cell: Cell, r: LedReason) -> Option<bool> {
  let entry = match reasons.get_mut(&cell) {
    Some(s) => s,
    None => return None,
  };
  if !entry.remove(&r) { return None; }
  if entry.is_empty() {
    reasons.remove(&cell);
    Some(false)
  } else {
    None
  }
}

fn triangle(phase: f32) -> f32 {
  // phase is in [0, 1)
  if phase < 0.5 {
    4.0 * phase - 1.0
  } else {
    3.0 - 4.0 * phase
  }
}

// Per-voice audio state. The envelope is a simple "ramp env toward
// target_env by ramp_per_sample each sample, clamping at target." A
// voice is removed once env=0 AND target_env=0.
#[derive(Debug, Clone, Copy)]
struct VoiceState {
  // Read by the accretion state machine (later commit) for the
  // originVoice-still-alive checks. Unused for now.
  #[allow(dead_code)]
  id:              VoiceId,
  freq:            f32,
  phase:           f32,
  env:             f32,
  target_env:      f32,
  ramp_per_sample: f32,
}

type VoiceId = u64;

// What gave rise to this voice. The accretion state machine (later
// commit) is what populates the Accreted variant; for now every
// voice is Fingered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
enum VoiceSource {
  Fingered { xy: (i32, i32) },
  Accreted { pitch: i32 },
}

type VoiceMap = HashMap<VoiceSource, VoiceState>;

// Render one cpal callback's worth of audio into `data` from `voices`.
// Pulled out of the closure so unit tests can exercise it without cpal
// or PipeWire.
fn render_block(
  voices: &mut VoiceMap,
  data: &mut [f32],
  channels: usize,
  sample_rate: f32,
) {
  for frame in data.chunks_mut(channels) {
    let mut mix = 0.0_f32;
    voices.retain(|_, v| {
      // Step env toward target_env.
      let delta = v.target_env - v.env;
      if delta.abs() <= v.ramp_per_sample {
        v.env = v.target_env;
      } else if delta > 0.0 {
        v.env += v.ramp_per_sample;
      } else {
        v.env -= v.ramp_per_sample;
      }
      // Voice is gone once env and target both reach 0.
      if v.env == 0.0 && v.target_env == 0.0 {
        return false;
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
      *out = s;
    }
  }
}

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

  // Shared state: VoiceSource -> VoiceState. Currently only Fingered
  // entries; the accretion machinery (later commit) populates Accreted.
  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
  let mut next_voice_id: VoiceId = 0;

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
        render_block(&mut voices, data, channels, sample_rate);
      },
      |e| eprintln!("audio stream error: {e}"),
      None, )
    .expect("build output stream");
  stream.play().expect("start stream");

  eprintln!("ready. press keys on the grid. Ctrl-C to quit.");

  // Pitch-class equivalence (mod octave): all keys producing the same
  // pitch class light up together when any one is pressed, and all
  // stay lit until none is pressed. Only well-defined when the
  // tuning args are integers.
  let pitch_class: Option<PitchClass> =
    if edo.fract() == 0.0 && x_step.fract() == 0.0 && y_step.fract() == 0.0 {
      Some(build_pitch_class(x_step as i32, y_step as i32, edo as i32, GRID_W, GRID_H))
    } else {
      eprintln!("non-integer EDO/steps: lighting only the pressed key");
      None
    };
  let mut led_reasons: LedReasons = HashMap::new();

  // Windows, front-to-back. Smaller control windows occlude the EDO
  // grid below them. The EDO window covers the whole 16×16; the
  // accretion 2×2 sits in the bottom-left, the emit-toggle 1×1
  // sits next to it.
  let windows: Vec<Window> = vec![
    Window { id: WindowId::Accretion2x2,   rect: ((0, 14), (1, 15)) },
    Window { id: WindowId::EmitToggle1x1,  rect: ((2, 15), (2, 15)) },
    Window { id: WindowId::Edo,            rect: ((0, 0),  (15, 15)) },
  ];
  // Per-window button maps; their actions are no-ops in this commit
  // (the accretion state machine wires them up in a later commit).
  let _accretion_buttons: HashMap<Cell, Button> = {
    let mut m = HashMap::new();
    m.insert((0, 14), Button::Toggle { state: false, on: ButtonRole::AccreteOn,
                                       off: ButtonRole::AccreteOff });
    m.insert((1, 14), Button::Fire   { fire: ButtonRole::WipeFire });
    m.insert((0, 15), Button::Fire   { fire: ButtonRole::SilentFire });
    m.insert((1, 15), Button::Toggle { state: false, on: ButtonRole::EmitOn,
                                       off: ButtonRole::EmitOff });
    m
  };
  let _emit_toggle_button: Button =
    Button::Toggle { state: true, on: ButtonRole::EmitIsToggleOn,
                                  off: ButtonRole::EmitIsToggleOff };

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
          let cell = (x, y);
          let win = match window_for_cell(&windows, cell) {
            Some(w) => w,
            None => continue,
          };
          match win {
            WindowId::Accretion2x2 => {
              eprintln!("{} accretion-control x={x:>2} y={y:>2}",
                        if s == 1 { "press  " } else { "release" });
              // No-op until the accretion state machine lands.
            }
            WindowId::EmitToggle1x1 => {
              eprintln!("{} emit-is-toggle x={x:>2} y={y:>2}",
                        if s == 1 { "press  " } else { "release" });
              // No-op until the accretion state machine lands.
            }
            WindowId::Edo => {
              let mut vs = voices.lock().unwrap();
              let key = VoiceSource::Fingered { xy: cell };
              if s == 1 {
                let freq = freq_for(x, y, fund, edo, x_step, y_step);
                eprintln!("press   x={x:>2} y={y:>2}  f={freq:.2} Hz");
                // (b)-style retrigger: each press is a fresh voice with a
                // new id; any existing entry at this xy is overwritten.
                let id = next_voice_id;
                next_voice_id += 1;
                vs.insert(key, VoiceState {
                  id,
                  freq,
                  phase: 0.0,
                  env: 0.0,
                  target_env: 1.0,
                  ramp_per_sample: 1.0 / (ATTACK_SECS * sample_rate),
                });
                drop(vs);
                let r = LedReason::PitchEquivalent { source_xy: cell };
                for c in cells_for_pitch_of(pitch_class.as_ref(), cell) {
                  if let Some(true) = add_reason(&mut led_reasons, c, r) {
                    set_led(&windows, WindowId::Edo, c, true,
                            &sock, device, &led_set);
                  }
                }
              } else {
                eprintln!("release x={x:>2} y={y:>2}");
                if let Some(v) = vs.get_mut(&key) {
                  v.target_env = 0.0;
                  v.ramp_per_sample = v.env / (RELEASE_SECS * sample_rate);
                }
                drop(vs);
                let r = LedReason::PitchEquivalent { source_xy: cell };
                for c in cells_for_pitch_of(pitch_class.as_ref(), cell) {
                  if let Some(false) = remove_reason(&mut led_reasons, c, r) {
                    set_led(&windows, WindowId::Edo, c, false,
                            &sock, device, &led_set);
                  }
                }
              }
            }
          }
        }}
      Err(_) => { /* timeout, loop again */ }}} }

#[cfg(test)]
mod tests {
  use super::*;

  // Render N frames (mono) for a single voice with the given initial state.
  fn render_voice(v: VoiceState, n: usize, sample_rate: f32) -> Vec<f32> {
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; n];
    render_block(&mut voices, &mut data, 1, sample_rate);
    data
  }

  #[test]
  fn empty_voices_produce_silence() {
    let mut voices: VoiceMap = HashMap::new();
    let mut data = vec![0.123_f32; 64]; // pre-fill nonzero
    render_block(&mut voices, &mut data, 1, 48000.0);
    assert!(data.iter().all(|&s| s == 0.0), "expected all zeros");
  }

  #[test]
  fn sustained_voice_produces_triangle_output() {
    let sr = 48000.0;
    let v = VoiceState {
      id: 0, freq: 440.0, phase: 0.0, env: 1.0,
      target_env: 1.0, ramp_per_sample: 0.0,
    };
    let data = render_voice(v, 1024, sr);
    let peak = data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    // Capped by AMPLITUDE=0.15.
    assert!(peak > 0.10 && peak < 0.20,
      "triangle peak out of range: {peak} (want 0.10..0.20)");
  }

  #[test]
  fn pitch_class_46_9_1_groups_x1_yminus9_together() {
    // Jeff's default layout: edo=46, x_step=9, y_step=1.
    // (x, y) and (x+1, y-9) should share a pitch class.
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let p_4_10 = pc.key_to_pitch[&(4, 10)];
    let p_5_1 = pc.key_to_pitch[&(5, 1)];
    assert_eq!(p_4_10, p_5_1, "(4,10) and (5,1) should be enharmonic");
    let group = &pc.pitch_to_keys[&p_4_10];
    assert!(group.contains(&(4, 10)));
    assert!(group.contains(&(5, 1)));
  }

  #[test]
  fn cells_for_pitch_of_with_pc_returns_full_equivalence_group() {
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let g = cells_for_pitch_of(Some(&pc), (4, 10));
    assert!(g.contains(&(4, 10)));
    assert!(g.contains(&(5, 1)));
  }

  #[test]
  fn cells_for_pitch_of_without_pc_falls_back_to_single_key() {
    assert_eq!(cells_for_pitch_of(None, (7, 3)), vec![(7, 3)]);
  }

  #[test]
  fn add_then_remove_same_reason_returns_lit_then_dark_transition() {
    let mut reasons: LedReasons = HashMap::new();
    let r = LedReason::PitchEquivalent { source_xy: (3, 3) };
    assert_eq!(add_reason(&mut reasons, (4, 4), r), Some(true), "newly lit");
    assert_eq!(add_reason(&mut reasons, (4, 4), r), None, "already lit, same reason");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r), Some(false), "newly dark");
    assert!(!reasons.contains_key(&(4, 4)));
  }

  #[test]
  fn two_reasons_must_both_be_removed_before_dark_transition() {
    let mut reasons: LedReasons = HashMap::new();
    let r1 = LedReason::PitchEquivalent { source_xy: (3, 3) };
    let r2 = LedReason::PitchEquivalent { source_xy: (5, 5) };
    add_reason(&mut reasons, (4, 4), r1);
    assert_eq!(add_reason(&mut reasons, (4, 4), r2), None,
               "second reason on already-lit cell: no transition");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r1), None,
               "removing one of two: still lit");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r2), Some(false),
               "removing the last: now dark");
  }

  #[test]
  fn pitch_class_group_press_release_via_led_reasons() {
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let mut reasons: LedReasons = HashMap::new();

    // Press (4,10): every cell in the pitch-class group transitions to lit.
    let r_410 = LedReason::PitchEquivalent { source_xy: (4, 10) };
    let mut newly_lit_410 = vec![];
    for c in cells_for_pitch_of(Some(&pc), (4, 10)) {
      if let Some(true) = add_reason(&mut reasons, c, r_410) {
        newly_lit_410.push(c);
      }
    }
    assert!(newly_lit_410.contains(&(4, 10)));
    assert!(newly_lit_410.contains(&(5, 1)));

    // Press (5,1) — same group; no further LED transitions.
    let r_51 = LedReason::PitchEquivalent { source_xy: (5, 1) };
    for c in cells_for_pitch_of(Some(&pc), (5, 1)) {
      assert_eq!(add_reason(&mut reasons, c, r_51), None,
                 "cell {c:?} should already be lit");
    }

    // Release (5,1) while (4,10) is still pressed — no dark transitions.
    for c in cells_for_pitch_of(Some(&pc), (5, 1)) {
      assert_eq!(remove_reason(&mut reasons, c, r_51), None,
                 "cell {c:?} still lit by (4,10)'s reason");
    }

    // Release (4,10) — every group cell transitions to dark.
    let mut newly_dark = vec![];
    for c in cells_for_pitch_of(Some(&pc), (4, 10)) {
      if let Some(false) = remove_reason(&mut reasons, c, r_410) {
        newly_dark.push(c);
      }
    }
    assert!(newly_dark.contains(&(4, 10)));
    assert!(newly_dark.contains(&(5, 1)));
  }

  fn standard_windows() -> Vec<Window> {
    vec![
      Window { id: WindowId::Accretion2x2,  rect: ((0, 14), (1, 15)) },
      Window { id: WindowId::EmitToggle1x1, rect: ((2, 15), (2, 15)) },
      Window { id: WindowId::Edo,           rect: ((0, 0),  (15, 15)) },
    ]
  }

  #[test]
  fn event_in_2x2_goes_to_accretion_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 14)), Some(WindowId::Accretion2x2));
    assert_eq!(window_for_cell(&ws, (1, 15)), Some(WindowId::Accretion2x2));
  }

  #[test]
  fn event_in_1x1_goes_to_emit_toggle_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (2, 15)), Some(WindowId::EmitToggle1x1));
  }

  #[test]
  fn event_outside_smalls_goes_to_edo_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 0)),  Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (8, 8)),  Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (15, 15)), Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (3, 15)), Some(WindowId::Edo)); // just past 1x1
  }

  #[test]
  fn edo_writing_into_2x2_cell_is_dropped() {
    let ws = standard_windows();
    assert!(!visible(&ws, WindowId::Edo, (0, 14)));
    assert!(!visible(&ws, WindowId::Edo, (1, 15)));
  }

  #[test]
  fn edo_writing_into_owned_cell_passes() {
    let ws = standard_windows();
    assert!(visible(&ws, WindowId::Edo, (0, 0)));
    assert!(visible(&ws, WindowId::Edo, (15, 14)));
  }

  #[test]
  fn accretion_window_writes_pass_for_its_own_cells() {
    let ws = standard_windows();
    assert!(visible(&ws, WindowId::Accretion2x2, (0, 14)));
    assert!(visible(&ws, WindowId::Accretion2x2, (1, 15)));
    // …but not into cells outside its rect.
    assert!(!visible(&ws, WindowId::Accretion2x2, (2, 15)));
    assert!(!visible(&ws, WindowId::Accretion2x2, (5, 5)));
  }

  #[test]
  fn release_decays_to_zero_and_drops_voice() {
    let sr = 48000.0;
    let release_samples = (RELEASE_SECS * sr) as usize; // 2400
    let v = VoiceState {
      id: 0, freq: 220.0, phase: 0.0, env: 1.0,
      target_env: 0.0,
      ramp_per_sample: 1.0 / (RELEASE_SECS * sr),
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; release_samples + 200];
    render_block(&mut voices, &mut data, 1, sr);
    assert!(voices.is_empty(), "voice should have been dropped after release");
    let tail = &data[release_samples + 100..];
    assert!(tail.iter().all(|&s| s == 0.0),
      "tail after release should be silent");
  }
}
