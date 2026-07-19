//! The pedal hooks: the legacy feet-accrete mirror (`feet_accrete_hook`) and the
//! rig-declared pedal bindings (`rig_pedal_actions` / `rig_pedal_hook`), plus the
//! dispatchers shared with the on-grid buttons -- accrete (`drive_accrete`),
//! editmode (`editmode_press`), and the factored pulse (`factored_pulse_press`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::rig::{
  AccreteControlKind, EditmodeControlKind, PulseFactorRig, Rig, SoftstepWindowRig,
};

use crate::drumkit_runtime;
use crate::types::VoiceMap;

use super::accrete;
use super::keys::{capture_grid_held_into, erase_grid_held_into, held_pitches};
use super::polyrhythm::{PolyrhythmState, TempoFactorButton};
use super::ring::{GridRing, Reason};
use super::synth;

/// Which accrete button a KMSS pedal mirrors, and for which pedal TRIPLE (0 =
/// pedals 1/2/3, 1 = pedals 8/9/0). Jeff's mapping (misc.org "feet accrete" + "two
/// monome-specific accrete banks"): the older (monobright) grid's bank -> 1/2/3,
/// the other grid's -> 8/9/0, each triple in the on-grid order clear /
/// needs-holding / accrete.
pub(super) fn feet_accrete_button(pedal: u8) -> Option<(usize, AccreteControlKind)> {
  match pedal {
    1 => Some((0, AccreteControlKind::Clear)),
    2 => Some((0, AccreteControlKind::NeedsHolding)),
    3 => Some((0, AccreteControlKind::Accrete)),
    8 => Some((1, AccreteControlKind::Clear)),
    9 => Some((1, AccreteControlKind::NeedsHolding)),
    0 => Some((1, AccreteControlKind::Accrete)),
    _ => None,
  }
}

/// Build the drumkit pedal hook that mirrors the accrete trios onto the KMSS
/// (TODO/misc.org "feet accrete"). `triple_banks[t]` is the grid whose bank pedal
/// triple `t` drives (0 = pedals 1/2/3 = the older grid, 1 = pedals 8/9/0); a
/// triple mirrors only while ITS grid's feet-accrete toggle is on, so the softstep
/// can accrete for one monome, both, or neither. Consuming an event suppresses
/// that pedal's sample; a pedal whose toggle is off drums as usual. A pedal
/// "press" is the decoder's Fire (down) and its Release (up), so holding pedal 3
/// or 0 is exactly holding that bank's accrete button.
/// `softstep_id` pins the mirror to ONE board. The two triples (1/2/3 and 8/9/0)
/// distinguish the two GRIDS, not the two boards -- so with a second SoftStep
/// connected its pedal 3 would otherwise mirror this board's pedal 3, both boards
/// driving one bank. The rig-declared bindings supersede this hook; it stays for the
/// existing single-board drums rig.
#[allow(clippy::too_many_arguments)]
pub(super) fn feet_accrete_hook(
  softstep_id: String,
  feet_accrete_on: Arc<Vec<AtomicBool>>,
  triple_banks: [usize; 2],
  ring: Arc<Mutex<Vec<GridRing>>>,
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) -> drumkit_runtime::PedalHook {
  Arc::new(move |device, pedal, down| {
    if device != softstep_id {
      return false;
    }
    let Some((triple, button)) = feet_accrete_button(pedal) else {
      return false;
    };
    let grid = triple_banks[triple];
    if !feet_accrete_on.get(grid).map(|b| b.load(Ordering::Relaxed)).unwrap_or(false) {
      return false;
    }
    // The trio only; erase has no pedal here (feet_accrete_button never yields it).
    if button == AccreteControlKind::Erase {
      return false;
    }
    drive_accrete(grid, button, down, &ring, &held_all, &voices, release_secs, sample_rate)
  })
}

