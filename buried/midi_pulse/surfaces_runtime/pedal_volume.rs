//! The EX-P expression pedals' volume taper (`PedalVolumeCurve`) and the thread
//! that polls the MPC-20 bridge and aims each mapped grid's gain at it
//! (`expression_pedal_loop`).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use midi_pulse::expression_pedals::NUM_PEDALS;

use crate::types::VoiceMap;

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
pub(super) fn expression_pedal_loop(
  live: Arc<Live>,
  voices: Arc<Mutex<VoiceMap>>,
  pedal_gains: Arc<Mutex<Vec<f32>>>,
) {
  use midi_pulse::expression_pedals::{PedalReader, DEFAULT_PORT};
  let reader = match PedalReader::connect(DEFAULT_PORT) {
    Ok(r) => r,
    Err(e) => {
      eprintln!("\x1b[1;31mexpression pedals skipped: {e}\x1b[0m");
      return;
    }
  };
  println!("expression pedals: connected to {:?}", reader.port_name());
  let mut last = [f32::NAN; NUM_PEDALS];
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
      // No CC yet = no position to trust; keep unity rather than muting the grid.
      // A reload re-applies a known position through the (possibly new) taper.
      if p.updates == 0 || (!reloaded && last[pedal] == p.norm) {
        continue;
      }
      last[pedal] = p.norm;
      let gain = curve.gain(p.norm);
      if let Some(g) = pedal_gains.lock().unwrap_or_else(|e| e.into_inner()).get_mut(grid) {
        *g = gain;
      }
      synth::set_grid_pedal_gain(&voices, grid, gain);
    }
    thread::sleep(Duration::from_millis(10));
  }
}
