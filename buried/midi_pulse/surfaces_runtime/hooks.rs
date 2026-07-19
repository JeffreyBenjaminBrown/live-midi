//! The pedal hooks: the legacy feet-accrete mirror (`feet_accrete_hook`) and the
//! rig-declared pedal bindings (`rig_pedal_actions` / `rig_pedal_hook`), plus the
//! dispatchers shared with the on-grid buttons -- accrete (`drive_accrete`),
//! editmode (`editmode_press`), and the factored pulse (`factored_pulse_press`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use midi_pulse::rig::{
  AccreteControlKind, EditmodeControlKind, PulseFactorRig, Rig, SoftstepWindowRig,
};

use crate::drumkit_runtime;
use crate::types::VoiceMap;

use super::accrete::{self, AccreteState};
use super::edit;
use super::keys::{capture_grid_held_into, erase_grid_held_into, held_pitches};
use super::polyrhythm::{PolyrhythmState, TempoFactorButton};
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
  accrete: Arc<Mutex<Vec<AccreteState>>>,
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: Arc<Mutex<VoiceMap>>,
  edit: Arc<Mutex<Vec<edit::EditState>>>,
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
    drive_accrete(
      grid, button, down, &accrete, &held_all, &voices, &edit, release_secs, sample_rate,
    )
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
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  release_secs: f32,
  sample_rate: f32,
) -> bool {
  let mut activated = accrete::Activated::default();
  {
    let mut banks = accrete.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = banks.get_mut(grid) else {
      return false;
    };
    match (button, down) {
      (AccreteControlKind::Clear, true) => state.press_clear(),
      (AccreteControlKind::Clear, false) => state.release_clear(),
      (AccreteControlKind::NeedsHolding, true) => activated = state.press_needs_holding(),
      (AccreteControlKind::NeedsHolding, false) => {}
      (AccreteControlKind::Accrete, true) => activated.accrete = state.press_accrete(),
      (AccreteControlKind::Accrete, false) => state.release_accrete(),
      (AccreteControlKind::Erase, true) => activated.erase = state.press_erase(),
      (AccreteControlKind::Erase, false) => state.release_erase(),
    }
  }
  if button == AccreteControlKind::Clear && down {
    // Clear removes only the SUSTAIN reason (symmetric with editmode clear,
    // queue.org "accrete-editmode pedals like accrete-sustain"): a drone whose
    // pitch is in edit mode keeps ringing. This supersedes the old panic-button
    // coupling that also emptied edit mode -- that fix existed because clear used
    // to silence edited drones too, stranding SILENT notes in edit mode ("a
    // dancing ghost that won't go away"). Spared drones stay audible, so they are
    // not ghosts, and both the exit gesture and the editmode clear can still
    // dismiss them; the full kill is the two clears together.
    let edited: HashSet<i32> = edit
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(grid)
      .map(|e| e.pitches().collect())
      .unwrap_or_default();
    synth::release_sustained_voices(voices, grid, &edited, release_secs, sample_rate);
  }
  if activated.accrete {
    capture_grid_held_into(held_all, accrete, edit, grid);
  }
  if activated.erase {
    // A needs-holding flip can activate a physically-held ERASE button.
    erase_grid_held_into(held_all, accrete, grid);
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
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) {
  let edited: HashSet<i32> = {
    let mut states = edit.lock().unwrap_or_else(|e| e.into_inner());
    let Some(e) = states.get_mut(grid) else { return };
    let pitches: HashSet<i32> = e.pitches().collect();
    e.clear();
    pitches
  };
  if edited.is_empty() {
    return;
  }
  let sustained: HashSet<i32> = {
    let banks = accrete.lock().unwrap_or_else(|e| e.into_inner());
    banks.get(grid).map(|b| b.sustained_pitches().collect()).unwrap_or_default()
  };
  // Only the drones whose SOLE reason was edit mode end; a sustained drone keeps
  // ringing (its bank membership is untouched), and a fingered voice was never a
  // drone at all.
  let dying: HashSet<i32> = edited.difference(&sustained).copied().collect();
  synth::end_drones_at(voices, grid, &dying, release_secs, sample_rate);
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
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
) {
  let fingered = held_pitches(held_all, grid);
  let sustained: Vec<i32> = {
    let banks = accrete.lock().unwrap_or_else(|e| e.into_inner());
    banks.get(grid).map(|b| b.sustained_pitches().collect()).unwrap_or_default()
  };
  let mut states = edit.lock().unwrap_or_else(|e| e.into_inner());
  let Some(e) = states.get_mut(grid) else { return };
  for pitch in fingered.into_iter().chain(sustained) {
    e.enter(pitch);
  }
}

/// Dispatch one editmode_control press (shared by the pedal hook and the on-grid
/// buttons, so hands and feet cannot diverge). Key-down only; key-up is the
/// caller's LED business.
#[allow(clippy::too_many_arguments)]
pub(super) fn editmode_press(
  grid: usize,
  control: EditmodeControlKind,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  accrete: &Arc<Mutex<Vec<AccreteState>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
  release_secs: f32,
  sample_rate: f32,
) {
  match control {
    EditmodeControlKind::Clear => {
      editmode_clear(grid, edit, accrete, voices, release_secs, sample_rate);
    }
    EditmodeControlKind::Accrete => editmode_accrete(grid, edit, accrete, held_all),
  }
}

/// A tempo-factor press on `grid`, from EITHER surface -- a softstep pedal or the
/// grid's own upper-right pad run exactly this, so hands and feet cannot diverge.
/// What a multiplier acts on depends on the grid's edit state (1_vision "per-voice
/// slow AM edit"): with notes in edit mode it retunes THOSE NOTES, leaving the
/// grid's tempo factor alone; with none it moves the tempo factor.
///
/// =1 is never an edit control -- Jeff: "the only edit controls are (*) and (/),
/// not (=)" -- so it always goes to the tempo-factor/switch dance.
pub(super) fn factored_pulse_press(
  grid: usize,
  factor: TempoFactorButton,
  poly: &Arc<Mutex<PolyrhythmState>>,
  edit: &Arc<Mutex<Vec<edit::EditState>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: &Arc<Mutex<VoiceMap>>,
) {
  let edited: HashSet<i32> = edit
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .get(grid)
    .map(|e| e.pitches().collect())
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
      poly.lock().unwrap_or_else(|e| e.into_inner()).press(grid, factor, Instant::now());
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
  accrete: Arc<Mutex<Vec<AccreteState>>>,
  held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  voices: Arc<Mutex<VoiceMap>>,
  poly: Arc<Mutex<PolyrhythmState>>,
  edit: Arc<Mutex<Vec<edit::EditState>>>,
  tap_window: Duration,
  release_secs: f32,
  sample_rate: f32,
) -> drumkit_runtime::PedalHook {
  Arc::new(move |device, pedal, down| {
    let Some(action) = actions.get(&(device.to_string(), pedal)) else {
      return false;
    };
    match *action {
      PedalAction::Accrete { grid, control } => drive_accrete(
        grid, control, down, &accrete, &held_all, &voices, &edit, release_secs, sample_rate,
      ),
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
          factored_pulse_press(grid, factor, &poly, &edit, &held_all, &voices);
        }
        true
      }
      PedalAction::Editmode { grid, control } => {
        if down {
          // Shared with the on-grid editmode buttons (`editmode_press`).
          editmode_press(
            grid, control, &edit, &accrete, &held_all, &voices, release_secs, sample_rate,
          );
        }
        true
      }
    }
  })
}