/// Apply one accrete-button edge to one grid's bank, from a foot or a finger.
///
/// Shared by the legacy feet-accrete mirror and the rig-declared `accrete_control`
/// pedals so the two cannot drift. Decides under the accrete lock and touches voices
/// only after it drops -- the module's no-nested-locks rule, same as the on-grid
/// buttons. Returns whether the press was consumed.
#[allow(clippy::too_many_arguments)]
pub(super) fn drive_accrete(
  grid: usize,
  button: AccreteControlKind,
  down: bool,
  ring: &Arc<Mutex<Vec<GridRing>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) -> bool {
  let mut activated = accrete::Activated::default();
  // A sustain clear removes only the SUSTAIN reason (symmetric with editmode clear,
  // queue.org "accrete-editmode pedals like accrete-sustain"): a drone whose pitch is
  // in edit mode keeps ringing (audibly -- not the old silent "dancing ghost"), a
  // fingered pitch's finger is untouched, and only the reason-less drones end. The one
  // `remove_reason` call replaces the old flush + edited-keep-set dance. The finger
  // multiset is snapshotted before the ring lock (no nested locks).
  let held_for_clear = if button == AccreteControlKind::Clear && down {
    held_pitches(held_all, grid)
  } else {
    Vec::new()
  };
  let ended: Vec<i32> = {
    let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
    let Some(gr) = rings.get_mut(grid) else {
      return false;
    };
    match (button, down) {
      (AccreteControlKind::Clear, true) => {
        gr.accrete.press_clear();
        let all: Vec<i32> = gr.store.iter(Reason::Sustain).collect();
        gr.store.remove_reason(Reason::Sustain, all, |p| {
          held_for_clear.iter().filter(|&&h| h == p).count()
        })
      }
      (AccreteControlKind::Clear, false) => {
        gr.accrete.release_clear();
        Vec::new()
      }
      (AccreteControlKind::NeedsHolding, true) => {
        activated = gr.accrete.press_needs_holding();
        Vec::new()
      }
      (AccreteControlKind::NeedsHolding, false) => Vec::new(),
      (AccreteControlKind::Accrete, true) => {
        activated.accrete = gr.accrete.press_accrete();
        Vec::new()
      }
      (AccreteControlKind::Accrete, false) => {
        gr.accrete.release_accrete();
        Vec::new()
      }
      (AccreteControlKind::Erase, true) => {
        activated.erase = gr.accrete.press_erase();
        Vec::new()
      }
      (AccreteControlKind::Erase, false) => {
        gr.accrete.release_erase();
        Vec::new()
      }
    }
  };
  if !ended.is_empty() {
    synth::end_drones_at(voices, grid, &ended.into_iter().collect(), release_secs, sample_rate);
  }
  if activated.accrete {
    capture_grid_held_into(held_all, ring, grid);
  }
  if activated.erase {
    // A needs-holding flip can activate a physically-held ERASE button.
    erase_grid_held_into(held_all, ring, grid);
  }
  true
}

/// What a rig-declared pedal does. Resolved once at bring-up from
/// `[[softstep_windows]]`, keyed by (softstep id, printed label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PedalAction {
  Accrete { grid: usize, control: AccreteControlKind },
  Tap,
  FactoredPulse { grid: usize, factor: TempoFactorButton },
  Editmode { grid: usize, control: EditmodeControlKind },
}

/// Build the (device, pedal) -> action map from the rig. `grid_of` maps a monome id
/// to its play-grid index; a pedal naming a monome that is not a play grid (or is
/// absent) is dropped, so the rig loads around missing gear like everything else.
pub(super) fn rig_pedal_actions(
  rig: &Rig,
  grid_of: impl Fn(&str) -> Option<usize>,
) -> HashMap<(String, u8), PedalAction> {
  let mut map = HashMap::new();
  for window in &rig.softstep_windows {
    let action = match window {
      SoftstepWindowRig::AccreteControl { pedal, monome, control, .. } => {
        grid_of(monome).map(|grid| (*pedal, PedalAction::Accrete { grid, control: *control }))
      }
      SoftstepWindowRig::TapTempoPedal { pedal, .. } => Some((*pedal, PedalAction::Tap)),
      SoftstepWindowRig::PulseFactorPedal { pedal, monome, factor, .. } => {
        grid_of(monome).map(|grid| {
          let factor = match factor {
            PulseFactorRig::Double => TempoFactorButton::Times2,
            PulseFactorRig::Triple => TempoFactorButton::Times3,
            PulseFactorRig::Half => TempoFactorButton::Div2,
            PulseFactorRig::Third => TempoFactorButton::Div3,
            PulseFactorRig::Unity => TempoFactorButton::Unity,
          };
          (*pedal, PedalAction::FactoredPulse { grid, factor })
        })
      }
      SoftstepWindowRig::EditmodeControl { pedal, monome, control, .. } => {
        grid_of(monome).map(|grid| (*pedal, PedalAction::Editmode { grid, control: *control }))
      }
      SoftstepWindowRig::Drumkit { .. } => None,
    };
    if let Some((pedal, action)) = action {
      map.insert((window.softstep().to_string(), pedal), action);
    }
  }
  map
}

