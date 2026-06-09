use crate::consts::{CELL_SET_ACCRETION_TARGET, FLASH_PHASE_MS, HEARTBEAT_SECS};
use crate::pitch::{build_pitch_class, freq_for};
use crate::state::{
  chord_press, chord_release, control_press, control_release, edo_press, edo_release,
};
use crate::types::{AppState, Brightness, Button, LedCmd, VoiceMap, Window, WindowId};
use crate::voices::render_block_with_amplitude;
use crate::windows::{set_led, window_for_cell};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use midi_pulse::config::{Config, MonomeWindowConfig, SinkConfig};
use midi_pulse::monome;
use rosc::{decoder, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run_from_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  let edo_window = config
    .monome_windows
    .iter()
    .find_map(|window| {
      if let MonomeWindowConfig::EdoNoteGrid {
        monome,
        tuning,
        sink,
        ..
      } = window
      {
        Some((monome, tuning, sink))
      } else {
        None
      }
    })
    .ok_or("sawwave config requires an edo_note_grid window")?;
  let monome_config = config
    .monomes
    .iter()
    .find(|monome| monome.id == *edo_window.0)
    .ok_or("edo_note_grid references an unknown monome")?;
  let tuning = config
    .tunings
    .iter()
    .find(|tuning| tuning.id == *edo_window.1)
    .ok_or("edo_note_grid references an unknown tuning")?;
  let sink = config
    .sinks
    .iter()
    .find(|sink| sink.id() == edo_window.2.as_str())
    .ok_or("edo_note_grid references an unknown sink")?;
  let SinkConfig::CpalSawwave {
    sample_rate,
    buffer_frames,
    amplitude,
    attack_secs,
    release_secs,
    accretion_level,
    ..
  } = sink
  else {
    return Err("sawwave runtime requires a cpal_sawwave sink".into());
  };
  let grid_size = monome_config.select.size.unwrap_or([16, 16]);
  run(RuntimeSettings {
    prefix: monome_config.prefix.clone(),
    listen_port: monome_config.listen_port,
    grid_w: grid_size[0],
    grid_h: grid_size[1],
    fund: tuning.fundamental_hz,
    edo: tuning.edo as i32,
    x_step: tuning.x_step as i32,
    y_step: tuning.y_step as i32,
    sample_rate: *sample_rate,
    buffer_frames: *buffer_frames,
    amplitude: *amplitude,
    attack_secs: *attack_secs,
    release_secs: *release_secs,
    accretion_level: *accretion_level,
    windows: config_windows(&config.monome_windows),
  })
}

struct RuntimeSettings {
  prefix: String,
  listen_port: u16,
  grid_w: i32,
  grid_h: i32,
  fund: f64,
  edo: i32,
  x_step: i32,
  y_step: i32,
  sample_rate: u32,
  buffer_frames: u32,
  amplitude: f32,
  attack_secs: f32,
  release_secs: f32,
  accretion_level: f32,
  windows: Vec<Window>,
}

