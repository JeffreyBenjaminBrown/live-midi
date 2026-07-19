//! The looper's cpal output stream.
//!
//! Minimal boilerplate mirroring `sawwave_runtime::start_audio_stream` (without
//! the diagnostics atomics). Duplicated rather than factored: the cpal path can't
//! be unit-tested, and factoring shared *untestable* glue would carry regression
//! risk with no test to catch it. The realtime callback locks `voices` (the only
//! lock it ever takes) and mixes through the unchanged `render_block`.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::sync::{Arc, Mutex};

use edo_surface::types::{AmShapeFamily, VoiceMap};
use edo_surface::voices::BlockRenderer;

pub struct Audio {
  /// Kept alive for the run; dropping it stops the stream. `None` in the null path
  /// (headless / mock runs), which opens no device and makes no sound.
  _stream: Option<cpal::Stream>,
  pub sample_rate: f32,
}

/// A silent audio "device": no cpal stream, so it needs no sound card and never
/// renders. Used for headless / mock-rig runs (`MIDI_PULSE_NO_AUDIO`), where we drive
/// the grids and inspect LEDs/state, not sound. The note sink still ref-counts notes
/// (so the LEDs behave); only the cpal render callback is absent.
pub fn start_null(requested_sample_rate: u32) -> Audio {
  Audio { _stream: None, sample_rate: requested_sample_rate as f32 }
}

pub fn start(
  voices: Arc<Mutex<VoiceMap>>,
  requested_sample_rate: u32,
  requested_buffer_frames: u32,
  amplitude: f32,
  oversample: usize,
  am_shape_family: AmShapeFamily,
) -> Result<Audio, Box<dyn std::error::Error>> {
  // Prefer the JACK host so this synth is an ordinary JACK node that shares the
  // sound card via PipeWire, rather than cpal's default ALSA host grabbing the
  // device exclusively and evicting PipeWire (which silences all other audio,
  // e.g. the Claude stop-hook beep). Built with cpal's `jack` feature; launch
  // under `pw-jack` so libjack resolves to PipeWire's. If the `jack` feature is
  // not compiled in, fall back to the previous default (ALSA) host.
  let host = match cpal::available_hosts()
    .into_iter()
    .find(|id| format!("{id:?}").eq_ignore_ascii_case("jack"))
  {
    Some(jack_id) => cpal::host_from_id(jack_id)
      .map_err(|e| format!("JACK host unavailable ({e}); launch under `pw-jack`"))?,
    None => cpal::default_host(),
  };
  let device = host.default_output_device().ok_or("no default output device")?;
  let default_cfg = device.default_output_config()?;
  let default_channels = default_cfg.channels();
  let supported = device
    .supported_output_configs()?
    .filter(|rig| {
      rig.sample_format() == SampleFormat::F32
        && rig.min_sample_rate().0 <= requested_sample_rate
        && rig.max_sample_rate().0 >= requested_sample_rate
    })
    .max_by_key(|rig| (rig.channels() == default_channels, rig.channels()))
    .map(|rig| rig.with_sample_rate(cpal::SampleRate(requested_sample_rate)))
    .unwrap_or(default_cfg);
  let sample_format = supported.sample_format();
  if sample_format != SampleFormat::F32 {
    return Err(format!("looper requires F32 output, got {sample_format:?}").into());
  }
  let sample_rate = supported.sample_rate().0 as f32;
  let channels = supported.channels() as usize;
  let stream_config = cpal::StreamConfig {
    channels: supported.channels(),
    sample_rate: supported.sample_rate(),
    buffer_size: cpal::BufferSize::Fixed(requested_buffer_frames),
  };
  // The decimation filter is stateful across callbacks, so the renderer lives outside
  // the closure (moved in, mutated each call).
  let mut renderer = BlockRenderer::new(oversample);
  let stream = device.build_output_stream(
    &stream_config,
    move |data: &mut [f32], _| {
      // Recover from poisoning so a panicked grid thread can't permanently kill
      // audio output.
      let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
      // The AM shape family is rig-level (6_plan 2.5); resolve_settings sources it.
      renderer.render(&mut voices, data, channels, sample_rate, amplitude, am_shape_family);
    },
    |error| eprintln!("looper audio stream error: {error:?}"),
    None,
  )?;
  stream.play()?;
  Ok(Audio { _stream: Some(stream), sample_rate })
}
