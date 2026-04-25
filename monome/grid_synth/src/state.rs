//! AppState lifecycle and the event handlers that drive it.
//!
//! Per the design dialog, "methods" are free functions taking
//! &mut AppState; the only impl block is AppState::new (kept as a
//! constructor so "go to definition" jumps to the type).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::consts::{ACCRETION_TARGET, ATTACK_SECS, RELEASE_SECS};
use crate::leds::{add_reason, remove_reason};
use crate::pitch::{cells_for_pitch, cells_for_pitch_of, freq_for_pitch};
use crate::types::{
  AppState, Brightness, Button, ButtonAction, LedCmd, MonomeKey, PitchClass,
  PitchLedReason, VoiceId, VoiceMap, VoiceSource, VoiceState, WindowId,
};
use crate::voices::{ramp_all_accretion_to_zero, spawn_accretion_voice, voice_alive_with_id};

impl AppState {
  pub fn new(
    voices: Arc<Mutex<VoiceMap>>,
    pitch_class: PitchClass,
    fund: f64, edo: i32, sample_rate: f32,
  ) -> Self {
    let mut control_buttons = HashMap::new();
    control_buttons.insert((0, 14),
      Button::Toggle { state: false, on: ButtonAction::AccreteOn,
                                     off: ButtonAction::AccreteOff });
    control_buttons.insert((1, 14), Button::Fire { fire: ButtonAction::WipeFire });
    control_buttons.insert((0, 15), Button::Fire { fire: ButtonAction::SilentFire });
    // Emit defaults to Toggle (since emit_is_toggle defaults to true).
    control_buttons.insert((1, 15),
      Button::Toggle { state: false, on: ButtonAction::EmitOn,
                                     off: ButtonAction::EmitOff });
    control_buttons.insert((2, 15),
      Button::Toggle { state: true,  on: ButtonAction::EmitIsToggleOn,
                                     off: ButtonAction::EmitIsToggleOff });
    AppState {
      voices, pitch_accretion: HashMap::new(),
      accrete_on: false, emit_on: false, emit_is_toggle: true,
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
  // If accrete is on AND this pitch is not yet in the slot:
  // introduce it with this voice's id as the originVoice, and (if
  // emit is on) light the corresponding cells.
  if state.accrete_on && !state.pitch_accretion.contains_key(&abs_pitch) {
    let mut ids = HashSet::new();
    ids.insert(id);
    state.pitch_accretion.insert(abs_pitch, ids);
    if state.emit_on {
      let r = PitchLedReason::Chord { pitch: abs_pitch };
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
  // Decide the voice's fate.
  let mut vs = state.voices.lock().unwrap();
  if let Some(this_voice) = vs.get(&VoiceSource::Fingered { xy: cell }).copied() {
    let in_slot = state.pitch_accretion.contains_key(&abs_pitch);
    let acc_voice_exists =
      vs.contains_key(&VoiceSource::Accreted { pitch: abs_pitch });
    let (is_originvoice, last_alive_originvoice) =
      match state.pitch_accretion.get(&abs_pitch) {
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
    let should_transform = in_slot && state.emit_on && !acc_voice_exists
                        && is_originvoice && last_alive_originvoice;
    if should_transform {
      let v = vs.remove(&VoiceSource::Fingered { xy: cell }).unwrap();
      vs.insert(VoiceSource::Accreted { pitch: abs_pitch }, VoiceState {
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

// Apply one ButtonAction. Mutates state, returns LED diffs caused by
// that mutation (from the EDO grid for Chord-variant reasons; the
// button's own LED is the dispatcher's responsibility).
pub fn do_action(state: &mut AppState, action: ButtonAction) -> Vec<LedCmd> {
  match action {
    ButtonAction::AccreteOn => {
      state.accrete_on = true;
      // Atomic snapshot per pitch.
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
        if !state.pitch_accretion.contains_key(&p) {
          state.pitch_accretion.insert(p, ids);
          newly_introduced.push(p);
        }
      }
      if state.emit_on {
        add_chord_pitchled_reasons(state, &newly_introduced)
      } else { vec![] }
    }
    ButtonAction::AccreteOff => {
      state.accrete_on = false;
      vec![]
    }
    ButtonAction::EmitOn => {
      state.emit_on = true;
      let pitches: Vec<i32> = state.pitch_accretion.keys().copied().collect();
      // Spawn accretion voices for pitches with no held originVoice.
      {
        let mut vs = state.voices.lock().unwrap();
        for &p in &pitches {
          let originvoices = &state.pitch_accretion[&p];
          let any_alive = originvoices.iter()
            .any(|&id| voice_alive_with_id(&vs, id));
          if !any_alive {
            spawn_accretion_voice(&mut vs, p, state.fund, state.edo,
                                  &mut state.next_voice_id, state.sample_rate);
          }
        }
      }
      add_chord_pitchled_reasons(state, &pitches)
    }
    // PITFALL: pressing silent while emit is in Nursed mode is silly.
    // The emit Button's `state` is still true (you're holding the
    // key); silent flips emit_on to false and the accretion voices
    // ramp out, but the next emit press / release cycle restores
    // them. To stop accretion in Nursed mode, just release the emit
    // key.
    ButtonAction::EmitOff | ButtonAction::SilentFire => {
      let was_on = state.emit_on;
      state.emit_on = false;
      if !was_on { return vec![]; }
      {
        let mut vs = state.voices.lock().unwrap();
        ramp_all_accretion_to_zero(&mut vs, state.sample_rate);
      }
      let pitches: Vec<i32> = state.pitch_accretion.keys().copied().collect();
      remove_chord_pitchled_reasons(state, &pitches)
    }
    ButtonAction::WipeFire => {
      let pitches: Vec<i32> = state.pitch_accretion.keys().copied().collect();
      state.pitch_accretion.clear();
      {
        let mut vs = state.voices.lock().unwrap();
        ramp_all_accretion_to_zero(&mut vs, state.sample_rate);
      }
      if state.emit_on {
        remove_chord_pitchled_reasons(state, &pitches)
      } else { vec![] }
    }
    ButtonAction::EmitIsToggleOn | ButtonAction::EmitIsToggleOff => {
      let new_mode_is_toggle = matches!(action, ButtonAction::EmitIsToggleOn);
      state.emit_is_toggle = new_mode_is_toggle;
      // Rebuild the emit Button at (1,15) with the new variant,
      // preserving its current `state` so that the audible / visual
      // emit state survives the mode flip (snapshot).
      let cur = state.control_buttons.get(&(1, 15)).copied();
      let cur_state = match cur {
        Some(Button::Toggle { state, .. }) | Some(Button::Nursed { state, .. }) => state,
        _ => false,
      };
      let new_btn = if new_mode_is_toggle {
        Button::Toggle { state: cur_state,
                         on: ButtonAction::EmitOn, off: ButtonAction::EmitOff }
      } else {
        Button::Nursed { state: cur_state,
                         on: ButtonAction::EmitOn, off: ButtonAction::EmitOff }
      };
      state.control_buttons.insert((1, 15), new_btn);
      vec![]
    }
  }
}

pub fn add_chord_pitchled_reasons(state: &mut AppState, pitches: &[i32]) -> Vec<LedCmd> {
  let mut diffs = vec![];
  for &p in pitches {
    let r = PitchLedReason::Chord { pitch: p };
    for c in cells_for_pitch(&state.pitch_class, p, state.edo) {
      if let Some(true) = add_reason(&mut state.pitchled_reasons, c, r) {
        diffs.push((WindowId::Edo, c, Brightness::Bright));
      }
    }
  }
  diffs
}

pub fn remove_chord_pitchled_reasons(state: &mut AppState, pitches: &[i32]) -> Vec<LedCmd> {
  let mut diffs = vec![];
  for &p in pitches {
    let r = PitchLedReason::Chord { pitch: p };
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
  use crate::consts::RELEASE_SECS;
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

  #[test]
  fn press_then_release_with_accrete_on_emit_on_transforms_to_accretion() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    do_action(&mut s, ButtonAction::EmitOn);
    let cell = (0, 0);  // pitch 0
    edo_press(&mut s, cell);
    assert!(s.pitch_accretion.contains_key(&0));
    // Before release: only fingered voice exists.
    assert!(voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    edo_release(&mut s, cell);
    // After release: voice transformed to Accreted.
    assert!(!voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    let v = s.voices.lock().unwrap()[&VoiceSource::Accreted { pitch: 0 }];
    assert_eq!(v.target_env, ACCRETION_TARGET);
  }

  #[test]
  fn press_then_release_with_accrete_on_emit_off_leaves_no_accretion_voice() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    let cell = (0, 0);
    edo_press(&mut s, cell);
    assert!(s.pitch_accretion.contains_key(&0));
    edo_release(&mut s, cell);
    // Voice ramps to 0 normally; no accretion voice exists.
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Fingered { xy: cell }];
    assert_eq!(v.target_env, 0.0);
  }

  #[test]
  fn emit_on_with_held_finger_does_not_spawn_accretion() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    let cell = (0, 0);
    edo_press(&mut s, cell);  // pitch 0 enters slot via this press
    do_action(&mut s, ButtonAction::EmitOn);
    // The originVoice is still alive: emit-on must skip the spawn.
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
  }

  #[test]
  fn emit_on_with_dead_originvoice_spawns_accretion() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    let cell = (0, 0);
    edo_press(&mut s, cell);
    edo_release(&mut s, cell);
    // Voice is now mid-release. Drain it manually with render_block.
    let sr = 48000.0;
    let mut buf = vec![0.0_f32; (RELEASE_SECS * sr) as usize + 200];
    {
      let mut vs = s.voices.lock().unwrap();
      render_block(&mut vs, &mut buf, 1, sr);
    }
    assert!(voice_keys(&s).is_empty(), "originVoice should have decayed");
    do_action(&mut s, ButtonAction::EmitOn);
    // Now no live originVoice; emit-on spawns a fresh accretion voice.
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
  }

  #[test]
  fn second_press_at_same_pitch_after_accretion_voice_sums_then_ramps_to_zero() {
    let mut s = fresh_state();
    // Get an accretion voice for pitch 0 going (no held originVoice).
    s.pitch_accretion.insert(0, [99].into_iter().collect()); // dead origin id
    do_action(&mut s, ButtonAction::EmitOn);
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    let cell = (0, 0);
    edo_press(&mut s, cell);
    // Both voices coexist (the deliberate press always wins / sums).
    assert!(voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    edo_release(&mut s, cell);
    // Accretion voice exists, so finger ramps to 0 (no transform).
    let vs = s.voices.lock().unwrap();
    let f = &vs[&VoiceSource::Fingered { xy: cell }];
    assert_eq!(f.target_env, 0.0);
  }

  #[test]
  fn wipe_with_emit_on_clears_accretion_and_ramps_voices() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::AccreteOn);
    do_action(&mut s, ButtonAction::EmitOn);
    edo_press(&mut s, (0, 0));
    edo_release(&mut s, (0, 0));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    do_action(&mut s, ButtonAction::WipeFire);
    assert!(s.pitch_accretion.is_empty());
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Accreted { pitch: 0 }];
    assert_eq!(v.target_env, 0.0, "wipe should ramp accretion voices to 0");
  }

  #[test]
  fn silent_with_emit_on_ramps_voices_and_sets_emit_off() {
    let mut s = fresh_state();
    s.pitch_accretion.insert(0, [99].into_iter().collect());
    do_action(&mut s, ButtonAction::EmitOn);
    assert!(s.emit_on);
    do_action(&mut s, ButtonAction::SilentFire);
    assert!(!s.emit_on);
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Accreted { pitch: 0 }];
    assert_eq!(v.target_env, 0.0);
  }

  #[test]
  fn silent_with_emit_off_is_noop() {
    let mut s = fresh_state();
    do_action(&mut s, ButtonAction::SilentFire);
    assert!(!s.emit_on);
    assert!(voice_keys(&s).is_empty());
  }

  #[test]
  fn multi_enharmonic_originvoice_keeps_p_alive_until_last_release() {
    // (4,10) and (5,1) are enharmonic in 46/9/1 — same absolute pitch.
    let mut s = fresh_state();
    let p = s.pitch_class.key_to_pitch[&(4, 10)];
    assert_eq!(p, s.pitch_class.key_to_pitch[&(5, 1)]);
    // Hold both, then accrete (atomic snapshot).
    edo_press(&mut s, (4, 10));
    edo_press(&mut s, (5, 1));
    do_action(&mut s, ButtonAction::AccreteOn);
    do_action(&mut s, ButtonAction::EmitOn);
    assert_eq!(s.pitch_accretion[&p].len(), 2,
               "both finger voices should be co-originVoices");
    // Release one: not the last alive originVoice → ramp to 0, no transform.
    edo_release(&mut s, (4, 10));
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted { pitch: p }));
    // Release the other: now the last alive originVoice → transform.
    edo_release(&mut s, (5, 1));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: p }));
  }

  #[test]
  fn accrete_on_snapshot_captures_held_keys_atomically_per_pitch() {
    let mut s = fresh_state();
    // Hold both enharmonic keys before accrete-on.
    edo_press(&mut s, (4, 10));
    edo_press(&mut s, (5, 1));
    let p = s.pitch_class.key_to_pitch[&(4, 10)];
    do_action(&mut s, ButtonAction::AccreteOn);
    assert_eq!(s.pitch_accretion[&p].len(), 2);
  }

  #[test]
  fn accrete_on_snapshot_does_not_grow_existing_pitch_accretion_entry() {
    let mut s = fresh_state();
    // Put pitch 0 into the slot already (manually, with a dead id).
    s.pitch_accretion.insert(0, [99].into_iter().collect());
    // Hold a key for pitch 0, then snapshot via accrete-on.
    edo_press(&mut s, (0, 0));
    do_action(&mut s, ButtonAction::AccreteOn);
    // Snapshot must NOT grow the originVoice set — leave the dead id alone.
    let set = &s.pitch_accretion[&0];
    assert_eq!(set.len(), 1);
    assert!(set.contains(&99));
  }

  #[test]
  fn emit_is_toggle_flip_does_not_disturb_emit_on() {
    let mut s = fresh_state();
    s.pitch_accretion.insert(0, [99].into_iter().collect());
    // Press the emit button itself, not just the action: the button's
    // own `state` field (UI on/off) gets flipped by the dispatcher,
    // and that's what the rebuild reads.
    control_press(&mut s, (1, 15), WindowId::Accretion2x2);
    assert!(s.emit_on);
    // Press emit-is-toggle (initially Toggle/state=true → state=false).
    control_press(&mut s, (2, 15), WindowId::EmitToggle1x1);
    assert!(s.emit_on, "emit state preserved across mode flip");
    assert!(!s.emit_is_toggle);
    // The emit Button at (1,15) is now a Nursed variant with state preserved.
    match s.control_buttons[&(1, 15)] {
      Button::Nursed { state, .. } => assert!(state),
      _ => panic!("emit button should be Nursed after flip"),
    }
  }
}