/// The rate multiplier a tempo-factor button applies to an edit-mode note. `None`
/// for `Unity`: `=1` is a switch, not a multiplier, and is never an edit control.
pub(super) fn tempo_factor_ratio(factor: TempoFactorButton) -> Option<f32> {
  match factor {
    TempoFactorButton::Times2 => Some(2.0),
    TempoFactorButton::Times3 => Some(3.0),
    TempoFactorButton::Div2 => Some(0.5),
    TempoFactorButton::Div3 => Some(1.0 / 3.0),
    TempoFactorButton::Unity => None,
  }
}

/// The editmode `clear` control on `grid`, from EITHER surface -- a softstep pedal
/// or the grid's own button run exactly this. Every pitch leaves edit mode; each
/// voice then rings on iff it still has another reason -- a finger, or the sustain
/// bank -- and an edit-only drone ends by the ordinary release ramp. Symmetric
/// with the sustain accrete's clear: each clear removes only its OWN reason, so a
/// note both edited and sustained survives either clear alone and dies to both
/// (the full-kill combo is the two clears together). One lock at a time, per the
/// module's rule.
pub(super) fn editmode_clear(
  grid: usize,
  ring: &Arc<Mutex<Vec<GridRing>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) {
  // Remove the EDIT reason from every edited pitch and end exactly the drones with no
  // remaining reason -- a sustained drone keeps ringing (its bank membership is
  // untouched), and a fingered voice was never a drone. One `remove_reason` call.
  let ended: Vec<i32> = {
    let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
    let Some(gr) = rings.get_mut(grid) else { return };
    let edited: Vec<i32> = gr.store.iter(Reason::Edit).collect();
    gr.store.remove_reason(Reason::Edit, edited, |_| 0)
  };
  synth::end_drones_at(voices, grid, &ended.into_iter().collect(), release_secs, sample_rate);
}

/// The editmode `accrete` control on `grid`: a ONE-SHOT that puts every voice
/// currently sounding on this grid -- every fingered voice and every sustained
/// voice -- into edit mode. One-shot rather than a hold, unlike the sustain
/// accrete: the moment anything is edited the grid becomes a pitch-picker (every
/// press drags instead of playing), so a "capture notes played while held" phase
/// cannot exist for edit mode. Nothing needs doing at the voice level: fingered
/// voices keep their fingers, sustained voices keep their drones, and being
/// edited is simply one more reason to ring.
pub(super) fn editmode_accrete(
  grid: usize,
  ring: &Arc<Mutex<Vec<GridRing>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
) {
  let fingered = held_pitches(held_all, grid);
  let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
  let Some(gr) = rings.get_mut(grid) else { return };
  let sustained: Vec<i32> = gr.store.iter(Reason::Sustain).collect();
  for pitch in fingered.into_iter().chain(sustained) {
    gr.edit.enter(pitch, &mut gr.store);
  }
}

/// Dispatch one editmode_control press (shared by the pedal hook and the on-grid
/// buttons, so hands and feet cannot diverge). Key-down only; key-up is the
/// caller's LED business.
pub(super) fn editmode_press(
  grid: usize,
  control: EditmodeControlKind,
  ring: &Arc<Mutex<Vec<GridRing>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) {
  match control {
    EditmodeControlKind::Clear => {
      editmode_clear(grid, ring, voices, release_secs, sample_rate);
    }
    EditmodeControlKind::Accrete => editmode_accrete(grid, ring, held_all),
  }
}

