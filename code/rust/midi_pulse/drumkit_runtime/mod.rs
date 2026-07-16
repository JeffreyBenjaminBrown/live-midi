//! The KMSS drumkit runtime: read the SoftStep's tether/hosted sensor stream and
//! fire drum one-shots at a level derived from how hard each pad is PRESSED.
//!
//! On start it switches the device into tether mode (over rawmidi via `amidi`),
//! reads the per-sensor Control-Change stream through midir, derives per-pad onset +
//! attack-peak pressure (`decode`), and fires samples through a `cpal_sampler` sink
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

use midi_pulse::midi;
use midi_pulse::rig::{
  drum_samples_dir, load_softstep_params, Rig, SinkRig, SoftstepParams, SoftstepWindowRig,
};

use audio::{Sampler, Trigger, VoiceId};
use decode::{collect_control_changes, gain_from_pressure, DrumEvent, TetherDecoder};
use samples::DrumSample;

/// A host runtime's tap on the pedal stream, called with `(printed label, down)` on
/// every Fire (down) and Release (up). Returning `true` CONSUMES the event: the pad
/// plays no sample for it (the surfaces runtime's feet-accrete mirror repurposes
/// pads this way while its toggle is on). Releases are delivered too, but nothing
/// sample-side listens to them.
pub type PedalHook = Arc<dyn Fn(u8, bool) -> bool + Send + Sync>;

/// The most recently played (non-ditto) hit in a drumkit window: its sample and the
/// source pad's static per-pad `gain` trim (NOT pressure-scaled), so a `Ditto` pad
/// can reproduce the same sample at the same trim while still resolving loudness
/// from its OWN press's pressure.
struct LastHit {
  sample: Arc<DrumSample>,
  gain: f32,
}

/// What a pad does when struck.
enum PadKind {
  /// Fire this sample at `gain` (the full-pressure level; the timer scales it down by
  /// pressure). `voice_label` is for the startup summary / trace only.
  Sample { sample: Arc<DrumSample>, gain: f32, voice_label: String },
  /// "A generalized double-bass pedal": retrigger the most recently played sample in
  /// this window, using that sample's own pad's static `gain` trim but THIS press's
  /// OWN pressure (not the original hit's pressure). A no-op if nothing has played
  /// yet. A ditto press never updates `last` itself, so pad, ditto, ditto repeats
  /// the same sample rather than the ditto echoing itself.
  Ditto,
}

/// One pedal's resolved binding: what it does when struck, into which sink, and the
/// window-shared "last hit" cell a `Ditto` pad reads from. Cloning a `Trigger` is
/// cheap (an `Arc`), so a pad copies its sink's handle.
struct PadBinding {
  kind: PadKind,
  trigger: Trigger,
  /// Shared by every pad in the same drumkit window (one cell per window, created
  /// while building that window's pads).
  last: Arc<Mutex<Option<LastHit>>>,
}

