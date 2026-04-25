//! AppState lifecycle and the event handlers that drive it.
//!
//! Per the design dialog, "methods" are free functions taking
//! &mut AppState; the only impl block is AppState::new (kept as a
//! constructor so "go to definition" jumps to the type).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::consts::{
  ACCRETION_TARGET, ATTACK_SECS, CELL_ACCRETE_ON, CELL_EMIT_IS_TOGGLE,
  CELL_SET_ACCRETION_TARGET, CELL_WIPE, INITIALLY_ACCRETING_CHORD,
  N_CHORDS, RELEASE_SECS, SILENCE_CHORD,
};
use crate::leds::{add_reason, remove_reason};
use crate::pitch::{cells_for_pitch, cells_for_pitch_of, freq_for_pitch};
use crate::types::{
  AppState, Brightness, Button, ButtonAction, ChordId, LedCmd, MonomeKey,
  PitchClass, PitchLedReason, VoiceId, VoiceMap, VoiceSource, VoiceState, WindowId,
};
use crate::voices::{ramp_chord_accretion_to_zero, spawn_accretion_voice, voice_alive_with_id};

impl AppState {
  pub fn new(
    voices: Arc<Mutex<VoiceMap>>,
    pitch_class: PitchClass,
    fund: f64, edo: i32, sample_rate: f32,
  ) -> Self {
    let mut control_buttons = HashMap::new();
    control_buttons.insert(CELL_WIPE, Button::Fire { fire: ButtonAction::WipeFire });
    control_buttons.insert(CELL_ACCRETE_ON,
      Button::Toggle { state: false, on: ButtonAction::AccreteOn,
                                     off: ButtonAction::AccreteOff });
    control_buttons.insert(CELL_EMIT_IS_TOGGLE,
      Button::Toggle { state: true,  on: ButtonAction::EmitIsToggleOn,
                                     off: ButtonAction::EmitIsToggleOff });
    control_buttons.insert(CELL_SET_ACCRETION_TARGET,
      Button::Fire { fire: ButtonAction::SetTargetFire });
    AppState {
      voices,
      chords: vec![HashMap::new(); N_CHORDS],
      accretion_target: INITIALLY_ACCRETING_CHORD,
      emitting_chord: None,
      pressed_chords: vec![],
      accrete_on: false, emit_is_toggle: true,
      next_voice_id: 0, pitchled_reasons: HashMap::new(),
      control_buttons,
      pitch_class, fund, edo, sample_rate,
    }
  }
}

// EDO window press.
pub fn edo_press(state: &mut AppState, cell: MonomeKey) -> Vec<LedCmd> {
  let abs_pitch = match state.pitch_class.key_to_pitch.get(&cell) {
    Some(&p) => p,
    None => return vec![],
  };
  let id = state.next_voice_id;
  state.next_voice_id += 1;
  {
    let mut vs = state.voices.lock().unwrap();
    vs.insert(VoiceSource::Fingered { xy: cell }, VoiceState {
      id,
      freq: freq_for_pitch(abs_pitch, state.fund, state.edo),
      phase: 0.0, env: 0.0,
      target_env: 1.0,
      ramp_per_sample: 1.0 / (ATTACK_SECS * state.sample_rate),
    });
  }
  let mut diffs = vec![];
  // Pitch-equivalent lighting.
  let r = PitchLedReason::PitchEquivalent { source_xy: cell };
  for c in cells_for_pitch_of(&state.pitch_class, cell) {
    if let Some(true) = add_reason(&mut state.pitchled_reasons, c, r) {
      diffs.push((WindowId::Edo, c, Brightness::Bright));
    }
  }
  // If accrete is on AND this pitch is not yet in the target's slot:
  // introduce it with this voice's id as the originVoice, and (if
  // the emitting chord IS the target) light the corresponding cells.
  let target = state.accretion_target;
  if state.accrete_on && !state.chords[target].contains_key(&abs_pitch) {
    let mut ids = HashSet::new();
    ids.insert(id);
    state.chords[target].insert(abs_pitch, ids);
    if state.emitting_chord == Some(target) {
      let r = PitchLedReason::Chord { chord: target, pitch: abs_pitch };
      for c in cells_for_pitch(&state.pitch_class, abs_pitch, state.edo) {
        if let Some(true) = add_reason(&mut state.pitchled_reasons, c, r) {
          diffs.push((WindowId::Edo, c, Brightness::Bright));
        }
      }
    }
  }
  diffs
}