fn run(settings: RuntimeSettings) -> Result<(), Box<dyn std::error::Error>> {
  assert!(settings.edo > 0, "edo must be positive, got {}", settings.edo);

  let sock = UdpSocket::bind(("0.0.0.0", settings.listen_port))
    .unwrap_or_else(|e| panic!("bind UDP :{}: {e}", settings.listen_port));
  sock.set_read_timeout(Some(Duration::from_millis(50)))?;

  let mut device_port = monome::discover_devices(&sock, settings.listen_port)
    .into_iter()
    .rev()
    .find(|device| device.grid_w == settings.grid_w && device.grid_h == settings.grid_h)
    .map(|device| device.port)
    .expect("no matching monome found; is serialoscd running and the grid plugged in?");
  let mut device: SocketAddr = format!("127.0.0.1:{device_port}").parse()?;
  monome::register(&sock, device, &settings.prefix, settings.listen_port);

  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
  let audio = start_audio_stream(
    Arc::clone(&voices),
    settings.sample_rate,
    settings.buffer_frames,
    settings.amplitude,
  )?;

  let pitch_class = build_pitch_class(
    settings.x_step,
    settings.y_step,
    settings.edo,
    settings.grid_w,
    settings.grid_h,
  );
  let mut state = AppState::new_with_audio_params(
    Arc::clone(&voices),
    pitch_class,
    settings.fund,
    settings.edo,
    audio.sample_rate,
    crate::types::AudioParams {
      amplitude: settings.amplitude,
      attack_secs: settings.attack_secs,
      release_secs: settings.release_secs,
      accretion_level: settings.accretion_level,
    },
  );
  repaint(&sock, device, &settings.prefix, &settings.windows, &state);

  println!("monome EDO sawwave started");
  println!(
    "  tuning: fund={} Hz edo={} x_step={} y_step={}",
    settings.fund, settings.edo, settings.x_step, settings.y_step,
  );
  println!(
    "  audio: {} Hz, {} channels, {:?}",
    audio.sample_rate as u32, audio.channels, audio.buffer_size,
  );
  println!("Press Ctrl-C to exit...");

  static STOP: AtomicBool = AtomicBool::new(false);
  extern "C" fn on_sigint(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
  }
  unsafe {
    libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
  }

  let key_addr = format!("{}/grid/key", settings.prefix);
  let led_level_set = format!("{}/grid/led/level/set", settings.prefix);
  let mut buf = [0u8; 2048];
  let start = Instant::now();
  let mut last_flash_phase = 2u8;
  let mut last_heartbeat = Instant::now();

  while !STOP.load(Ordering::SeqCst) {
    if state.target_select_mode {
      let phase = ((start.elapsed().as_millis() / FLASH_PHASE_MS) & 1) as u8;
      if phase != last_flash_phase {
        last_flash_phase = phase;
        let brightness = if phase == 0 { Brightness::Bright } else { Brightness::Off };
        set_led(
          &settings.windows,
          WindowId::ControlsTop,
          CELL_SET_ACCRETION_TARGET,
          brightness,
          &sock,
          device,
          &led_level_set,
        );
      }
    } else {
      last_flash_phase = 2;
    }

    if last_heartbeat.elapsed().as_secs_f64() >= HEARTBEAT_SECS {
      let cb = audio.cb_count.swap(0, Ordering::Relaxed);
      let frames = audio.sample_count.swap(0, Ordering::Relaxed);
      let peak = f32::from_bits(audio.peak_bits.swap(0, Ordering::Relaxed));
      let voice_n = state.voices.lock().unwrap().len();
      eprintln!("[hb] cb={cb} frames={frames} voices={voice_n} peak={peak:.3}");
      last_heartbeat = Instant::now();
    }

    let pkt = match sock.recv_from(&mut buf) {
      Ok((n, _)) => match decoder::decode_udp(&buf[..n]) {
        Ok((_, packet)) => packet,
        Err(e) => {
          eprintln!("OSC decode error: {e:?}");
          continue;
        }
      },
      Err(_) => continue,
    };
    let OscPacket::Message(message) = pkt else {
      continue;
    };
    if message.addr == "/serialosc/device" && message.args.len() >= 3 {
      if let Some(OscType::Int(port)) = message.args.get(2) {
        let port = *port as u16;
        if port != device_port {
          device_port = port;
          device = format!("127.0.0.1:{port}").parse()?;
          monome::register(&sock, device, &settings.prefix, settings.listen_port);
          repaint(&sock, device, &settings.prefix, &settings.windows, &state);
        }
      }
      continue;
    }
    if message.addr != key_addr || message.args.len() != 3 {
      continue;
    }
    let (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(s))) =
      (message.args.first(), message.args.get(1), message.args.get(2))
    else {
      continue;
    };
    let cell = (*x, *y);
    let Some(window) = window_for_cell(&settings.windows, cell) else {
      continue;
    };
    let press = *s == 1;
    let diffs: Vec<LedCmd> = match window {
      WindowId::Edo => {
        if press {
          let f = freq_for(*x, *y, settings.fund, settings.edo, settings.x_step, settings.y_step);
          eprintln!("press x={x:>2} y={y:>2} f={f:.2} Hz");
          edo_press(&mut state, cell)
        } else {
          edo_release(&mut state, cell)
        }
      }
      WindowId::ControlsTop => {
        if press {
          control_press(&mut state, cell, window)
        } else {
          control_release(&mut state, cell, window)
        }
      }
      WindowId::ControlsBottom => {
        let chord_id = *x as usize;
        if press {
          chord_press(&mut state, chord_id)
        } else {
          chord_release(&mut state, chord_id)
        }
      }
    };
    for (from, cell, brightness) in diffs {
      set_led(
        &settings.windows,
        from,
        cell,
        brightness,
        &sock,
        device,
        &led_level_set,
      );
    }
  }

  monome::send_led_all(&sock, device, &settings.prefix, 0);
  Ok(())
}

