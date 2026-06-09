//! Audio rendering and per-voice helpers.

use crate::consts::AMPLITUDE;
#[cfg(test)]
use crate::consts::RELEASE_SECS;
use crate::pitch::freq_for_pitch;
use crate::types::{ChordId, Timbre, VoiceId, VoiceMap, VoiceSource, VoiceState, Waveform};

pub fn triangle(phase: f32) -> f32 {
  // phase is in [0, 1)
  if phase < 0.5 {
    4.0 * phase - 1.0
  } else {
    3.0 - 4.0 * phase
  }
}

/// One sample of `waveform` at `phase` in [0,1). `dt` is the per-sample phase
/// increment (effective_freq / sample_rate); the discontinuous waveforms (square,
/// saw) use it for PolyBLEP band-limiting. sine/triangle ignore it.
pub fn osc(waveform: Waveform, phase: f32, dt: f32) -> f32 {
  match waveform {
    Waveform::Sine => (std::f32::consts::TAU * phase).sin(),
    Waveform::Triangle => triangle(phase),
    Waveform::Saw => {
      // Naive saw -1..+1, minus the PolyBLEP residual at the wrap discontinuity.
      let naive = 2.0 * phase - 1.0;
      naive - poly_blep(phase, dt)
    }
    Waveform::Square => {
      let naive = if phase < 0.5 { 1.0 } else { -1.0 };
      // Up-step at phase 0, down-step at phase 0.5.
      naive + poly_blep(phase, dt) - poly_blep((phase + 0.5).fract(), dt)
    }
  }
}

/// PolyBLEP residual for a unit *upward* step at phase 0 (wrapping). Subtract it
/// for a downward step. The standard 2-sample polynomial correction; with no
/// increment (dt<=0) there's nothing to band-limit.
fn poly_blep(t: f32, dt: f32) -> f32 {
  if dt <= 0.0 {
    return 0.0;
  }
  if t < dt {
    let x = t / dt;
    return x + x - x * x - 1.0;
  }
  if t > 1.0 - dt {
    let x = (t - 1.0) / dt;
    return x * x + x + x + 1.0;
  }
  0.0
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
  render_block_with_amplitude(voices, data, channels, sample_rate, AMPLITUDE);
}

pub fn render_block_with_amplitude(
  voices: &mut VoiceMap,
  data: &mut [f32],
  channels: usize,
  sample_rate: f32,
  amplitude: f32,
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
      let dt = v.freq / sample_rate;
      v.phase += dt;
      if v.phase >= 1.0 {
        v.phase -= 1.0;
      }
      // C2 will modulate dt (FM) and apply AM here; for now: waveform + gain.
      mix += osc(v.timbre.waveform, v.phase, dt) * v.env * v.timbre.gain * amplitude;
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

// Spawn a fresh accretion voice for chord/pitch at env=0 ramping to
// ACCRETION_TARGET over ATTACK_SECS. Overwrites any existing entry
// at Accreted{chord, pitch}.
pub fn spawn_accretion_voice(
  voices: &mut VoiceMap, chord: ChordId, pitch: i32,
  fund: f64, edo: i32, next_voice_id: &mut VoiceId, sample_rate: f32,
  accretion_level: f32, attack_secs: f32,
) {
  let id = *next_voice_id;
  *next_voice_id += 1;
  voices.insert(VoiceSource::Accreted { chord, pitch }, VoiceState {
    id,
    freq: freq_for_pitch(pitch, fund, edo),
    phase: 0.0,
    env: 0.0,
    target_env: accretion_level,
    ramp_per_sample: accretion_level / (attack_secs * sample_rate),
    timbre: Timbre::default(),
    am_phase: 0.0,
    fm_phase: 0.0,
  });
}

// Set target_env=0 on every Accreted voice belonging to `chord`;
// ramp completes in RELEASE_SECS regardless of starting env.
pub fn ramp_chord_accretion_to_zero(
  voices: &mut VoiceMap,
  chord: ChordId,
  sample_rate: f32,
  release_secs: f32,
) {
  for (src, v) in voices.iter_mut() {
    if let VoiceSource::Accreted { chord: c, .. } = src {
      if *c == chord {
        v.target_env = 0.0;
        v.ramp_per_sample = v.env / (release_secs * sample_rate);
      }
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
      timbre: Timbre::default(), am_phase: 0.0, fm_phase: 0.0,
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
      timbre: Timbre::default(), am_phase: 0.0, fm_phase: 0.0,
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

  #[test]
  fn osc_shapes_have_expected_values() {
    let dt = 0.0001; // away from discontinuities, PolyBLEP is inert
    assert!((osc(Waveform::Sine, 0.25, dt) - 1.0).abs() < 1e-3);
    assert!(osc(Waveform::Sine, 0.0, dt).abs() < 1e-3);
    assert!((osc(Waveform::Triangle, 0.5, dt) - 1.0).abs() < 1e-3);
    assert!(osc(Waveform::Triangle, 0.25, dt).abs() < 1e-3);
    assert!(osc(Waveform::Square, 0.25, dt) > 0.9);
    assert!(osc(Waveform::Square, 0.75, dt) < -0.9);
    assert!(osc(Waveform::Saw, 0.5, dt).abs() < 1e-2);
    assert!(osc(Waveform::Saw, 0.25, dt) < 0.0);
    assert!(osc(Waveform::Saw, 0.75, dt) > 0.0);
  }

  #[test]
  fn per_voice_gain_scales_amplitude() {
    let sr = 48000.0;
    let mk = |gain: f32| VoiceState {
      id: 0, freq: 440.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0,
      timbre: Timbre { gain, ..Timbre::default() }, am_phase: 0.0, fm_phase: 0.0,
    };
    let peak = |g: f32| render_voice(mk(g), 1024, sr).iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    let (full, half) = (peak(1.0), peak(0.5));
    assert!((half / full - 0.5).abs() < 0.05, "gain 0.5 should ~halve peak: full={full} half={half}");
  }

  #[test]
  fn poly_blep_keeps_square_and_saw_bounded() {
    let dt = 1.0 / 100.0;
    for i in 0..100 {
      let p = i as f32 / 100.0;
      for wf in [Waveform::Square, Waveform::Saw] {
        let v = osc(wf, p, dt);
        assert!(v.abs() <= 1.5, "{wf:?} at {p} = {v} out of bounds");
      }
    }
  }
}
