//! The pedal hooks: the rig-declared pedal bindings (`rig_pedal_actions` /
//! `rig_pedal_hook`), plus the dispatchers shared with the on-grid buttons -- accrete
//! (`drive_accrete`), editmode (`editmode_press`), and the factored pulse
//! (`factored_pulse_press`). The old gated `softstep_accretes_toggle` mirror (a hook
//! keyed on an on-grid toggle, hardcoding pedals 1/2/3 + 8/9/0) was retired 2026-07 --
//! see TODO/cleaning/2_plan.org; rig-declared `accrete_control` pedals replace it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::rig::{
  AccreteControlKind, EditmodeControlKind, PulseFactorRig, Rig, SoftstepWindowRig,
};

use crate::drumkit_runtime;
use crate::types::{VoiceMap, VoiceSource};

use super::accrete;
use super::keys::{capture_grid_held_into, erase_grid_held_into, held_pitches};
use super::polyrhythm::{PolyrhythmState, TempoFactorButton};
use super::ring::{GridRing, Reason};
use super::synth;

/// Apply one accrete-button edge to one grid's bank, from a foot or a finger.
///
/// Shared by every rig-declared `accrete_control` pedal (and the on-grid buttons, via
/// their own call sites) so they cannot drift. Decides under the accrete lock and
/// touches voices only after it drops -- the module's no-nested-locks rule, same as
/// the on-grid buttons. Returns whether the press was consumed.
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
  // A sustain clear removes the SUSTAIN reason from every sustained pitch (branch-3 queue
  // item 4). Sustain is the only life-support reason now, so this ends every drone a
  // finger is not holding -- INCLUDING an edited-and-sustained one, which it also
  // deselects (`remove_sustain` cascades edit membership away: the invariant edited ⊆
  // sustained, and nothing silent may stay selected). This supersedes the old symmetric
  // model where a clear spared edited drones. The finger multiset is snapshotted before
  // the ring lock (no nested locks).
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
        gr.store.remove_sustain(all, |p| held_for_clear.iter().filter(|&&h| h == p).count())
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

/// The editmode `clear` control on `grid`, from EITHER surface -- a softstep pedal or
/// the grid's own button run exactly this. Pure DESELECTION (branch-3 queue item 4):
/// every pitch leaves edit mode, but each is still sustained (edited ⊆ sustained), so
/// this ENDS NO VOICE. It is the many-note twin of the exit gesture, and like it, it
/// silences nothing.
///
/// This supersedes the old symmetric-clears model, where the editmode clear ended an
/// edit-only drone and the "full kill" was both clears together. Now the sustain clear
/// alone ends (and deselects) an edited note, and the editmode clear only deselects.
pub(super) fn editmode_clear(grid: usize, ring: &Arc<Mutex<Vec<GridRing>>>) {
  let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
  let Some(gr) = rings.get_mut(grid) else { return };
  gr.edit.clear(&mut gr.store);
  // The chord layer's half of the selection: unflag every live chord voice. Pure
  // deselection there too -- a chord voice keeps ringing on its chord reason.
  for v in gr.chord.live.values_mut() {
    v.edited = false;
  }
}

/// The editmode `accrete` control on `grid`: a ONE-SHOT that puts every voice currently
/// sounding on this grid -- every fingered voice and every sustained voice -- into edit
/// mode. One-shot rather than a hold, unlike the sustain accrete: the moment anything is
/// edited the grid becomes a pitch-picker (every press drags instead of playing), so a
/// "capture notes played while held" phase cannot exist for edit mode.
///
/// `edit.enter` also sustains each pitch (edited ⊆ sustained), so a fingered-only voice
/// becomes sustained too and drones after its finger lifts. Nothing needs doing at the
/// voice level: fingered voices keep their fingers, sustained voices keep their drones.
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
  // "Apply edit mode to all" says ALL: every live chord voice joins the selection
  // too -- by its own flag, gaining no sustain reason (chord-storage-v2).
  for v in gr.chord.live.values_mut() {
    v.edited = true;
  }
}

