//! Audio rendering and per-voice helpers.

use crate::consts::{ACCRETION_TARGET, AMPLITUDE, ATTACK_SECS, RELEASE_SECS};
use crate::pitch::freq_for_pitch;
use crate::types::{VoiceId, VoiceMap, VoiceSource, VoiceState};

pub fn triangle(phase: f32) -> f32 {
  // phase is in [0, 1)
  if phase < 0.5 {
    4.0 * phase - 1.0
  } else {
    3.0 - 4.0 * phase
  }
}

// Render one cpal callback's worth of audio into `data` from `voices`.
// Pulled out of the cpal closure so unit tests can exercise it
// without cpal or PipeWire.
pub fn render_block(
  voices: &mut VoiceMap,
  data: &mut [f32],
  channels: usize,
  sample_rate: f32,
) {
  for frame in data.chunks_mut(channels) {
    let mut mix = 0.0_f32;
    voices.retain(|_, v| {
      // Step env toward target_env.
      let delta = v.target_env - v.env;
      if delta.abs() <= v.ramp_per_sample {
        v.env = v.target_env;
      } else if delta > 0.0 {
        v.env += v.ramp_per_sample;
      } else {
        v.env -= v.ramp_per_sample;
      }
      // Voice is gone once env and target both reach 0.
      if v.env == 0.0 && v.target_env == 0.0 {
        return false;
      }
      v.phase += v.freq / sample_rate;
      if v.phase >= 1.0 {
        v.phase -= 1.0;
      }
      mix += triangle(v.phase) * v.env * AMPLITUDE;
      true
    });
    let s = mix.clamp(-0.95, 0.95);
    for out in frame.iter_mut() {
      *out = s;
    }
  }
}

// Is the voice with id `id` currently being held / sustained?
// "Alive" means target_env > 0 — voices ramping to 0 don't count;
// the user has lifted their finger (or emit went off) and the voice
// is on its way out, not blocking transform / spawn decisions.
pub fn voice_alive_with_id(voices: &VoiceMap, id: VoiceId) -> bool {
  voices.values().any(|v| v.id == id && v.target_env > 0.0)
}

// Spawn a fresh accretion voice for `pitch` at env=0 ramping to
// ACCRETION_TARGET over ATTACK_SECS. Overwrites any existing entry
// at Accreted{pitch}.
pub fn spawn_accretion_voice(
  voices: &mut VoiceMap, pitch: i32,
  fund: f64, edo: i32, next_voice_id: &mut VoiceId, sample_rate: f32,
) {
  let id = *next_voice_id;
  *next_voice_id += 1;
  voices.insert(VoiceSource::Accreted { pitch }, VoiceState {
    id,
    freq: freq_for_pitch(pitch, fund, edo),
    phase: 0.0,
    env: 0.0,
    target_env: ACCRETION_TARGET,
    ramp_per_sample: ACCRETION_TARGET / (ATTACK_SECS * sample_rate),
  });
}

// Set target_env=0 on every Accreted voice; ramp completes in
// RELEASE_SECS regardless of starting env.
pub fn ramp_all_accretion_to_zero(voices: &mut VoiceMap, sample_rate: f32) {
  for (src, v) in voices.iter_mut() {
    if matches!(src, VoiceSource::Accreted { .. }) {
      v.target_env = 0.0;
      v.ramp_per_sample = v.env / (RELEASE_SECS * sample_rate);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

  // Render N frames (mono) for a single voice with the given initial state.
  fn render_voice(v: VoiceState, n: usize, sample_rate: f32) -> Vec<f32> {
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; n];
    render_block(&mut voices, &mut data, 1, sample_rate);
    data
  }

  #[test]
  fn empty_voices_produce_silence() {
    let mut voices: VoiceMap = HashMap::new();
    let mut data = vec![0.123_f32; 64]; // pre-fill nonzero
    render_block(&mut voices, &mut data, 1, 48000.0);
    assert!(data.iter().all(|&s| s == 0.0), "expected all zeros");
  }

  #[test]
  fn sustained_voice_produces_triangle_output() {
    let sr = 48000.0;
    let v = VoiceState {
      id: 0, freq: 440.0, phase: 0.0, env: 1.0,
      target_env: 1.0, ramp_per_sample: 0.0,
    };
    let data = render_voice(v, 1024, sr);
    let peak = data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    // Capped by AMPLITUDE=0.15.
    assert!(peak > 0.10 && peak < 0.20,
      "triangle peak out of range: {peak} (want 0.10..0.20)");
  }

  #[test]
  fn release_decays_to_zero_and_drops_voice() {
    let sr = 48000.0;
    let release_samples = (RELEASE_SECS * sr) as usize; // 2400
    let v = VoiceState {
      id: 0, freq: 220.0, phase: 0.0, env: 1.0,
      target_env: 0.0,
      ramp_per_sample: 1.0 / (RELEASE_SECS * sr),
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; release_samples + 200];
    render_block(&mut voices, &mut data, 1, sr);
    assert!(voices.is_empty(), "voice should have been dropped after release");
    let tail = &data[release_samples + 100..];
    assert!(tail.iter().all(|&s| s == 0.0),
      "tail after release should be silent");
  }
}
