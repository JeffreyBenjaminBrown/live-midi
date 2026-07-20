//! The EX-P expression pedals' volume taper (`PedalVolumeCurve`) and the thread
//! that polls the MPC-20 bridge and aims each mapped grid's gain at it
//! (`expression_pedal_loop`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::expression_pedals::NUM_PEDALS;

use crate::types::VoiceMap;

use super::pedal_slide::{PedalSlideState, VolumeReturn};
use super::synth;
use super::{Live, STOP};

/// One volume pedal's taper: the standard fader law, exponential with a linear
/// splice at the heel (rig `curve_initial_lin_frac` / `curve_remainder_exp_db`).
///
/// Loudness is roughly logarithmic in amplitude, so the perceptually even taper is
/// dB-LINEAR in travel -- an exponential. Its one flaw is that it never reaches 0,
/// which the splice fixes: over the first `lin_frac` of the travel the gain fades
/// linearly from exact silence up to the exponential's floor (`-exp_db` dB), and
/// from there the exponential covers `exp_db` dB up to unity at full toe. The two
/// halves meet, so the curve is continuous, monotonic, and pinned at 0 and 1.
/// (Tried before this: linear -- numb at the toe, a cliff at the heel -- then
/// quadratic, better but still not spread right. This is the console fader's law.)
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PedalVolumeCurve {
  pub(super) lin_frac: f32,
  pub(super) exp_db: f32,
}

impl PedalVolumeCurve {
  pub(super) fn gain(&self, norm: f32) -> f32 {
    let x = norm.clamp(0.0, 1.0);
    if x <= 0.0 {
      return 0.0;
    }
    // The exponential's floor: the gain where the splice hands over.
    let floor = 10.0_f32.powf(-self.exp_db / 20.0);
    if x < self.lin_frac {
      return floor * (x / self.lin_frac);
    }
    10.0_f32.powf(-self.exp_db * (1.0 - x) / ((1.0 - self.lin_frac) * 20.0))
  }
}

