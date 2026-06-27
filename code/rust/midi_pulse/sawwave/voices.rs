//! Audio rendering and per-voice helpers.
//!
//! Oversampling note: the per-voice synthesis is nonlinear (the `tanh`/clip AM
//! waveshaper) and multiplicative (the AM/FM multiply), so it manufactures harmonics
//! and sidebands above the carrier. At sub-audio AM that is harmless, but pushed to
//! audio rate those products cross Nyquist and fold back as inharmonic aliases that a
//! plain output filter cannot remove (they are already in-band). `BlockRenderer` with
//! `oversample = N` runs the whole mix at N x the rate -- so the products land below
//! the high-rate Nyquist -- then a `Decimator` low-passes and downsamples, removing
//! them before they fold. The edge-limited AM square (`am_shape_value`) and this
//! oversampling are complementary: the cap bounds how many harmonics exist (so a
//! modest N suffices), oversampling gives them room. Band-limited generation (the
//! `osc` PolyBLEP) handles oscillator edges; oversampling handles the nonlinear mix.

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

/// How steep the "square" end of the AM shape is allowed to get. A *true* square
/// (vertical edges) is what clicks: at full depth the amplitude steps between its
/// floor and 1.0 in a single sample, a broadband transient. Real synths never emit
/// an ideal square here -- they cap the edge slope (slew limiter / BLEP / a "smooth"
/// knob), leaving a near-square with a short but finite rise. These caps hold the
/// edge at very roughly ~4% of the LFO period -- visually square, but spread over
/// enough samples at tremolo rates that it doesn't click. The morph stops steepening
/// past these; the top sliver of the shape row all renders this same click-free
/// near-square. Tune up for crisper edges (toward clicking), down for softer.
const AM_SQUARE_MAX_K: f32 = 16.0; // SinToSquare: tanh sharpness; edge ~ 2/(pi*k)
const AM_SQUARE_MAX_GAIN: f32 = 12.5; // TriToSquare: clip gain;   edge ~ 1/(2*gain)

