//! The drum sampler's cpal output stream + polyphonic one-shot voice pool.
//!
//! Mirrors `looper_runtime::audio` for host/format/JACK selection (duplicated
//! rather than factored: the cpal path can't be unit-tested, so shared untestable
//! glue would carry regression risk with no test to catch it). The realtime
//! callback locks the voice pool (its only lock) and mixes one-shot samples.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::sync::{Arc, Mutex};

use super::samples::DrumSample;

/// A fixed pool size: drum hits are short and sparse, but allow generous overlap
/// (fast rolls, two feet, a ringing crash under new hits) without ever allocating
/// in the realtime callback.
const VOICE_SLOTS: usize = 64;

/// A playing voice's gain ramps toward its target over ~this long (one-pole), so a
/// mid-flight loudness revise -- a harder peak arriving during the attack window -- does
/// not click. Short enough to reach the new level well within a drum hit.
const GAIN_RAMP_SECS: f32 = 0.005;

/// One sounding sample. `pos` is a fractional read index into `sample.frames`;
/// `step = file_rate / output_rate` resamples by linear interpolation.
#[derive(Clone)]
struct Voice {
  sample: Arc<DrumSample>,
  pos: f64,
  step: f64,
  gain: f32,        // current gain; ramps toward `target_gain`
  target_gain: f32, // where a mid-window loudness revise moves the gain to
  ramp: f32,        // per-frame one-pole coefficient toward `target_gain`
  gen: u64,         // generation, so a stale VoiceId can't revise a reused slot
}

/// Handle to a triggered voice, so a later `Revise` can find and re-gain it. Becomes stale
/// once the slot is reused (the generation won't match), and `set_target_gain` is then a
/// no-op. `Copy`, so the timer can keep one per pad cheaply.
#[derive(Clone, Copy)]
pub struct VoiceId {
  slot: usize,
  gen: u64,
}

/// The shared, realtime-safe voice pool. Triggering (MIDI thread) and mixing
/// (audio thread) both lock this briefly; no allocation happens under the lock.
pub struct VoicePool {
  voices: Vec<Option<Voice>>,
  next_gen: u64,
}

impl VoicePool {
  fn new() -> Self {
    VoicePool { voices: vec![None; VOICE_SLOTS], next_gen: 0 }
  }

  /// Start `sample` playing and return its [`VoiceId`]. Reuses a free slot; if all are
  /// busy it steals the voice nearest its end (least musical to drop).
  pub fn trigger(&mut self, sample: Arc<DrumSample>, output_rate: f32, gain: f32) -> VoiceId {
    let step = sample.sample_rate as f64 / output_rate.max(1.0) as f64;
    let ramp = 1.0 - (-1.0 / (GAIN_RAMP_SECS * output_rate.max(1.0))).exp();
    let gen = self.next_gen;
    self.next_gen += 1;
    let voice = Voice { sample, pos: 0.0, step, gain, target_gain: gain, ramp, gen };
    let slot = self.voices.iter().position(|v| v.is_none()).unwrap_or_else(|| {
      // All busy: steal the most-finished voice (largest fraction of its length played).
      let mut best = 0usize;
      let mut best_frac = -1.0f64;
      for (i, v) in self.voices.iter().enumerate() {
        if let Some(v) = v {
          let frac = v.pos / v.sample.frames.len().max(1) as f64;
          if frac > best_frac {
            best_frac = frac;
            best = i;
          }
        }
      }
      best
    });
    self.voices[slot] = Some(voice);
    VoiceId { slot, gen }
  }

  /// Ramp a still-playing voice toward a new gain (a mid-window loudness revise). A no-op
  /// if that voice has already ended or its slot was reused (generation mismatch).
  pub fn set_target_gain(&mut self, id: VoiceId, gain: f32) {
    if let Some(slot) = self.voices.get_mut(id.slot) {
      if let Some(v) = slot {
        if v.gen == id.gen {
          v.target_gain = gain;
        }
      }
    }
  }

