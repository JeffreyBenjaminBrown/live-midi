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
/// rigs sound identical on the default and only the other shapes move to match.
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

/// Entries in the makeup table, and the largest drive ratio `r = sigma/s` it covers.
///
/// `R_MAX` is deliberately far past musical use. `r` grows with the master `amplitude`,
/// with `sqrt(N)`, and with `1/scale`, so a heavy rig reaches `r ~ 20` easily; a table
/// that stopped at 16 would be extrapolating exactly where the distortion is heaviest.
/// Above `R_MAX` the makeup continues linearly from the last entry (`m = m_last * r/R_MAX`),
/// which is continuous and errs by at most +0.7 dB out at `r = 1000`, on the *loud* side.
///
/// It is tempting to use `m = r` up there instead -- "the output RMS plateaus at `s`, so
/// restoring `sigma = r*s` needs a gain of `r`". That is wrong for `k < 2`: the plateau is
/// approached very slowly (at `k = 0.5`, output RMS is only 0.57 `s` at `r = 16`), and the
/// resulting under-boost reaches 11 dB. The linear continuation makes no such assumption.
const MAKEUP_LEN: usize = 1024;
const MAKEUP_R_MAX: f32 = 256.0;
/// Ceiling on the automatic makeup: a numerical guard against a degenerate rig (a `scale`
/// of 1e-38 would otherwise give an infinite `r`), not a musical limit. At 60 dB it cannot
/// bind anywhere inside the table -- the largest makeup at `r = R_MAX` is 692x, at the
/// extreme `k = 0.25` -- so it only ever clamps the continuation beyond it.
/// A cap that binds silently re-introduces the very volume drop this module exists to fix;
/// the first cut capped at 8x and did exactly that at `scale = 0.1` with a full chord.
const MAKEUP_CAP: f32 = 1024.0;
/// Every normalized waveform has RMS `1/sqrt(3)` (see `waveform_norm`), so a voice
/// whose amplitude coefficient is `b` has power `b^2 / 3`.
const VOICE_POWER_PER_B2: f32 = 1.0 / 3.0;

