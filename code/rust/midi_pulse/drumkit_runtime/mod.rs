//! The KMSS drumkit runtime: read pedal presses from a SoftStep and fire drum
//! one-shots through a `cpal_sampler` sink.
//!
//! Shape mirrors the other runtimes: `run_from_config` wires the config's devices,
//! sinks and windows, then blocks until Enter / Ctrl-C. The press-decode and the
//! audio mixing live in `decode` and `audio` (both unit-tested); this module is the
//! I/O shell (sample loading, midir connection, the pedal->voice routing table).

mod audio;
mod decode;
mod samples;

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Instant;

use midir::{MidiInput, MidiInputConnection, MidiInputPort};

use midi_pulse::config::{drum_samples_dir, Config, SinkConfig, SoftstepWindowConfig};

use audio::{Sampler, Trigger};
use decode::{collect_program_changes, pedal_from_program, Debouncer};
use samples::DrumSample;

/// One pedal's resolved binding: which sample to fire, at what gain, into which
/// sink. Cloning a `Trigger` is cheap (an `Arc`), so a pad copies its sink's handle.
struct PadBinding {
  sample: Arc<DrumSample>,
  gain: f32,
  trigger: Trigger,
  /// For the startup summary only.
  voice_label: String,
}

/// What a single KMSS device needs at runtime: its pedal table, its debounce
/// window, and the port-name substring used to find it.
struct DeviceBuild {
  pedal_map: [Option<PadBinding>; 10],
  debounce_ms: u64,
  select_substring: String,
}

pub fn run_from_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  let no_audio = std::env::var_os("MIDI_PULSE_NO_AUDIO").is_some();

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
        debounce_ms: 0,
        select_substring: softstep.select.name_substring().to_string(),
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

  // 3. Connect to each device and route presses. Connections are held alive in the
  // returned Vec for the lifetime of the run.
  let mut connections: Vec<MidiInputConnection<()>> = Vec::new();
  for (device_id, build) in devices {
    let DeviceBuild { pedal_map, debounce_ms, select_substring } = build;
    print_device_summary(&device_id, &pedal_map, debounce_ms);

    let midi_in = MidiInput::new(&format!("kmss-drumkit-{device_id}"))?;
    let port = select_input_port(&midi_in, &select_substring)?;
    let port_name = midi_in.port_name(&port).unwrap_or_else(|_| "<unknown>".to_string());
    println!("  device {device_id:?}: bound MIDI input {port_name:?}");

    let mut debouncer = Debouncer::new(debounce_ms);
    // Set MIDI_PULSE_TRACE=1 to log every received MIDI buffer and what it fired --
    // the way to see whether the device actually sends two PCs for a two-pad stomp.
    let trace = std::env::var_os("MIDI_PULSE_TRACE").is_some();
    let mut programs: Vec<u8> = Vec::with_capacity(8);
    let conn = midi_in
      .connect(
        &port,
        "kmss-in",
        move |_timestamp, message, _| {
          if trace {
            eprintln!("[kmss] rx {message:02X?}");
          }
          programs.clear();
          collect_program_changes(message, &mut programs);
          for &program in &programs {
            let pedal = pedal_from_program(program);
            match &pedal_map[pedal as usize] {
              Some(binding) => {
                let fired = debouncer.accept(pedal, Instant::now());
                if trace {
                  eprintln!(
                    "[kmss]   program {program} -> pedal {pedal} ({}): {}",
                    binding.voice_label,
                    if fired { "FIRE" } else { "debounced" },
                  );
                }
                if fired {
                  binding.trigger.fire(Arc::clone(&binding.sample), binding.gain);
                }
              }
              None if trace => {
                eprintln!("[kmss]   program {program} -> pedal {pedal}: unmapped");
              }
              None => {}
            }
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

  println!("\nDrumkit ready. Step on the pedals. Press Enter to exit...");
  let mut line = String::new();
  io::stdin().read_line(&mut line)?;
  Ok(())
}

/// Choose the KMSS input port: any whose name contains `substring`, preferring the
/// performance port ("MIDI 1") when several match (the KMSS exposes both a
/// performance and an editor port, both containing "SSCOM").
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
  println!("  device {device_id:?} pedal map (debounce {debounce_ms} ms):");
  // Print in printed-label order 1..9, then 0, skipping unmapped pedals.
  for label in (1..=9).chain(std::iter::once(0)) {
    if let Some(binding) = &pedal_map[label as usize] {
      println!("    pedal {label} -> {}", binding.voice_label);
    }
  }
}