  /// Mix all active voices into the interleaved output buffer. `amplitude` is the
  /// sink master gain; the per-frame sum is clamped to [-1, 1] to avoid wrap.
  fn mix(&mut self, data: &mut [f32], channels: usize, amplitude: f32) {
    for frame in data.chunks_mut(channels.max(1)) {
      let mut sum = 0.0f32;
      for slot in self.voices.iter_mut() {
        let Some(v) = slot else { continue };
        let frames = &v.sample.frames;
        let i = v.pos as usize;
        if i >= frames.len() {
          *slot = None;
          continue;
        }
        // Linear interpolation between frame i and i+1 (i+1 may be past the end).
        let a = frames[i];
        let b = if i + 1 < frames.len() { frames[i + 1] } else { a };
        let frac = (v.pos - i as f64) as f32;
        // One-pole "de-zipper": glide the gain toward its (instantly-set) target. The gain
        // is always continuous, which is what kills the click; the slope change when the
        // target moves is inaudible. Snap once within epsilon so a long-ringing voice does
        // not creep toward the target forever (which would slow the mix on denormals).
        v.gain += (v.target_gain - v.gain) * v.ramp;
        if (v.target_gain - v.gain).abs() < 1e-6 {
          v.gain = v.target_gain;
        }
        sum += (a + (b - a) * frac) * v.gain;
        v.pos += v.step;
        if v.pos as usize >= frames.len() {
          *slot = None;
        }
      }
      let s = (sum * amplitude).clamp(-1.0, 1.0);
      for ch in frame.iter_mut() {
        *ch = s;
      }
    }
  }
}

/// A trigger handle: cheap to clone (an `Arc` + the output rate), `Send`, and
/// carries no cpal stream -- so it can live inside the MIDI callback while the
/// stream stays on the runtime's main thread.
#[derive(Clone)]
pub struct Trigger {
  pool: Arc<Mutex<VoicePool>>,
  output_rate: f32,
}

impl Trigger {
  /// Fire a one-shot; returns a [`VoiceId`] for later gain revisions. Recovers from a
  /// poisoned lock so a panicked audio thread can't permanently wedge triggering.
  pub fn fire(&self, sample: Arc<DrumSample>, gain: f32) -> VoiceId {
    let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
    pool.trigger(sample, self.output_rate, gain)
  }

  /// Ramp a previously-fired voice toward a new gain (a mid-window loudness revise).
  pub fn set_target_gain(&self, id: VoiceId, gain: f32) {
    let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
    pool.set_target_gain(id, gain);
  }
}

/// A running sampler sink: owns the cpal stream (kept alive for the run) and hands
/// out `Trigger`s. `None` stream = the silent/headless path.
pub struct Sampler {
  _stream: Option<cpal::Stream>,
  pool: Arc<Mutex<VoicePool>>,
  output_rate: f32,
}

impl Sampler {
  /// A silent sampler: no cpal stream, so it needs no sound card and makes no
  /// sound. Used for headless runs (`MIDI_PULSE_NO_AUDIO`); triggers still record
  /// into the pool harmlessly.
  pub fn start_null(requested_sample_rate: u32) -> Sampler {
    Sampler {
      _stream: None,
      pool: Arc::new(Mutex::new(VoicePool::new())),
      output_rate: requested_sample_rate as f32,
    }
  }

  /// Open the cpal/JACK output stream and start mixing. See
  /// `looper_runtime::audio::start` for the host/format rationale.
  pub fn start(
    requested_sample_rate: u32,
    requested_buffer_frames: u32,
    amplitude: f32,
  ) -> Result<Sampler, Box<dyn std::error::Error>> {
    // Prefer the JACK host so we share the card via PipeWire instead of grabbing
    // ALSA exclusively (launch under `pw-jack`); fall back to the default host.
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
    if supported.sample_format() != SampleFormat::F32 {
      return Err(format!("drum sampler requires F32 output, got {:?}", supported.sample_format()).into());
    }
    let output_rate = supported.sample_rate().0 as f32;
    let channels = supported.channels() as usize;
    let stream_config = cpal::StreamConfig {
      channels: supported.channels(),
      sample_rate: supported.sample_rate(),
      buffer_size: cpal::BufferSize::Fixed(requested_buffer_frames),
    };

    let pool = Arc::new(Mutex::new(VoicePool::new()));
    let pool_for_cb = Arc::clone(&pool);
    let stream = device.build_output_stream(
      &stream_config,
      move |data: &mut [f32], _| {
        let mut pool = pool_for_cb.lock().unwrap_or_else(|e| e.into_inner());
        pool.mix(data, channels, amplitude);
      },
      |error| eprintln!("drum sampler stream error: {error:?}"),
      None,
    )?;
    stream.play()?;
    Ok(Sampler { _stream: Some(stream), pool, output_rate })
  }

