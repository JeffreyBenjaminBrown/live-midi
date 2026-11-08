//! The surfaces synth's cpal output stream: one shared voice map summed by the
//! block renderer, feeding both grids' voices at once (each voice carries its own
//! grid's waveform). A `cpal_strings` sink swaps the sawwave engine's renderer for
//! the strings engine's inside the same callback. Opening the device (the JACK
//! host, the F32 format) is `crate::audio_output`, shared with the editor.
//! Launch under `pw-jack`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::audio_output::{open_output, OutputStream};

use super::reload::LiveParams;
use super::Live;
use crate::expression_pedals::NUM_PEDALS;
use crate::strings::pedal_targets::effective_params_and_energy_position;
use crate::strings::{StringsParams, StringsRenderer};
use crate::types::{AmShapeFamily, VoiceMap, VoiceSource};
use crate::voices::{BlockRenderer, DistortionStage};

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
pub(crate) fn start(
  voices: Arc<Mutex<VoiceMap>>,
  requested_sample_rate: u32,
  requested_buffer_frames: u32,
  oversample: usize,
  am_shape_family: AmShapeFamily,
  live: Arc<Live>,
  distortion_on: Arc<Vec<AtomicBool>>,
  pedal_positions: Arc<[AtomicU32; NUM_PEDALS]>,
  strings: Option<StringsParams>,
) -> Result<Audio, Box<dyn std::error::Error>> {
  // The decimation filter is stateful across callbacks, so the renderer lives outside
  // the closure (moved in, mutated each call). `flags` is the per-grid distortion
  // snapshot, reused across callbacks to keep allocation out of the audio path.
  let output: OutputStream = open_output(requested_sample_rate, requested_buffer_frames,
    move |sample_rate, channels| {
      let mut renderer = BlockRenderer::new(oversample);
      let mut flags = vec![false; distortion_on.len()];
      let mut strings_renderer: Option<StringsRenderer> = // owned the same way, if the sink is strings
        strings.map(|params| StringsRenderer::new(params, sample_rate));
      move |data: &mut [f32]| {
        if let Some(strings_renderer) = strings_renderer.as_mut() {
          // The strings engine. Its parameters, the master amplitude and the
          // pedal-slide smoother are hot-reloadable, so they are read fresh each
          // callback; a reload to a rig without strings parameters (another sink
          // kind, which needs a restart) keeps the ones running. The distortion
          // toggles are ignored: the strings engine has no distortion bus. Every pedal's
          // raw position is read once per callback (NaN until it reports); the live
          // pedal targets turn them into the effective parameters, and the energy
          // pedal's slot is the energy's position.
          let p: LiveParams = // a copy, so the lock is held only for it
            *live.params.lock().unwrap_or_else(|e| e.into_inner());
          strings_renderer.slide_slew_secs = p.slide_pedal_smoother_secs;
          let strings_params: StringsParams = p.strings.unwrap_or(*strings_renderer.params());
          let positions: [f32; NUM_PEDALS] = read_pedal_positions(&pedal_positions);
          let (effective_params, energy_position): (StringsParams, f32) =
            effective_params_and_energy_position(
              &strings_params, &p.pedal_targets, p.string_energy_pedal, &positions);
          let mut voices: MutexGuard<VoiceMap> = voices.lock().unwrap_or_else(|e| e.into_inner());
          strings_renderer.render_block(
            &mut voices, data, channels, sample_rate, p.amplitude, &effective_params,
            energy_position);
          return; }
        // The master amplitude + distortion curve are hot-reloadable ('r'), and the
        // per-grid distortion toggles are live -- read everything fresh each callback.
        // The pedal-slide smoother is live too (`[slide].pedal_smoother_ms`); a cheap
        // field write keeps 'r' able to retune the glide feel.
        let (amplitude, curve) = {
          let p = live.params.lock().unwrap_or_else(|e| e.into_inner());
          renderer.slide_slew_secs = p.slide_pedal_smoother_secs;
          (p.amplitude, p.distortion)
        };
        // The makeup table matches that curve; cloning the Arc is one atomic increment.
        // Its cost was paid on the reload thread (see `live_makeup`).
        let makeup = Arc::clone(&live.makeup.lock().unwrap_or_else(|e| e.into_inner()));
        for (f, b) in flags.iter_mut().zip(distortion_on.iter()) {
          *f = b.load(Ordering::Relaxed);
        }
        let distortion = flags
          .iter()
          .any(|f| *f)
          .then(|| DistortionStage { curve, makeup: &makeup });
        // Recover from poisoning so a panicked grid thread can't permanently kill audio.
        let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
        // The AM family shapes any `[[timbres]]` tremolo; inert while depths are 0.
        // Distortion is per-monome: a voice routes to the dirty (distorted) bus iff
        // its grid's toggle is on -- fingered voices and sustained (accrete) drones
        // both follow their own grid's toggle; retired release tails (no grid worth
        // routing specially) stay on the clean bus, as they always have.
        renderer.render_with_distortion(
          &mut voices,
          data,
          channels,
          sample_rate,
          amplitude,
          am_shape_family,
          distortion,
          |src| {
            let grid = match src {
              VoiceSource::SurfaceFinger { grid, .. } => *grid,
              VoiceSource::SurfaceDrone { grid, .. } => *grid,
              _ => return false,
            };
            flags.get(grid).copied().unwrap_or(false)
          },
        );
      } })?;
  let sample_rate: f32 = output.sample_rate;
  Ok(Audio { _stream: Some(output.stream), sample_rate })
}

/// Every pedal's position as the pedal thread last published it, NaN for a pedal that
/// has never reported.
fn read_pedal_positions(
  pedal_positions: &[AtomicU32; NUM_PEDALS],
) -> [f32; NUM_PEDALS] {
  pedal_positions.each_ref().map(|slot| f32::from_bits(slot.load(Ordering::Relaxed))) }