// EDO window release.
pub fn edo_release(state: &mut AppState, cell: MonomeKey) -> Vec<LedCmd> {
  let abs_pitch = match state.pitch_class.key_to_pitch.get(&cell) {
    Some(&p) => p,
    None => return vec![],
  };
  let mut diffs = vec![];
  // Decide the voice's fate. The release-transform fires only if
  // this voice's pitch is in the *currently emitting* chord's slot
  // and this voice is the last alive originVoice for it (per
  // chord-storage spec, Round 1 answer (A)).
  let mut vs = state.voices.lock().unwrap();
  if let Some(this_voice) = vs.get(&VoiceSource::Fingered { xy: cell }).copied() {
    let em = state.emitting_chord;
    let (in_slot, originvoices_opt) = match em {
      Some(c) => (
        state.chords[c].contains_key(&abs_pitch),
        state.chords[c].get(&abs_pitch),
      ),
      None => (false, None),
    };
    let acc_voice_exists = em.is_some()
      && vs.contains_key(&VoiceSource::Accreted { chord: em.unwrap(), pitch: abs_pitch });
    let (is_originvoice, last_alive_originvoice) = match originvoices_opt {
      None => (false, false),
      Some(originvoices) => {
        let mine = originvoices.contains(&this_voice.id);
        // "alive" matches voice_alive_with_id: target_env > 0,
        // i.e., the user is still holding that key.
        let any_other_alive = originvoices.iter().any(|&id| {
          id != this_voice.id
            && vs.values().any(|v| v.id == id && v.target_env > 0.0)
        });
        (mine, mine && !any_other_alive)
      }
    };
    let should_transform = in_slot && em.is_some() && !acc_voice_exists
                        && is_originvoice && last_alive_originvoice;
    if should_transform {
      let chord = em.unwrap();
      let v = vs.remove(&VoiceSource::Fingered { xy: cell }).unwrap();
      vs.insert(VoiceSource::Accreted { chord, pitch: abs_pitch }, VoiceState {
        id: v.id, freq: v.freq, phase: v.phase, env: v.env,
        target_env: ACCRETION_TARGET,
        ramp_per_sample:
          (v.env - ACCRETION_TARGET).abs() / (RELEASE_SECS * state.sample_rate),
      });
    } else if let Some(v) = vs.get_mut(&VoiceSource::Fingered { xy: cell }) {
      v.target_env = 0.0;
      v.ramp_per_sample = v.env / (RELEASE_SECS * state.sample_rate);
    }
  }
  drop(vs);
  // Pitch-equivalent LED reasons go away with the press.
  let r = PitchLedReason::PitchEquivalent { source_xy: cell };
  for c in cells_for_pitch_of(&state.pitch_class, cell) {
    if let Some(false) = remove_reason(&mut state.pitchled_reasons, c, r) {
      diffs.push((WindowId::Edo, c, Brightness::Off));
    }
  }
  diffs
}

