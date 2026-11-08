//! Opening the sound card for a synth: the JACK host when there is one (so the
//! synth shares the card through PipeWire instead of grabbing it), an F32
//! stream at the requested rate, a fixed buffer. Launch under `pw-jack`.
//!
//! A runtime-independent piece: a runtime hands it a function that builds
//! its render callback once the real sample rate and channel count are known.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

pub struct OutputStream {
  /// Dropping it stops the sound.
  pub stream: cpal::Stream,
  pub sample_rate: f32,
  pub channels: usize,
}

/// Open the default output and start `make_render(sample_rate, channels)`'s
/// callback on it. The callback fills interleaved F32 frames.
pub fn open_output<Render, MakeRender>(
  requested_sample_rate: u32,
  requested_buffer_frames: u32,
  make_render: MakeRender,
) -> Result<OutputStream, String>
where
  Render: FnMut(&mut [f32]) + Send + 'static,
  MakeRender: FnOnce(f32, usize) -> Render,
{
  let host: cpal::Host = match cpal::available_hosts().into_iter()
    .find(|id| format!("{id:?}").eq_ignore_ascii_case("jack")) {
    Some(jack) => cpal::host_from_id(jack)
      .map_err(|e| format!("JACK host unavailable ({e}); launch under `pw-jack`"))?,
    None => cpal::default_host(), };
  let device: cpal::Device = host.default_output_device().ok_or("no default output device")?;
  let default_config = device.default_output_config().map_err(|e| e.to_string())?;
  let default_channels: u16 = default_config.channels();
  let config = device.supported_output_configs().map_err(|e| e.to_string())?
    .filter(|range| range.sample_format() == SampleFormat::F32
      && range.min_sample_rate().0 <= requested_sample_rate
      && range.max_sample_rate().0 >= requested_sample_rate)
    .max_by_key(|range| (range.channels() == default_channels, range.channels()))
    .map(|range| range.with_sample_rate(cpal::SampleRate(requested_sample_rate)))
    .unwrap_or(default_config);
  if config.sample_format() != SampleFormat::F32 {
    return Err(format!("needs F32 output, got {:?}", config.sample_format())); }
  let sample_rate: f32 = config.sample_rate().0 as f32;
  let channels: usize = config.channels() as usize;
  let stream_config: cpal::StreamConfig = cpal::StreamConfig {
    channels: config.channels(),
    sample_rate: config.sample_rate(),
    buffer_size: cpal::BufferSize::Fixed(requested_buffer_frames), };
  let mut render: Render = make_render(sample_rate, channels);
  let stream: cpal::Stream = device.build_output_stream(
    &stream_config,
    move |data: &mut [f32], _| render(data),
    |error| eprintln!("audio stream error: {error:?}"),
    None, ).map_err(|e| e.to_string())?;
  stream.play().map_err(|e| e.to_string())?;
  Ok(OutputStream { stream, sample_rate, channels }) }