/// What a single KMSS device needs at runtime: its pedal table (indexed by printed
/// label) and the port-name substring used to find its MIDI input.
struct DeviceBuild {
  pedal_map: [Option<PadBinding>; 10],
  select_substring: String,
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

pub fn run_from_rig(rig: &Rig) -> Result<(), Box<dyn std::error::Error>> {
  // Arm Ctrl-C / SIGTERM restoration FIRST, before any audio or MIDI thread spawns,
  // so the signal block is inherited by all of them and a stray signal can't leave
  // the device stuck in tether mode. `start` enters tether mode once setup succeeds.
  let session = start(rig, tether::arm())?;

  println!("\nDrumkit ready (tether mode, pressure-sensitive). Step on the pedals. Press Enter to exit...");
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
  rig: &Rig,
  tether_session: tether::TetherSession,
) -> Result<DrumSession, Box<dyn std::error::Error>> {
  start_with_hook(rig, tether_session, None)
}

/// `start`, with an optional [`PedalHook`] tapping the pedal stream (see the type's
/// doc). `start` itself passes `None`.
pub fn start_with_hook(
  rig: &Rig,
  tether_session: tether::TetherSession,
  hook: Option<PedalHook>,
) -> Result<DrumSession, Box<dyn std::error::Error>> {
  let no_audio = std::env::var_os("MIDI_PULSE_NO_AUDIO").is_some();
  let trace = std::env::var_os("MIDI_PULSE_TRACE").is_some();
  // Shared SoftStep detection/pressure parameters (rigs/softstep.toml), not per-rig.
  let params = load_softstep_params()?;

  // 1. Start a sampler per cpal_sampler sink referenced by some drumkit window.
  let referenced: HashSet<&str> = rig
    .softstep_windows
    .iter()
    .map(|w| match w {
      SoftstepWindowRig::Drumkit { sink, .. } => sink.as_str(),
    })
    .collect();
  let mut samplers: HashMap<String, Sampler> = HashMap::new();
  for sink in &rig.sinks {
    if let SinkRig::CpalSampler { id, sample_rate, buffer_frames, amplitude } = sink {
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
  for softstep in &rig.softsteps {
    devices.insert(
      softstep.id.clone(),
      DeviceBuild {
        pedal_map: std::array::from_fn(|_| None),
        select_substring: softstep.select.name_substring().to_string(),
      },
    );
  }
  for window in &rig.softstep_windows {
    let SoftstepWindowRig::Drumkit { softstep, sink, pads, .. } = window;
    let sampler = samplers
      .get(sink)
      .ok_or_else(|| format!("drumkit window references unbuilt sink {sink:?}"))?;
    let device = devices
      .get_mut(softstep)
      .ok_or_else(|| format!("drumkit window references unknown softstep {softstep:?}"))?;
    // One "last hit" cell per window, shared by every pad in it (including its
    // ditto pad, if any) -- see `PadKind::Ditto`.
    let last: Arc<Mutex<Option<LastHit>>> = Arc::new(Mutex::new(None));
    for pad in pads {
      let kind = if pad.ditto {
        PadKind::Ditto
      } else {
        // Validated: a non-ditto pad always names a sample.
        let sample_name = pad.sample.as_ref().expect("validated: sample present when ditto is false");
        let path = drum_samples_dir().join(sample_name);
        let sample = samples::load_wav(&path)?;
        PadKind::Sample { sample, gain: pad.gain, voice_label: sample_name.clone() }
      };
      device.pedal_map[(pad.pedal % 10) as usize] =
        Some(PadBinding { kind, trigger: sampler.trigger(), last: Arc::clone(&last) });
    }
  }

  // 3. Setup succeeded -> switch the device into tether mode (and arm restoration).
  tether_session
    .enter()
    .map_err(|e| format!("could not enter tether mode (needs alsa-utils `amidi`): {e}"))?;
  println!("  device in tether mode (pressure-sensitive); will restore standalone on exit");

  // 4. Per device: spawn the poll/fire timer and connect the MIDI input. Connections
  // and timers are held alive for the run.
  let mut connections: Vec<MidiInputConnection<()>> = Vec::new();
  let mut timers: Vec<(Arc<AtomicBool>, JoinHandle<()>)> = Vec::new();
  for (device_id, build) in devices {
    let DeviceBuild { pedal_map, select_substring } = build;
    print_device_summary(&device_id, &pedal_map, params);

    let decoder = Arc::new(Mutex::new(TetherDecoder::new(params)));

    // Timer thread: fire hits whose attack window has elapsed. It owns the pad map
    // and sample triggers; the MIDI callback only feeds sensor readings in.
    let stop = Arc::new(AtomicBool::new(false));
    let timer = {
      let decoder = Arc::clone(&decoder);
      let stop = Arc::clone(&stop);
      let hook = hook.clone();
      std::thread::Builder::new()
        .name(format!("kmss-voices-{device_id}"))
        .spawn(move || {
          run_voice_timer(decoder, pedal_map, stop, trace, params.gain_db_range, hook)
        })?
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
    return Err("drumkit rig declares no softstep devices to bind".into());
  }

  Ok(DrumSession {
    timers,
    _connections: connections,
    _samplers: samplers,
    _tether: tether_session,
  })
}

/// What a `Fire` on this pad should do: the sample + RESOLVED gain to play (always
/// pressure-scaled by THIS press's own `pressure` (0.0..1.0) -- for a `Ditto` pad, its
/// source pad's static `gain` trim scaled by the ditto press's own pressure, not the
/// original hit's), or `None` for a silent no-op (only possible for `Ditto`, before
/// anything in its window has played). A `Sample` fire records its sample and its
/// own static `gain` trim into `last` (for a sibling ditto pad to find later); a
/// `Ditto` fire deliberately does NOT touch `last`, so pad, ditto, ditto repeats the
/// ORIGINAL sample rather than the ditto echoing itself. Pure apart from the `last`
/// mutex, so this is the unit-testable core of the ditto trigger logic
/// (`run_voice_timer` just wires its result to a real `Trigger`).
fn resolve_fire(
  kind: &PadKind,
  last: &Mutex<Option<LastHit>>,
  pressure: f32,
  db_range: f32,
) -> Option<(Arc<DrumSample>, f32)> {
  match kind {
    PadKind::Sample { sample, gain, .. } => {
      let resolved = gain * gain_from_pressure(pressure, db_range);
      *last.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(LastHit { sample: Arc::clone(sample), gain: *gain });
      Some((Arc::clone(sample), resolved))
    }
    PadKind::Ditto => {
      let guard = last.lock().unwrap_or_else(|e| e.into_inner());
      let hit = guard.as_ref()?;
      Some((Arc::clone(&hit.sample), hit.gain * gain_from_pressure(pressure, db_range)))
    }
  }
}

/// What a `Revise` on this pad should ramp its currently-playing voice to, or `None`
/// to leave it alone. A ditto pad's gain now comes from its OWN press (like a
/// `Sample` pad's), so a mid-window revise of the ditto press itself ramps it too --
/// scaling the copied sample's static trim (read back out of `last`) by the revised
/// pressure. `None` only if `last` is empty (shouldn't happen mid-window; a Revise
/// implies an earlier Fire already populated it).
fn resolve_revise(
  kind: &PadKind,
  last: &Mutex<Option<LastHit>>,
  pressure: f32,
  db_range: f32,
) -> Option<f32> {
  match kind {
    PadKind::Sample { gain, .. } => Some(gain * gain_from_pressure(pressure, db_range)),
    PadKind::Ditto => {
      let guard = last.lock().unwrap_or_else(|e| e.into_inner());
      let hit = guard.as_ref()?;
      Some(hit.gain * gain_from_pressure(pressure, db_range))
    }
  }
}

/// The poll loop for one device: every ~1 ms, ask the decoder for events. A `Fire` starts
/// a voice at `pad.gain * gain_from_pressure(pressure)`; a `Revise` ramps that pad's
/// current voice up to a higher pressure read later in the attack window.
fn run_voice_timer(
  decoder: Arc<Mutex<TetherDecoder>>,
  pad_map: [Option<PadBinding>; 10],
  stop: Arc<AtomicBool>,
  trace: bool,
  db_range: f32,
  hook: Option<PedalHook>,
) {
  let mut events: Vec<DrumEvent> = Vec::with_capacity(8);
  // The most recent voice started per pad label, so a Revise can find and re-gain it.
  let mut current: [Option<VoiceId>; 10] = [None; 10];
  while !stop.load(Ordering::Relaxed) {
    // Poll at 1 ms: fires on onset promptly and applies mid-window loudness revises with
    // little delay (the device itself only refreshes every ~10 ms).
    std::thread::sleep(Duration::from_millis(1));
    events.clear();
    {
      let mut decoder = decoder.lock().unwrap_or_else(|e| e.into_inner());
      decoder.poll(Instant::now(), &mut events);
    }
    for event in &events {
      match *event {
        DrumEvent::Fire { label, pressure } => {
          let slot = (label % 10) as usize;
          // A hook that consumes the press owns this pedal for now: no sample, and
          // no stale voice left for a later Revise to re-gain.
          if hook.as_ref().is_some_and(|h| h(label, true)) {
            if trace {
              eprintln!("[kmss]   pad {label} FIRE consumed by the host hook");
            }
            current[slot] = None;
            continue;
          }
          if let Some(binding) = &pad_map[slot] {
            match resolve_fire(&binding.kind, &binding.last, pressure, db_range) {
              Some((sample, gain)) => {
                if trace {
                  let desc = match &binding.kind {
                    PadKind::Sample { voice_label, .. } => voice_label.as_str(),
                    PadKind::Ditto => "ditto",
                  };
                  eprintln!("[kmss]   pad {label} FIRE pressure {pressure:.3} -> {desc} (gain {gain:.3})");
                }
                current[slot] = Some(binding.trigger.fire(sample, gain));
              }
              // Only a Ditto pad can resolve to nothing: no sample has played in its
              // window yet.
              None if trace => eprintln!("[kmss]   pad {label} DITTO: nothing has played yet, no-op"),
              None => {}
            }
          } else if trace {
            eprintln!("[kmss]   pad {label} pressure {pressure:.3}: unmapped");
          }
        }
        DrumEvent::Revise { label, pressure } => {
          let slot = (label % 10) as usize;
          if let (Some(binding), Some(id)) = (&pad_map[slot], current[slot]) {
            if let Some(gain) = resolve_revise(&binding.kind, &binding.last, pressure, db_range) {
              if trace {
                eprintln!("[kmss]   pad {label} REVISE pressure {pressure:.3} -> gain {gain:.3}");
              }
              binding.trigger.set_target_gain(id, gain);
            }
          }
        }
        // One-shot samples ignore releases; the hook (feet-accrete) needs them.
        DrumEvent::Release { label } => {
          if let Some(h) = hook.as_ref() {
            let _ = h(label, false);
          }
        }
      }
    }
  }
}

/// Choose the KMSS input port: any whose name contains `substring`, preferring the
/// performance port ("MIDI 1") -- the one that carries the tether sensor stream.
/// A side-effect-free probe: is at least one of the rig's SoftStep devices plugged
/// in right now? Enumerates MIDI input ports and matches each `[[softsteps]]`
/// `select` substring the same way [`select_input_port`] binds it -- but opens no
/// device and enters no tether mode. The surfaces runtime uses this to decide whether
/// to bring up the drumkit at all (a missing SoftStep should skip the drums, not sink
/// the whole run). Returns false when the rig declares no softsteps, or when the
/// MIDI subsystem can't even be opened.
pub fn any_softstep_present(rig: &Rig) -> bool {
  if rig.softsteps.is_empty() {
    return false;
  }
  let Ok(midi_in) = MidiInput::new("kmss-probe") else {
    return false;
  };
  let names: Vec<String> =
    midi_in.ports().iter().filter_map(|p| midi_in.port_name(p).ok()).collect();
  rig
    .softsteps
    .iter()
    .any(|s| names.iter().any(|n| n.contains(s.select.name_substring())))
}

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
  let names: Vec<String> = matches.iter().map(|(_, n)| n.clone()).collect();
  let (idx, guessed) = midi::preferred_port_index(&names, &midi::SOFTSTEP_DATA_PORT_PREFERENCE)
    .expect("matches is non-empty");
  if guessed {
    eprintln!(
      "warning: {} MIDI input ports match {substring:?} and none looks like a data port \
       ({names:?}); binding {:?}. Narrow select.name_contains if that is the wrong one.",
      names.len(),
      names[idx],
    );
  }
  Ok(matches.remove(idx).0)
}

fn print_device_summary(
  device_id: &str,
  pedal_map: &[Option<PadBinding>; 10],
  params: SoftstepParams,
) {
  println!(
    "  device {device_id:?} pedal map (tether, pressure-sensitive, debounce {} ms, de-stick {} ms):",
    params.debounce_ms, params.silence_to_zero_ms,
  );
  // Print in printed-label order 1..9, then 0, skipping unmapped pedals.
  for label in (1..=9).chain(std::iter::once(0)) {
    if let Some(binding) = &pedal_map[label as usize] {
      let desc = match &binding.kind {
        PadKind::Sample { voice_label, .. } => voice_label.as_str(),
        PadKind::Ditto => "ditto (repeats the last-played sample)",
      };
      println!("    pedal {label} -> {desc}");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const DB_RANGE: f32 = 20.0;

  fn sample(tag: f32) -> Arc<DrumSample> {
    // A one-sample "sample" whose single frame IDs it for equality checks below.
    Arc::new(DrumSample { frames: vec![tag], sample_rate: 48000 })
  }

  #[test]
  fn a_sample_fire_resolves_pressure_scaled_gain_and_records_its_static_trim() {
    let last: Mutex<Option<LastHit>> = Mutex::new(None);
    let snare = sample(1.0);
    let kind = PadKind::Sample { sample: Arc::clone(&snare), gain: 0.8, voice_label: "snare.wav".into() };
    let (fired, gain) = resolve_fire(&kind, &last, 0.5, DB_RANGE).expect("a Sample pad always fires");
    assert!(Arc::ptr_eq(&fired, &snare), "fires its own sample");
    assert!(gain < 0.8, "a sub-max-pressure hit resolves below the pad's base gain: {gain}");
    let recorded = last.lock().unwrap();
    let hit = recorded.as_ref().expect("a Sample fire records itself into `last`");
    assert!(Arc::ptr_eq(&hit.sample, &snare));
    assert!(
      (hit.gain - 0.8).abs() < 1e-6,
      "records the pad's static gain TRIM (0.8), not the pressure-resolved gain ({gain}): got {}",
      hit.gain
    );
  }

  #[test]
  fn ditto_before_anything_played_is_a_silent_no_op() {
    let last: Mutex<Option<LastHit>> = Mutex::new(None);
    assert_eq!(resolve_fire(&PadKind::Ditto, &last, 0.8, DB_RANGE), None, "nothing to retrigger yet");
  }

  #[test]
  fn ditto_uses_the_source_pads_trim_but_its_own_presss_pressure() {
    let last: Mutex<Option<LastHit>> = Mutex::new(None);
    let kick = sample(2.0);
    let kick_kind = PadKind::Sample { sample: Arc::clone(&kick), gain: 0.8, voice_label: "kick.wav".into() };
    resolve_fire(&kick_kind, &last, 0.5, DB_RANGE).unwrap();
    // A max-pressure ditto press: same sample, gain = the KICK's 0.8 trim scaled by
    // the DITTO's own (max) pressure -- i.e. unity pressure factor -> 0.8 exactly.
    let (echoed, echoed_gain) = resolve_fire(&PadKind::Ditto, &last, 1.0, DB_RANGE)
      .expect("a hit has played -- ditto retriggers it");
    assert!(Arc::ptr_eq(&echoed, &kick), "ditto retriggers the kick's own sample");
    assert!((echoed_gain - 0.8).abs() < 1e-6, "pressure 1.0 against the kick's 0.8 trim = 0.8: {echoed_gain}");
  }

  #[test]
  fn a_hard_ditto_press_plays_louder_than_the_soft_hit_it_echoes() {
    // "soft hit on pad X, hard ditto press": same sample, but the gain must come
    // from the DITTO press's own (hard) pressure, not the original soft hit's.
    let last: Mutex<Option<LastHit>> = Mutex::new(None);
    let hat = sample(5.0);
    let hat_kind = PadKind::Sample { sample: Arc::clone(&hat), gain: 1.0, voice_label: "hat.wav".into() };
    let (_, soft_gain) = resolve_fire(&hat_kind, &last, 0.15, DB_RANGE).unwrap();
    let (echoed, hard_ditto_gain) =
      resolve_fire(&PadKind::Ditto, &last, 1.0, DB_RANGE).expect("a hit has played -- ditto retriggers it");
    assert!(Arc::ptr_eq(&echoed, &hat), "still the same (hat) sample");
    assert!(
      hard_ditto_gain > soft_gain,
      "a hard ditto press ({hard_ditto_gain}) must play louder than the soft original hit ({soft_gain})"
    );
    assert!((hard_ditto_gain - 1.0).abs() < 1e-6, "pressure 1.0 against the hat's 1.0 trim = unity: {hard_ditto_gain}");
  }

  #[test]
  fn ditto_press_does_not_become_the_new_last_hit() {
    // "pad, ditto, ditto" must repeat the SAME sound, not have the second ditto echo
    // the first -- i.e. a Ditto fire must never write into `last`.
    let last: Mutex<Option<LastHit>> = Mutex::new(None);
    let snare = sample(3.0);
    let kind = PadKind::Sample { sample: Arc::clone(&snare), gain: 1.0, voice_label: "snare.wav".into() };
    resolve_fire(&kind, &last, 0.8, DB_RANGE).unwrap();
    resolve_fire(&PadKind::Ditto, &last, 0.15, DB_RANGE).expect("first ditto");
    resolve_fire(&PadKind::Ditto, &last, 0.05, DB_RANGE).expect("second ditto");
    let recorded = last.lock().unwrap();
    let hit = recorded.as_ref().expect("still holds the original hit");
    assert!(Arc::ptr_eq(&hit.sample, &snare), "still the snare, not a ditto sample");
    assert!((hit.gain - 1.0).abs() < 1e-6, "still the snare pad's own static trim");
  }

  #[test]
  fn revise_scales_a_sample_pad_and_a_ditto_pad_once_something_has_played() {
    let last: Mutex<Option<LastHit>> = Mutex::new(None);
    let hat = sample(4.0);
    let hat_kind = PadKind::Sample { sample: Arc::clone(&hat), gain: 1.0, voice_label: "hat.wav".into() };
    let scaled = resolve_revise(&hat_kind, &last, 0.5, DB_RANGE).expect("a Sample pad revises");
    assert!(scaled > 0.0 && scaled < 1.0, "a sub-max pressure revises below unity: {scaled}");

    // Before anything has played, a ditto revise has no sample to revise against.
    assert_eq!(
      resolve_revise(&PadKind::Ditto, &last, 1.0, DB_RANGE),
      None,
      "nothing has played yet -- no last hit to revise against"
    );

    // Once a hit has played, a ditto press's OWN revise now ramps too (its gain is
    // its own pressure's, not the original hit's, so it must correct the same way).
    resolve_fire(&hat_kind, &last, 0.5, DB_RANGE).unwrap();
    let ditto_scaled = resolve_revise(&PadKind::Ditto, &last, 1.0, DB_RANGE)
      .expect("a hit has played -- ditto revises against its trim");
    assert!((ditto_scaled - 1.0).abs() < 1e-6, "pressure 1.0 against the hat's 1.0 trim = unity: {ditto_scaled}");
  }
}