struct AudioRuntime {
  _stream: cpal::Stream,
  sample_rate: f32,
  channels: usize,
  buffer_size: cpal::BufferSize,
  cb_count: Arc<AtomicU64>,
  sample_count: Arc<AtomicU64>,
  peak_bits: Arc<AtomicU32>,
}

fn start_audio_stream(
  voices: Arc<Mutex<VoiceMap>>,
  requested_sample_rate: u32,
  requested_buffer_frames: u32,
  amplitude: f32,
) -> Result<AudioRuntime, Box<dyn std::error::Error>> {
  let host = cpal::default_host();
  let device = host.default_output_device().ok_or("no default output device")?;
  let default_cfg = device.default_output_config()?;
  let default_channels = default_cfg.channels();
  let supported = device
    .supported_output_configs()?
    .filter(|config| {
      config.sample_format() == SampleFormat::F32
        && config.min_sample_rate().0 <= requested_sample_rate
        && config.max_sample_rate().0 >= requested_sample_rate
    })
    .max_by_key(|config| (config.channels() == default_channels, config.channels()))
    .map(|config| config.with_sample_rate(cpal::SampleRate(requested_sample_rate)))
    .unwrap_or(default_cfg);
  let sample_format = supported.sample_format();
  if sample_format != SampleFormat::F32 {
    return Err(format!("sawwave runtime requires F32 output, got {sample_format:?}").into());
  }
  let sample_rate = supported.sample_rate().0 as f32;
  let channels = supported.channels() as usize;
  let buffer_size = cpal::BufferSize::Fixed(requested_buffer_frames);
  let stream_config = cpal::StreamConfig {
    channels: supported.channels(),
    sample_rate: supported.sample_rate(),
    buffer_size,
  };

  let cb_count = Arc::new(AtomicU64::new(0));
  let sample_count = Arc::new(AtomicU64::new(0));
  let peak_bits = Arc::new(AtomicU32::new(0));
  let cb_count_audio = Arc::clone(&cb_count);
  let sample_count_audio = Arc::clone(&sample_count);
  let peak_bits_audio = Arc::clone(&peak_bits);
  let stream = device.build_output_stream(
    &stream_config,
    move |data: &mut [f32], _| {
      let mut voices = voices.lock().unwrap();
      // Legacy runtime has no AM (default timbres), so the family is inert.
      render_block_with_amplitude(
        &mut voices, data, channels, sample_rate, amplitude,
        crate::types::AmShapeFamily::default(),
      );
      cb_count_audio.fetch_add(1, Ordering::Relaxed);
      sample_count_audio.fetch_add((data.len() / channels) as u64, Ordering::Relaxed);
      let peak = data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
      let current = f32::from_bits(peak_bits_audio.load(Ordering::Relaxed));
      if peak > current {
        peak_bits_audio.store(peak.to_bits(), Ordering::Relaxed);
      }
    },
    |error| eprintln!("audio stream error: {error:?}"),
    None,
  )?;
  stream.play()?;

  Ok(AudioRuntime {
    _stream: stream,
    sample_rate,
    channels,
    buffer_size,
    cb_count,
    sample_count,
    peak_bits,
  })
}

