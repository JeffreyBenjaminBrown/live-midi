//! Loading WAV one-shots into memory as mono f32.
//!
//! Samples are downmixed to mono at load and stored with their native sample rate;
//! the sampler voice resamples per-frame at trigger time (so any source rate plays
//! correctly against any output rate). One-shots are small, so loading whole into
//! RAM is fine.

use std::path::Path;
use std::sync::Arc;

/// A loaded one-shot: mono samples in roughly [-1, 1] plus the file's sample rate.
#[derive(Clone, Debug, PartialEq)]
pub struct DrumSample {
  pub frames: Vec<f32>,
  pub sample_rate: u32,
}

/// Load a WAV (PCM int of any bit depth, or float) and downmix to mono f32.
pub fn load_wav(path: &Path) -> Result<Arc<DrumSample>, String> {
  let mut reader =
    hound::WavReader::open(path).map_err(|e| format!("open sample {}: {e}", path.display()))?;
  let spec = reader.spec();
  let channels = spec.channels.max(1) as usize;

  // Interleaved samples -> normalized f32.
  let interleaved: Vec<f32> = match spec.sample_format {
    hound::SampleFormat::Float => reader
      .samples::<f32>()
      .collect::<Result<Vec<f32>, _>>()
      .map_err(|e| format!("read float samples {}: {e}", path.display()))?,
    hound::SampleFormat::Int => {
      // hound yields each integer sign-extended into i32; full scale is 2^(bits-1).
      let full_scale = (1i64 << (spec.bits_per_sample.max(1) - 1)) as f32;
      reader
        .samples::<i32>()
        .map(|s| s.map(|v| v as f32 / full_scale))
        .collect::<Result<Vec<f32>, _>>()
        .map_err(|e| format!("read int samples {}: {e}", path.display()))?
    }
  };

  let frames = downmix(&interleaved, channels);
  if frames.is_empty() {
    return Err(format!("sample {} has no audio frames", path.display()));
  }
  Ok(Arc::new(DrumSample { frames, sample_rate: spec.sample_rate }))
}

/// Average interleaved channels down to one. `channels >= 1`.
fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
  if channels <= 1 {
    return interleaved.to_vec();
  }
  interleaved
    .chunks(channels)
    .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn downmix_averages_stereo_to_mono() {
    // L,R interleaved: (1.0,-1.0),(0.5,0.5) -> 0.0, 0.5
    let mono = downmix(&[1.0, -1.0, 0.5, 0.5], 2);
    assert_eq!(mono, vec![0.0, 0.5]);
  }

  #[test]
  fn loads_a_round_tripped_int_wav() {
    let dir = std::env::temp_dir();
    let path = dir.join("midi_pulse_drumkit_test.wav");
    let spec = hound::WavSpec {
      channels: 1,
      sample_rate: 22050,
      bits_per_sample: 16,
      sample_format: hound::SampleFormat::Int,
    };
    {
      let mut w = hound::WavWriter::create(&path, spec).expect("create wav");
      w.write_sample(i16::MAX).unwrap(); // ~ +1.0
      w.write_sample(i16::MIN).unwrap(); // ~ -1.0
      w.write_sample(0i16).unwrap();
      w.finalize().unwrap();
    }
    let sample = load_wav(&path).expect("load wav");
    assert_eq!(sample.sample_rate, 22050);
    assert_eq!(sample.frames.len(), 3);
    assert!((sample.frames[0] - 1.0).abs() < 0.001, "{}", sample.frames[0]);
    assert!((sample.frames[1] + 1.0).abs() < 0.001, "{}", sample.frames[1]);
    assert_eq!(sample.frames[2], 0.0);
    let _ = std::fs::remove_file(&path);
  }
}