/// A tempo-factor press on `grid`, from EITHER surface -- a softstep pedal or the
/// grid's own upper-right pad run exactly this, so hands and feet cannot diverge.
/// What a multiplier acts on depends on the grid's edit state (1_vision "per-voice
/// slow AM edit"): with notes in edit mode it retunes THOSE NOTES, leaving the
/// grid's tempo factor alone; with none it moves the tempo factor.
///
/// =1 is never an edit control -- Jeff: "the only edit controls are (*) and (/),
/// not (=)" -- so it always goes to the tempo-factor/switch dance. But an on/off
/// transition of that switch (queue item "=1 should affect the voices in edit
/// mode") DOES reach this grid's edit-mode voices, distinctly from a multiplier's
/// retune: it SETS their factored-pulse rate to this grid's applied tempo (cycling
/// turned ON) or to 0 (turned OFF), via `synth::set_factored_pulse_rate_at`. The
/// lone unity-snap (`PolyrhythmState::press` returns `None`) does not -- see the
/// polyrhythm module docs.
pub(super) fn factored_pulse_press(
  grid: usize,
  factor: TempoFactorButton,
  poly: &Arc<Mutex<PolyrhythmState>>,
  ring: &Arc<Mutex<Vec<GridRing>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
) {
  let edited: HashSet<i32> = ring
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .get(grid)
    .map(|gr| gr.store.iter(Reason::Edit).collect())
    .unwrap_or_default();
  match tempo_factor_ratio(factor) {
    Some(ratio) if !edited.is_empty() => {
      // Multiply, don't set: "slower ones continue to be slower than faster
      // ones". Applies to ALL edited notes at once -- the deliberate
      // asymmetry against a pitch drag, which moves only the nearest (2d).
      // `held` is needed because a fingered voice is keyed by its CELL, not
      // its pitch; only a drone is keyed by pitch.
      let held: HashMap<(i32, i32), i32> = held_all
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(grid)
        .cloned()
        .unwrap_or_default();
      synth::scale_factored_pulse_rate(voices, grid, ratio, &edited, &held);
    }
    _ => {
      // Snapshot the switch transition under the polyrhythm lock (computing the
      // target Hz there too, while `grid`'s state is still in hand), then drop
      // it before touching voices -- the module's no-nested-locks rule.
      let hz: Option<f32> = {
        let mut p = poly.lock().unwrap_or_else(|e| e.into_inner());
        p.press(grid, factor, Instant::now())
          .map(|turned_on| if turned_on { p.applied_hz(grid).unwrap_or(0.0) } else { 0.0 })
      };
      // `hz` is `Some` only for =1's own on/off transitions (every factor
      // button, and the lone unity-snap, report `None` -- see
      // `PolyrhythmState::press`), so a multiplier press never lands here.
      if let Some(hz) = hz {
        if !edited.is_empty() {
          let held: HashMap<(i32, i32), i32> = held_all
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(grid)
            .cloned()
            .unwrap_or_default();
          synth::set_factored_pulse_rate_at(voices, grid, &edited, &held, hz);
        }
      }
    }
  }
}