// Press in any control window. Returns LED diffs (the button's own
// light, plus any caused by the dispatched ButtonAction).
pub fn control_press(state: &mut AppState, cell: MonomeKey, win: WindowId) -> Vec<LedCmd> {
  let button = match state.control_buttons.get_mut(&cell) {
    Some(b) => b,
    None => return vec![],
  };
  let (action, new_state) = match button {
    Button::Toggle { state, on, off } => {
      *state = !*state;
      (Some(if *state { *on } else { *off }), Some(*state))
    }
    Button::Nursed { state, on, .. } => {
      *state = true;
      (Some(*on), Some(true))
    }
    Button::Fire { fire } => (Some(*fire), None),
  };
  let mut diffs = vec![];
  if let Some(lit) = new_state {
    diffs.push((win, cell, if lit { Brightness::Bright } else { Brightness::Off }));
  }
  if let Some(action) = action { diffs.extend(do_action(state, action)); }
  diffs
}

// Release in any control window. Only Nursed buttons produce an
// off-event on release; Toggle and Fire ignore release.
pub fn control_release(state: &mut AppState, cell: MonomeKey, win: WindowId) -> Vec<LedCmd> {
  let button = match state.control_buttons.get_mut(&cell) {
    Some(b) => b,
    None => return vec![],
  };
  let (action, new_state) = match button {
    Button::Nursed { state, off, .. } => {
      *state = false;
      (Some(*off), Some(false))
    }
    _ => (None, None),
  };
  let mut diffs = vec![];
  if let Some(lit) = new_state {
    diffs.push((win, cell, if lit { Brightness::Bright } else { Brightness::Off }));
  }
  if let Some(action) = action { diffs.extend(do_action(state, action)); }
  diffs
}

// Press of a chord button at (chord_id, 15). Handles both Toggle
// and Nursed modes, the pressed_chords stack, and emitter
// switching with all attendant LED diffs.
pub fn chord_press(state: &mut AppState, chord_id: ChordId) -> Vec<LedCmd> {
  // Update pressed_chords: push to top, removing any earlier occurrence.
  state.pressed_chords.retain(|&c| c != chord_id);
  state.pressed_chords.push(chord_id);

  if state.emit_is_toggle {
    // Toggle mode: pressing the currently-emitting chord toggles it
    // off; pressing any other chord switches the emitter to it.
    if state.emitting_chord == Some(chord_id) {
      switch_emitter_to(state, None)
    } else {
      switch_emitter_to(state, Some(chord_id))
    }
  } else {
    // Nursed mode: top of stack becomes the emitter.
    switch_emitter_to(state, Some(chord_id))
  }
}

// Release of a chord button at (chord_id, 15).
pub fn chord_release(state: &mut AppState, chord_id: ChordId) -> Vec<LedCmd> {
  // Remove from pressed_chords wherever it is.
  state.pressed_chords.retain(|&c| c != chord_id);

  if state.emit_is_toggle {
    // Toggle ignores release for emitter purposes.
    return vec![];
  }
  // Nursed: if this was the emitter, pick new top of stack as emitter.
  if state.emitting_chord == Some(chord_id) {
    let new_emitter = state.pressed_chords.last().copied();
    switch_emitter_to(state, new_emitter)
  } else {
    vec![]
  }
}

// Move emitter from current to `new`. Ramps out the previous chord's
// accretion voices and removes its Chord-variant LED reasons; spawns
// the new chord's accretion voices (skipping pitches with held
// originVoices) and adds its Chord-variant LED reasons. Repaints
// both the prev and new chord cells' LEDs.
pub fn switch_emitter_to(state: &mut AppState, new: Option<ChordId>) -> Vec<LedCmd> {
  let prev = state.emitting_chord;
  if prev == new { return vec![]; }
  let mut diffs = vec![];
  state.emitting_chord = new;

  // Stop previous emitter (if any).
  if let Some(p) = prev {
    let pitches: Vec<i32> = state.chords[p].keys().copied().collect();
    {
      let mut vs = state.voices.lock().unwrap();
      ramp_chord_accretion_to_zero(&mut vs, p, state.sample_rate);
    }
    diffs.extend(remove_chord_pitchled_reasons(state, p, &pitches));
    // Repaint prev's chord-button cell.
    diffs.push((WindowId::ControlsBottom, (p as i32, 15),
                chord_button_brightness(state, p)));
  }

  // Start new emitter (if any).
  if let Some(n) = new {
    let pitches: Vec<i32> = state.chords[n].keys().copied().collect();
    {
      let mut vs = state.voices.lock().unwrap();
      for &p in &pitches {
        let originvoices = &state.chords[n][&p];
        let any_alive = originvoices.iter()
          .any(|&id| voice_alive_with_id(&vs, id));
        if !any_alive {
          spawn_accretion_voice(&mut vs, n, p, state.fund, state.edo,
                                &mut state.next_voice_id, state.sample_rate);
        }
      }
    }
    diffs.extend(add_chord_pitchled_reasons(state, n, &pitches));
    diffs.push((WindowId::ControlsBottom, (n as i32, 15),
                chord_button_brightness(state, n)));
  }
  diffs
}