/// Dispatch one editmode_control press (shared by the pedal hook and the on-grid
/// buttons, so hands and feet cannot diverge). Key-down only; key-up is the caller's LED
/// business. Neither branch ends a voice now (branch-3 queue item 4: clear is pure
/// deselection, accrete only adds membership), so no voices / release params are needed.
pub(super) fn editmode_press(
  grid: usize,
  control: EditmodeControlKind,
  ring: &Arc<Mutex<Vec<GridRing>>>,
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
) {
  match control {
    EditmodeControlKind::Clear => editmode_clear(grid, ring),
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
  // The edit selection's two halves, snapshotted under one ring lock: the piano
  // layer's edited pitches, and the edit-flagged chord voices as their VOICE KEYS
  // (a chord voice is selected per voice, not per pitch -- two voices at one pitch
  // may differ in flag).
  let (edited, chord_keys): (HashSet<i32>, HashSet<VoiceSource>) = {
    let rings = ring.lock().unwrap_or_else(|e| e.into_inner());
    match rings.get(grid) {
      Some(gr) => (
        gr.store.iter(Reason::Edit).collect(),
        gr.chord
          .live
          .iter()
          .filter(|(_, v)| v.edited)
          .map(|(seq, _)| VoiceSource::SurfaceChord { grid, seq: *seq })
          .collect(),
      ),
      None => Default::default(),
    }
  };
  match tempo_factor_ratio(factor) {
    Some(ratio) if !edited.is_empty() || !chord_keys.is_empty() => {
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
      synth::scale_factored_pulse_rate(voices, grid, ratio, &edited, &held, &chord_keys);
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
        if !edited.is_empty() || !chord_keys.is_empty() {
          let held: HashMap<(i32, i32), i32> = held_all
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(grid)
            .cloned()
            .unwrap_or_default();
          synth::set_factored_pulse_rate_at(voices, grid, &edited, &held, &chord_keys, hz);
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
          editmode_press(grid, control, &ring, &held_all);
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
    // fingerless) -- the shape `set_factored_pulse_rate_at` selects by pitch. Editing 10
    // implies sustaining it (edited ⊆ sustained), so add both reasons.
    {
      let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
      rings[0].store.add(Reason::Sustain, 10);
      rings[0].store.add(Reason::Edit, 10);
    }
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

  /// The chord layer joins the selection (chord-storage-v2): an edit-flagged chord
  /// voice follows the multipliers and =1 exactly as a piano-layer edited note does,
  /// an unflagged one never moves, and the editmode accrete/clear controls flip the
  /// flags in bulk.
  #[test]
  fn the_factored_pulse_controls_reach_edit_flagged_chord_voices() {
    use crate::surfaces_runtime::chords::{StoredChord, StoredVoice};
    let poly = Arc::new(Mutex::new(PolyrhythmState::new(1)));
    poly.lock().unwrap().set_fixed_tempo(1.0, Instant::now());
    let ring = Arc::new(Mutex::new(vec![GridRing::new(accrete::AccreteState::new())]));
    let held_all: Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>> =
      Arc::new(Mutex::new(vec![HashMap::new()]));
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let mut sink = synth::SurfaceSink::new(
      0, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5,
      Arc::new(Mutex::new(vec![1.0])), Arc::new(Mutex::new(vec![1.0])),
    );
    // Two pulsed chord voices from one slot; flag only the first.
    let sv = StoredVoice {
      pitch: 20, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
      osc_phase: 0.0, pulse_factor: 3.0, pulse_phase: 0.0,
    };
    let spawned = {
      let mut rings = ring.lock().unwrap();
      rings[0].chord.save(0, StoredChord { voices: vec![sv, sv] });
      rings[0].chord.begin_recall(0)
    };
    for (seq, v) in &spawned {
      sink.spawn_chord_voice(*seq, v, 1.0); // 3 Hz each
    }
    let (flag_seq, other_seq) = (spawned[0].0, spawned[1].0);
    ring.lock().unwrap()[0].chord.live.get_mut(&flag_seq).unwrap().edited = true;
    let freq_of = |seq: u64| {
      voices.lock().unwrap()[&VoiceSource::SurfaceChord { grid: 0, seq }].factored_pulse_freq
    };

    // x2 doubles the flagged chord voice only.
    factored_pulse_press(0, TempoFactorButton::Times2, &poly, &ring, &held_all, &voices);
    assert_eq!(freq_of(flag_seq), 6.0, "the edit-flagged chord voice doubles");
    assert_eq!(freq_of(other_seq), 3.0, "the unflagged one is untouched");

    // The editmode CLEAR unflags it: a further multiplier no longer reaches it.
    editmode_clear(0, &ring);
    assert!(!ring.lock().unwrap()[0].chord.live[&flag_seq].edited);
    factored_pulse_press(0, TempoFactorButton::Times2, &poly, &ring, &held_all, &voices);
    assert_eq!(freq_of(flag_seq), 6.0, "unflagged now: the multiplier passes it by");

    // The editmode ACCRETE flags every live chord voice ("apply edit mode to all").
    editmode_accrete(0, &ring, &held_all);
    assert!(ring.lock().unwrap()[0].chord.live.values().all(|v| v.edited));
    factored_pulse_press(0, TempoFactorButton::Times3, &poly, &ring, &held_all, &voices);
    assert_eq!(freq_of(flag_seq), 18.0, "both voices now follow");
    assert_eq!(freq_of(other_seq), 9.0);
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
    {
      // Edited implies sustained (edited ⊆ sustained): add both reasons.
      let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
      rings[0].store.add(Reason::Sustain, 10);
      rings[0].store.add(Reason::Edit, 10);
    }
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
