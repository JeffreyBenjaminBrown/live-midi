//! The surfaces synth's cpal output stream: one shared voice map summed by the
//! block renderer, feeding both grids' voices at once (each voice carries its own
//! grid's waveform). Mirrors `looper_runtime::audio` / `sawwave_runtime` for
//! host/format/JACK selection -- duplicated rather than factored, matching the
//! codebase's convention that untestable cpal glue is duplicated (a shared helper
//! would carry regression risk with no test to catch it). Launch under `pw-jack`.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::synth::SUSTAIN_BASE;
use super::Live;
use crate::types::{AmShapeFamily, VoiceMap, VoiceSource};
use crate::voices::BlockRenderer;

pub struct Audio {
  /// Kept alive for the run; dropping it stops the stream. `None` in the null path
  /// (headless / mock runs), which opens no device and makes no sound.
  _stream: Option<cpal::Stream>,
  pub sample_rate: f32,
}

/// A silent audio "device": no cpal stream, so it needs no sound card and never
/// renders. Used for headless / mock-rig runs (`MIDI_PULSE_NO_AUDIO`), where we drive
/// the grids and inspect LEDs/state, not sound.
pub fn start_null(requested_sample_rate: u32) -> Audio {
  Audio { _stream: None, sample_rate: requested_sample_rate as f32 }
}

#[allow(clippy::too_many_arguments)]
pub fn start(
  voices: Arc<Mutex<VoiceMap>>,
  requested_sample_rate: u32,
  requested_buffer_frames: u32,
  oversample: usize,
  am_shape_family: AmShapeFamily,
  live: Arc<Live>,
  distortion_on: Arc<Vec<AtomicBool>>,
) -> Result<Audio, Box<dyn std::error::Error>> {
  // Prefer the JACK host so this synth is an ordinary JACK node sharing the sound
  // card via PipeWire, rather than cpal's default ALSA host grabbing the device
  // exclusively (which would evict PipeWire and the drum sampler). Launch under
  // `pw-jack`; fall back to the default host if the `jack` feature is absent.
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
    return Err(format!("surfaces synth requires F32 output, got {sample_format:?}").into());
  }
  let sample_rate = supported.sample_rate().0 as f32;
  let channels = supported.channels() as usize;
  let stream_config = cpal::StreamConfig {
    channels: supported.channels(),
    sample_rate: supported.sample_rate(),
    buffer_size: cpal::BufferSize::Fixed(requested_buffer_frames),
  };
  // The decimation filter is stateful across callbacks, so the renderer lives outside
  // the closure (moved in, mutated each call). `flags` is the per-grid distortion
  // snapshot, reused across callbacks to keep allocation out of the audio path.
  let mut renderer = BlockRenderer::new(oversample);
  let mut flags = vec![false; distortion_on.len()];
  let stream = device.build_output_stream(
    &stream_config,
    move |data: &mut [f32], _| {
      // The master amplitude + distortion curve are hot-reloadable ('r'), and the
      // per-grid distortion toggles are live -- read everything fresh each callback.
      let (amplitude, distortion_params) = {
        let p = live.params.lock().unwrap_or_else(|e| e.into_inner());
        (p.amplitude, p.distortion)
      };
      for (f, b) in flags.iter_mut().zip(distortion_on.iter()) {
        *f = b.load(Ordering::Relaxed);
      }
      let distortion = flags.iter().any(|f| *f).then_some(distortion_params);
      // Recover from poisoning so a panicked grid thread can't permanently kill audio.
      let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
      // The AM family shapes any `[[timbres]]` tremolo; inert while depths are 0.
      // Distortion is per-monome: a voice routes to the dirty (distorted) bus iff
      // its grid's toggle is on. Fingered voices carry `chord: grid`; sustained
      // (accrete) drones carry `chord: SUSTAIN_BASE + grid` -- both follow their
      // grid's toggle.
      renderer.render_with_distortion(
        &mut voices,
        data,
        channels,
        sample_rate,
        amplitude,
        am_shape_family,
        distortion,
        |src| {
          let VoiceSource::Accreted { chord, .. } = src else {
            return false;
          };
          let grid = if *chord >= SUSTAIN_BASE { *chord - SUSTAIN_BASE } else { *chord };
          flags.get(grid).copied().unwrap_or(false)
        },
      );
    },
    |error| eprintln!("surfaces audio stream error: {error:?}"),
    None,
  )?;
  stream.play()?;
  Ok(Audio { _stream: Some(stream), sample_rate })
}
