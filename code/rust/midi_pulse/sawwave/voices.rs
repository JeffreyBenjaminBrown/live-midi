//! Audio rendering and per-voice helpers.

use crate::consts::AMPLITUDE;
#[cfg(test)]
use crate::consts::RELEASE_SECS;
use crate::pitch::freq_for_pitch;
use crate::types::{
  Am, AmShapeFamily, ChordId, Timbre, VoiceId, VoiceMap, VoiceSource, VoiceState, Waveform,
};

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

/// Per-waveform loudness-normalization gain. The four oscillators all swing to
/// +-1.0 but carry very different *power*: at unit peak their RMS is sine
/// 1/sqrt(2), triangle 1/sqrt(3), saw 1/sqrt(3), square 1. So a raw square sounds
/// far louder than a triangle and sine sits in between, which makes switching
/// waveforms a volume jump as much as a timbre change.
///
/// We equalize on *RMS* (signal power). Power -- not peak, and not rectified area
/// under the curve -- is the measure that tracks perceived loudness for sustained
/// broadband tones, and being a single linear gain it commutes cleanly with every
/// later stage (per-voice `gain`, AM, FM, the sink amplitude). True loudness is of
/// course frequency- and harmonic-content-dependent (equal-loudness contours), and
/// these are bright vs dark spectra; RMS is the pragmatic, signal-chain-friendly
/// proxy, not a psychoacoustic model.
///
/// Target = the *quietest* shape's RMS (1/sqrt(3), triangle/saw), so every gain is
/// <= 1 and no waveform's peak grows past +-1.0 -- this only ever attenuates, never
/// adds clipping. Triangle (the default timbre) is left at unity, so existing
/// configs sound identical on the default and only the other shapes move to match.
pub fn waveform_norm(waveform: Waveform) -> f32 {
  match waveform {
    Waveform::Sine => 0.816_496_6,    // sqrt(2/3): RMS 1/sqrt(2) -> 1/sqrt(3)
    Waveform::Triangle => 1.0,        // already 1/sqrt(3)
    Waveform::Saw => 1.0,             // already 1/sqrt(3)
    Waveform::Square => 0.577_350_3,  // 1/sqrt(3): RMS 1 -> 1/sqrt(3)
  }
}

/// The slow-AM LFO wave (bipolar, in [-1,1]) at `phase`, morphed by `shape` in
/// [0,1] within `family`. shape 0 = the soft end (sine / triangle), shape 1 = a
/// square; in between it interpolates.
pub fn am_shape_value(family: AmShapeFamily, shape: f32, phase: f32) -> f32 {
  let s = (std::f32::consts::TAU * phase).sin();
  let shape = shape.clamp(0.0, 1.0);
  match family {
    // tanh waveshaper: tanh(k*sin)/tanh(k). k->0 is a sine, k->inf a square.
    AmShapeFamily::SinToSquare => {
      if shape <= 0.0 {
        s
      } else if shape >= 1.0 {
        if s >= 0.0 { 1.0 } else { -1.0 }
      } else {
        let k = shape / (1.0 - shape); // 0..inf as shape 0..1
        (k * s).tanh() / k.tanh()
      }
    }
    // Triangle steepened toward a square: clip a gained triangle. gain 1 (shape 0)
    // is the triangle untouched; gain->inf (shape 1) clips it to a square.
    AmShapeFamily::TriToSquare => {
      let tri = triangle(phase); // -1..1, peak at phase 0.5
      let m = if shape >= 1.0 { 1.0e6 } else { 1.0 / (1.0 - shape) };
      (tri * m).clamp(-1.0, 1.0)
    }
  }
}

/// The AM amplitude multiplier in [1-depth, 1] for the LFO at `am_phase`. depth 0
/// returns 1.0 (no AM). "more depth = wider AM", dipping toward silence.
pub fn am_multiplier(am: Am, family: AmShapeFamily, am_phase: f32) -> f32 {
  if am.depth <= 0.0 {
    return 1.0;
  }
  let unipolar = (am_shape_value(family, am.shape, am_phase) + 1.0) * 0.5; // 0..1
  1.0 - am.depth * (1.0 - unipolar)
}

/// The FM carrier-frequency multiplier: 2^(depth_cents * sin(2*pi*fm_phase) / 1200).
/// depth_cents 0 returns 1.0 (no FM).
pub fn fm_factor(depth_cents: f32, fm_phase: f32) -> f32 {
  if depth_cents == 0.0 {
    return 1.0;
  }
  2.0_f32.powf(depth_cents * (std::f32::consts::TAU * fm_phase).sin() / 1200.0)
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
  render_block_with_amplitude(
    voices, data, channels, sample_rate, AMPLITUDE, AmShapeFamily::default(),
  );
}