fn config_windows(config_windows: &[MonomeWindowConfig]) -> Vec<Window> {
  let mut edo = None;
  let mut controls_top = None;
  let mut controls_bottom = None;

  for window in config_windows {
    match window {
      MonomeWindowConfig::EdoNoteGrid { .. } => {
        edo = Some(rect_to_window(WindowId::Edo, window.rect()));
      }
      MonomeWindowConfig::ChordWipeButton { .. }
      | MonomeWindowConfig::ChordAccreteToggle { .. }
      | MonomeWindowConfig::ChordEmitModeToggle { .. }
      | MonomeWindowConfig::ChordTargetButton { .. } => {
        merge_window_rect(&mut controls_top, WindowId::ControlsTop, window.rect());
      }
      MonomeWindowConfig::ChordSlotButtons { .. } => {
        controls_bottom = Some(rect_to_window(WindowId::ControlsBottom, window.rect()));
      }
      _ => {}
    }
  }

  // The imported sawwave compositor is front-to-back. Config files are
  // back-to-front, and the unbundled chord controls are composed into
  // the legacy coarse windows here.
  let mut windows = vec![];
  if let Some(window) = controls_top {
    windows.push(window);
  }
  if let Some(window) = controls_bottom {
    windows.push(window);
  }
  if let Some(window) = edo {
    windows.push(window);
  }
  windows
}

fn rect_to_window(id: WindowId, rect: [i32; 4]) -> Window {
  let [x0, y0, x1, y1] = rect;
  Window {
    id,
    rect: ((x0, y0), (x1, y1)),
  }
}

fn merge_window_rect(target: &mut Option<Window>, id: WindowId, rect: [i32; 4]) {
  let next = rect_to_window(id, rect);
  match target {
    Some(existing) => {
      existing.rect.0 .0 = existing.rect.0 .0.min(next.rect.0 .0);
      existing.rect.0 .1 = existing.rect.0 .1.min(next.rect.0 .1);
      existing.rect.1 .0 = existing.rect.1 .0.max(next.rect.1 .0);
      existing.rect.1 .1 = existing.rect.1 .1.max(next.rect.1 .1);
    }
    None => *target = Some(next),
  }
}

fn repaint(
  sock: &UdpSocket,
  device: SocketAddr,
  prefix: &str,
  windows: &[Window],
  state: &AppState,
) {
  let led_level_set = format!("{prefix}/grid/led/level/set");
  for (&cell, button) in &state.control_buttons {
    let brightness = if cell == CELL_SET_ACCRETION_TARGET {
      Brightness::Dim
    } else {
      let lit = match button {
        Button::Toggle { state, .. } | Button::Nursed { state, .. } => *state,
        Button::Fire { .. } => false,
      };
      if lit {
        Brightness::Bright
      } else {
        Brightness::Off
      }
    };
    set_led(windows, WindowId::ControlsTop, cell, brightness, sock, device, &led_level_set);
  }
  for chord in 0..crate::consts::N_CHORDS {
    let cell = (chord as i32, 15);
    let brightness = crate::state::chord_button_brightness(state, chord);
    set_led(
      windows,
      WindowId::ControlsBottom,
      cell,
      brightness,
      sock,
      device,
      &led_level_set,
    );
  }
  for &cell in state.pitchled_reasons.keys() {
    set_led(
      windows,
      WindowId::Edo,
      cell,
      Brightness::Bright,
      sock,
      device,
      &led_level_set,
    );
  }
}