  pub fn output_rate(&self) -> f32 {
    self.output_rate
  }

  pub fn trigger(&self) -> Trigger {
    Trigger { pool: Arc::clone(&self.pool), output_rate: self.output_rate }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample(frames: Vec<f32>, rate: u32) -> Arc<DrumSample> {
    Arc::new(DrumSample { frames, sample_rate: rate })
  }

  #[test]
  fn mix_plays_a_voice_to_completion_then_frees_the_slot() {
    let mut pool = VoicePool::new();
    // Same rate as output -> step 1.0, no interpolation drift.
    pool.trigger(sample(vec![0.5, -0.5], 48000), 48000.0, 1.0);
    let mut out = [0.0f32; 2]; // mono output, 2 frames
    pool.mix(&mut out, 1, 1.0);
    assert_eq!(out, [0.5, -0.5]);
    // Voice is now exhausted; a further mix is silence.
    let mut out2 = [9.0f32; 2];
    pool.mix(&mut out2, 1, 1.0);
    assert_eq!(out2, [0.0, 0.0]);
  }

  #[test]
  fn mix_sums_two_voices_and_applies_amplitude() {
    let mut pool = VoicePool::new();
    pool.trigger(sample(vec![0.4], 48000), 48000.0, 1.0);
    pool.trigger(sample(vec![0.4], 48000), 48000.0, 1.0);
    let mut out = [0.0f32; 1];
    pool.mix(&mut out, 1, 0.5);
    // (0.4 + 0.4) * 0.5 = 0.4
    assert!((out[0] - 0.4).abs() < 1e-6, "{}", out[0]);
  }

  #[test]
  fn mix_clamps_to_avoid_wrap() {
    let mut pool = VoicePool::new();
    for _ in 0..4 {
      pool.trigger(sample(vec![1.0], 48000), 48000.0, 1.0);
    }
    let mut out = [0.0f32; 1];
    pool.mix(&mut out, 1, 1.0); // sum 4.0 -> clamped to 1.0
    assert_eq!(out[0], 1.0);
  }

  #[test]
  fn mix_writes_all_channels() {
    let mut pool = VoicePool::new();
    pool.trigger(sample(vec![0.3], 48000), 48000.0, 1.0);
    let mut out = [0.0f32; 2]; // one stereo frame
    pool.mix(&mut out, 2, 1.0);
    assert_eq!(out, [0.3, 0.3], "mono source fanned to both channels");
  }

  #[test]
  fn set_target_gain_ramps_a_playing_voice_up() {
    let mut pool = VoicePool::new();
    // A long DC sample so the gain ramp is visible over many frames.
    let id = pool.trigger(sample(vec![1.0; 2000], 48000), 48000.0, 0.2);
    pool.set_target_gain(id, 1.0); // revise louder mid-flight
    let mut out = [0.0f32; 500];
    pool.mix(&mut out, 1, 1.0);
    assert!(out[0] < 0.3, "starts near the fire gain (0.2): {}", out[0]);
    assert!(out[499] > 0.8, "ramps up toward the target (1.0): {}", out[499]);
    assert!(out[499] > out[0], "monotonic ramp up");
  }

  #[test]
  fn set_target_gain_is_a_noop_for_a_stale_voice() {
    let mut pool = VoicePool::new();
    let id = pool.trigger(sample(vec![1.0], 48000), 48000.0, 0.5); // slot 0, gen 0
    pool.mix(&mut [0.0f32; 4], 1, 1.0); // play to completion -> frees the slot
    // A fresh voice reuses the slot at gain 0.3 (DC sample value 1.0, so out == gain).
    pool.trigger(sample(vec![1.0; 500], 48000), 48000.0, 0.3); // slot 0, gen 1
    pool.set_target_gain(id, 1.0); // stale generation -> no-op (would otherwise ramp to 1.0)
    let mut out = [0.0f32; 500];
    pool.mix(&mut out, 1, 1.0);
    assert!((out[499] - 0.3).abs() < 1e-3, "stale id did not ramp the reused voice: {}", out[499]);
  }
}