/// The EX-P volume-pedal thread: poll the MPC-20 bridge reader (~100 Hz) and aim
/// each mapped grid at its pedal's position. The pedal's reliable ~1..119 CC
/// travel normalizes to a position 0..1 (`expression_pedals::normalize`), which
/// the pedal's own `PedalVolumeCurve` tapers into the amplitude factor
/// (exponential with a linear splice at the heel; full heel = silent, full toe =
/// unity). The per-sample slew that keeps a
/// sweep from zippering lives in the engine (`voices::GAIN_SLEW_SECS`) -- this
/// thread only moves TARGETS: the shared per-grid gain (for future note-ons) and
/// every sounding voice's `grid_gain_target`. A pedal that has never moved
/// contributes unity, so an unplugged pedal cannot mute its grid. A missing
/// bridge is a red report, not an error: the instrument plays on without pedals,
/// like any other absent gear.
///
/// The bindings and tapers come from `Live`, re-read every poll, so the curve_*
/// knobs are hot-reloadable ('r'): a reload bumps the generation, and every pedal
/// that has ever reported a position is re-applied through its new taper
/// immediately -- a curve tweak is audible without moving the pedal. (A rig that
/// STARTS with no pedals never spawns this thread; adding the first pedal needs a
/// restart, like any other bound resource.)
/// Apply one pedal reading to its grid: the shared per-grid gain (future note-ons)
/// and every sounding voice's target -- CHORD voices included, since the pedal is
/// UNIFORM (queues/branch-2.org "the uniform pedal", superseding the round-2
/// edit-only pickup). A recalled chord still starts at its SAVED loudness and
/// keeps it until the pedal next moves (the EX-P only sends CCs under a foot);
/// the engine's per-sample slew smooths every adoption.
pub(super) fn apply_pedal_gain(
  voices: &Arc<Mutex<VoiceMap>>,
  pedal_gains: &Arc<Mutex<Vec<f32>>>,
  grid: usize,
  gain: f32,
) {
  if let Some(g) = pedal_gains.lock().unwrap_or_else(|e| e.into_inner()).get_mut(grid) {
    *g = gain;
  }
  synth::set_grid_pedal_gain(voices, grid, gain);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn expression_pedal_loop(
  live: Arc<Live>,
  voices: Arc<Mutex<VoiceMap>>,
  pedal_gains: Arc<Mutex<Vec<f32>>>,
  pedal_slide_on: Arc<Vec<AtomicBool>>,
  pedal_slide: Arc<Vec<Mutex<PedalSlideState>>>,
  fund: f64,
  edo: i32,
) {
  use crate::expression_pedals::{PedalReader, DEFAULT_PORT};
  let reader = match PedalReader::connect(DEFAULT_PORT) {
    Ok(r) => r,
    Err(e) => {
      eprintln!("\x1b[1;31mexpression pedals skipped: {e}\x1b[0m");
      return;
    }
  };
  println!("expression pedals: connected to {:?}", reader.port_name());
  let mut last = [f32::NAN; NUM_PEDALS];
  // Per-pedal slide bookkeeping: whether the grid was in slide mode last poll (to catch
  // the ON->OFF edge that starts the volume return), and the anchored-kink volume
  // return itself (`None` = the ordinary taper is in charge).
  let mut mode_was = [false; NUM_PEDALS];
  let mut vol_return: [Option<VolumeReturn>; NUM_PEDALS] = [None; NUM_PEDALS];
  let mut last_generation = live.generation.load(Ordering::SeqCst);
  while !STOP.load(Ordering::SeqCst) {
    let generation = live.generation.load(Ordering::SeqCst);
    let reloaded = generation != last_generation;
    last_generation = generation;
    let map = live.params.lock().unwrap_or_else(|e| e.into_inner()).expression_pedals;
    let pedals = reader.pedals();
    for (pedal, binding) in map.iter().enumerate() {
      let Some((grid, curve)) = *binding else { continue };
      let p = pedals[pedal];
      let mode = pedal_slide_on.get(grid).map(|b| b.load(Ordering::Relaxed)).unwrap_or(false);
      // The ON->OFF edge: begin the anchored-kink volume return from the grid's frozen
      // volume, so the pedal returning to volume duty changes the loudness immediately
      // in either direction rather than yanking it to its physical position or going
      // dead until it crosses (2_discussion amendment).
      if mode_was[pedal] && !mode {
        let frozen = pedal_slide
          .get(grid)
          .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).frozen_volume())
          .unwrap_or(1.0);
        vol_return[pedal] = Some(VolumeReturn::new(p.norm, frozen));
        last[pedal] = f32::NAN; // force the next ordinary reading to apply
      }
      mode_was[pedal] = mode;

      if mode {
        // Slide duty: the pedal drives this grid's slide engine, not its volume. Feed
        // the normalized position in and apply the resulting per-voice drives.
        if p.updates == 0 {
          continue;
        }
        let drives = pedal_slide.get(grid).map(|s| {
          let mut st = s.lock().unwrap_or_else(|e| e.into_inner());
          // (`on_pedal` also queues midpoint-flip re-files; the grid thread owns the
          // ring/sink a re-file touches, so it drains them during its repaint -- the
          // pedal thread only drives the voices.)
          st.on_pedal(p.norm);
          st.drives()
        });
        if let Some(drives) = drives {
          synth::apply_slide_drives(&voices, &drives, fund, edo);
        }
        continue;
      }

      // Volume duty. While the anchored-kink return is active, its map is in charge
      // (immediate in both directions) until the pedal reaches an endpoint, when it
      // reverts to the ordinary taper.
      if let Some(vr) = vol_return[pedal].as_mut() {
        if p.updates == 0 {
          continue;
        }
        match vr.on_pedal(p.norm) {
          Some(gain) => {
            apply_pedal_gain(&voices, &pedal_gains, grid, gain);
            continue;
          }
          None => {
            // Reverted at an endpoint: fall through to the ordinary taper this poll.
            vol_return[pedal] = None;
          }
        }
      }
      // No CC yet = no position to trust; keep unity rather than muting.
      if p.updates == 0 || (!reloaded && last[pedal] == p.norm) {
        continue;
      }
      last[pedal] = p.norm;
      apply_pedal_gain(&voices, &pedal_gains, grid, curve.gain(p.norm));
    }
    thread::sleep(Duration::from_millis(10));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::surfaces_runtime::accrete::AccreteState;
  use crate::surfaces_runtime::chords::{StoredChord, StoredVoice};
  use crate::surfaces_runtime::ring::GridRing;
  use crate::types::{Timbre, VoiceSource};
  use std::collections::HashMap;

  /// The UNIFORM pedal (queues/branch-2.org): one pedal reading re-aims every
  /// voice on the grid, chord voices included -- edited or not. A recalled chord
  /// spawns at its SAVED pedal component (asserted here as the pre-CC state) and
  /// adopts the live pedal on the first reading after recall.
  #[test]
  fn a_pedal_reading_reaches_every_chord_voice_uniformly() {
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let ring = Arc::new(Mutex::new(vec![GridRing::new(AccreteState::new())]));
    let pedal_gains = Arc::new(Mutex::new(vec![1.0_f32]));
    let mut sink = synth::SurfaceSink::new(
      0, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5,
      Arc::clone(&pedal_gains), Arc::new(Mutex::new(vec![1.0])),
    );
    sink.note_on((0, 0), 30, Timbre::default(), None);
    let sv = StoredVoice {
      pitch: 20, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 0.6,
      osc_phase: 0.0, pulse_factor: 0.0, pulse_phase: 0.0,
    };
    let spawned = {
      let mut rings = ring.lock().unwrap();
      rings[0].chord.save(0, StoredChord { voices: vec![sv, sv] });
      rings[0].chord.begin_recall(0)
    };
    for (seq, v) in &spawned {
      sink.spawn_chord_voice(*seq, v, 1.0);
    }
    {
      let v = voices.lock().unwrap();
      assert_eq!(
        v[&VoiceSource::SurfaceChord { grid: 0, seq: spawned[0].0 }].grid_gain_target, 0.6,
        "until the pedal moves, a recalled voice keeps its SAVED loudness",
      );
    }

    apply_pedal_gain(&voices, &pedal_gains, 0, 0.25);

    assert_eq!(pedal_gains.lock().unwrap()[0], 0.25, "future note-ons see the pedal");
    let v = voices.lock().unwrap();
    let target = |src: VoiceSource| v[&src].grid_gain_target;
    assert_eq!(target(VoiceSource::SurfaceFinger { grid: 0, cell: (0, 0) }), 0.25, "piano layer follows");
    for (seq, _) in &spawned {
      assert_eq!(
        target(VoiceSource::SurfaceChord { grid: 0, seq: *seq }), 0.25,
        "every chord voice follows too -- the pedal is origin-blind",
      );
    }
  }
}
