// grid_synth: press a button on the monome, hear a triangle wave.
//
// Pitch layout (parameterised):
//     f(x, y) = fundamental * 2^((x_step * x + y_step * y) / edo)
//   (0, 0) = fundamental Hz.
//
// Defaults: fundamental=220, edo=46, x_step=9, y_step=1  (so (0,0)
// is 220 Hz, +x = +9/46 oct, +y = +1/46 oct). Override via CLI args:
//     grid_synth [FUND [EDO [X_STEP [Y_STEP]]]]
// e.g. grid_synth 440 72 7 1  for 440 Hz root in a 72-EDO layout.
//
// Architecture (now split across modules, see types.rs / consts.rs
// and the per-domain modules pitch / voices / leds / windows / osc /
// state / diagnostics):
//   - Main thread: discovers the grid via serialoscd's detector on
//     :12002, registers as OSC receiver, then loops reading OSC
//     messages and dispatching to AppState event handlers in
//     state.rs. Every 1 s it logs a heartbeat with the audio
//     thread's callback / sample / peak counters, and triggers
//     diagnostic capture if it sees STALL twice in a row.
//   - Audio thread (owned by cpal): each callback locks the voices
//     map briefly, calls render_block, and bumps the heartbeat
//     counters. First callback also self-promotes to SCHED_FIFO.
//   - SIGINT handler: flips a static AtomicBool the loop watches;
//     on exit, /grid/led/all 0 wipes the device.

mod consts;
mod diagnostics;
mod leds;
mod osc;
mod pitch;
mod state;
mod types;
mod voices;
mod windows;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use rosc::{decoder, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::consts::{
  GRID_H, GRID_W, HEARTBEAT_SECS, LISTEN_PORT, PREFIX,
};
use crate::diagnostics::capture_stall_diagnostics;
use crate::osc::{discover_device, register, send_osc};
use crate::pitch::{build_pitch_class, freq_for};
use crate::state::{control_press, control_release, edo_press, edo_release};
use crate::types::{
  AppState, Brightness, Button, LedCmd, PitchClass, VoiceMap, Window, WindowId,
};
use crate::voices::render_block;
use crate::windows::{set_led, window_for_cell};

fn main() {
  let args: Vec<String> = std::env::args().collect();
  // fund stays f64 (it's a continuous frequency in Hz). edo / x_step
  // / y_step are integer-only — without that, pitch math sees floating
  // -point jitter and accretion's "is this pitch in the slot?" check
  // gets racy. Reject non-integer here at startup.
  let fund: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(220.0);
  let edo: i32 = args.get(2).map(|s| s.parse::<i32>()
        .unwrap_or_else(|_| panic!("edo must be a positive integer, got {s:?}")))
    .unwrap_or(46);
  let x_step: i32 = args.get(3).map(|s| s.parse::<i32>()
        .unwrap_or_else(|_| panic!("x_step must be an integer, got {s:?}")))
    .unwrap_or(9);
  let y_step: i32 = args.get(4).map(|s| s.parse::<i32>()
        .unwrap_or_else(|_| panic!("y_step must be an integer, got {s:?}")))
    .unwrap_or(1);
  assert!(edo > 0, "edo must be positive, got {edo}");
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
    None => discover_device(&sock, listen_port)
      .expect("no monome device found; is serialoscd running and a grid plugged in?"),
  };
  let mut device: SocketAddr = format!("127.0.0.1:{device_port}").parse().unwrap();
  eprintln!("device port: {device_port}");

  register(&sock, device, listen_port);