// Brightness for chord button N's own cell, given current state.
//   Bright if N is the emitting chord;
//   Dim    if N is the accretion target (and not silence);
//   Off    otherwise.
pub fn chord_button_brightness(state: &AppState, chord: ChordId) -> Brightness {
  if state.emitting_chord == Some(chord) {
    Brightness::Bright
  } else if chord == state.accretion_target && chord != SILENCE_CHORD {
    Brightness::Dim
  } else {
    Brightness::Off
  }
}

// Apply one ButtonAction. Mutates state, returns LED diffs caused by
// that mutation (from the EDO grid for Chord-variant reasons; the
// button's own LED is the dispatcher's responsibility).
pub fn do_action(state: &mut AppState, action: ButtonAction) -> Vec<LedCmd> {
  match action {
    ButtonAction::AccreteOn => {
      state.accrete_on = true;
      // Atomic snapshot per pitch — into the accretion target's slot.
      let target = state.accretion_target;
      let vs = state.voices.lock().unwrap();
      let mut by_pitch: HashMap<i32, HashSet<VoiceId>> = HashMap::new();
      for (src, v) in vs.iter() {
        if let VoiceSource::Fingered { xy } = src {
          if let Some(&p) = state.pitch_class.key_to_pitch.get(xy) {
            by_pitch.entry(p).or_default().insert(v.id);
          }
        }
      }
      drop(vs);
      let mut newly_introduced: Vec<i32> = vec![];
      for (p, ids) in by_pitch {
        if !state.chords[target].contains_key(&p) {
          state.chords[target].insert(p, ids);
          newly_introduced.push(p);
        }
      }
      // Light up newly-introduced pitches only if the emitting chord
      // is the same chord we just modified.
      if state.emitting_chord == Some(target) {
        add_chord_pitchled_reasons(state, target, &newly_introduced)
      } else { vec![] }
    }
    ButtonAction::AccreteOff => {
      state.accrete_on = false;
      vec![]
    }
    ButtonAction::WipeFire => {
      // Wipe affects the accretion target. If the target IS also the
      // emitter, ramp its voices and clear its LED reasons.
      let target = state.accretion_target;
      let pitches: Vec<i32> = state.chords[target].keys().copied().collect();
      state.chords[target].clear();
      if state.emitting_chord == Some(target) {
        {
          let mut vs = state.voices.lock().unwrap();
          ramp_chord_accretion_to_zero(&mut vs, target, state.sample_rate);
        }
        remove_chord_pitchled_reasons(state, target, &pitches)
      } else { vec![] }
    }
    ButtonAction::EmitIsToggleOn | ButtonAction::EmitIsToggleOff => {
      let new_mode_is_toggle = matches!(action, ButtonAction::EmitIsToggleOn);
      let was_toggle = state.emit_is_toggle;
      state.emit_is_toggle = new_mode_is_toggle;
      // Mode-flip rules from Round 2:
      //   Toggle → Nursed: keep emitting iff the emitting chord is
      //     in pressed_chords; otherwise silence.
      //   Nursed → Toggle: snapshot — top of pressed_chords (if any)
      //     becomes the new toggle-on chord. emitting_chord already
      //     reflects this; no audible change.
      if was_toggle && !new_mode_is_toggle {
        // Toggle → Nursed.
        let keep = match state.emitting_chord {
          Some(c) => state.pressed_chords.contains(&c),
          None    => false,
        };
        if !keep {
          return switch_emitter_to(state, None);
        }
      }
      // Nursed → Toggle: nothing to do — pressed_chords stays as
      // informational record; emitting_chord stays whatever it was.
      vec![]
    }
    ButtonAction::SetTargetFire => {
      // commit 6 implements the modal target-select flow. No-op.
      vec![]
    }
  }
}

