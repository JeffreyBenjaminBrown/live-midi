//! The KMSS drumkit runtime: read the SoftStep's tether/hosted sensor stream and
//! fire drum one-shots with VELOCITY derived from how hard each pad is struck.
//!
//! On start it switches the device into tether mode (over rawmidi via `amidi`),
//! reads the per-sensor Control-Change stream through midir, derives per-pad onset +
//! attack-peak velocity (`decode`), and fires samples through a `cpal_sampler` sink
//! (`audio`). On exit -- including Ctrl-C -- it restores standalone mode (`tether`).
//! Two feet stream independently, so a simultaneous two-pad strike fires both: the
//! limitation the old Program-Change path could not escape (see `learnings/keith-mcmillen-softstep.org`).

pub mod audio;
pub mod decode;
pub mod samples;
pub mod tether;

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use midir::{MidiInput, MidiInputConnection, MidiInputPort};

use midi_pulse::config::{drum_samples_dir, Config, SinkConfig, SoftstepWindowConfig};

use audio::{Sampler, Trigger};
use decode::{collect_control_changes, gain_from_velocity, Hit, TetherDecoder};
use samples::DrumSample;

/// One pedal's resolved binding: which sample to fire, at what *base* gain (the
/// vel-127 level, scaled down by velocity), into which sink. Cloning a `Trigger` is
/// cheap (an `Arc`), so a pad copies its sink's handle.
struct PadBinding {
  sample: Arc<DrumSample>,
  gain: f32,
  trigger: Trigger,
  /// For the startup summary / trace only.
  voice_label: String,
}

/// What a single KMSS device needs at runtime: its pedal table (indexed by printed
/// label) and the port-name substring used to find its MIDI input.
struct DeviceBuild {
  pedal_map: [Option<PadBinding>; 10],
  select_substring: String,
  /// Per-pad minimum gap between hits (widest among the device's drumkit windows).
  debounce_ms: u64,
}

/// A live drumkit: the sampler streams, the bound MIDI inputs, and the per-device
/// voice timers, plus the tether session that restores standalone mode. Kept alive
/// for the run; dropping it stops the timers, releases the MIDI connections, and
/// (via the tether session) restores the device to standalone mode. This is what
/// lets the drumkit run *alongside* other surfaces (the two-grid runtime) in one
/// process rather than only as the whole app.
pub struct DrumSession {
  timers: Vec<(Arc<AtomicBool>, JoinHandle<()>)>,
  // Dropped after the timers stop (field order): the MIDI callbacks stop feeding the
  // decoders, then the tether session restores standalone mode last.
  _connections: Vec<MidiInputConnection<()>>,
  _samplers: HashMap<String, Sampler>,
  _tether: tether::TetherSession,
}

impl Drop for DrumSession {
  fn drop(&mut self) {
    for (stop, _) in &self.timers {
      stop.store(true, Ordering::Relaxed);
    }
    for (_, handle) in self.timers.drain(..) {
      let _ = handle.join();
    }
    // `_connections`, `_samplers`, then `_tether` drop after this in field order, so
    // standalone mode is restored only once the sensor stream has been released.
  }
}

pub fn run_from_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  // Arm Ctrl-C / SIGTERM restoration FIRST, before any audio or MIDI thread spawns,
  // so the signal block is inherited by all of them and a stray signal can't leave
  // the device stuck in tether mode. `start` enters tether mode once setup succeeds.
  let session = start(config, tether::arm())?;

  println!("\nDrumkit ready (tether mode, velocity-sensitive). Step on the pedals. Press Enter to exit...");
  let mut line = String::new();
  io::stdin().read_line(&mut line)?;

  // Dropping the session stops the voice timers, releases the MIDI connections, and
  // restores standalone mode.
  drop(session);
  Ok(())
}