/// The RMS gain the soft-clipper applies to a Gaussian input of std `sigma`, as a
/// function of the drive ratio `r = sigma / s`. Depends only on `(r, k)`, since the
/// curve is `f(y; s, k) = s * F(y/s; k)`.
///
/// `g(r) = RMS[F(r z)] / r` for `z ~ N(0, 1)`. Simpson over the (even) integrand on
/// `z in [0, 8]`, doubled -- exact to six decimals against a 200k-point reference and
/// against the closed form `sqrt((1 - Q(r)) / r^2)`, `Q(r) = sqrt(pi/2)/r * e^(1/2r^2) *
/// erfc(1/(r sqrt 2))`, which exists only at `k = 2`. See
/// `learnings/distortion-volume-compensation.org`.
fn gaussian_rms_gain(r: f64, k: f64) -> f64 {
  if r < 1e-6 {
    return 1.0; // unity slope at the origin
  }
  const Z_MAX: f64 = 8.0;
  const N: usize = 256; // even, for Simpson; within 1e-4 dB of a 2048-node reference
  let integrand = |z: f64| {
    let u = r * z;
    let f = u / (1.0 + u.abs().powf(k)).powf(1.0 / k);
    f * f * (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt()
  };
  let h = Z_MAX / N as f64;
  let mut sum = integrand(0.0) + integrand(Z_MAX);
  for i in 1..N {
    sum += integrand(i as f64 * h) * if i % 2 == 1 { 4.0 } else { 2.0 };
  }
  (2.0 * sum * h / 3.0).sqrt() / r
}

/// The distortion's loudness compensation: the gain that restores the dirty bus to the
/// RMS the *clean* bus would have had, so switching a grid's `distortion_toggle` on is a
/// timbre change and not a volume drop.
///
/// The clipper's loss depends on exactly one thing -- the RMS `sigma` of the summed
/// dirty bus -- so the correction is `1 / g(sigma / s)`. `sigma` is not measured from
/// the audio (a detector would lag, ripple, and pump); the renderer already knows every
/// voice's instantaneous amplitude, so `accumulate_voices` sums their powers and gets
/// `sigma` exactly, for free. `Makeup` is therefore a *pure function of the envelopes*:
/// it changes only as fast as they do, never within a waveform cycle, so it corrects
/// the level without touching the waveshaping that makes the sound.
///
/// The table holds `1 / g(r)` sampled uniformly in `sqrt(r / R_MAX)` -- warped because
/// `1/g` has a `sqrt(r)` cusp at the origin whenever `k < 1` (a uniform table there is
/// off by 0.64 dB; warped it is off by 0.0001 dB).
///
/// Building it integrates `g` 512 times: do it at rig load or hot-reload, never on the
/// audio thread. Lookups are a `sqrt`, a lerp, and a `min`.
#[derive(Clone, Debug)]
pub struct Makeup {
  trim: f32,
  auto: bool,
  /// One-pole time constant for the *applied* makeup, in seconds. `0` (the default) is
  /// the exact, instantaneous correction; see `slew_secs` on `BlockRenderer`.
  slew_secs: f32,
  /// `1/g(r)` sampled uniformly in `sqrt(r / R_MAX)`. Empty when `!auto`.
  inv_g: Vec<f32>,
}

impl Makeup {
  /// Build for elbow harshness `shape` (k). `trim` multiplies the result -- equal RMS
  /// is not equal *loudness* (a distorted signal is brighter, so it reads louder), so
  /// this is the by-ear knob; ~0.8 is a reasonable start. With `auto` false the table is
  /// skipped and `trim` acts alone, as a plain constant makeup gain.
  ///
  /// `slew_secs` lags the applied makeup behind its target. `0` is exact. Nonzero trades
  /// attack punch for the clipper's own dynamic compression -- see
  /// `learnings/distortion-volume-compensation.org`; it is a character knob, not a fix.
  ///
  /// The table is sampled uniformly in `sqrt(r)` because `1/g` has a `sqrt(r)` cusp at
  /// the origin whenever `k < 1`. Building it integrates `g` `MAKEUP_LEN` times: do it at
  /// rig load or hot-reload, never on the audio thread.
  pub fn new(shape: f32, trim: f32, auto: bool, slew_secs: f32) -> Self {
    let inv_g = if auto && shape > 0.0 {
      (0..MAKEUP_LEN)
        .map(|i| {
          let warped = i as f32 / (MAKEUP_LEN - 1) as f32;
          let r = MAKEUP_R_MAX * warped * warped;
          (1.0 / gaussian_rms_gain(r as f64, shape as f64)) as f32
        })
        .collect()
    } else {
      Vec::new()
    };
    Makeup { trim, auto, slew_secs, inv_g }
  }

  /// No compensation: the pre-2026-07 behavior (the clipper's loss stands).
  pub fn off() -> Self {
    Makeup { trim: 1.0, auto: false, slew_secs: 0.0, inv_g: Vec::new() }
  }

  pub fn slew_secs(&self) -> f32 {
    self.slew_secs
  }

  /// The gain to apply to the distorted dirty bus, given that bus's RMS `sigma` and the
  /// curve's asymptote `scale`. Monotonically increasing in `sigma` -- the clipper eats
  /// more of a louder bus, so more must be given back. (That monotonicity is why lagging
  /// this gain *reduces* attack punch rather than preserving it: at a strike the target
  /// jumps up, and a lagged makeup under-compensates exactly when the clipper bites
  /// hardest.)
  pub fn gain(&self, sigma: f32, scale: f32) -> f32 {
    if !self.auto || self.inv_g.is_empty() || scale <= 0.0 || sigma <= 0.0 {
      return self.trim;
    }
    let r = sigma / scale;
    let last = self.inv_g[MAKEUP_LEN - 1];
    let m = if r >= MAKEUP_R_MAX || !r.is_finite() {
      // Continue linearly from the last entry: `m = r / h(R_MAX)` where `h = g*r` is the
      // clipper's output RMS in units of `s`. Continuous at R_MAX, and since `h` only
      // rises from there toward 1, this errs on the loud side by at most `1/h(R_MAX)`.
      last * (r / MAKEUP_R_MAX)
    } else {
      let x = (r / MAKEUP_R_MAX).sqrt() * (MAKEUP_LEN - 1) as f32;
      let i = (x as usize).min(MAKEUP_LEN - 2);
      let frac = x - i as f32;
      self.inv_g[i] * (1.0 - frac) + self.inv_g[i + 1] * frac
    };
    // `min` propagates NaN's operand order, so guard explicitly: a NaN `r` must not
    // become a NaN gain multiplying the whole bus.
    if m.is_finite() { m.min(MAKEUP_CAP) * self.trim } else { MAKEUP_CAP * self.trim }
  }
}

/// The distortion applied to one render: the curve, plus the loudness compensation that
/// undoes its level loss. Borrowed rather than owned so the (allocated) makeup table is
/// built once per rig load and merely read by the audio callback.
#[derive(Clone, Copy)]
pub struct DistortionStage<'a> {
  pub curve: Distortion,
  pub makeup: &'a Makeup,
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
/// when oversampling) and return the summed contributions, pre-clamp, split into a
/// `(clean, dirty)` pair by the `dirty` route (the per-monome distortion: the dirty
/// bus gets distorted, the clean one does not; see `render_with_distortion`), plus
/// `dirt_pow` -- the summed *power* of the dirty voices, which is what the
/// distortion's loudness compensation needs (see `Makeup`).
/// Running the nonlinear per-voice work (osc, AM, FM, and the AM multiply) at N x
/// the rate is the whole point of oversampling: the harmonics/sidebands it
/// manufactures then land below the *high-rate* Nyquist instead of folding back
/// into the band. Dead voices are dropped here, exactly as the single-rate path did.
fn accumulate_voices<F: Fn(&VoiceSource) -> bool>(
  voices: &mut VoiceMap,
  frac: f32,
  sample_rate: f32,
  amplitude: f32,
  shape_family: AmShapeFamily,
  dirty: &F,
) -> (f32, f32, f32) {
  let mut clean = 0.0_f32;
  let mut dirt = 0.0_f32;
  let mut dirt_pow = 0.0_f32;
  voices.retain(|src, v| {
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
    // The polyrhythm pulse: a unipolar triangle in [0,1] -- 1 at the cycle start,
    // falling to 0 at the half-cycle, rising back to 1 -- at the tempo applied at
    // this note's onset. Separate from the timbre AM below. Unlike the descending
    // saw it replaces, it returns to the peak *continuously*, so the cycle boundary
    // is not a step in amplitude (a saw's 0 -> 1 wrap is a click waiting to happen).
    let pulse = if v.tempo_am_freq > 0.0 {
      v.tempo_am_phase += v.tempo_am_freq * frac / sample_rate;
      if v.tempo_am_phase >= 1.0 {
        v.tempo_am_phase -= 1.0;
      }
      (2.0 * v.tempo_am_phase - 1.0).abs()
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
    // `b` is this voice's instantaneous amplitude coefficient -- everything but the
    // oscillator. Since `osc * wf_norm` has RMS 1/sqrt(3) for *every* waveform, the
    // voice's RMS is exactly `b / sqrt(3)`, and incoherent voices (distinct EDO
    // pitches) add in power. Summing `b^2` therefore hands the distortion the dirty
    // bus's RMS for free -- no detector, no lag. `b >= 0`: every factor is.
    let b = v.env * v.timbre.gain * amm * pulse * amplitude;
    let s = osc(v.timbre.waveform, v.phase, dt) * wf_norm * b;
    if dirty(src) {
      dirt += s;
      dirt_pow += b * b;
    } else {
      clean += s;
    }
    true
  });
  (clean, dirt, dirt_pow)
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
  /// The applied distortion makeup, held across callbacks so a nonzero
  /// `Makeup::slew_secs` can lag it toward its target. Unused (and overwritten every
  /// sub-sample) when the slew is off, which is the default.
  makeup_state: f32,
}

impl BlockRenderer {
  pub fn new(oversample: usize) -> Self {
    let oversample = oversample.max(1);
    let decimator = (oversample > 1).then(|| Decimator::new(oversample));
    BlockRenderer { oversample, decimator, makeup_state: 1.0 }
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
    self.render_with_distortion(
      voices, data, channels, sample_rate, amplitude, shape_family, None, |_| false,
    );
  }

  /// `render`, with an optional distortion applied to the summed DIRTY bus (pre-
  /// clamp, at the oversampled rate when oversampling -- a nonlinearity belongs
  /// inside the anti-aliasing loop, like the clamp). `dirty` routes each voice:
  /// dirty voices are summed and distorted together, clean voices bypass and are
  /// added after -- the per-monome distortion (misc.org "distortion / This should
  /// be per-monome"). `None` is bit-identical to `render` whatever the route. The
  /// surfaces runtime passes `Some` while any grid's distortion toggle is on,
  /// routing that grid's voices dirty.
  ///
  /// The stage's `makeup` then restores the dirty bus to the RMS the clean bus would
  /// have had, from the voice envelopes the accumulator already summed -- recomputed
  /// every sub-sample, so it is continuous through note births, deaths and decays and
  /// needs no smoothing. `Makeup::off()` reproduces the uncompensated clipper exactly.
  #[allow(clippy::too_many_arguments)]
  pub fn render_with_distortion<F: Fn(&VoiceSource) -> bool>(
    &mut self,
    voices: &mut VoiceMap,
    data: &mut [f32],
    channels: usize,
    sample_rate: f32,
    amplitude: f32,
    shape_family: AmShapeFamily,
    distortion: Option<DistortionStage<'_>>,
    dirty: F,
  ) {
    let oversample = self.oversample;
    // The makeup is stepped at the *sub-sample* rate, since that is where it is applied.
    let sub_rate = sample_rate * oversample as f32;
    let slew_coeff = match distortion {
      Some(stage) if stage.makeup.slew_secs() > 0.0 && sub_rate > 0.0 => {
        1.0 - (-1.0 / (stage.makeup.slew_secs() * sub_rate)).exp()
      }
      _ => 1.0, // no slew: apply the target exactly, bit-identically
    };
    let mut makeup_state = self.makeup_state;
    let mut post = |clean: f32, dirt: f32, dirt_pow: f32| match distortion {
      Some(stage) => {
        let sigma = (dirt_pow * VOICE_POWER_PER_B2).sqrt();
        let target = stage.makeup.gain(sigma, stage.curve.scale);
        // `>= 1.0` short-circuits to the target itself rather than `state + (target -
        // state) * 1.0`, which is not bit-identical in f32.
        if slew_coeff >= 1.0 {
          makeup_state = target;
        } else {
          makeup_state += (target - makeup_state) * slew_coeff;
        }
        (clean + makeup_state * distort(dirt, stage.curve)).clamp(-0.95, 0.95)
      }
      None => (clean + dirt).clamp(-0.95, 0.95),
    };
    match &mut self.decimator {
      // Plain path: one sub-step per output sample, no filtering, no latency.
      None => {
        for frame in data.chunks_mut(channels) {
          let (clean, dirt, dirt_pow) =
            accumulate_voices(voices, 1.0, sample_rate, amplitude, shape_family, &dirty);
          let s = post(clean, dirt, dirt_pow);
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
            let (clean, dirt, dirt_pow) =
              accumulate_voices(voices, frac, sample_rate, amplitude, shape_family, &dirty);
            decim.push(post(clean, dirt, dirt_pow));
          }
          let s = decim.output();
          for out in frame.iter_mut() {
            *out = s;
          }
        }
      }
    }
    self.makeup_state = makeup_state;
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

// Spawn a fresh accretion voice for chord/pitch. It is struck exactly like a
// fingered note -- env=0 rising to the peak over `attack_secs`, then the pluck
// decay toward `sustain_level` x peak -- so an emitted chord tone sounds no
// different from the same pitch held under a finger. Overwrites any existing
// entry at Accreted{chord, pitch}.
pub fn spawn_accretion_voice(
  voices: &mut VoiceMap, chord: ChordId, pitch: i32,
  fund: f64, edo: i32, next_voice_id: &mut VoiceId, sample_rate: f32,
  attack_secs: f32, sustain_level: f32, decay_secs: f32,
) {
  let id = *next_voice_id;
  *next_voice_id += 1;
  let (sustain_env, decay_per_sample) =
    pluck_envelope(sustain_level, decay_secs, 1.0, sample_rate);
  voices.insert(VoiceSource::Accreted { chord, pitch }, VoiceState {
    id,
    freq: freq_for_pitch(pitch, fund, edo),
    freq_target: 0.0,
    glide_per_sample: 1.0,
    tempo_am_freq: 0.0, tempo_am_phase: 0.0,
    phase: 0.0,
    env: 0.0,
    target_env: 1.0,
    ramp_per_sample: 1.0 / (attack_secs * sample_rate),
    sustain_env,
    decay_per_sample,
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
  fn the_tempo_pulse_is_a_unipolar_triangle_peaking_at_each_cycle_start() {
    // A 10 Hz pulse at 48 kHz -> a 4800-sample cycle. The pulse is |2*phase - 1|:
    // 1 at the cycle start, 0 at the half-cycle (sample 2400), back to 1 at the
    // wrap. Probe the carrier's peak in a two-cycle window (96 samples of a 1 kHz
    // sine) around each phase of interest.
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
    let full = AMPLITUDE * waveform_norm(Waveform::Sine);
    let start = peak(0..96); // phase ~0
    let quarter = peak(1152..1248); // phase ~1/4, rising edge of |2p-1| going down
    let trough = peak(2352..2448); // phase ~1/2
    let three_quarter = peak(3552..3648); // phase ~3/4
    let restart = peak(4752..4848); // across the wrap, phase ~1

    assert!(start > 0.95 * full, "starts at the peak: {start} vs {full}");
    assert!(trough < 0.05 * start, "reaches zero at the half-cycle: {trough}");
    assert!(restart > 0.95 * start, "returns to the peak at the wrap: {restart}");

    // The half-way points sit at half amplitude -- and, crucially, at the SAME
    // amplitude as each other. That symmetry is what makes it a triangle: a
    // descending saw would read 0.75 here and 0.25 there.
    for (name, p) in [("quarter", quarter), ("three_quarter", three_quarter)] {
      assert!((p - 0.5 * start).abs() < 0.06 * start, "{name} sits at half: {p}");
    }
    assert!(
      (quarter - three_quarter).abs() < 0.03 * start,
      "the triangle is symmetric about the trough: {quarter} vs {three_quarter}",
    );

    // tempo_am_freq = 0 leaves the note un-pulsed (the plain render).
    let flat = VoiceState { tempo_am_freq: 0.0, ..v };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, flat);
    let mut plain = vec![0.0_f32; 4800];
    render_block(&mut voices, &mut plain, 1, sr);
    let late = plain[2352..2448].iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    assert!(late > 0.9 * start, "no pulse: the mid-cycle is as loud as the start");
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

  /// `n` sine voices at mutually incoherent frequencies, each at full envelope.
  fn incoherent_voices(n: usize) -> VoiceMap {
    let mut voices: VoiceMap = HashMap::new();
    for i in 0..n {
      // 58-EDO steps over 80 Hz, spread so no two voices share a harmonic.
      let freq = 80.0 * 2.0_f32.powf((7 * i) as f32 / 58.0);
      voices.insert(VoiceSource::Fingered { xy: (i as i32, 0) }, VoiceState {
        id: i as u64, freq, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: i as f32 * 0.137, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
        timbre: Timbre { waveform: Waveform::Sine, ..Timbre::default() },
        am_phase: 0.0, fm_phase: 0.0,
      });
    }
    voices
  }

  fn rms(d: &[f32]) -> f32 {
    (d.iter().map(|x| x * x).sum::<f32>() / d.len() as f32).sqrt()
  }

  #[test]
  fn render_distortion_compresses_the_summed_mix_and_off_is_identical() {
    // Two loud voices sum past the elbow: with distortion and NO makeup the output RMS
    // drops; with `None` the render is bit-identical to the plain `render` path.
    // (`Makeup::off()` is the pre-compensation behavior; see the makeup tests below,
    // where the same clipper preserves RMS instead.)
    let sr = 48000.0;
    let off = Makeup::off();
    let render = |distortion: Option<DistortionStage<'_>>| {
      let mut voices = incoherent_voices(2);
      let mut data = vec![0.0_f32; 4800];
      BlockRenderer::new(1).render_with_distortion(
        &mut voices, &mut data, 1, sr, 0.8, AmShapeFamily::default(), distortion, |_| true,
      );
      data
    };
    let clean = render(None);
    let heavy = render(Some(DistortionStage {
      curve: Distortion { scale: 0.3, shape: 2.0 },
      makeup: &off,
    }));
    assert!(rms(&heavy) < rms(&clean) * 0.7, "distortion compresses: clean={} heavy={}", rms(&clean), rms(&heavy));
    assert!(heavy.iter().all(|s| s.abs() < 0.3 + 1e-4), "bounded by the scale asymptote");
    let mut plain_voices = incoherent_voices(2);
    let mut plain = vec![0.0_f32; 4800];
    BlockRenderer::new(1).render(&mut plain_voices, &mut plain, 1, sr, 0.8, AmShapeFamily::default());
    assert_eq!(clean, plain, "distortion None is bit-identical to render()");
  }

  #[test]
  fn gaussian_rms_gain_matches_the_closed_form_at_k2() {
    // At k = 2 the Gaussian RMS gain has a closed form,
    //   g(r) = sqrt((1 - Q(r)) / r^2),  Q(r) = sqrt(pi/2)/r * e^(1/2r^2) * erfc(1/(r sqrt2))
    // (see learnings/distortion-volume-compensation.org). Values below are that formula,
    // cross-checked against a 400k-sample Monte Carlo.
    for (r, expect) in [
      (0.05, 0.996_289), (0.087, 0.988_994), (0.35, 0.872_052),
      (1.0, 0.586_788), (4.0, 0.215_137), (12.0, 0.079_152),
    ] {
      let got = gaussian_rms_gain(r, 2.0);
      assert!((got - expect).abs() < 1e-4, "g({r}) = {got}, want {expect}");
    }
    // Limits: unity slope at the origin, and output RMS -> s (so g -> 1/r) far past it.
    assert!((gaussian_rms_gain(1e-9, 2.0) - 1.0).abs() < 1e-6, "g -> 1 as r -> 0");
    let far = gaussian_rms_gain(64.0, 2.0);
    assert!((far * 64.0 - 1.0).abs() < 0.02, "g -> 1/r: output RMS plateaus at s");
    // Monotone decreasing in r, for the shapes we ship and the sub-1 shape Jeff plays.
    for k in [0.5, 1.0, 2.0, 4.0] {
      let mut prev = 1.1;
      for i in 1..=64 {
        let g = gaussian_rms_gain(i as f64 * 0.25, k);
        assert!(g < prev, "g monotone decreasing at k={k}");
        prev = g;
      }
    }
  }

  #[test]
  fn makeup_table_inverts_the_gain_and_off_is_unity() {
    // The table is sampled in sqrt(r), because 1/g has a sqrt cusp at the origin
    // whenever k < 1. Check the lerp against the integral it approximates, across the
    // whole shape range a rig may ask for -- including k below 1, where the cusp lives.
    for k in [0.25_f32, 0.5, 1.0, 2.0, 4.0, 8.0] {
      let m = Makeup::new(k, 1.0, true, 0.0);
      for i in 0..200 {
        // log-spaced over the table's full domain
        let r = 10.0_f32.powf(-3.0 + (MAKEUP_R_MAX.log10() + 3.0) * i as f32 / 199.0);
        let exact = 1.0 / gaussian_rms_gain(r as f64, k as f64) as f32;
        let got = m.gain(r, 1.0); // sigma = r, scale = 1
        // The cap must never bind inside the table: it exists to stop an infinity, not to
        // limit the correction. (Largest in-domain makeup is 692x, at k = 0.25, r = R_MAX.)
        assert!(exact < MAKEUP_CAP, "k={k} r={r}: the cap binds in-domain ({exact})");
        let err_db = 20.0 * (got / exact).log10();
        assert!(err_db.abs() < 0.02, "k={k} r={r}: makeup {got} vs {exact} ({err_db} dB)");
      }
      // Past the table, the linear continuation stays close and errs on the LOUD side --
      // never the silent one, which is the failure this whole module exists to prevent.
      // (Unless the 60 dB cap intervenes, which past r = 256 it eventually must; that is
      // a rig with `scale` some 300x below the bus RMS, and it is not a musical setting.)
      for r in [300.0_f32, 1000.0] {
        let exact = 1.0 / gaussian_rms_gain(r as f64, k as f64) as f32;
        let got = m.gain(r, 1.0);
        if exact >= MAKEUP_CAP {
          assert_eq!(got, MAKEUP_CAP, "k={k} r={r}: past the cap, clamp");
          continue;
        }
        assert!(got >= exact * 0.999, "k={k} r={r}: extrapolation under-boosts");
        assert!(20.0 * (got / exact).log10() < 1.0, "k={k} r={r}: extrapolation overshoots");
      }
      // Degenerate inputs cannot poison the bus with a NaN or an infinity.
      assert!(m.gain(f32::INFINITY, 1.0).is_finite(), "k={k}: infinite sigma");
      assert!(m.gain(f32::NAN, 1.0).is_finite(), "k={k}: NaN sigma");
      assert!(m.gain(1e30, 1e-30).is_finite(), "k={k}: runaway drive ratio");
    }
    // `off` is a plain unity gain; a trim with auto off is a plain constant.
    assert_eq!(Makeup::off().gain(0.3, 1.0), 1.0);
    assert_eq!(Makeup::new(2.0, 0.8, false, 0.0).gain(0.3, 1.0), 0.8);
    // The trim scales the automatic gain.
    let auto = Makeup::new(2.0, 1.0, true, 0.0);
    let trimmed = Makeup::new(2.0, 0.5, true, 0.0);
    assert!((trimmed.gain(1.0, 1.0) - 0.5 * auto.gain(1.0, 1.0)).abs() < 1e-6);
  }

  #[test]
  fn makeup_is_robust_across_the_settings_a_player_can_dial() {
    // Regression for two bugs in the first cut, both of which bit only where the
    // distortion is working hardest -- heavy `scale`, low `shape`:
    //
    //  (a) the makeup was capped at 8x, which BINDS at settings a player reaches
    //      (scale = 0.1 with 16 notes needs 10x), silently restoring the exact volume
    //      drop this module exists to remove;
    //  (b) above the table's range the makeup used `m = r`, on the reasoning that the
    //      clipper's output RMS plateaus at `scale`. It does -- but so slowly that at
    //      k = 0.5, r = 16 it has only reached 0.57 * scale, so `m = r` under-boosted
    //      by 11 dB.
    //
    // Sweep the whole plausible dial and demand the makeup is the true 1/g throughout.
    let amp = 0.15_f32;
    for &k in &[0.25_f32, 0.5, 1.0, 2.0, 4.0] {
      let m = Makeup::new(k, 1.0, true, 0.0);
      for &scale in &[2.0_f32, 1.0, 0.3, 0.1, 0.05, 0.02] {
        for n in [1_usize, 4, 16, 32] {
          let sigma = amp * (n as f32 / 3.0).sqrt();
          let exact = 1.0 / gaussian_rms_gain((sigma / scale) as f64, k as f64) as f32;
          let got = m.gain(sigma, scale);
          assert!(
            got < MAKEUP_CAP * 0.99,
            "k={k} scale={scale} n={n}: the cap binds on a playable rig (makeup {got})",
          );
          let err_db = 20.0 * (got / exact).log10();
          assert!(err_db.abs() < 0.05, "k={k} scale={scale} n={n}: {err_db:.3} dB off");
        }
      }
    }
  }

  /// One plucked voice: strike at full envelope, exponential decay toward `sustain`.
  /// 440 Hz so a 20 ms window still holds many periods.
  fn plucked_voice(sample_rate: f32, sustain: f32, decay_secs: f32) -> VoiceMap {
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, VoiceState {
      id: 0, freq: 440.0, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0,
      tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0,
      sustain_env: sustain, decay_per_sample: (-1.0 / (decay_secs * sample_rate)).exp(),
      timbre: Timbre { waveform: Waveform::Sine, ..Timbre::default() },
      am_phase: 0.0, fm_phase: 0.0,
    });
    voices
  }

  #[test]
  fn slewing_the_makeup_costs_attack_punch_rather_than_protecting_it() {
    // The tempting intuition -- "let the correction ease in, so attacks stay loud" -- is
    // backwards, and this test is here to keep it from being re-introduced.
    //
    // `Makeup::gain` is monotonically INCREASING in sigma: the clipper eats more of a
    // louder bus. So at a strike the makeup's target jumps UP, and a lagged makeup is
    // too SMALL exactly when the clipper is biting hardest -- the attack is squashed and
    // then swells as the makeup catches up. The exact (slew = 0) makeup reproduces the
    // clean envelope, which is the most attack a level-restoring gain can preserve.
    let sr = 48000.0;
    let curve = Distortion { scale: 1.0, shape: 0.5 }; // the rig Jeff plays
    let render = |slew_secs: f32, distort_it: bool| {
      let makeup = Makeup::new(curve.shape, 1.0, true, slew_secs);
      let mut voices = plucked_voice(sr, 0.35, 0.5);
      let mut data = vec![0.0_f32; 28_800]; // 0.6 s
      let stage = distort_it.then_some(DistortionStage { curve, makeup: &makeup });
      BlockRenderer::new(1).render_with_distortion(
        &mut voices, &mut data, 1, sr, 0.15, AmShapeFamily::default(), stage, |_| true,
      );
      data
    };
    // Attack contrast = strike RMS (first 20 ms) over sustain RMS (around 0.55 s).
    let contrast = |d: &[f32]| 20.0 * (rms(&d[0..960]) / rms(&d[26_400..27_360])).log10();

    let clean = contrast(&render(0.0, false));
    let exact = contrast(&render(0.0, true));
    let slewed_50 = contrast(&render(0.050, true));
    let slewed_200 = contrast(&render(0.200, true));

    // Exact makeup reproduces the clean note's own attack contrast.
    assert!(
      (exact - clean).abs() < 0.15,
      "exact makeup should preserve the clean attack contrast: clean {clean:.2} dB, exact {exact:.2} dB",
    );
    // Every slew erodes it, monotonically in the time constant.
    assert!(slewed_50 < exact - 0.5, "50 ms slew should cost attack: {slewed_50:.2} vs {exact:.2}");
    assert!(slewed_200 < slewed_50 - 0.5, "200 ms should cost more: {slewed_200:.2}");
    // ...and it settles to the same place: the slew is a transient, not a level shift.
    let tail = |d: &[f32]| rms(&d[26_400..27_360]);
    let (a, b) = (tail(&render(0.0, true)), tail(&render(0.200, true)));
    assert!(20.0 * (b / a).log10() < 0.2, "slewed makeup converges in steady state");
  }

  #[test]
  fn zero_slew_is_the_exact_makeup_and_a_huge_slew_is_a_constant() {
    // The slew knob interpolates between the two options in the writeup: 0 is the exact,
    // per-sample correction (E); as it grows the makeup freezes at one value, which is a
    // plain constant makeup gain (B). Nothing else is reachable, and 0 must be exact.
    let sr = 48000.0;
    let curve = Distortion { scale: 0.3, shape: 2.0 };
    let sigma_of = |n: usize| 0.15_f32 * (n as f32 / 3.0).sqrt();

    // slew = 0: the applied gain is exactly `Makeup::gain` at the instantaneous sigma.
    let exact = Makeup::new(curve.shape, 1.0, true, 0.0);
    let mut voices = incoherent_voices(4);
    let mut data = vec![0.0_f32; 480];
    BlockRenderer::new(1).render_with_distortion(
      &mut voices, &mut data, 1, sr, 0.15, AmShapeFamily::default(),
      Some(DistortionStage { curve, makeup: &exact }), |_| true,
    );
    // Four voices at full envelope: sigma is constant, so the first sample's output is
    // clean+0 -> makeup * distort(dirt) with the makeup read straight off the table.
    let m = exact.gain(sigma_of(4), curve.scale);
    assert!(m > 1.2, "a 4-note bus at scale 0.3 needs real makeup, got {m}");

    // A slew far longer than the render leaves the makeup pinned at its initial 1.0, so
    // the output is the uncompensated clipper -- exactly `Makeup::off()`.
    let frozen = Makeup::new(curve.shape, 1.0, true, 1e6);
    let off = Makeup::off();
    let render = |makeup: &Makeup| {
      let mut voices = incoherent_voices(4);
      let mut data = vec![0.0_f32; 480];
      BlockRenderer::new(1).render_with_distortion(
        &mut voices, &mut data, 1, sr, 0.15, AmShapeFamily::default(),
        Some(DistortionStage { curve, makeup }), |_| true,
      );
      data
    };
    let a = render(&frozen);
    let b = render(&off);
    for i in 0..a.len() {
      assert!((a[i] - b[i]).abs() < 1e-6, "a frozen makeup is a constant makeup at {i}");
    }
  }

  #[test]
  fn makeup_restores_the_clean_rms_across_note_counts() {
    // The point of the whole exercise: with auto makeup the distorted bus carries the
    // RMS the clean bus would have had, however many notes are sounding and however
    // hard the clipper is driven. Without it, the same render collapses.
    //
    // Half a second: the closest voices beat at ~7 Hz, so a shorter window's RMS still
    // carries the beat and wobbles by up to 0.5 dB about the ensemble value. At 0.5 s
    // (and this spacing) the clean bus also stays under the +-0.95 clamp at every n, so
    // the reference is the unclipped sum and the comparison measures only the model.
    let sr = 48000.0;
    let render = |n: usize, distortion: Option<DistortionStage<'_>>| {
      let mut voices = incoherent_voices(n);
      let mut data = vec![0.0_f32; 24_000];
      BlockRenderer::new(1).render_with_distortion(
        &mut voices, &mut data, 1, sr, 0.15, AmShapeFamily::default(), distortion, |_| true,
      );
      data
    };
    // `scale = 1.0, shape = 0.5` is the rig Jeff actually plays: ~5 dB of loss on a
    // single note, ~9 dB on sixteen. `shape = 2.0` is the committed default. The last
    // two are heavy enough that the first cut's 8x makeup cap bound and quietly gave the
    // volume drop back -- they are the regression.
    for (scale, shape) in
      [(1.0_f32, 0.5_f32), (1.0, 2.0), (0.3, 2.0), (0.1, 1.0), (0.1, 0.5), (0.05, 1.0)]
    {
      let curve = Distortion { scale, shape };
      let makeup = Makeup::new(shape, 1.0, true, 0.0);
      for n in [1, 2, 4, 8, 16] {
        let clean = rms(&render(n, None));
        let compensated = rms(&render(n, Some(DistortionStage { curve, makeup: &makeup })));
        let err_db = 20.0 * (compensated / clean).log10();
        // n = 1 is a lone sinusoid, not Gaussian -- it dwells near its peaks, so the
        // model over-corrects (by 0.73 dB at worst). Known, bounded, under the JND for a
        // timbre change this size, and fixable by blending in the exact sine gain if it
        // ever matters. Measured worst for n >= 2 is 0.35 dB. See the doc.
        let tol = if n == 1 { 1.0 } else { 0.5 };
        assert!(
          err_db.abs() < tol,
          "s={scale} k={shape} n={n}: compensated is {err_db:.2} dB off clean (tol {tol})",
        );
      }
    }
  }

  #[test]
  fn makeup_never_raises_the_peak_above_the_clean_peak() {
    // Restoring RMS cannot re-introduce hard clipping: the distorted waveform is
    // squarer (lower crest factor), so at equal RMS its peak is strictly lower. This is
    // why the makeup is safe to apply before the +-0.95 clamp.
    let sr = 48000.0;
    let peak = |d: &[f32]| d.iter().fold(0.0_f32, |m, x| m.max(x.abs()));
    let render = |n: usize, distortion: Option<DistortionStage<'_>>| {
      let mut voices = incoherent_voices(n);
      let mut data = vec![0.0_f32; 24_000];
      BlockRenderer::new(1).render_with_distortion(
        &mut voices, &mut data, 1, sr, 0.15, AmShapeFamily::default(), distortion, |_| true,
      );
      data
    };
    for (scale, shape) in [(1.0_f32, 0.5_f32), (0.3, 2.0), (0.1, 1.0)] {
      let curve = Distortion { scale, shape };
      let makeup = Makeup::new(shape, 1.0, true, 0.0);
      for n in [2, 4, 8, 16] {
        let clean = peak(&render(n, None));
        assert!(clean < 0.95, "the clean reference must not be clamped (n={n}): {clean}");
        let compensated = peak(&render(n, Some(DistortionStage { curve, makeup: &makeup })));
        assert!(
          compensated <= clean + 1e-6,
          "s={scale} k={shape} n={n}: compensated peak {compensated} > clean peak {clean}",
        );
      }
    }
  }

  #[test]
  fn makeup_is_continuous_through_note_birth_and_death() {
    // Why a per-sample makeup cannot click, stated three ways.
    let amp = 0.15_f32;
    let sigma = |envs: &[f32]| -> f32 {
      (envs.iter().map(|e| (e * amp) * (e * amp)).sum::<f32>() * VOICE_POWER_PER_B2).sqrt()
    };

    for k in [0.5_f32, 2.0] {
      let m = Makeup::new(k, 1.0, true, 0.0);

      // (1) A voice enters and leaves the bank at exactly zero power, because its
      // envelope ramps from 0 and it is dropped only once it reaches 0 again. So
      // dropping a dead voice is bit-for-bit a no-op: there is nothing to click.
      assert_eq!(
        m.gain(sigma(&[1.0, 1.0, 0.0]), 1.0),
        m.gain(sigma(&[1.0, 1.0]), 1.0),
        "k={k}: a zero-envelope voice contributes no power",
      );

      // (2) The invariant that makes the whole scheme safe: the *compensated* bus's RMS
      // is `m(sigma) * g(sigma/s) * sigma`, and that is `sigma` identically -- the clean
      // bus's own envelope. So the output envelope is the clean envelope, and it is
      // continuous exactly where the clean one is.
      for i in 1..=200 {
        let sg = i as f32 * 0.005;
        let out_rms = m.gain(sg, 1.0) * gaussian_rms_gain(sg as f64, k as f64) as f32 * sg;
        assert!((out_rms - sg).abs() < 2e-3 * sg, "k={k}: compensated RMS != clean RMS at {sg}");
      }

      // (3) Over the bank a sounding instrument actually occupies, the gain itself moves
      // slowly: a third voice born over the 3 ms attack (the fastest envelope we ship)
      // shifts it by well under 0.1% per sample.
      let attack = 144; // 0.003 s * 48 kHz
      let mut prev = m.gain(sigma(&[1.0, 1.0]), 1.0);
      let mut worst = 0.0_f32;
      for i in 0..=attack {
        let g = m.gain(sigma(&[1.0, 1.0, i as f32 / attack as f32]), 1.0);
        worst = worst.max((g - prev).abs() / g);
        prev = g;
      }
      assert!(worst < 1e-3, "k={k}: makeup moves {worst} per sample during an attack");
    }

    // The one place `m` alone is not smooth: `1/g` has a `sqrt(sigma)` cusp at silence
    // when k < 1, so a note struck into an *empty* bank swings the gain hard over its
    // first samples. It is inaudible, and (2) is why: `m` is only ever multiplied by a
    // signal whose own RMS is `g * sigma`, and g's cusp cancels m's exactly. Asserting
    // the cusp is real keeps the reason for (2) from being forgotten.
    let soft = Makeup::new(0.5, 1.0, true, 0.0);
    let hard = Makeup::new(2.0, 1.0, true, 0.0);
    let step = |m: &Makeup| (m.gain(0.001, 1.0) - m.gain(0.0, 1.0)).abs();
    assert!(step(&soft) > 1e-2, "k < 1 has a cusp in the makeup at sigma = 0");
    assert!(step(&hard) < 1e-3, "k >= 1 does not");
  }

  #[test]
  fn per_voice_routing_distorts_only_the_dirty_bus() {
    // Voice A routes dirty, voice B clean: the output equals distort(A) + B exactly
    // -- B bypasses the clipper entirely (the per-monome distortion contract).
    let sr = 48000.0;
    // Routing is about which bus a voice lands on, so hold the makeup at unity here;
    // `makeup_restores_the_clean_rms_across_note_counts` covers the compensation.
    let off = Makeup::off();
    let heavy = DistortionStage { curve: Distortion { scale: 0.3, shape: 2.0 }, makeup: &off };
    let mk = |xy: (i32, i32), freq: f32| {
      (VoiceSource::Fingered { xy }, VoiceState {
        id: 0, freq, freq_target: 0.0, glide_per_sample: 1.0, tempo_am_freq: 0.0, tempo_am_phase: 0.0, phase: 0.0, env: 1.0, target_env: 1.0, ramp_per_sample: 0.0, sustain_env: 1.0, decay_per_sample: 1.0,
        timbre: Timbre { waveform: Waveform::Sine, ..Timbre::default() },
        am_phase: 0.0, fm_phase: 0.0,
      })
    };
    let render = |sources: &[(VoiceSource, VoiceState)], distortion, dirty: fn(&VoiceSource) -> bool| {
      let mut voices: VoiceMap = sources.iter().cloned().collect();
      let mut data = vec![0.0_f32; 480];
      BlockRenderer::new(1).render_with_distortion(
        &mut voices, &mut data, 1, sr, 0.4, AmShapeFamily::default(), distortion, dirty,
      );
      data
    };
    let a = mk((0, 0), 220.0);
    let b = mk((1, 0), 330.0);
    let routed = render(&[a.clone(), b.clone()], Some(heavy), |src| {
      matches!(src, VoiceSource::Fingered { xy: (0, 0) })
    });
    let a_dirty_alone = render(&[a], Some(heavy), |_| true);
    let b_clean_alone = render(&[b.clone()], Some(heavy), |_| false);
    let b_plain = render(&[b], None, |_| false);
    assert_eq!(b_clean_alone, b_plain, "a clean-routed voice bypasses the clipper exactly");
    for i in 0..routed.len() {
      let expect = a_dirty_alone[i] + b_clean_alone[i];
      assert!(
        (routed[i] - expect).abs() < 1e-6,
        "sample {i}: routed={} vs distort(A)+B={}",
        routed[i],
        expect,
      );
    }
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