pub fn add_chord_pitchled_reasons(state: &mut AppState, chord: ChordId, pitches: &[i32]) -> Vec<LedCmd> {
  let mut diffs = vec![];
  for &p in pitches {
    let r = PitchLedReason::Chord { chord, pitch: p };
    for c in cells_for_pitch(&state.pitch_class, p, state.edo) {
      if let Some(true) = add_reason(&mut state.pitchled_reasons, c, r) {
        diffs.push((WindowId::Edo, c, Brightness::Bright));
      }
    }
  }
  diffs
}

pub fn remove_chord_pitchled_reasons(state: &mut AppState, chord: ChordId, pitches: &[i32]) -> Vec<LedCmd> {
  let mut diffs = vec![];
  for &p in pitches {
    let r = PitchLedReason::Chord { chord, pitch: p };
    for c in cells_for_pitch(&state.pitch_class, p, state.edo) {
      if let Some(false) = remove_reason(&mut state.pitchled_reasons, c, r) {
        diffs.push((WindowId::Edo, c, Brightness::Off));
      }
    }
  }
  diffs
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::consts::{INITIALLY_ACCRETING_CHORD, RELEASE_SECS, SILENCE_CHORD};
  use crate::pitch::build_pitch_class;
  use crate::voices::render_block;

  fn fresh_state() -> AppState {
    let voices = Arc::new(Mutex::new(HashMap::new()));
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    AppState::new(voices, pc, 220.0, 46, 48000.0)
  }

  fn voice_keys(state: &AppState) -> HashSet<VoiceSource> {
    state.voices.lock().unwrap().keys().copied().collect()
  }

  // === EDO + accrete + emit interaction (preserved from commit 4) =======

  #[test]
  fn press_then_release_with_accrete_on_emit_on_transforms_to_accretion() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    chord_press(&mut s, INITIALLY_ACCRETING_CHORD); // start emit
    let cell = (0, 0);  // pitch 0
    edo_press(&mut s, cell);
    assert!(s.chords[INITIALLY_ACCRETING_CHORD].contains_key(&0));
    assert!(voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    let target_pitch = VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: 0,
    };
    assert!(!voice_keys(&s).contains(&target_pitch));
    edo_release(&mut s, cell);
    assert!(!voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    assert!(voice_keys(&s).contains(&target_pitch));
    let v = s.voices.lock().unwrap()[&target_pitch];
    assert_eq!(v.target_env, ACCRETION_TARGET);
  }

  #[test]
  fn press_then_release_with_accrete_on_emit_off_leaves_no_accretion_voice() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    let cell = (0, 0);
    edo_press(&mut s, cell);
    assert!(s.chords[INITIALLY_ACCRETING_CHORD].contains_key(&0));
    edo_release(&mut s, cell);
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: 0,
    }));
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Fingered { xy: cell }];
    assert_eq!(v.target_env, 0.0);
  }

  #[test]
  fn emit_on_with_held_finger_does_not_spawn_accretion() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    let cell = (0, 0);
    edo_press(&mut s, cell);
    chord_press(&mut s, INITIALLY_ACCRETING_CHORD);
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: 0,
    }));
  }

  #[test]
  fn emit_on_with_dead_originvoice_spawns_accretion() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    let cell = (0, 0);
    edo_press(&mut s, cell);
    edo_release(&mut s, cell);
    let sr = 48000.0;
    let mut buf = vec![0.0_f32; (RELEASE_SECS * sr) as usize + 200];
    {
      let mut vs = s.voices.lock().unwrap();
      render_block(&mut vs, &mut buf, 1, sr);
    }
    assert!(voice_keys(&s).is_empty(), "originVoice should have decayed");
    chord_press(&mut s, INITIALLY_ACCRETING_CHORD);
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: 0,
    }));
  }

  #[test]
  fn second_press_at_same_pitch_after_accretion_voice_sums_then_ramps_to_zero() {
    let mut s = fresh_state();
    s.chords[INITIALLY_ACCRETING_CHORD]
      .insert(0, [99].into_iter().collect()); // dead origin id
    chord_press(&mut s, INITIALLY_ACCRETING_CHORD);
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: 0,
    }));
    let cell = (0, 0);
    edo_press(&mut s, cell);
    assert!(voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: 0,
    }));
    edo_release(&mut s, cell);
    let vs = s.voices.lock().unwrap();
    let f = &vs[&VoiceSource::Fingered { xy: cell }];
    assert_eq!(f.target_env, 0.0);
  }

  #[test]
  fn multi_enharmonic_originvoice_keeps_p_alive_until_last_release() {
    let mut s = fresh_state();
    let p = s.pitch_class.key_to_pitch[&(4, 10)];
    assert_eq!(p, s.pitch_class.key_to_pitch[&(5, 1)]);
    edo_press(&mut s, (4, 10));
    edo_press(&mut s, (5, 1));
    do_action(&mut s, ButtonAction::AccreteOn);
    chord_press(&mut s, INITIALLY_ACCRETING_CHORD);
    assert_eq!(s.chords[INITIALLY_ACCRETING_CHORD][&p].len(), 2,
               "both finger voices should be co-originVoices");
    edo_release(&mut s, (4, 10));
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: p,
    }));
    edo_release(&mut s, (5, 1));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: p,
    }));
  }

  #[test]
  fn accrete_on_snapshot_captures_held_keys_atomically_per_pitch() {
    let mut s = fresh_state();
    edo_press(&mut s, (4, 10));
    edo_press(&mut s, (5, 1));
    let p = s.pitch_class.key_to_pitch[&(4, 10)];
    do_action(&mut s, ButtonAction::AccreteOn);
    assert_eq!(s.chords[INITIALLY_ACCRETING_CHORD][&p].len(), 2);
  }

  #[test]
  fn accrete_on_snapshot_does_not_grow_existing_pitch_accretion_entry() {
    let mut s = fresh_state();
    s.chords[INITIALLY_ACCRETING_CHORD]
      .insert(0, [99].into_iter().collect());
    edo_press(&mut s, (0, 0));
    do_action(&mut s, ButtonAction::AccreteOn);
    let set = &s.chords[INITIALLY_ACCRETING_CHORD][&0];
    assert_eq!(set.len(), 1);
    assert!(set.contains(&99));
  }

  // === Wipe ============================================================

  #[test]
  fn wipe_clears_target_and_ramps_accretion_voices_when_target_is_emitter() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    chord_press(&mut s, INITIALLY_ACCRETING_CHORD);
    edo_press(&mut s, (0, 0));
    edo_release(&mut s, (0, 0));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: 0,
    }));
    do_action(&mut s, ButtonAction::WipeFire);
    assert!(s.chords[INITIALLY_ACCRETING_CHORD].is_empty());
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Accreted {
      chord: INITIALLY_ACCRETING_CHORD, pitch: 0,
    }];
    assert_eq!(v.target_env, 0.0);
  }

  #[test]
  fn wipe_target_no_audible_change_when_target_not_emitter() {
    // Target = chord 1; emitting = chord 2. Wipe affects chord 1's
    // slot but doesn't touch chord 2's voices.
    let mut s = fresh_state();
    s.chords[INITIALLY_ACCRETING_CHORD]
      .insert(5, [99].into_iter().collect()); // chord 1 has pitch 5
    s.chords[2].insert(7, [98].into_iter().collect());  // chord 2 has pitch 7
    chord_press(&mut s, 2);  // emit chord 2
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { chord: 2, pitch: 7 }));
    do_action(&mut s, ButtonAction::WipeFire);
    assert!(s.chords[INITIALLY_ACCRETING_CHORD].is_empty());
    // Chord 2's voice survives untouched.
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Accreted { chord: 2, pitch: 7 }];
    assert!(v.target_env > 0.0, "chord 2's voice should still be live");
  }

  // === Chord-button: Toggle mode ========================================

  #[test]
  fn chord_press_in_toggle_mode_starts_emit() {
    let mut s = fresh_state();  // emit_is_toggle = true
    s.chords[1].insert(0, HashSet::new());  // chord 1 has pitch 0
    chord_press(&mut s, 1);
    assert_eq!(s.emitting_chord, Some(1));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { chord: 1, pitch: 0 }));
  }

  #[test]
  fn chord_press_in_toggle_mode_switches_emitter() {
    let mut s = fresh_state();
    s.chords[1].insert(0, HashSet::new());
    s.chords[2].insert(7, HashSet::new());
    chord_press(&mut s, 1);
    assert_eq!(s.emitting_chord, Some(1));
    chord_press(&mut s, 2);
    assert_eq!(s.emitting_chord, Some(2));
    let vs = s.voices.lock().unwrap();
    // Chord 1's voice ramps to 0 (no longer emitting).
    let v1 = &vs[&VoiceSource::Accreted { chord: 1, pitch: 0 }];
    assert_eq!(v1.target_env, 0.0);
    // Chord 2's voice is live.
    let v2 = &vs[&VoiceSource::Accreted { chord: 2, pitch: 7 }];
    assert!(v2.target_env > 0.0);
  }

  #[test]
  fn chord_press_same_chord_in_toggle_toggles_off() {
    let mut s = fresh_state();
    s.chords[1].insert(0, HashSet::new());
    chord_press(&mut s, 1);
    assert_eq!(s.emitting_chord, Some(1));
    chord_press(&mut s, 1);
    assert_eq!(s.emitting_chord, None);
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Accreted { chord: 1, pitch: 0 }];
    assert_eq!(v.target_env, 0.0);
  }

  // === Chord-button: Nursed mode ========================================

  fn flip_to_nursed(s: &mut AppState) {
    do_action(s, ButtonAction::EmitIsToggleOff);
    assert!(!s.emit_is_toggle);
  }

  #[test]
  fn chord_press_in_nursed_mode_pushes_stack() {
    let mut s = fresh_state();
    flip_to_nursed(&mut s);
    s.chords[1].insert(0, HashSet::new());
    s.chords[2].insert(7, HashSet::new());
    chord_press(&mut s, 1);
    assert_eq!(s.pressed_chords, vec![1]);
    assert_eq!(s.emitting_chord, Some(1));
    chord_press(&mut s, 2);
    assert_eq!(s.pressed_chords, vec![1, 2]);
    assert_eq!(s.emitting_chord, Some(2));
  }

  #[test]
  fn chord_release_in_nursed_pops_and_restores_underneath() {
    let mut s = fresh_state();
    flip_to_nursed(&mut s);
    s.chords[1].insert(0, HashSet::new());
    s.chords[2].insert(7, HashSet::new());
    chord_press(&mut s, 1);  // stack: [1], emit 1
    chord_press(&mut s, 2);  // stack: [1, 2], emit 2
    chord_release(&mut s, 2);  // stack: [1], emit 1 again
    assert_eq!(s.pressed_chords, vec![1]);
    assert_eq!(s.emitting_chord, Some(1));
    let vs = s.voices.lock().unwrap();
    let v1 = &vs[&VoiceSource::Accreted { chord: 1, pitch: 0 }];
    assert!(v1.target_env > 0.0, "chord 1's voice should respawn on top");
  }

  #[test]
  fn chord_release_in_nursed_of_buried_does_not_change_emitter() {
    let mut s = fresh_state();
    flip_to_nursed(&mut s);
    s.chords[1].insert(0, HashSet::new());
    s.chords[2].insert(7, HashSet::new());
    chord_press(&mut s, 1);
    chord_press(&mut s, 2);
    chord_release(&mut s, 1);  // release the buried one
    assert_eq!(s.pressed_chords, vec![2]);
    assert_eq!(s.emitting_chord, Some(2));
  }

  // === Silence button ===================================================

  #[test]
  fn silence_in_toggle_acts_as_fire_to_kill_current_emitter() {
    let mut s = fresh_state();  // Toggle mode
    s.chords[1].insert(0, HashSet::new());
    chord_press(&mut s, 1);
    assert_eq!(s.emitting_chord, Some(1));
    chord_press(&mut s, SILENCE_CHORD);  // press silence
    assert_eq!(s.emitting_chord, Some(SILENCE_CHORD));
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Accreted { chord: 1, pitch: 0 }];
    assert_eq!(v.target_env, 0.0);
  }

  #[test]
  fn silence_in_nursed_emits_silence_then_restores_on_release() {
    let mut s = fresh_state();
    flip_to_nursed(&mut s);
    s.chords[1].insert(0, HashSet::new());
    chord_press(&mut s, 1);
    assert_eq!(s.emitting_chord, Some(1));
    chord_press(&mut s, SILENCE_CHORD);
    assert_eq!(s.emitting_chord, Some(SILENCE_CHORD));
    chord_release(&mut s, SILENCE_CHORD);
    assert_eq!(s.emitting_chord, Some(1), "chord 1 underneath should resume");
  }

  // === Mode flip ========================================================

  #[test]
  fn mode_flip_toggle_to_nursed_with_held_chord_keeps_emitting() {
    let mut s = fresh_state();  // Toggle mode
    s.chords[1].insert(0, HashSet::new());
    chord_press(&mut s, 1);
    // Chord 1 is in pressed_chords (we pressed it just now).
    assert!(s.pressed_chords.contains(&1));
    do_action(&mut s, ButtonAction::EmitIsToggleOff);
    assert!(!s.emit_is_toggle);
    assert_eq!(s.emitting_chord, Some(1), "still emitting after flip");
  }

  #[test]
  fn mode_flip_toggle_to_nursed_without_held_chord_silences() {
    let mut s = fresh_state();
    s.chords[1].insert(0, HashSet::new());
    chord_press(&mut s, 1);
    // Simulate the user releasing the button physically (Toggle
    // mode normally ignores release for emitter, but pressed_chords
    // tracks physical state regardless).
    s.pressed_chords.retain(|&c| c != 1);
    do_action(&mut s, ButtonAction::EmitIsToggleOff);
    assert_eq!(s.emitting_chord, None, "no held chord → silenced on flip");
  }

  #[test]
  fn mode_flip_nursed_to_toggle_snapshots_top_of_stack() {
    let mut s = fresh_state();
    flip_to_nursed(&mut s);
    s.chords[1].insert(0, HashSet::new());
    s.chords[2].insert(7, HashSet::new());
    chord_press(&mut s, 1);
    chord_press(&mut s, 2);
    assert_eq!(s.emitting_chord, Some(2));
    do_action(&mut s, ButtonAction::EmitIsToggleOn);
    assert!(s.emit_is_toggle);
    // Top of stack (chord 2) is the snapshot — emitting_chord stays Some(2).
    assert_eq!(s.emitting_chord, Some(2));
  }
}