/// Bring up the drumkit and return a live [`DrumSession`] without blocking. The
/// caller owns the `tether` session (armed via [`tether::arm`] for the standalone
/// runtime, or unarmed via [`tether::session`] for a host runtime that owns its own
/// signal handling) and keeps the returned `DrumSession` alive for the run. Reads
/// `MIDI_PULSE_NO_AUDIO` / `MIDI_PULSE_TRACE` from the environment, like the
/// standalone path.
pub fn start(
  config: &Config,
  tether_session: tether::TetherSession,
) -> Result<DrumSession, Box<dyn std::error::Error>> {
  let no_audio = std::env::var_os("MIDI_PULSE_NO_AUDIO").is_some();
  let trace = std::env::var_os("MIDI_PULSE_TRACE").is_some();

  // 1. Start a sampler per cpal_sampler sink referenced by some drumkit window.
  let referenced: HashSet<&str> = config
    .softstep_windows
    .iter()
    .map(|w| match w {
      SoftstepWindowConfig::Drumkit { sink, .. } => sink.as_str(),
    })
    .collect();
  let mut samplers: HashMap<String, Sampler> = HashMap::new();
  for sink in &config.sinks {
    if let SinkConfig::CpalSampler { id, sample_rate, buffer_frames, amplitude } = sink {
      if !referenced.contains(id.as_str()) {
        continue;
      }
      let sampler = if no_audio {
        Sampler::start_null(*sample_rate)
      } else {
        Sampler::start(*sample_rate, *buffer_frames, *amplitude)
          .map_err(|e| format!("start sampler sink {id:?}: {e}"))?
      };
      println!(
        "  sampler sink {id:?}: {} Hz output{}",
        sampler.output_rate(),
        if no_audio { " (headless: MIDI_PULSE_NO_AUDIO, no sound)" } else { "" },
      );
      samplers.insert(id.clone(), sampler);
    }
  }

  // 2. Build each device's pedal table from its windows (which partition pedals).
  let mut devices: HashMap<String, DeviceBuild> = HashMap::new();
  for softstep in &config.softsteps {
    devices.insert(
      softstep.id.clone(),
      DeviceBuild {
        pedal_map: std::array::from_fn(|_| None),
        select_substring: softstep.select.name_substring().to_string(),
        debounce_ms: 0,
      },
    );
  }
  for window in &config.softstep_windows {
    let SoftstepWindowConfig::Drumkit { softstep, sink, debounce_ms, pads, .. } = window;
    let sampler = samplers
      .get(sink)
      .ok_or_else(|| format!("drumkit window references unbuilt sink {sink:?}"))?;
    let device = devices
      .get_mut(softstep)
      .ok_or_else(|| format!("drumkit window references unknown softstep {softstep:?}"))?;
    // A device's debounce is the widest window among its drumkit windows (they
    // partition pedals, so in the common one-window case this is just its value).
    device.debounce_ms = device.debounce_ms.max(*debounce_ms);
    for pad in pads {
      let path = drum_samples_dir().join(&pad.sample);
      let sample = samples::load_wav(&path)?;
      device.pedal_map[(pad.pedal % 10) as usize] = Some(PadBinding {
        sample,
        gain: pad.gain,
        trigger: sampler.trigger(),
        voice_label: pad.sample.clone(),
      });
    }
  }

  // 3. Setup succeeded -> switch the device into tether mode (and arm restoration).
  tether_session
    .enter()
    .map_err(|e| format!("could not enter tether mode (needs alsa-utils `amidi`): {e}"))?;
  println!("  device in tether mode (velocity-sensitive); will restore standalone on exit");

  // 4. Per device: spawn the poll/fire timer and connect the MIDI input. Connections
  // and timers are held alive for the run.
  let mut connections: Vec<MidiInputConnection<()>> = Vec::new();
  let mut timers: Vec<(Arc<AtomicBool>, JoinHandle<()>)> = Vec::new();
  for (device_id, build) in devices {
    let DeviceBuild { pedal_map, select_substring, debounce_ms } = build;
    print_device_summary(&device_id, &pedal_map, debounce_ms);

    let decoder = Arc::new(Mutex::new(TetherDecoder::new(debounce_ms)));

    // Timer thread: fire hits whose attack window has elapsed. It owns the pad map
    // and sample triggers; the MIDI callback only feeds sensor readings in.
    let stop = Arc::new(AtomicBool::new(false));
    let timer = {
      let decoder = Arc::clone(&decoder);
      let stop = Arc::clone(&stop);
      std::thread::Builder::new()
        .name(format!("kmss-voices-{device_id}"))
        .spawn(move || run_voice_timer(decoder, pedal_map, stop, trace))?
    };
    timers.push((stop, timer));

    let midi_in = MidiInput::new(&format!("kmss-drumkit-{device_id}"))?;
    let port = select_input_port(&midi_in, &select_substring)?;
    let port_name = midi_in.port_name(&port).unwrap_or_else(|_| "<unknown>".to_string());
    println!("  device {device_id:?}: bound MIDI input {port_name:?}");

    let decoder_cb = Arc::clone(&decoder);
    let mut ccs: Vec<(u8, u8)> = Vec::with_capacity(16);
    let conn = midi_in
      .connect(
        &port,
        "kmss-in",
        move |_timestamp, message, _| {
          if trace {
            eprintln!("[kmss] rx {message:02X?}");
          }
          ccs.clear();
          collect_control_changes(message, &mut ccs);
          if ccs.is_empty() {
            return;
          }
          let now = Instant::now();
          let mut decoder = decoder_cb.lock().unwrap_or_else(|e| e.into_inner());
          for &(cc, val) in &ccs {
            decoder.on_cc(cc, val, now);
          }
        },
        (),
      )
      .map_err(|e| format!("connect to {port_name:?}: {e}"))?;
    connections.push(conn);
  }

  if connections.is_empty() {
    return Err("drumkit config declares no softstep devices to bind".into());
  }

  Ok(DrumSession {
    timers,
    _connections: connections,
    _samplers: samplers,
    _tether: tether_session,
  })
}