  // Shared state: VoiceSource -> VoiceState. Both Fingered (a key is
  // held) and Accreted (the slot is sounding it) variants live here.
  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));

  // --- Audio stream setup ---
  let host = cpal::default_host();
  let device_audio = host
    .default_output_device()
    .expect("no default output device");
  // Prefer a 48 kHz F32 config: PipeWire runs at 48 k natively, so
  // asking for 44.1 k forces an extra resampler in the chain. Match
  // the default config's channel count — picking the wrong one (e.g.
  // a mono variant PipeWire exposes but doesn't actually route)
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

  // Heartbeat counters. Read from the main loop ~1×/s; the audio
  // thread bumps them on every callback. Lets us tell three failure
  // modes apart when sound goes silent: (1) callbacks stopped firing
  // (PipeWire dropped the stream), (2) callbacks fire but peak=0 with
  // voices present (render bug), (3) callbacks fire and peak>0 but
  // you hear nothing (PipeWire routing — restart the graph).
  let cb_count       = Arc::new(AtomicU64::new(0));
  let sample_count   = Arc::new(AtomicU64::new(0));
  // Peak is f32; store it as bits so we can use AtomicU32. Race
  // between "load current" and "store new max" is fine — observability,
  // not correctness, and the worst case loses one sample's max.
  let peak_bits      = Arc::new(AtomicU32::new(0));
  // Recorded by the audio thread on first callback so we can read
  // /proc/self/task/<tid>/status from the main thread on stall.
  let audio_tid      = Arc::new(AtomicU32::new(0));
  let cb_count_audio     = Arc::clone(&cb_count);
  let sample_count_audio = Arc::clone(&sample_count);
  let peak_bits_audio    = Arc::clone(&peak_bits);
  let audio_tid_audio    = Arc::clone(&audio_tid);

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
            // Record TID so the main thread can inspect this thread's
            // /proc state when investigating a stall.
            audio_tid_audio.store(libc::gettid() as u32, Ordering::Relaxed);
          }
        }
        let mut voices = voices_audio.lock().unwrap();
        render_block(&mut voices, data, channels, sample_rate);
        // Heartbeat: count this callback, the frames we wrote, and
        // the peak |sample| we produced.
        cb_count_audio.fetch_add(1, Ordering::Relaxed);
        sample_count_audio.fetch_add((data.len() / channels) as u64, Ordering::Relaxed);
        let peak = data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
        let peak_u = peak.to_bits();
        let cur = peak_bits_audio.load(Ordering::Relaxed);
        if peak_u > cur || f32::from_bits(cur) < peak {
          peak_bits_audio.store(peak_u, Ordering::Relaxed);
        }
      },
      |e| eprintln!("audio stream error: {e:?}"),
      None, )
    .expect("build output stream");
  stream.play().expect("start stream");

  eprintln!("ready. press keys on the grid. Ctrl-C to quit.");

  // Build pitch_class and the per-event state container.
  let pitch_class: PitchClass =
    build_pitch_class(x_step, y_step, edo, GRID_W, GRID_H);
  let mut state = AppState::new(
    Arc::clone(&voices), pitch_class, fund, edo, sample_rate,
  );

  // Windows, front-to-back. Smaller control windows occlude the EDO
  // grid below them.
  let windows: Vec<Window> = vec![
    Window { id: WindowId::Accretion2x2,   rect: ((0, 14), (1, 15)) },
    Window { id: WindowId::EmitToggle1x1,  rect: ((2, 15), (2, 15)) },
    Window { id: WindowId::Edo,            rect: ((0, 0),  (15, 15)) },
  ];

  // --- Main event loop: OSC from grid ---
  let key_addr = format!("{PREFIX}/grid/key");
  let led_level_set = format!("{PREFIX}/grid/led/level/set");
  let led_all = format!("{PREFIX}/grid/led/all");
  let mut buf = [0u8; 2048];

  // Paint every LED that should be lit, given the current state.
  // Called at startup (after register() clears the grid) and again
  // after every re-register triggered by a /serialosc/device port
  // change — without that second call, a stale-tty announcement
  // mid-session silently wipes our control LEDs and any
  // pitch-equivalent / accretion lighting.
  let repaint = |state: &AppState, device: SocketAddr| {
    // Control buttons (Toggle/Nursed): light if state is true.
    for (&cell, button) in &state.control_buttons {
      let lit = match button {
        Button::Toggle { state, .. } | Button::Nursed { state, .. } => *state,
        Button::Fire { .. } => false,
      };
      let b = if lit { Brightness::Bright } else { Brightness::Off };
      if let Some(win) = window_for_cell(&windows, cell) {
        set_led(&windows, win, cell, b, &sock, device, &led_level_set);
      }
    }
    // EDO grid: any cell with at least one PitchLedReason is lit.
    for &cell in state.pitchled_reasons.keys() {
      set_led(&windows, WindowId::Edo, cell, Brightness::Bright,
              &sock, device, &led_level_set);
    }
  };
  repaint(&state, device);

  // SIGINT handler: just flip a flag the main loop watches. Avoids
  // running anything non-async-signal-safe from the handler itself.
  static STOP: AtomicBool = AtomicBool::new(false);
  extern "C" fn on_sigint(_: libc::c_int) { STOP.store(true, Ordering::SeqCst); }
  let handler: extern "C" fn(libc::c_int) = on_sigint;
  unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t); }

  // Heartbeat cadence: poll the audio counters every HEARTBEAT_SECS
  // and log one summary line. STALL warning if no callbacks fired.
  // Two STALL heartbeats in a row triggers an automatic snapshot
  // of PipeWire / process state to /tmp/grid_synth-stall-*.txt so
  // we can investigate without needing a terminal ready.
  let mut last_heartbeat = Instant::now();
  let mut consecutive_stalls: u32 = 0;
  let mut stall_captured = false;
  loop {
    if STOP.load(Ordering::SeqCst) { break; }
    // Heartbeat — log once per HEARTBEAT_SECS regardless of OSC activity.
    let elapsed = last_heartbeat.elapsed();
    if elapsed.as_secs_f64() >= HEARTBEAT_SECS {
      let cb = cb_count.swap(0, Ordering::Relaxed);
      let frames = sample_count.swap(0, Ordering::Relaxed);
      let peak = f32::from_bits(peak_bits.swap(0, Ordering::Relaxed));
      let voice_n = state.voices.lock().unwrap().len();
      let elapsed_ms = elapsed.as_millis();
      eprintln!(
        "[hb] {elapsed_ms}ms cb={cb} frames={frames} voices={voice_n} peak={peak:.3}{}{}",
        if cb == 0 { " STALL no-callbacks" } else { "" },
        if voice_n > 0 && peak == 0.0 && cb > 0 {
          " WARN voices-but-silent"
        } else { "" },
      );
      if cb == 0 {
        consecutive_stalls += 1;
        if consecutive_stalls == 2 && !stall_captured {
          stall_captured = true;
          capture_stall_diagnostics("first-stall",
                                    audio_tid.load(Ordering::Relaxed));
        }
      } else {
        if consecutive_stalls > 0 {
          eprintln!("[hb] callbacks resumed after {consecutive_stalls} stall heartbeats");
        }
        consecutive_stalls = 0;
        stall_captured = false; // arm capture for any future stall episode
      }
      last_heartbeat = Instant::now();
    }
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
                register(&sock, device, listen_port);
                // register() just sent /grid/led/all 0; restore our
                // current LED state on the new port.
                repaint(&state, device);
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
          let press = s == 1;
          let diffs: Vec<LedCmd> = match win {
            WindowId::Edo => {
              if press {
                let f = freq_for(x, y, fund, edo, x_step, y_step);
                eprintln!("press   x={x:>2} y={y:>2}  f={f:.2} Hz");
                edo_press(&mut state, cell)
              } else {
                eprintln!("release x={x:>2} y={y:>2}");
                edo_release(&mut state, cell)
              }
            }
            WindowId::Accretion2x2 | WindowId::EmitToggle1x1 => {
              eprintln!("{} control x={x:>2} y={y:>2}",
                        if press { "press  " } else { "release" });
              if press { control_press(&mut state, cell, win) }
              else     { control_release(&mut state, cell, win) }
            }
          };
          for (from, c, b) in diffs {
            set_led(&windows, from, c, b, &sock, device, &led_level_set);
          }
        }}
      Err(_) => { /* timeout, loop again */ }}}
  // Loop exited (Ctrl-C). Wipe the grid so we don't leave stale
  // LEDs lit on the device.
  send_osc(&sock, device, &led_all, vec![OscType::Int(0)]);
  eprintln!("monome cleared; bye.");
}
