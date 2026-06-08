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

use crate::types::VoiceMap;
use crate::voices::render_block_with_amplitude;

pub struct Audio {
  /// Kept alive for the run; dropping it stops the stream.
  _stream: cpal::Stream,
  pub sample_rate: f32,
}

pub fn start(
  voices: Arc<Mutex<VoiceMap>>,
  requested_sample_rate: u32,
  requested_buffer_frames: u32,
  amplitude: f32,
) -> Result<Audio, Box<dyn std::error::Error>> {
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
    return Err(format!("looper requires F32 output, got {sample_format:?}").into());
  }
  let sample_rate = supported.sample_rate().0 as f32;
  let channels = supported.channels() as usize;
  let stream_config = cpal::StreamConfig {
    channels: supported.channels(),
    sample_rate: supported.sample_rate(),
    buffer_size: cpal::BufferSize::Fixed(requested_buffer_frames),
  };
  let stream = device.build_output_stream(
    &stream_config,
    move |data: &mut [f32], _| {
      // Recover from poisoning so a panicked grid thread can't permanently kill
      // audio output.
      let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
      render_block_with_amplitude(&mut voices, data, channels, sample_rate, amplitude);
    },
    |error| eprintln!("looper audio stream error: {error:?}"),
    None,
  )?;
  stream.play()?;
  Ok(Audio { _stream: stream, sample_rate })
}