/// The poll/fire loop for one device: every few ms, ask the decoder for matured hits
/// and fire each sample at `pad.gain * gain_from_velocity(velocity)`.
fn run_voice_timer(
  decoder: Arc<Mutex<TetherDecoder>>,
  pad_map: [Option<PadBinding>; 10],
  stop: Arc<AtomicBool>,
  trace: bool,
) {
  let mut hits: Vec<Hit> = Vec::with_capacity(8);
  while !stop.load(Ordering::Relaxed) {
    // Poll at 1 ms: the device only refreshes every ~10 ms so this captures nothing
    // extra, but it shaves the delay between the attack window elapsing and firing.
    std::thread::sleep(Duration::from_millis(1));
    hits.clear();
    {
      let mut decoder = decoder.lock().unwrap_or_else(|e| e.into_inner());
      decoder.poll(Instant::now(), &mut hits);
    }
    for hit in &hits {
      if let Some(binding) = &pad_map[(hit.label % 10) as usize] {
        let gain = binding.gain * gain_from_velocity(hit.velocity);
        if trace {
          eprintln!(
            "[kmss]   pad {} vel {} -> {} (gain {gain:.3})",
            hit.label, hit.velocity, binding.voice_label,
          );
        }
        binding.trigger.fire(Arc::clone(&binding.sample), gain);
      } else if trace {
        eprintln!("[kmss]   pad {} vel {}: unmapped", hit.label, hit.velocity);
      }
    }
  }
}

/// Choose the KMSS input port: any whose name contains `substring`, preferring the
/// performance port ("MIDI 1") -- the one that carries the tether sensor stream.
fn select_input_port(midi_in: &MidiInput, substring: &str) -> Result<MidiInputPort, String> {
  let mut matches: Vec<(MidiInputPort, String)> = midi_in
    .ports()
    .into_iter()
    .filter_map(|p| midi_in.port_name(&p).ok().map(|n| (p, n)))
    .filter(|(_, name)| name.contains(substring))
    .collect();
  if matches.is_empty() {
    let available: Vec<String> =
      midi_in.ports().iter().filter_map(|p| midi_in.port_name(p).ok()).collect();
    return Err(format!(
      "no MIDI input port matching {substring:?}; available ports: {available:?}",
    ));
  }
  if let Some(idx) = matches.iter().position(|(_, name)| name.contains("MIDI 1")) {
    return Ok(matches.remove(idx).0);
  }
  Ok(matches.remove(0).0)
}

fn print_device_summary(device_id: &str, pedal_map: &[Option<PadBinding>; 10], debounce_ms: u64) {
  println!("  device {device_id:?} pedal map (tether, velocity-sensitive, debounce {debounce_ms} ms):");
  // Print in printed-label order 1..9, then 0, skipping unmapped pedals.
  for label in (1..=9).chain(std::iter::once(0)) {
    if let Some(binding) = &pedal_map[label as usize] {
      println!("    pedal {label} -> {}", binding.voice_label);
    }
  }
}
