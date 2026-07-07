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
/// saw) use it for PolyBLEP band-limiting. sine/triangle ignore it. A *negative*
/// `dt` (a through-zero oscillator running backward) band-limits on its magnitude
/// -- the edge width is the same whichever way the phase crosses it.
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
/// increment (dt=0) there's nothing to band-limit. Only the increment's magnitude
/// matters: a backward-running (through-zero) oscillator crosses the same
/// discontinuity over the same number of samples.
fn poly_blep(t: f32, dt: f32) -> f32 {
  let dt = dt.abs();
  if dt == 0.0 {
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

/// The `(sustain_env, decay_per_sample)` pair for a voice struck to `peak` under a
/// sink's pluck settings (see TODO/misc.org "synth attacks should be louder"): after
/// the attack the envelope decays exponentially, time constant `decay_secs`, toward
/// `sustain_level * peak` -- a rough plucked-string curve (instant rise, exponential
/// fall) that plateaus at a sustain instead of dying, so held notes and accrete
/// drones keep ringing. `sustain_level >= 1` or `decay_secs <= 0` disables the decay
/// (the flat pre-pluck envelope). The sustain is floored just above zero so a held
/// voice never becomes a released one.
pub fn pluck_envelope(sustain_level: f32, decay_secs: f32, peak: f32, sample_rate: f32) -> (f32, f32) {
  if sustain_level >= 1.0 || decay_secs <= 0.0 {
    return (peak, 1.0);
  }
  let sustain = sustain_level.max(0.001) * peak;
  (sustain, (-1.0 / (decay_secs * sample_rate)).exp())
}

/// The FM carrier-frequency multiplier: 2^(depth_cents * sin(2*pi*fm_phase) / 1200).
/// depth_cents 0 returns 1.0 (no FM).
///
/// This FM is *exponential* (depth in cents, i.e. a pitch offset), so the multiplier
/// is always > 0: no modulation depth can push the carrier frequency below 0 Hz. That
/// makes "through-zero FM" -- the linear-FM behavior where a below-zero frequency
/// flips phase and runs the oscillator backward -- unreachable from these controls.
/// The oscillator core itself IS through-zero-capable (a negative increment runs the
/// phase backward, band-limited on |dt|; see `accumulate_voices` / `poly_blep`), so
/// any future *linear* FM gets through-zero behavior for free.
pub fn fm_factor(depth_cents: f32, fm_phase: f32) -> f32 {
  if depth_cents == 0.0 {
    return 1.0;
  }
  2.0_f32.powf(depth_cents * (std::f32::consts::TAU * fm_phase).sin() / 1200.0)
}

/// A global saturating distortion applied to the *summed* voice mix (not per voice):
/// the algebraic soft-clipper `f(y) = y / (1 + |y/s|^k)^(1/k)` from
/// `learnings/distortion.org`. `scale` (s) is the asymptote -- the output can never
/// exceed +-s, smaller = earlier/heavier bite; `shape` (k) is the elbow's harshness --
/// 1 = gentlest, 2 = the smooth sweet spot, ~4+ = hard-ish, -> inf = hard clip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distortion {
  pub scale: f32,
  pub shape: f32,
}

/// One sample of the soft-clipper. Odd (no DC), unity slope at the origin, monotone,
/// plateauing at +-scale. Runs on the summed mix at the (possibly oversampled) render
/// rate, so the harmonics it manufactures are band-limited by the same decimation
/// path as the rest of the nonlinear mix.
pub fn distort(y: f32, d: Distortion) -> f32 {
  if d.scale <= 0.0 || d.shape <= 0.0 {
    return y; // degenerate parameters: pass through rather than blow up
  }
  let u = (y / d.scale).abs();
  y / (1.0 + u.powf(d.shape)).powf(1.0 / d.shape)
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
    // Envelope. Linear attack toward target_env; when the attack peaks on a held
    // voice whose sustain sits below the peak, the target drops to sustain_env and
    // the env DECAYS toward it exponentially (the pluck) -- so a fresh strike rings
    // out over already-sounding notes. note_off ramps linearly to 0 as ever.
    if v.target_env > 0.0 && v.env >= v.target_env && v.sustain_env < v.target_env {
      v.env = v.target_env;
      v.target_env = v.sustain_env;
    }
    if v.target_env > 0.0 && v.env > v.target_env && v.decay_per_sample < 1.0 {
      // The pluck decay: retain `decay_per_sample` of the distance to sustain each
      // full-rate sample (linearized for `frac` sub-steps), snapping when close.
      // (A flat voice above a positive target -- e.g. the chord-accretion
      // transform -- keeps the linear ramp below instead.)
      let keep = 1.0 - (1.0 - v.decay_per_sample) * frac;
      v.env = v.target_env + (v.env - v.target_env) * keep;
      if v.env - v.target_env < 1e-4 {
        v.env = v.target_env;
      }
    } else {
      // Step env linearly toward target_env (by `frac` of a full-rate step).
      let ramp = v.ramp_per_sample * frac;
      let delta = v.target_env - v.env;
      if delta.abs() <= ramp {
        v.env = v.target_env;
      } else if delta > 0.0 {
        v.env += ramp;
      } else {
        v.env -= ramp;
      }
    }
    // Voice is gone once env and target both reach 0.
    if v.env == 0.0 && v.target_env == 0.0 {
      return false;
    }
    // Frequency glide (the slide feature): while active, walk freq multiplicatively
    // toward freq_target (pitch-linear), snapping there once crossed -- then the
    // glide ends. Inert while glide_per_sample == 1.0 (freq_target is ignored).
    if v.glide_per_sample != 1.0 && v.freq != v.freq_target {
      let step = 1.0 + (v.glide_per_sample - 1.0) * frac;
      v.freq *= step;
      let crossed = (v.glide_per_sample > 1.0 && v.freq >= v.freq_target)
        || (v.glide_per_sample < 1.0 && v.freq <= v.freq_target);
      if crossed {
        v.freq = v.freq_target;
        v.glide_per_sample = 1.0;
      }
    }
    // The polyrhythm pulse: a descending saw (1 at cycle start, falling to 0) at
    // the tempo applied at this note's onset. Separate from the timbre AM below.
    let pulse = if v.tempo_am_freq > 0.0 {
      v.tempo_am_phase += v.tempo_am_freq * frac / sample_rate;
      if v.tempo_am_phase >= 1.0 {
        v.tempo_am_phase -= 1.0;
      }
      1.0 - v.tempo_am_phase
    } else {
      1.0
    };
    // Advance the per-voice AM/FM LFOs (by `frac` of a full-rate step).
    v.am_phase += v.timbre.am.freq * frac / sample_rate;
    if v.am_phase >= 1.0 { v.am_phase -= 1.0; }
    v.fm_phase += v.timbre.fm.freq * frac / sample_rate;
    if v.fm_phase >= 1.0 { v.fm_phase -= 1.0; }
    // FM modulates the carrier increment; PolyBLEP band-limits at this same dt -- the
    // per-sub-sample increment, so it stays correct at whatever rate we step.
    let dt = v.freq * fm_factor(v.timbre.fm.depth_cents, v.fm_phase) * frac / sample_rate;
    // Through-zero: rem_euclid keeps phase in [0,1) even for a negative dt (a
    // "negative-frequency" voice runs backward -- for a sine, exactly a 180-degree
    // phase flip, as a through-zero FM oscillator should) or a |dt| > 1.
    v.phase = (v.phase + dt).rem_euclid(1.0);
    let amm = am_multiplier(v.timbre.am, shape_family, v.am_phase);
    let wf_norm = waveform_norm(v.timbre.waveform);
    mix += osc(v.timbre.waveform, v.phase, dt) * wf_norm * v.env * v.timbre.gain * amm * pulse * amplitude;
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
    self.render_with_distortion(voices, data, channels, sample_rate, amplitude, shape_family, None);
  }

  /// `render`, with an optional global distortion applied to the summed mix (pre-
  /// clamp, at the oversampled rate when oversampling -- a nonlinearity belongs
  /// inside the anti-aliasing loop, like the clamp). `None` is bit-identical to
  /// `render`. The surfaces runtime passes `Some` while its distortion toggle is on.
  #[allow(clippy::too_many_arguments)]
  pub fn render_with_distortion(
    &mut self,
    voices: &mut VoiceMap,
    data: &mut [f32],
    channels: usize,
    sample_rate: f32,
    amplitude: f32,
    shape_family: AmShapeFamily,
    distortion: Option<Distortion>,
  ) {
    let oversample = self.oversample;
    let post = |mix: f32| match distortion {
      Some(d) => distort(mix, d).clamp(-0.95, 0.95),
      None => mix.clamp(-0.95, 0.95),
    };
    match &mut self.decimator {
      // Plain path: one sub-step per output sample, no filtering, no latency.
      None => {
        for frame in data.chunks_mut(channels) {
          let mix = accumulate_voices(voices, 1.0, sample_rate, amplitude, shape_family);
          let s = post(mix);
          for out in frame.iter_mut() {
            *out = s;
          }
        }
      }
      // Oversampled path: N sub-steps per output sample, then decimate. The distortion
      // and clamp run at the high rate, so their harmonics are band-limited before
      // they can fold.
      Some(decim) => {
        let frac = 1.0 / oversample as f32;
        for frame in data.chunks_mut(channels) {
          for _ in 0..oversample {
            let mix = accumulate_voices(voices, frac, sample_rate, amplitude, shape_family);
            decim.push(post(mix));
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
    freq_target: 0.0,
    glide_per_sample: 1.0,
    tempo_am_freq: 0.0, tempo_am_phase: 0.0,
    phase: 0.0,
    env: 0.0,
    target_env: accretion_level,
    ramp_per_sample: accretion_level / (attack_secs * sample_rate),
    // Accretion voices are steady drones: no pluck decay.
    sustain_env: accretion_level,
    decay_per_sample: 1.0,
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
      id: 0, freq: 440.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0,
      target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
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
      id: 0, freq: 220.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0,
      target_env: 0.0,
      ramp_per_sample: 1.0 / (RELEASE_SECS * sr),
      sustain_env: 1.0, decay_per_sample: 1.0,
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
      id: 0, freq: 440.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
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
        id: 0, freq: 100.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
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
        id: 0, freq: 1000.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
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
          id: 0, freq, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
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
  fn through_zero_a_negative_frequency_sine_is_the_forward_sine_phase_flipped() {
    // Through-zero FM behavior (TODO/misc.org): an oscillator forced below 0 Hz must
    // not stop; it runs backward, which for a sine is exactly a 180-degree phase flip
    // (sin(-x) = -sin(x)) -- the ring-modulator identity at the zero line. Render the
    // same voice at +f and -f: sample for sample, one is the negation of the other.
    let sr = 48000.0;
    let render = |freq: f32| {
      let v = VoiceState {
        id: 0, freq, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
        timbre: Timbre { waveform: Waveform::Sine, ..Timbre::default() },
        am_phase: 0.0, fm_phase: 0.0,
      };
      render_voice(v, 1024, sr)
    };
    let (fwd, bwd) = (render(300.0), render(-300.0));
    let peak = fwd.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    assert!(peak > 0.05, "the forward render must actually sound: peak={peak}");
    for (i, (a, b)) in fwd.iter().zip(&bwd).enumerate() {
      assert!((a + b).abs() < 1e-4, "sample {i}: backward {b} should be -(forward {a})");
    }
  }

  #[test]
  fn through_zero_keeps_phase_in_range_and_saw_band_limited() {
    // A backward-running voice must keep its phase in [0,1) (rem_euclid wrap) and
    // PolyBLEP must band-limit on |dt|, so a discontinuous waveform stays bounded.
    let sr = 48000.0;
    let v = VoiceState {
      id: 0, freq: -220.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
      timbre: Timbre { waveform: Waveform::Saw, ..Timbre::default() },
      am_phase: 0.0, fm_phase: 0.0,
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; 4096];
    render_block(&mut voices, &mut data, 1, sr);
    let state = voices.values().next().expect("voice survives");
    assert!((0.0..1.0).contains(&state.phase), "phase wrapped into [0,1): {}", state.phase);
    let peak = data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    assert!(peak > 0.05 && peak <= 0.95, "backward saw sounds and stays bounded: {peak}");
    // The PolyBLEP correction itself is symmetric in dt.
    let dt = 1.0 / 100.0;
    for i in 0..100 {
      let p = i as f32 / 100.0;
      assert_eq!(poly_blep(p, dt), poly_blep(p, -dt), "poly_blep must depend on |dt| only");
    }
  }

  #[test]
  fn pluck_envelope_maps_settings_to_voice_fields() {
    let sr = 48000.0;
    // Disabled: sustain at/above 1, or no decay time.
    assert_eq!(pluck_envelope(1.0, 0.5, 1.0, sr), (1.0, 1.0));
    assert_eq!(pluck_envelope(0.35, 0.0, 1.0, sr), (1.0, 1.0));
    // Enabled: sustain scales the peak; the retention is just under 1.
    let (sus, keep) = pluck_envelope(0.35, 0.5, 1.0, sr);
    assert!((sus - 0.35).abs() < 1e-6);
    assert!(keep < 1.0 && keep > 0.9999, "per-sample retention ~ 1-1/(tau*sr): {keep}");
    // A zero sustain is floored above zero so a held voice never reads as released.
    let (sus0, _) = pluck_envelope(0.0, 0.5, 1.0, sr);
    assert!(sus0 > 0.0);
  }

  #[test]
  fn pluck_strikes_ring_out_then_settle_at_the_sustain() {
    // A plucked voice: fast attack to the peak, exponential decay toward 0.35, and
    // it neither dies out while held nor stays at the peak. Use a short 50 ms time
    // constant so one second of audio spans many taus.
    let sr = 48000.0;
    let (sustain_env, decay_per_sample) = pluck_envelope(0.35, 0.05, 1.0, sr);
    let v = VoiceState {
      id: 0, freq: 220.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 0.0,
      target_env: 1.0,
      ramp_per_sample: 1.0 / (0.003 * sr),
      sustain_env, decay_per_sample,
      timbre: Timbre { waveform: Waveform::Sine, ..Timbre::default() },
      am_phase: 0.0, fm_phase: 0.0,
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; sr as usize];
    render_block(&mut voices, &mut data, 1, sr);
    let peak = |range: std::ops::Range<usize>| {
      data[range].iter().fold(0.0_f32, |a, &x| a.max(x.abs()))
    };
    let early = peak(0..4800); // the strike (attack + first tau)
    let late = peak(43_200..48_000); // long settled (> 15 taus)
    let full = AMPLITUDE * waveform_norm(Waveform::Sine);
    assert!(early > 0.9 * full, "the strike reaches the full peak: {early} vs {full}");
    let settled = late / early;
    assert!((settled - 0.35).abs() < 0.05, "settles at the sustain fraction: {settled}");
    // Still held: the voice survives, ringing at the sustain.
    let state = voices.values().next().expect("held voice survives");
    assert!((state.env - sustain_env).abs() < 1e-3, "env parked at sustain: {}", state.env);
    // Release from the sustain still dies to zero and drops the voice.
    {
      let v = voices.values_mut().next().unwrap();
      v.target_env = 0.0;
      v.ramp_per_sample = v.env / (0.05 * sr);
    }
    let mut tail = vec![0.0_f32; 4800];
    render_block(&mut voices, &mut tail, 1, sr);
    assert!(voices.is_empty(), "released voice is removed");
  }

  #[test]
  fn a_flat_voice_above_a_positive_target_still_ramps_down_linearly() {
    // The chord-accretion transform: a full-env voice re-targeted to the (lower)
    // accretion level with NO pluck decay must ramp down the linear way, not stall.
    let sr = 48000.0;
    let v = VoiceState {
      id: 0, freq: 220.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0,
      target_env: 0.5,
      ramp_per_sample: 0.5 / (0.05 * sr),
      sustain_env: 0.5, decay_per_sample: 1.0,
      timbre: Timbre::default(), am_phase: 0.0, fm_phase: 0.0,
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Accreted { chord: 0, pitch: 10 }, v);
    let mut data = vec![0.0_f32; 9600]; // 200 ms >> the 50 ms ramp
    render_block(&mut voices, &mut data, 1, sr);
    let state = voices.values().next().expect("voice survives");
    assert_eq!(state.env, 0.5, "ramped down to the accretion level");
  }

  #[test]
  fn the_tempo_pulse_is_a_descending_saw_that_restarts_each_cycle() {
    // A 10 Hz pulse at 48 kHz: within one 4800-sample cycle the peak amplitude
    // decays monotonically, then jumps back up at the cycle boundary. Compare the
    // loudest sample of the first vs last tenth of a cycle, and across the wrap.
    let sr = 48000.0;
    let v = VoiceState {
      id: 0, freq: 1000.0, freq_target: 0.0, glide_per_sample: 1.0,
      tempo_am_freq: 10.0, tempo_am_phase: 0.0,
      phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0,
      sustain_env: 1.0, decay_per_sample: 1.0,
      timbre: Timbre { waveform: Waveform::Sine, ..Timbre::default() },
      am_phase: 0.0, fm_phase: 0.0,
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; 9600]; // two pulse cycles
    render_block(&mut voices, &mut data, 1, sr);
    let peak = |range: std::ops::Range<usize>| {
      data[range].iter().fold(0.0_f32, |a, &x| a.max(x.abs()))
    };
    let start = peak(0..480);
    let end = peak(4320..4800);
    let restart = peak(4800..5280);
    assert!(start > 0.9 * AMPLITUDE * waveform_norm(Waveform::Sine), "starts at the peak: {start}");
    assert!(end < 0.2 * start, "decays toward silence by cycle end: {end} vs {start}");
    assert!(restart > 0.8 * start, "the saw restarts at the next cycle: {restart}");
    // tempo_am_freq = 0 leaves the note un-pulsed (the plain render).
    let flat = VoiceState { tempo_am_freq: 0.0, ..v };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, flat);
    let mut plain = vec![0.0_f32; 4800];
    render_block(&mut voices, &mut plain, 1, sr);
    let late = plain[4320..4800].iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    assert!(late > 0.9 * start, "no pulse: the tail is as loud as the start");
  }

  #[test]
  fn glide_walks_freq_to_the_target_and_stops() {
    // A voice gliding 220 -> 440 over 100 ms: mid-glide the freq sits strictly
    // between the endpoints, after the duration it is exactly the target, and the
    // glide flag has cleared (so the voice never overshoots or re-glides).
    let sr = 48000.0;
    let dur = 0.1_f32;
    let glide = (440.0_f32 / 220.0).powf(1.0 / (dur * sr));
    let v = VoiceState {
      id: 0, freq: 220.0, freq_target: 440.0, glide_per_sample: glide, tempo_am_freq: 0.0, tempo_am_phase: 0.0,
      phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0,
      sustain_env: 1.0, decay_per_sample: 1.0,
      timbre: Timbre::default(), am_phase: 0.0, fm_phase: 0.0,
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut half = vec![0.0_f32; (dur * sr / 2.0) as usize];
    render_block(&mut voices, &mut half, 1, sr);
    let mid = voices.values().next().unwrap().freq;
    assert!(mid > 250.0 && mid < 420.0, "mid-glide freq between the endpoints: {mid}");
    let mut rest = vec![0.0_f32; (dur * sr) as usize];
    render_block(&mut voices, &mut rest, 1, sr);
    let state = voices.values().next().unwrap();
    assert_eq!(state.freq, 440.0, "landed exactly on the target");
    assert_eq!(state.glide_per_sample, 1.0, "the glide ended");
    // Downward glides clamp too.
    let glide_down = (110.0_f32 / 440.0).powf(1.0 / (dur * sr));
    voices.values_mut().next().map(|v| {
      v.freq_target = 110.0;
      v.glide_per_sample = glide_down;
    });
    let mut down = vec![0.0_f32; (2.0 * dur * sr) as usize];
    render_block(&mut voices, &mut down, 1, sr);
    assert_eq!(voices.values().next().unwrap().freq, 110.0);
  }

  #[test]
  fn distort_is_a_bounded_odd_soft_clipper_with_unity_slope_at_zero() {
    let d = Distortion { scale: 1.0, shape: 2.0 };
    // Odd: no DC asymmetry.
    for y in [0.1_f32, 0.5, 1.0, 3.0] {
      assert!((distort(-y, d) + distort(y, d)).abs() < 1e-6, "odd at {y}");
    }
    // Unity slope through the origin: tiny signals pass ~unchanged (clean when quiet).
    let tiny = 1e-3_f32;
    assert!((distort(tiny, d) / tiny - 1.0).abs() < 1e-3, "transparent near zero");
    // Bounded by the scale asymptote, approached monotonically.
    let mut prev = 0.0;
    for i in 1..200 {
      let y = i as f32 * 0.1;
      let out = distort(y, d);
      assert!(out < d.scale, "never exceeds the asymptote: f({y}) = {out}");
      assert!(out > prev, "monotone at {y}");
      prev = out;
    }
    assert!(prev > 0.95 * d.scale, "plateaus near the asymptote: {prev}");
    // k = 2 has the closed form y/sqrt(1 + y^2) at s = 1.
    let y = 1.3_f32;
    assert!((distort(y, d) - y / (1.0 + y * y).sqrt()).abs() < 1e-5);
    // A smaller scale bites harder at the same level.
    let heavy = Distortion { scale: 0.3, shape: 2.0 };
    assert!(distort(1.0, heavy) < distort(1.0, d), "smaller scale = heavier");
  }

  #[test]
  fn render_distortion_compresses_the_summed_mix_and_off_is_identical() {
    // Two loud voices sum past the elbow: with distortion the output RMS drops; with
    // `None` the render is bit-identical to the plain `render` path.
    let sr = 48000.0;
    let mk_voices = || {
      let mut voices: VoiceMap = HashMap::new();
      for (i, freq) in [220.0_f32, 330.0].iter().enumerate() {
        voices.insert(VoiceSource::Fingered { xy: (i as i32, 0) }, VoiceState {
          id: i as u64, freq: *freq, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
          timbre: Timbre { waveform: Waveform::Sine, ..Timbre::default() },
          am_phase: 0.0, fm_phase: 0.0,
        });
      }
      voices
    };
    let render = |distortion: Option<Distortion>| {
      let mut voices = mk_voices();
      let mut data = vec![0.0_f32; 4800];
      BlockRenderer::new(1).render_with_distortion(
        &mut voices, &mut data, 1, sr, 0.8, AmShapeFamily::default(), distortion,
      );
      data
    };
    let clean = render(None);
    let heavy = render(Some(Distortion { scale: 0.3, shape: 2.0 }));
    let rms = |d: &[f32]| (d.iter().map(|x| x * x).sum::<f32>() / d.len() as f32).sqrt();
    assert!(rms(&heavy) < rms(&clean) * 0.7, "distortion compresses: clean={} heavy={}", rms(&clean), rms(&heavy));
    assert!(heavy.iter().all(|s| s.abs() < 0.3 + 1e-4), "bounded by the scale asymptote");
    let mut plain_voices = mk_voices();
    let mut plain = vec![0.0_f32; 4800];
    BlockRenderer::new(1).render(&mut plain_voices, &mut plain, 1, sr, 0.8, AmShapeFamily::default());
    assert_eq!(clean, plain, "distortion None is bit-identical to render()");
  }

  #[test]
  fn fm_changes_the_rendered_samples() {
    let sr = 48000.0;
    let render = |depth_cents: f32| {
      let mut voices: VoiceMap = HashMap::new();
      voices.insert(VoiceSource::Fingered { xy: (0, 0) }, VoiceState {
        id: 0, freq: 300.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
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