/// The hook for rig-declared pedal bindings: sustain, tap tempo, and the factored pulse.
///
/// Unconditional, unlike the feet-accrete mirror -- no on-grid toggle gates it. Keyed
/// by (device, pedal) because the printed labels repeat across boards and each board
/// gives them different jobs.
#[allow(clippy::too_many_arguments)]
pub(super) fn rig_pedal_hook(
  actions: HashMap<(String, u8), PedalAction>,
  ring: Arc<Mutex<Vec<GridRing>>>,
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: Arc<Mutex<VoiceMap>>,
  poly: Arc<Mutex<PolyrhythmState>>,
  tap_window: Duration,
  release_secs: f32,
  sample_rate: f32,
) -> drumkit_runtime::PedalHook {
  Arc::new(move |device, pedal, down| {
    let Some(action) = actions.get(&(device.to_string(), pedal)) else {
      return false;
    };
    match *action {
      PedalAction::Accrete { grid, control } => {
        drive_accrete(grid, control, down, &ring, &held_all, &voices, release_secs, sample_rate)
      }
      // Tap and the tempo-factor buttons are key-down only, like their on-grid
      // twins. The press is still CONSUMED on key-up, or the pedal would drum on
      // release.
      PedalAction::Tap => {
        if down {
          poly.lock().unwrap_or_else(|e| e.into_inner()).tap(Instant::now(), tap_window);
        }
        true
      }
      PedalAction::FactoredPulse { grid, factor } => {
        if down {
          // Shared with the on-grid pad's factor cells: edit-mode retune vs
          // tempo-factor move, decided in one place (`factored_pulse_press`).
          factored_pulse_press(grid, factor, &poly, &ring, &held_all, &voices);
        }
        true
      }
      PedalAction::Editmode { grid, control } => {
        if down {
          // Shared with the on-grid editmode buttons (`editmode_press`).
          editmode_press(grid, control, &ring, &held_all, &voices, release_secs, sample_rate);
        }
        true
      }
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::types::{Timbre, VoiceSource};

  /// `factored_pulse_press` end to end: the =1 dance (queue item "=1 should
  /// affect the voices in edit mode") over a real `PolyrhythmState` + `GridRing`
  /// + voice map, wired exactly as the runtime wires them. Covers the whole
  /// three-press story (`polyrhythm.rs`'s own tests cover the switch alone):
  /// cold press turns cycling ON and starts the edit-mode drone's pulse at the
  /// grid's applied tempo; the immediate lone unity-snap leaves it alone even
  /// though it zeroes the exponents; the fast double-tap after THAT turns
  /// cycling OFF and stops the pulse. A non-edited drone never moves.
  #[test]
  fn unity_starts_and_stops_the_pulse_on_edit_mode_voices_only() {
    let poly = Arc::new(Mutex::new(PolyrhythmState::new(1)));
    poly.lock().unwrap().set_fixed_tempo(5.0, Instant::now()); // 5 Hz base, unity factor
    let ring = Arc::new(Mutex::new(vec![GridRing::new(accrete::AccreteState::new())]));
    let held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>> = Arc::new(Mutex::new(vec![HashMap::new()]));
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));

    // Pitch 10 is in edit mode; pitch 20 is not. Both ring as drones (sustained,
    // fingerless) -- the shape `set_factored_pulse_rate_at` selects by pitch.
    ring.lock().unwrap_or_else(|e| e.into_inner())[0].store.add(Reason::Edit, 10);
    let mut sink = synth::SurfaceSink::new(
      0, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5,
      Arc::new(Mutex::new(vec![1.0])), Arc::new(Mutex::new(vec![1.0])),
    );
    sink.note_on((0, 0), 10, Timbre::default(), None); // edited: struck with cycling off
    sink.sustain_note((0, 0), 10);
    sink.note_on((1, 1), 20, Timbre::default(), Some(7.0)); // not edited: its own onset rate
    sink.sustain_note((1, 1), 20);

    let edited_freq = || {
      voices.lock().unwrap()[&VoiceSource::SurfaceDrone { grid: 0, pitch: 10 }].factored_pulse_freq
    };
    let other_freq = || {
      voices.lock().unwrap()[&VoiceSource::SurfaceDrone { grid: 0, pitch: 20 }].factored_pulse_freq
    };
    assert_eq!(edited_freq(), 0.0, "struck while cycling was off: no pulse yet");

    // Press 1: cycling OFF -> ON. The edit-mode drone picks up the grid's
    // applied tempo; the non-edited drone keeps whatever it was struck with.
    factored_pulse_press(0, TempoFactorButton::Unity, &poly, &ring, &held_all, &voices);
    let hz = poly.lock().unwrap().applied_hz(0).expect("the runtime always seeds a base tempo");
    assert!((edited_freq() - hz).abs() < 1e-4, "the edit-mode voice starts pulsing at =1's applied tempo");
    assert_eq!(other_freq(), 7.0, "a non-edited voice keeps its onset rate");

    // Press 2, immediately after: the lone unity-snap (cycling already on, no
    // double-tap partner yet). It zeroes this grid's exponents -- a no-op here,
    // since there were none -- but it is NOT an on/off transition, so it must
    // not touch the edit-mode voice.
    factored_pulse_press(0, TempoFactorButton::Unity, &poly, &ring, &held_all, &voices);
    assert!((edited_freq() - hz).abs() < 1e-4, "the unity-snap does not re-rate edit-mode voices");

    // Press 3, fast on that one's heels: both presses landed while cycling was
    // on, so the double-tap fires and cycling turns OFF. The edit-mode voice's
    // pulse stops; the non-edited voice is still untouched.
    factored_pulse_press(0, TempoFactorButton::Unity, &poly, &ring, &held_all, &voices);
    assert_eq!(poly.lock().unwrap().factored_pulse_hz(0), None, "cycling is off");
    assert_eq!(edited_freq(), 0.0, "=1's fast double-tap stops the edit-mode voice's pulse");
    assert_eq!(other_freq(), 7.0, "still untouched throughout");
  }

  /// A multiplier press still only ever retunes (never sets/starts/stops) an
  /// edit-mode voice, and never touches a non-edited one -- the =1 change must
  /// not have blurred this line.
  #[test]
  fn a_multiplier_press_still_only_retunes_edited_voices_in_place() {
    let poly = Arc::new(Mutex::new(PolyrhythmState::new(1)));
    poly.lock().unwrap().set_fixed_tempo(1.0, Instant::now());
    let ring = Arc::new(Mutex::new(vec![GridRing::new(accrete::AccreteState::new())]));
    let held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>> = Arc::new(Mutex::new(vec![HashMap::new()]));
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    ring.lock().unwrap_or_else(|e| e.into_inner())[0].store.add(Reason::Edit, 10);
    let mut sink = synth::SurfaceSink::new(
      0, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5,
      Arc::new(Mutex::new(vec![1.0])), Arc::new(Mutex::new(vec![1.0])),
    );
    sink.note_on((0, 0), 10, Timbre::default(), None); // no pulse: x2 must not start one
    sink.sustain_note((0, 0), 10);

    factored_pulse_press(0, TempoFactorButton::Times2, &poly, &ring, &held_all, &voices);
    let freq = voices.lock().unwrap()[&VoiceSource::SurfaceDrone { grid: 0, pitch: 10 }].factored_pulse_freq;
    assert_eq!(freq, 0.0, "multiplying zero still cannot start a pulse -- unlike =1");
  }
}