pub fn render_block_with_amplitude(
  voices: &mut VoiceMap,
  data: &mut [f32],
  channels: usize,
  sample_rate: f32,
  amplitude: f32,
  shape_family: AmShapeFamily,
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
      // Advance the per-voice AM/FM LFOs.
      v.am_phase += v.timbre.am.freq / sample_rate;
      if v.am_phase >= 1.0 { v.am_phase -= 1.0; }
      v.fm_phase += v.timbre.fm.freq / sample_rate;
      if v.fm_phase >= 1.0 { v.fm_phase -= 1.0; }
      // FM modulates the carrier increment; PolyBLEP band-limits at this same dt.
      let dt = v.freq * fm_factor(v.timbre.fm.depth_cents, v.fm_phase) / sample_rate;
      v.phase += dt;
      if v.phase >= 1.0 { v.phase -= 1.0; }
      let amm = am_multiplier(v.timbre.am, shape_family, v.am_phase);
      let wf_norm = waveform_norm(v.timbre.waveform);
      mix += osc(v.timbre.waveform, v.phase, dt) * wf_norm * v.env * v.timbre.gain * amm * amplitude;
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
  use crate::types::Fm;
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
  fn waveform_norm_equalizes_rms_across_shapes() {
    // After normalization every shape should render at the same RMS (power), so
    // switching waveform is a timbre change, not a volume jump. Measure each at a
    // frequency low enough that PolyBLEP barely perturbs the ideal RMS.
    let sr = 48000.0;
    let rms = |waveform: Waveform| {
      let v = VoiceState {
        id: 0, freq: 100.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0,
        timbre: Timbre { waveform, gain: 1.0, am: Am::default(), fm: Fm::default() },
        am_phase: 0.0, fm_phase: 0.0,
      };
      let data = render_voice(v, sr as usize, sr); // ~1 s = many whole periods
      (data.iter().map(|x| x * x).sum::<f32>() / data.len() as f32).sqrt()
    };
    let reference = rms(Waveform::Triangle);
    for wf in [Waveform::Sine, Waveform::Saw, Waveform::Square] {
      let r = rms(wf);
      assert!((r / reference - 1.0).abs() < 0.03,
        "{wf:?} RMS {r} should match triangle {reference} within 3%");
    }
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

  #[test]
  fn am_shape_morphs_sine_to_square() {
    let p = 0.1;
    let sine = (std::f32::consts::TAU * p).sin();
    assert!((am_shape_value(AmShapeFamily::SinToSquare, 0.0, p) - sine).abs() < 1e-4);
    // sin(2pi*0.1) > 0, so the square end is +1.
    assert!((am_shape_value(AmShapeFamily::SinToSquare, 1.0, p) - 1.0).abs() < 1e-4);
    assert!(am_shape_value(AmShapeFamily::SinToSquare, 0.5, p).abs() <= 1.01);
  }

  #[test]
  fn am_shape_morphs_triangle_to_square() {
    let p = 0.1;
    assert!((am_shape_value(AmShapeFamily::TriToSquare, 0.0, p) - triangle(p)).abs() < 1e-4);
    assert!(am_shape_value(AmShapeFamily::TriToSquare, 1.0, 0.4).abs() > 0.99);
  }

  #[test]
  fn am_multiplier_spans_floor_to_one() {
    let am = Am { depth: 0.8, freq: 1.0, shape: 0.0 };
    let top = am_multiplier(am, AmShapeFamily::SinToSquare, 0.25); // sine peak
    let bot = am_multiplier(am, AmShapeFamily::SinToSquare, 0.75); // sine trough
    assert!((top - 1.0).abs() < 1e-3, "top={top}");
    assert!((bot - 0.2).abs() < 1e-3, "bot={bot} (1-depth)");
    let none = Am { depth: 0.0, ..am };
    assert_eq!(am_multiplier(none, AmShapeFamily::SinToSquare, 0.75), 1.0);
  }

  #[test]
  fn fm_factor_is_an_octave_at_1200_cents() {
    assert!((fm_factor(0.0, 0.3) - 1.0).abs() < 1e-6);
    assert!((fm_factor(1200.0, 0.25) - 2.0).abs() < 1e-3); // +1 octave
    assert!((fm_factor(1200.0, 0.75) - 0.5).abs() < 1e-3); // -1 octave
  }

  #[test]
  fn full_depth_am_lowers_average_energy() {
    let sr = 48000.0;
    let rms = |depth: f32| {
      let mut voices: VoiceMap = HashMap::new();
      voices.insert(VoiceSource::Fingered { xy: (0, 0) }, VoiceState {
        id: 0, freq: 1000.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0,
        timbre: Timbre {
          waveform: Waveform::Sine, gain: 1.0,
          am: Am { depth, freq: 50.0, shape: 0.0 }, fm: Fm::default(),
        },
        am_phase: 0.0, fm_phase: 0.0,
      });
      let mut data = vec![0.0_f32; 4800];
      render_block_with_amplitude(&mut voices, &mut data, 1, sr, 1.0, AmShapeFamily::SinToSquare);
      (data.iter().map(|x| x * x).sum::<f32>() / data.len() as f32).sqrt()
    };
    let (off, full) = (rms(0.0), rms(1.0));
    assert!(full < off * 0.85, "AM should cut average energy: off={off} full={full}");
    assert!(full > 0.0);
  }

  #[test]
  fn fm_changes_the_rendered_samples() {
    let sr = 48000.0;
    let render = |depth_cents: f32| {
      let mut voices: VoiceMap = HashMap::new();
      voices.insert(VoiceSource::Fingered { xy: (0, 0) }, VoiceState {
        id: 0, freq: 300.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0,
        timbre: Timbre {
          waveform: Waveform::Sine, gain: 1.0, am: Am::default(),
          fm: Fm { depth_cents, freq: 6.0 },
        },
        am_phase: 0.0, fm_phase: 0.0,
      });
      let mut data = vec![0.0_f32; 4800];
      render_block_with_amplitude(&mut voices, &mut data, 1, sr, 1.0, AmShapeFamily::default());
      data
    };
    let flat = render(0.0);
    let vib = render(600.0);
    let diff: f32 = flat.iter().zip(&vib).map(|(a, b)| (a - b).abs()).sum();
    assert!(diff > 1.0, "FM should change the rendered samples: diff={diff}");
  }
}