/// The slow-AM LFO wave (bipolar, in [-1,1]) at `phase`, morphed by `shape` in
/// [0,1] within `family`. shape 0 = the soft end (sine / triangle), shape 1 = a
/// near-square (edge-limited so it never clicks); in between it interpolates.
pub fn am_shape_value(family: AmShapeFamily, shape: f32, phase: f32) -> f32 {
  let s = (std::f32::consts::TAU * phase).sin();
  let shape = shape.clamp(0.0, 1.0);
  match family {
    // tanh waveshaper: tanh(k*sin)/tanh(k). k->0 is a sine; k grows toward a square
    // as shape->1 but is CAPPED at AM_SQUARE_MAX_K, so even at shape 1.0 the edge
    // keeps a finite rise and doesn't click (a hard sign() square would).
    AmShapeFamily::SinToSquare => {
      if shape <= 0.0 {
        s
      } else {
        let k = (shape / (1.0 - shape)).min(AM_SQUARE_MAX_K); // shape 1.0 -> capped
        (k * s).tanh() / k.tanh()
      }
    }
    // Triangle steepened toward a square: clip a gained triangle. gain 1 (shape 0)
    // is the triangle untouched; the gain grows toward a square as shape->1 but is
    // CAPPED at AM_SQUARE_MAX_GAIN, so full square keeps a finite edge (no click).
    AmShapeFamily::TriToSquare => {
      let tri = triangle(phase); // -1..1, peak at phase 0.5
      let m = (1.0 / (1.0 - shape)).min(AM_SQUARE_MAX_GAIN); // shape 1.0 -> capped
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

/// Advance every voice by `frac` of one output sample (1.0 on the plain path, 1/N
/// when oversampling) and return their summed contribution, pre-clamp. Running the
/// nonlinear per-voice work (osc, AM, FM, and the AM multiply) at N x the rate is the
/// whole point of oversampling: the harmonics/sidebands it manufactures then land
/// below the *high-rate* Nyquist instead of folding back into the band. Dead voices
/// are dropped here, exactly as the single-rate path did.
fn accumulate_voices(
  voices: &mut VoiceMap,
  frac: f32,
  sample_rate: f32,
  amplitude: f32,
  shape_family: AmShapeFamily,
) -> f32 {
  let mut mix = 0.0_f32;
  voices.retain(|_, v| {
    // Step env toward target_env (by `frac` of a full-rate step).
    let ramp = v.ramp_per_sample * frac;
    let delta = v.target_env - v.env;
    if delta.abs() <= ramp {
      v.env = v.target_env;
    } else if delta > 0.0 {
      v.env += ramp;
    } else {
      v.env -= ramp;
    }
    // Voice is gone once env and target both reach 0.
    if v.env == 0.0 && v.target_env == 0.0 {
      return false;
    }
    // Advance the per-voice AM/FM LFOs (by `frac` of a full-rate step).
    v.am_phase += v.timbre.am.freq * frac / sample_rate;
    if v.am_phase >= 1.0 { v.am_phase -= 1.0; }
    v.fm_phase += v.timbre.fm.freq * frac / sample_rate;
    if v.fm_phase >= 1.0 { v.fm_phase -= 1.0; }
    // FM modulates the carrier increment; PolyBLEP band-limits at this same dt -- the
    // per-sub-sample increment, so it stays correct at whatever rate we step.
    let dt = v.freq * fm_factor(v.timbre.fm.depth_cents, v.fm_phase) * frac / sample_rate;
    v.phase += dt;
    if v.phase >= 1.0 { v.phase -= 1.0; }
    let amm = am_multiplier(v.timbre.am, shape_family, v.am_phase);
    let wf_norm = waveform_norm(v.timbre.waveform);
    mix += osc(v.timbre.waveform, v.phase, dt) * wf_norm * v.env * v.timbre.gain * amm * amplitude;
    true
  });
  mix
}

/// A stateful per-stream renderer. With `oversample > 1` it runs the mix at N x the
/// output rate and decimates back down, so audio-rate AM / FM / waveshaping don't
/// alias (see the module-level note). `oversample = 1` is the plain path: no
/// decimator, no latency, bit-identical to the pre-oversampling render. The state
/// (the decimation filter's history) must persist across cpal callbacks, so this is
/// a struct the audio loop owns rather than a free function.
pub struct BlockRenderer {
  oversample: usize,
  decimator: Option<Decimator>,
}

impl BlockRenderer {
  pub fn new(oversample: usize) -> Self {
    let oversample = oversample.max(1);
    let decimator = (oversample > 1).then(|| Decimator::new(oversample));
    BlockRenderer { oversample, decimator }
  }

  /// Render one cpal callback's worth of audio into `data` from `voices`. Pulled out
  /// of the cpal closure so unit tests can exercise it without cpal or PipeWire.
  pub fn render(
    &mut self,
    voices: &mut VoiceMap,
    data: &mut [f32],
    channels: usize,
    sample_rate: f32,
    amplitude: f32,
    shape_family: AmShapeFamily,
  ) {
    let oversample = self.oversample;
    match &mut self.decimator {
      // Plain path: one sub-step per output sample, no filtering, no latency.
      None => {
        for frame in data.chunks_mut(channels) {
          let mix = accumulate_voices(voices, 1.0, sample_rate, amplitude, shape_family);
          let s = mix.clamp(-0.95, 0.95);
          for out in frame.iter_mut() {
            *out = s;
          }
        }
      }
      // Oversampled path: N sub-steps per output sample, then decimate. The clamp runs
      // at the high rate, so even hard-clip distortion is band-limited before folding.
      Some(decim) => {
        let frac = 1.0 / oversample as f32;
        for frame in data.chunks_mut(channels) {
          for _ in 0..oversample {
            let mix = accumulate_voices(voices, frac, sample_rate, amplitude, shape_family);
            decim.push(mix.clamp(-0.95, 0.95));
          }
          let s = decim.output();
          for out in frame.iter_mut() {
            *out = s;
          }
        }
      }
    }
  }
}

/// Backwards-compatible single-rate render (oversample = 1). Kept for the standalone
/// sawwave path and the unit tests; the looper drives a persistent `BlockRenderer`.
pub fn render_block_with_amplitude(
  voices: &mut VoiceMap,
  data: &mut [f32],
  channels: usize,
  sample_rate: f32,
  amplitude: f32,
  shape_family: AmShapeFamily,
) {
  BlockRenderer::new(1).render(voices, data, channels, sample_rate, amplitude, shape_family);
}

/// The decimation stage of an oversampled render: a windowed-sinc FIR low-pass at the
/// original Nyquist, then keep one sample per `factor`. Push every oversampled sample
/// (`push`); read one filtered output per `factor` pushes (`output`). Cutoff below the
/// post-decimation Nyquist means the high-rate content that would fold on downsampling
/// is removed first -- a plain output filter could not, since by then it has already
/// aliased in-band. Linear-phase (symmetric taps); its (len-1)/2-sample group delay is
/// well under a millisecond.
struct Decimator {
  taps: Vec<f32>,
  history: Vec<f32>, // ring buffer of the last taps.len() oversampled samples
  pos: usize,        // index of the most-recent pushed sample
}

impl Decimator {
  fn new(factor: usize) -> Self {
    let taps = lowpass_taps(factor);
    let len = taps.len();
    Decimator { taps, history: vec![0.0; len], pos: 0 }
  }

  fn push(&mut self, x: f32) {
    let n = self.history.len();
    self.pos = (self.pos + 1) % n;
    self.history[self.pos] = x;
  }

  /// FIR output for the current history; `taps[0]` weights the most-recent sample.
  fn output(&self) -> f32 {
    let n = self.history.len();
    let mut acc = 0.0_f32;
    for (i, &c) in self.taps.iter().enumerate() {
      acc += c * self.history[(self.pos + n - i) % n];
    }
    acc
  }
}

/// Windowed-sinc low-pass taps for decimating a `factor`x-oversampled stream: cutoff
/// at 0.45/factor cycles/sample (just under the post-decimation Nyquist of
/// 0.5/factor), a Blackman window (~74 dB stopband), length scaling with `factor` so
/// the transition band stays comparable. Normalized to unity DC gain.
fn lowpass_taps(factor: usize) -> Vec<f32> {
  use std::f32::consts::{PI, TAU};
  let len = (16 * factor) | 1; // odd, so there is a true center tap
  let fc = 0.45 / factor as f32; // cutoff in cycles/sample at the oversampled rate
  let mid = (len - 1) as f32 / 2.0;
  let last = (len - 1) as f32;
  let mut taps = vec![0.0_f32; len];
  let mut sum = 0.0_f32;
  for (i, t) in taps.iter_mut().enumerate() {
    let x = i as f32 - mid;
    let sinc = if x == 0.0 { 2.0 * fc } else { (TAU * fc * x).sin() / (PI * x) };
    let w = 0.42 - 0.5 * (TAU * i as f32 / last).cos() + 0.08 * (2.0 * TAU * i as f32 / last).cos();
    *t = sinc * w;
    sum += *t;
  }
  for t in &mut taps {
    *t /= sum;
  }
  taps
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
  fn full_square_am_shape_is_edge_limited_not_a_clicking_step() {
    // A true square jumps +-1 -> -+1 in one sample (the click). The edge-capped
    // near-square must instead cross over many samples -- so densely sampling one
    // period, the largest sample-to-sample step is small -- yet it still plateaus at
    // ~+-1 (still square-like, not softened to a sine).
    let n = 4000;
    for family in [AmShapeFamily::SinToSquare, AmShapeFamily::TriToSquare] {
      let vals: Vec<f32> =
        (0..n).map(|i| am_shape_value(family, 1.0, i as f32 / n as f32)).collect();
      let max_step =
        vals.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0_f32, f32::max);
      assert!(max_step < 0.25, "{family:?}: full-square edge should be gradual (no click), got step {max_step}");
      let peak = vals.iter().fold(0.0_f32, |a, &v| a.max(v.abs()));
      assert!(peak > 0.99, "{family:?}: should still plateau near +-1 (square-like), peak {peak}");
    }
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
  fn oversampling_removes_an_above_nyquist_alias() {
    // A 40 kHz sine point-sampled at 48 kHz folds to a loud 8 kHz alias. Run the SAME
    // voice through a 4x-oversampled renderer: 40 kHz is generated below the high-rate
    // Nyquist (96 kHz), then the decimation low-pass removes it before downsampling --
    // so it nearly vanishes. A 1 kHz tone (well in band) must survive both, proving we
    // filtered the alias rather than muting everything.
    let sr = 48000.0;
    let rms = |freq: f32, oversample: usize| {
      let mut voices: VoiceMap = HashMap::new();
      voices.insert(
        VoiceSource::Fingered { xy: (0, 0) },
        VoiceState {
          id: 0, freq, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0,
          timbre: Timbre {
            waveform: Waveform::Sine, gain: 1.0, am: Am::default(), fm: Fm::default(),
          },
          am_phase: 0.0, fm_phase: 0.0,
        },
      );
      let mut data = vec![0.0_f32; 4800];
      BlockRenderer::new(oversample)
        .render(&mut voices, &mut data, 1, sr, 1.0, AmShapeFamily::default());
      (data.iter().map(|x| x * x).sum::<f32>() / data.len() as f32).sqrt()
    };
    let (alias_plain, alias_os) = (rms(40_000.0, 1), rms(40_000.0, 4));
    assert!(alias_plain > 0.3, "plain render should fold a loud alias: {alias_plain}");
    assert!(alias_os < alias_plain * 0.1, "4x oversampling should kill it: plain={alias_plain} os={alias_os}");
    let (pass_plain, pass_os) = (rms(1_000.0, 1), rms(1_000.0, 4));
    assert!(pass_os > pass_plain * 0.8, "an in-band 1 kHz tone must pass: plain={pass_plain} os={pass_os}");
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
