//! Key handling: `handle_key` (the overlay dispatch), the per-voice edit-mode
//! Act logic (`handle_edit_press`), the release path (`release_cell`,
//! `cut_for_legato`), and the shared per-grid registries it publishes into
//! (`capture_grid_held(_into)` / `erase_grid_held(_into)`, `held_pitches`,
//! `publish_held`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::edo_play::{register_delta, shift_for_cell, step_for_cell, Shift};
use crate::rig::{AccreteControlKind, EditmodeControlKind};

use crate::types::{Timbre, VoiceSource, VoiceState};

use super::chords;
use super::grid::{slot_for_selector_cell, volume_cells, volume_gain_for_pos};
use super::hooks::{drive_accrete, editmode_press, factored_pulse_press};
use super::layer_controls::LayerTarget;
use super::momentary_chords;
use super::paint::publish_sounding;
use super::polyrhythm::TempoFactorButton;
use super::ring::{GridRing, Reason};
use super::settings::{current_slot, set_slot};
use super::synth::{self, set_grid_fader_gain};
use super::{edit, pedal_slide, GridThread, VOLUME_DB_RANGE};

fn timbre_at(rt: &GridThread, slot: usize) -> Timbre {
  let ts = rt.timbres[slot];
  Timbre {
    waveform: ts.waveform,
    gain: ts.amplitude,
    am: ts.am,
    fm: ts.fm,
    rel_am: ts.rel_am,
    rel_fm: ts.rel_fm,
  }
}

/// Route one debounced key edge by which overlay (if any) it falls in.
/// If this grid's edit selection is empty, put every voice it is currently sounding --
/// fingered, sustained, and chord-layer alike -- into edit mode: the editmode-accrete
/// one-shot, "just as if they had pressed the KMSS select-everything button" (Jeff). A
/// non-empty selection is left exactly as it is.
///
/// Shared by the two modes that need something to act on the instant you enter them:
/// fine transpose (the transpose keys need a selection) and pedal slide (the pedal
/// needs voices to glide). Both call this on entry so a bare toggle means "act on
/// everything sounding".
fn select_all_sounding_if_empty(rt: &GridThread) {
  let selection_empty = {
    let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &rings[rt.grid_index];
    !gr.store.any(Reason::Edit) && !gr.chord.live.values().any(|v| v.edited)
  };
  if selection_empty {
    editmode_press(
      rt.grid_index,
      EditmodeControlKind::Accrete,
      &rt.shared.ring,
      &rt.shared.held_all,
    );
  }
}

pub(super) fn handle_key(
  rt: &mut GridThread,
  register: &mut i32,
  held: &mut HashMap<(i32, i32), i32>,
  cell: (i32, i32),
  press: bool,
) {
  // Selector: a press sets the *controlled* grid's timbre slot (radio; future
  // notes) -- UNLESS that grid has an edit selection, in which case the press
  // re-timbres the selected voices instead (both layers, crossfaded) and the radio
  // stays put, exactly as the factored-pulse multipliers leave the tempo factor
  // alone in edit mode. No visible indicator, per Jeff: the lit cell keeps showing
  // the future-note timbre (moot while edit mode blocks new notes anyway).
  if let Some(slot) = slot_for_selector_cell(rt.overlays.selector_rect, cell) {
    if press {
      let target = rt.knobs.controls_index;
      let layered = {
        let mut controls = rt.shared.layer_controls.lock().unwrap_or_else(|e| e.into_inner());
        controls.get_mut(target).filter(|state| state.enabled).map(|state| {
          state.set_selected_target(slot);
          (state.target(), state.gain_for(state.target()))
        })
      };
      if let Some((layer, gain)) = layered {
        let timbre = timbre_at(rt, slot);
        let (_, sample_rate) = rt.sink.release_params();
        let chord = layer == LayerTarget::Chord;
        synth::retimbre_layer(&rt.shared.voices, target, chord, timbre, sample_rate);
        synth::set_layer_gain(&rt.shared.voices, target, chord, gain);
        return;
      }
      let (edited, chord_keys): (HashSet<i32>, HashSet<VoiceSource>) = {
        let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        match rings.get(target) {
          Some(gr) => (
            gr.store.iter(Reason::Edit).collect(),
            gr.chord
              .live
              .iter()
              .filter(|(_, v)| v.edited)
              .map(|(seq, _)| VoiceSource::SurfaceChord { grid: target, seq: *seq })
              .collect(),
          ),
          None => Default::default(),
        }
      };
      if edited.is_empty() && chord_keys.is_empty() {
        set_slot(&rt.shared.selected, target, slot);
      } else {
        let timbre = timbre_at(rt, slot);
        let held_target: HashMap<(i32, i32), i32> = rt
          .shared
          .held_all
          .lock()
          .unwrap_or_else(|e| e.into_inner())
          .get(target)
          .cloned()
          .unwrap_or_default();
        let (_, sample_rate) = rt.sink.release_params();
        synth::retimbre_voices(
          &rt.shared.voices, target, &edited, &held_target, &chord_keys, timbre, sample_rate,
        );
      }
    }
    return;
  }
  // Relative dB buttons for the two-layer instrument. Press LEDs are local to
  // this surface; the state and live gain updates apply to the controlled grid.
  if in_overlay(rt.overlays.volume_delta_rect, cell) {
    let pos = (cell.0 - rt.overlays.volume_delta_rect[0]) as usize;
    if pos < rt.volume_delta_down.len() {
      rt.volume_delta_down[pos] = press;
    }
    if press {
      let target_grid = rt.knobs.volume_delta_controls_index;
      let update = {
        let mut controls = rt.shared.layer_controls.lock().unwrap_or_else(|e| e.into_inner());
        controls.get_mut(target_grid).filter(|state| state.enabled).map(|state| {
          let layer = state.target();
          let changed_slot = state.apply_delta_cell(pos);
          let finger_gain = state.gain_for(LayerTarget::FingeredSustained);
          let chord_gain = state.gain_for(LayerTarget::Chord);
          let chord_shares_base =
            state.selected_for(LayerTarget::Chord) == changed_slot;
          (layer, finger_gain, chord_gain, chord_shares_base)
        })
      };
      if let Some((layer, finger_gain, chord_gain, chord_shares_base)) = update {
        if layer == LayerTarget::FingeredSustained {
          synth::set_layer_gain(&rt.shared.voices, target_grid, false, finger_gain);
          if chord_shares_base {
            synth::set_layer_gain(&rt.shared.voices, target_grid, true, chord_gain);
          }
        } else {
          synth::set_layer_gain(&rt.shared.voices, target_grid, true, chord_gain);
        }
      }
    }
    return;
  }
  // Volume strip: a press sets the *controlled* grid's loudness -- live (rescales its
  // sounding voices) and for its future notes.
  if in_overlay(rt.overlays.volume_rect, cell) {
    if press {
      set_volume(rt, cell.0);
    }
    return;
  }
  // Distortion toggle: key-down flips THIS grid's switch (the audio callback routes
  // each grid's voices by its own flag); key-up does nothing.
  if in_overlay(rt.overlays.distortion_rect, cell) {
    if press {
      let _ = rt.shared.distortion_on[rt.grid_index].fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // Slide toggle: key-down flips THIS grid's switch; key-up does nothing.
  if in_overlay(rt.overlays.slide_rect, cell) {
    if press {
      let _ = rt.shared.slide_on[rt.grid_index].fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // Mono toggle: key-down flips THIS grid's switch; key-up does nothing.
  if in_overlay(rt.overlays.mono_rect, cell) {
    if press {
      let _ = rt.shared.mono_on[rt.grid_index].fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // Pedal-slide toggle (TODO/pedal-slide): key-down switches THIS grid's EX-P pedal
  // between volume duty and the pitch-slide engine. Entering freezes the grid volume
  // where it is and starts with no managed voices, so the pedal is inert -- and no
  // pitch is yanked -- until the first pick. Leaving freezes every sliding voice
  // exactly where it is and drops the maps; the pedal thread resumes volume duty on
  // its next poll.
  if in_overlay(rt.overlays.pedal_slide_rect, cell) {
    if press {
      let now_on = !rt.shared.pedal_slide_on[rt.grid_index].load(Ordering::Relaxed);
      if now_on {
        // Entering with NOTHING selected first selects everything sounding, so the
        // pedal has voices to slide the instant you pick a target (branch-3 queue
        // "entering slide mode") -- the same one-shot fine transpose uses.
        select_all_sounding_if_empty(rt);
        let f = f32::from_bits(rt.shared.pedal_slide_frac[rt.grid_index].load(Ordering::Relaxed));
        // NaN = this pedal has never reported; enter agnostic about which side is home.
        rt.pedal_slide.enter((!f.is_nan()).then_some(f));
      } else {
        let frozen_voices = rt.pedal_slide.exit();
        synth::freeze_slide_voices(&rt.shared.voices, &frozen_voices);
      }
      // The shared flag flips LAST, once the engine is already in its new state. It is
      // read only by the pedal thread (to know whose volume it is no longer driving)
      // and by the LED paint; what the grid thread does is gated on the engine's own
      // `mode()`, so the two can never disagree about whether a step should run.
      rt.shared.pedal_slide_on[rt.grid_index].store(now_on, Ordering::Relaxed);
    }
    return;
  }
  // Fine transpose (queues/branch-2.org): key-down toggles THIS grid's mode. On
  // entry there is no X and the transpose starts at 0 -- the first play press places
  // the X and transposes nothing (branch-3 queue: seat it on the bass voice for a
  // readable display); on exit a nonzero transpose simply REMAINS in effect -- the
  // voices were moved live, so there is nothing left to apply -- and the X's position
  // is forgotten ("range changes do not persist").
  if in_overlay(rt.overlays.fine_transpose_rect, cell) {
    if press {
      if rt.fine.on {
        rt.fine.exit();
      } else {
        // Entering with NOTHING selected first selects everything sounding on this
        // monome -- the same select-everything one-shot pedal slide uses (see
        // `select_all_sounding_if_empty`). A non-empty selection is left as it is.
        select_all_sounding_if_empty(rt);
        // Enter UNCENTERED: no X yet. The first play press places the X (on the bass
        // voice, say, for a readable display) and transposes nothing; see `fine::press`.
        rt.fine.enter();
      }
    }
    return;
  }
  // The editmode buttons, through the same `editmode_press` the softstep pedals run:
  // clear empties THIS grid's edit SELECTION (branch-3 queue item 4: pure deselection --
  // every cleared note stays sustained, so nothing is silenced), accrete puts every
  // sounding voice into edit mode (sustaining any that were only fingered). Neither ends
  // a voice, so no release params are needed. Key-down only; key-up douses the LED.
  for (rect, down_flag, control) in [
    (rt.overlays.editmode_clear_rect, 0, EditmodeControlKind::Clear),
    (rt.overlays.editmode_accrete_rect, 1, EditmodeControlKind::Accrete),
  ] {
    if in_overlay(rect, cell) {
      if down_flag == 0 {
        rt.editmode_clear_down = press;
      } else {
        rt.editmode_accrete_down = press;
      }
      if press {
        editmode_press(rt.grid_index, control, &rt.shared.ring, &rt.shared.held_all);
      }
      return;
    }
  }
  // The chord block (chord-storage-v2): arm + 9 slots, key-down only (a "toggle"
  // monomekey ignores key-up). While ARMED, a slot press saves every voice this
  // grid is sounding and disarms; disarmed, a slot press toggles the stored chord
  // on (recall) and off.
  if in_overlay(rt.overlays.chord_rect, cell) {
    if press {
      chord_block_press(rt, held, cell);
    }
    return;
  }
  if in_overlay(rt.overlays.momentary_chord_rect, cell) {
    momentary_chord_key(rt, held, cell, press);
    return;
  }
  // The polyrhythm pad: | x3 x2 tap | /3 /2 =1 |, key-down only. The tap sets the
  // GLOBAL tempo; the tempo-factor buttons and the =1 factored-pulse switch act on
  // THIS grid, through the same `factored_pulse_press` as the softstep pedals --
  // so a multiplier retunes this grid's edit-mode notes exactly as a pedal would.
  if in_overlay(rt.overlays.poly_rect, cell) {
    if press {
      let (dx, dy) = (cell.0 - rt.overlays.poly_rect[0], cell.1 - rt.overlays.poly_rect[1]);
      let factor = match (dx, dy) {
        (2, 0) => {
          rt.shared.poly.lock().unwrap_or_else(|e| e.into_inner())
            .tap(Instant::now(), rt.knobs.tap_window);
          None
        }
        (0, 0) => Some(TempoFactorButton::Times3),
        (1, 0) => Some(TempoFactorButton::Times2),
        (0, 1) => Some(TempoFactorButton::Div3),
        (1, 1) => Some(TempoFactorButton::Div2),
        (2, 1) => Some(TempoFactorButton::Unity),
        _ => None,
      };
      if let Some(factor) = factor {
        factored_pulse_press(
          rt.grid_index, factor,
          &rt.shared.poly, &rt.shared.ring, &rt.shared.held_all, &rt.shared.voices,
        );
      }
    }
    return;
  }
  // The accrete (sustain) buttons. Each grid's controls act on ITS OWN bank (misc.org
  // "two monome-specific accrete banks"). Decisions are made under the accrete lock,
  // voices are touched after it drops (the module's no-nested-locks rule).
  if in_overlay(rt.overlays.clear_rect, cell) {
    // The sustain clear removes the SUSTAIN reason from every sustained pitch, ends
    // exactly the drones no finger holds (deselecting them -- `remove_sustain`
    // cascades edit membership away), and ends every CHORD-layer voice, untoggling
    // its slots (the widened "end all sustain and chord", chord-storage-v2). Same
    // code as the pedal: `drive_accrete` is the one implementation, so hands and
    // feet cannot diverge. (`held` was published to the shared registry by every
    // path that changes it, so the finger snapshot inside is current.)
    let (release_secs, sample_rate) = rt.sink.release_params();
    drive_accrete(
      rt.grid_index,
      AccreteControlKind::Clear,
      press,
      &rt.shared.ring,
      &rt.shared.held_all,
      &rt.shared.voices,
      release_secs,
      sample_rate,
    );
    return;
  }
  if in_overlay(rt.overlays.needs_holding_rect, cell) {
    if press {
      let activated = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
        .accrete
        .press_needs_holding();
      if activated.accrete {
        capture_grid_held(rt);
      }
      if activated.erase {
        erase_grid_held(rt);
      }
    }
    return;
  }
  if in_overlay(rt.overlays.accrete_rect, cell) {
    if press {
      let activated = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
        .accrete
        .press_accrete();
      if activated {
        capture_grid_held(rt);
      }
    } else {
      rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].accrete.release_accrete();
    }
    return;
  }
  // The erase button (misc.org "erase button"): accrete's shape under the same
  // needs-holding switch, but pressed pitches LEAVE this grid's sustained set
  // (each keeps sounding until its own finger lifts).
  if in_overlay(rt.overlays.erase_rect, cell) {
    if press {
      let activated = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
        .accrete
        .press_erase();
      if activated {
        erase_grid_held(rt);
      }
    } else {
      rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].accrete.release_erase();
    }
    return;
  }
  // Scroll pad: a press moves THIS grid's play register -- except that with an
  // edit selection live, the OCTAVE switchers retune the selection instead
  // (queues/branch-2.org: "the octave switchers' only effect is to drag the
  // voices that are in edit mode" -- the corner that normally views lower notes
  // moves them an octave lower, the other higher; the register does not move).
  // The four arrows scroll as ever.
  if let Some(shift) = shift_for_cell(rt.overlays.scroll_rect, cell) {
    if press {
      let octave = match shift {
        Shift::OctaveDown => Some(-rt.tuning.edo),
        Shift::OctaveUp => Some(rt.tuning.edo),
        _ => None,
      };
      if let Some(delta) = octave {
        let chord_target = {
          let controls = rt.shared.layer_controls.lock().unwrap_or_else(|e| e.into_inner());
          controls
            .get(rt.grid_index)
            .is_some_and(|state| state.enabled && state.targets_chords())
        };
        if chord_target {
          shift_momentary_chords(rt, delta);
          // Even silence consumes the octave corner in chord-target mode: it must
          // not unexpectedly scroll the play field.
          return;
        }
        // In fine transpose the corners move the X ITSELF -- even off-screen; the
        // register and the voices hold still until a transpose key says otherwise.
        if rt.fine.on {
          rt.fine.move_center(delta);
          return;
        }
        if shift_edited_voices(rt, held, delta) {
          publish_held(&rt.shared.held_all, rt.grid_index, held);
          publish_sounding(&rt.shared.sounding, rt.grid_index, held, rt.tuning.edo);
          return;
        }
      }
      *register += register_delta(shift, rt.tuning.x_step, rt.tuning.y_step, rt.tuning.edo);
    }
    return;
  }
  // Otherwise it is an edo play cell -- ignore presses outside the play grid.
  let [ex0, ey0, ex1, ey1] = rt.overlays.edo_rect;
  if cell.0 < ex0 || cell.0 > ex1 || cell.1 < ey0 || cell.1 > ey1 {
    return;
  }
  if press {
    let pitch = step_for_cell(rt.tuning.x_step, rt.tuning.y_step, *register, cell.0, cell.1);
    // Optional note echo (`echo_input`, off by default): mirrors the sawwave runtime,
    // but stays quiet unless asked so a startup warning isn't scrolled off screen.
    if rt.knobs.echo_input {
      let f = rt.tuning.fund * 2f64.powf(pitch as f64 / rt.tuning.edo as f64);
      eprintln!("press grid={} x={:>2} y={:>2} f={f:.2} Hz", rt.grid_index, cell.0, cell.1);
    }
    // Fine transpose: the play grid is a transpose CONTROLLER -- this press sets
    // the selection's transpose to (pitch - X center) and sounds nothing. Mono-
    // style: the newest press wins; the snap-back lives in the release path.
    // Every play-cell press is a transpose key while the mode is on: no notes,
    // no handles, no drags, no retriggers.
    if rt.fine.on {
      let delta = rt.fine.press(cell, pitch);
      if delta != 0 {
        shift_edited_voices(rt, held, delta);
      }
      publish_held(&rt.shared.held_all, rt.grid_index, held);
      publish_sounding(&rt.shared.sounding, rt.grid_index, held, rt.tuning.edo);
      return;
    }
    // Per-voice edit mode, BEFORE the play path: this press may be an edit trigger or
    // a pitch drag rather than a note, and in both of those cases it must not sound.
    if rt.overlays.momentary_chord_rect == super::NO_RECT
      && !handle_edit_press(rt, held, cell, pitch, register)
    {
      // A trigger or a drag: no note sounds, but `held` may have MOVED (a drag
      // re-pitches a finger's voice), so the shared maps still have to be republished
      // or the other grid reflects a pitch that is no longer sounding.
      publish_held(&rt.shared.held_all, rt.grid_index, held);
      publish_sounding(&rt.shared.sounding, rt.grid_index, held, rt.tuning.edo);
      return;
    }
    // A finger landing on a pitch where CHORD voices ring RESTRIKES them -- the
    // envelope-only retrigger; pitch, timbre, loudness, oscillator phase, and the
    // factored pulse all continue -- and spawns NO new voice (queues/branch-2.org:
    // "re-fingering a sustained or chord pitch should retrigger it in the envelope
    // sense but not in the polyrhythm pulse sense. That's what happens if a finger
    // lands on a preexisting chord voice." Only the REVERSE -- a recall landing on
    // an already-fingered pitch -- coexists). A drone at the same pitch retriggers
    // too: the finger adopts it exactly as it would with no chord around. Mono
    // cuts, slide, and the accrete capture are skipped for this press -- it is a
    // retrigger gesture, not a note-on.
    let chord_restrike: Vec<u64> = {
      let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
      rings[rt.grid_index]
        .chord
        .live
        .iter()
        .filter(|(_, v)| v.pitch == pitch)
        .map(|(s, _)| *s)
        .collect()
    };
    if !chord_restrike.is_empty() {
      rt.sink.restrike_chord_voices(&chord_restrike);
      if let Some(cut) = rt.sink.cut_sustained(pitch) {
        // The pitch also droned: the drone half of the retrigger. A fresh strike
        // (grid timbre) the finger owns, its oscillator and pulse continuing; the
        // pitch keeps its place in the accrete set, so release re-drones it.
        let slot = rt.timbres[current_slot(&rt.shared.selected, rt.grid_index)];
        let timbre = Timbre {
          waveform: slot.waveform,
          gain: slot.amplitude,
          am: slot.am,
          fm: slot.fm,
          rel_am: slot.rel_am,
          rel_fm: slot.rel_fm,
        };
        rt.sink.note_on_continuing(cell, pitch, timbre, cut);
        drone_became_fingered(rt, pitch, cell);
        held.insert(cell, pitch);
        let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        let gr = &mut rings[rt.grid_index];
        gr.accrete.note_played(pitch, &mut gr.store);
      }
      push_trail(
        &rt.shared.trail, pitch.rem_euclid(rt.tuning.edo), rt.tuning.edo,
        rt.knobs.trail_clobber_radius, rt.knobs.trails_max,
      );
      publish_held(&rt.shared.held_all, rt.grid_index, held);
      publish_sounding(&rt.shared.sounding, rt.grid_index, held, rt.tuning.edo);
      return;
    }
    // Mono: a new note cuts this grid's other fingered notes first. With slide on
    // too, the nearest cut note is not released but STOLEN: its voice will glide
    // into the new pitch legato-style, with no attack re-trigger (misc.org "slide
    // when mono is on should not re-trigger the attack"). Other cuts go through
    // the ordinary release path, so accrete still captures them -- and a cut that
    // sustains (accrete) becomes a drone as usual and cannot be stolen.
    let mut legato_from: Option<(i32, i32)> = None;
    if rt.shared.mono_on[rt.grid_index].load(Ordering::Relaxed) {
      let slide = rt.shared.slide_on[rt.grid_index].load(Ordering::Relaxed);
      let mut others: Vec<((i32, i32), i32)> =
        held.iter().filter(|(c, _)| **c != cell).map(|(c, p)| (*c, *p)).collect();
      // The nearest cut pitch is the legato source (with mono playing, the cut is
      // a single note anyway).
      others.sort_by_key(|(_, p)| (*p - pitch).abs());
      for (i, (other, _)) in others.into_iter().enumerate() {
        if slide && i == 0 {
          if cut_for_legato(rt, held, other) {
            legato_from = Some(other);
          }
        } else {
          release_cell(rt, held, other);
        }
      }
    }
    // A manual retrigger of a sustaining pitch cuts this grid's drone and the new
    // note replaces it (misc.org "retriggering a sustaining note replaces it") --
    // no doubling. The pitch keeps its place in the accrete set, so releasing the
    // replacing note re-drones it. After the mono block, so a colliding-pitch
    // drone a mono cut just captured is cut like any other.
    //
    // The cut hands back the outgoing drone's oscillator phase AND its factored
    // pulse (`None` when no drone rang here) so the plain-note branch below can
    // CONTINUE both instead of restarting them -- the retrigger is an envelope
    // event only (branch-3 item 5 for the phase; queues/branch-2.org for the pulse).
    // The sink rig may additionally detune only the retired tail; a zero setting
    // retains the historical exact-pitch cut.
    let retrigger = rt.sink.cut_sustained(pitch);
    let layered = {
      let controls = rt.shared.layer_controls.lock().unwrap_or_else(|e| e.into_inner());
      controls.get(rt.grid_index).filter(|state| state.enabled).map(|state| {
        (
          state.selected_for(LayerTarget::FingeredSustained),
          state.gain_for(LayerTarget::FingeredSustained),
        )
      })
    };
    let slot = layered
      .map(|(slot, _)| slot)
      .unwrap_or_else(|| current_slot(&rt.shared.selected, rt.grid_index));
    let timbre = timbre_at(rt, slot);
    // Slide: while on, glide into this note -- legato from the voice mono just
    // cut, or, with no stolen voice, by re-triggering the nearest recently-
    // released pitch (consuming it as a source); otherwise a plain note.
    let slide_on = rt.shared.slide_on[rt.grid_index].load(Ordering::Relaxed);
    let source = if legato_from.is_none() && slide_on {
      rt.slide.pick(pitch, Instant::now(), rt.knobs.slide_window)
    } else {
      None
    };
    // The factored pulse at THIS note's onset (fixed for the note's life): this
    // grid's applied tempo, and only while its =1 factored-pulse switch is on.
    let factored_pulse =
      rt.shared.poly.lock().unwrap_or_else(|e| e.into_inner()).factored_pulse_hz(rt.grid_index);
    // The note's gain components: the slot's amplitude (`timbre.gain`, above) and
    // the grid's fader (`fader_gain`, stamped by `note_on` from the shared per-grid
    // state) are stored separately and multiplied at render time, so a later fader
    // move never has to touch this note's slot amplitude.
    // (A stolen legato voice keeps ITS timbre and gain -- it is the same voice.)
    let stole = legato_from
      .map(|from| {
        rt.sink.note_on_legato(from, cell, pitch, rt.knobs.slide_duration_secs, factored_pulse)
      })
      .unwrap_or(false);
    if !stole {
      match source {
        Some(from) => rt.sink.note_on_gliding(
          cell, pitch, from, timbre, rt.knobs.slide_duration_secs, factored_pulse,
        ),
        // The ordinary retrigger-in-place case: continue the just-cut drone's
        // oscillator and pulse; with nothing cut, a plain fresh note (phase 0,
        // the grid's onset pulse).
        None => match retrigger {
          Some(cut) => {
            rt.sink.note_on_continuing(cell, pitch, timbre, cut);
            drone_became_fingered(rt, pitch, cell);
          }
          None => match layered {
            Some((_, gain)) => rt.sink.note_on_layered(cell, pitch, timbre, gain),
            None => rt.sink.note_on(cell, pitch, timbre, factored_pulse),
          },
        },
      }
    }
    if let Some((_, gain)) = layered {
      // Covers the sustaining-retrigger and dormant slide/mono paths as well as
      // fresh notes; this rig declares neither slide nor mono, but keeping the
      // layer invariant here makes the code robust to a later rig edit.
      synth::set_layer_gain(&rt.shared.voices, rt.grid_index, false, gain);
    }
    held.insert(cell, pitch);
    {
      let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
      let gr = &mut rings[rt.grid_index];
      gr.accrete.note_played(pitch, &mut gr.store);
    }
    push_trail(
      &rt.shared.trail, pitch.rem_euclid(rt.tuning.edo), rt.tuning.edo,
      rt.knobs.trail_clobber_radius, rt.knobs.trails_max,
    );
  } else {
    // A fine-transpose key coming up snaps the transpose to the most recent key
    // still held (the LAST release keeps it -- no revert to 0). A finger that
    // predates the mode is not in the transpose stack and releases normally even
    // while the mode is on.
    let fine_release = rt.fine.on.then(|| rt.fine.release(cell)).flatten();
    if let Some(delta) = fine_release {
      if delta != 0 {
        shift_edited_voices(rt, held, delta);
      }
    } else {
      release_cell(rt, held, cell);
    }
  }
  publish_held(&rt.shared.held_all, rt.grid_index, held);
  publish_sounding(&rt.shared.sounding, rt.grid_index, held, rt.tuning.edo);
}

fn capture_momentary_pitches(
  rt: &GridThread,
  held: &HashMap<(i32, i32), i32>,
) -> Vec<i32> {
  let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
  let gr = &rings[rt.grid_index];
  let pitches: Vec<i32> = held
    .values()
    .copied()
    .chain(gr.store.iter(Reason::Sustain))
    .chain(gr.momentary_chord.live_pitches())
    .collect();
  momentary_chords::normalized(&pitches)
}

pub(super) fn persist_momentary(rt: &GridThread) {
  let Some(target) = rt.shared.momentary_persist.as_ref() else {
    return;
  };
  let present = {
    let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let mut present = Vec::new();
    for (i, id) in target.device_ids.iter().enumerate() {
      if let Some(id) = id {
        present.push((id.clone(), rings[i].momentary_chord.slots.to_vec()));
      }
    }
    present
  };
  let mut all = target.all.lock().unwrap_or_else(|e| e.into_inner());
  for (id, slots) in present {
    all.insert(id, slots);
  }
  super::momentary_chords_persist::save(&target.path, &all);
}

/// Target/arm/eight-slot key edges for the pitch-only momentary block.
fn momentary_chord_key(
  rt: &mut GridThread,
  held: &HashMap<(i32, i32), i32>,
  cell: (i32, i32),
  press: bool,
) {
  let Some(hit) = momentary_chords::block_cell(rt.overlays.momentary_chord_rect, cell) else {
    return;
  };
  match hit {
    momentary_chords::BlockCell::Target => {
      if press {
        let mut controls = rt.shared.layer_controls.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = controls.get_mut(rt.grid_index).filter(|state| state.enabled) {
          state.toggle_target();
        }
      }
    }
    momentary_chords::BlockCell::Arm => {
      if !press {
        return;
      }
      let has_held_sources = {
        let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        !rings[rt.grid_index].momentary_chord.held_slots().is_empty()
      };
      if has_held_sources {
        let pitches = capture_momentary_pitches(rt, held);
        {
          let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
          let layer = &mut rings[rt.grid_index].momentary_chord;
          layer.overwrite_held(&pitches);
          layer.armed = false;
        }
        if !pitches.is_empty() {
          persist_momentary(rt);
        }
      } else {
        let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        let layer = &mut rings[rt.grid_index].momentary_chord;
        layer.armed = !layer.armed;
      }
    }
    momentary_chords::BlockCell::Slot(slot) => {
      if press {
        let armed = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
          .momentary_chord
          .armed;
        if armed {
          let pitches = capture_momentary_pitches(rt, held);
          {
            let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
            rings[rt.grid_index].momentary_chord.save(slot, &pitches);
          }
          if !pitches.is_empty() {
            persist_momentary(rt);
          }
        } else {
          let spawned = {
            let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
            rings[rt.grid_index].momentary_chord.press(slot)
          };
          let (timbre, gain) = {
            let controls = rt.shared.layer_controls.lock().unwrap_or_else(|e| e.into_inner());
            let state = &controls[rt.grid_index];
            (
              timbre_at(rt, state.selected_for(LayerTarget::Chord)),
              state.gain_for(LayerTarget::Chord),
            )
          };
          for (seq, pitch) in spawned {
            rt.sink.spawn_momentary_chord_voice(seq, pitch, timbre, gain);
          }
        }
      } else {
        let seqs = {
          let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
          rings[rt.grid_index].momentary_chord.release(slot)
        };
        let (release, sample_rate) = rt.sink.release_params();
        synth::end_chord_voices(
          &rt.shared.voices,
          rt.grid_index,
          &seqs,
          release,
          sample_rate,
        );
      }
    }
  }
}

fn shift_momentary_chords(rt: &mut GridThread, delta: i32) {
  let moved = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    rings[rt.grid_index].momentary_chord.shift_live(delta)
  };
  for (seq, pitch) in moved {
    rt.sink.glide_chord_voice(seq, pitch, rt.knobs.slide_duration_secs);
  }
}

/// A key-down inside the chord block: the arm toggle, a save (armed), or a recall
/// toggle (disarmed). Decisions happen under the ring lock; voices are touched
/// after it drops (the module's no-nested-locks rule).
pub(super) fn chord_block_press(
  rt: &mut GridThread,
  held: &HashMap<(i32, i32), i32>,
  cell: (i32, i32),
) {
  let Some(hit) = chords::block_cell(rt.overlays.chord_rect, cell) else {
    return;
  };
  match hit {
    chords::BlockCell::Arm => {
      let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
      let layer = &mut rings[rt.grid_index].chord;
      layer.armed = !layer.armed;
    }
    chords::BlockCell::Slot(slot) => {
      let armed = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
        .chord
        .armed;
      if armed {
        chord_save(rt, held, slot);
      } else if rt.pedal_slide.mode() {
        chord_slide_to(rt, held, slot);
      } else {
        chord_toggle(rt, slot);
      }
    }
  }
}

/// Save "every voice this monome is currently sounding" (1_vision) into `slot`:
/// fingered voices, sustained drones, and live chord voices -- release tails
/// excluded by construction (none of the three registries name them). Each input is
/// snapshotted under its own lock, sequentially. An empty capture only disarms.
fn chord_save(rt: &mut GridThread, held: &HashMap<(i32, i32), i32>, slot: usize) {
  let (sustained, live): (Vec<i32>, Vec<(u64, i32)>) = {
    let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &rings[rt.grid_index];
    (
      gr.store.iter(Reason::Sustain).collect(),
      gr.chord.live.iter().map(|(s, v)| (*s, v.pitch)).collect(),
    )
  };
  let base_hz = rt
    .shared
    .poly
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .tapped_hz()
    .unwrap_or(0.0);
  let mut parts: Vec<(i32, VoiceState)> = {
    let voices = rt.shared.voices.lock().unwrap_or_else(|e| e.into_inner());
    let grid = rt.grid_index;
    let mut parts = Vec::new();
    for (c, pitch) in held {
      if let Some(s) = voices.get(&VoiceSource::SurfaceFinger { grid, cell: *c }) {
        parts.push((*pitch, *s));
      }
    }
    // A sustained pitch still under a finger has no drone voice -- its finger voice
    // was captured above; the lookup simply misses.
    for pitch in sustained {
      if let Some(s) = voices.get(&VoiceSource::SurfaceDrone { grid, pitch }) {
        parts.push((pitch, *s));
      }
    }
    for (seq, pitch) in live {
      if let Some(s) = voices.get(&VoiceSource::SurfaceChord { grid, seq }) {
        parts.push((pitch, *s));
      }
    }
    parts
  };
  // Deterministic slot order (and deterministic timesetter tie-breaks): the held
  // and live registries iterate in arbitrary hash order.
  parts.sort_by_key(|(pitch, _)| *pitch);
  let chord = chords::snapshot(&parts, base_hz, rt.tuning.fund, rt.tuning.edo);
  let file = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    rings[rt.grid_index].chord.save(slot, chord);
    // Persist on every save (crash loses nothing): snapshot every grid's slots
    // under the same lock, write the file after it drops.
    rt.shared.persist.as_ref().map(|p| {
      let all: super::chords_persist::AllSlots = p
        .monome_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), rings[i].chord.slots.to_vec()))
        .collect();
      (Arc::clone(p), all)
    })
  };
  if let Some((p, all)) = file {
    super::chords_persist::save(&p.path, &all);
  }
}

/// Toggle `slot`: OFF ends its recall's voices (release ramp); ON spawns the stored
/// chord's voices at the current base tempo (an empty slot is inert).
fn chord_toggle(rt: &mut GridThread, slot: usize) {
  let base_hz = rt
    .shared
    .poly
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .tapped_hz()
    .unwrap_or(0.0);
  enum Act {
    Spawn(Vec<(u64, chords::StoredVoice)>),
    End(Vec<u64>),
  }
  let act = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let layer = &mut rings[rt.grid_index].chord;
    if layer.active[slot] {
      Act::End(layer.end_recall(slot))
    } else {
      Act::Spawn(layer.begin_recall(slot))
    }
  };
  match act {
    Act::Spawn(list) => {
      for (seq, v) in &list {
        rt.sink.spawn_chord_voice(*seq, v, base_hz);
      }
    }
    Act::End(seqs) => {
      let (release_secs, sample_rate) = rt.sink.release_params();
      synth::end_chord_voices(&rt.shared.voices, rt.grid_index, &seqs, release_secs, sample_rate);
    }
  }
}

/// A slot press while pedal slide is ON does not RECALL the chord -- it aims at it
/// (`1_vision`: "during slide mode, at most one chord can be selected from storage at a
/// time. Its voices all join the target set"). The stored chord's pitches become the
/// target set, and the voices already sounding on this grid glide into them.
///
/// This is bug 1. The reverted build offered the matcher only the EDIT-MODE voices, so
/// a recalled chord found nothing to pair with: every old voice faded out and every new
/// pitch faded in, which is the crossfade Jeff heard instead of a glide. The candidate
/// list here is the whole managed set the vision implies -- the chord voices already
/// sounding AND the edit-mode voices.
///
/// The chord LAYER is deliberately untouched: no `begin_recall`, no live registry
/// entries, no `active` flag. Those describe voices that exist, and under slide mode the
/// stored chord contributes only pitches. The slot lights through the pedal-slide
/// engine instead (see `target_slot`), so chord storage's own invariants stay intact.
fn chord_slide_to(rt: &mut GridThread, held: &HashMap<(i32, i32), i32>, slot: usize) {
  let grid = rt.grid_index;
  let (targets, candidates): (Vec<i32>, Vec<(VoiceSource, i32)>) = {
    let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &rings[grid];
    let Some(stored) = gr.chord.slots[slot].as_ref() else {
      return; // an empty slot is inert, exactly as it is outside slide mode
    };
    let targets = stored.voices.iter().map(|v| v.pitch).collect();
    // Every voice the pedal may move: the chord voices already sounding (seq-keyed,
    // never in the edit set -- the ones the reverted build could not see) and this
    // grid's edit-mode voices (cell-keyed while fingered, pitch-keyed as drones).
    let mut candidates: Vec<(VoiceSource, i32)> = gr
      .chord
      .live
      .iter()
      .map(|(seq, v)| (VoiceSource::SurfaceChord { grid, seq: *seq }, v.pitch))
      .collect();
    for pitch in gr.store.iter(Reason::Edit) {
      let key = match held.iter().find(|(_, hp)| **hp == pitch) {
        Some((c, _)) => VoiceSource::SurfaceFinger { grid, cell: *c },
        None => VoiceSource::SurfaceDrone { grid, pitch },
      };
      candidates.push((key, pitch));
    }
    (targets, candidates)
  };
  let outcome = rt.pedal_slide.match_targets(&targets, &candidates);
  rt.pedal_slide.set_target_slot(Some(slot));
  // Spare targets have nobody to glide into them, so they swell in from silence -- the
  // vision's "just fade in the other voices".
  for pitch in outcome.spawn_fade_ins {
    let ts = rt.timbres[current_slot(&rt.shared.selected, grid)];
    let timbre = Timbre {
      waveform: ts.waveform,
      gain: ts.amplitude,
      am: ts.am,
      fm: ts.fm,
      rel_am: ts.rel_am,
      rel_fm: ts.rel_fm,
    };
    if let Some(key) = rt.sink.spawn_slide_swell(pitch, timbre) {
      rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[grid]
        .store
        .add(Reason::Sustain, pitch);
      rt.pedal_slide.register_fade(key, pitch, pedal_slide::FadeDir::In);
    }
  }
  let drives = rt.pedal_slide.drives();
  let frozen = rt.sink.frozen_grid_gain();
  synth::apply_slide_drives(&rt.shared.voices, &drives, frozen, rt.tuning.fund, rt.tuning.edo);
}

/// The octave switchers' edit-mode action (queues/branch-2.org): move EVERY edited
/// voice -- both layers at once, like the multipliers, not the drag's nearest-one --
/// by `delta` steps (one octave = ±edo). Returns false when nothing is selected, so
/// the caller falls through to the ordinary register scroll. The register never
/// moves here; pitch CLASSES are octave-invariant, so the trail and the bright
/// reflection stay put -- only the exact-cell markers (the dances) and the
/// off-screen corners react. A pure glide, no envelope event: the bulk edit
/// controls (the multipliers) are click-free, and this is one of them.
fn shift_edited_voices(
  rt: &mut GridThread,
  held: &mut HashMap<(i32, i32), i32>,
  delta: i32,
) -> bool {
  // Decide + re-file every registry under the ring lock; glide voices after it
  // drops. The set shifts are computed WHOLE (discard all, then add all): moving
  // pitches one at a time would collapse a chain of edited pitches an octave
  // apart.
  let (piano, chords): (Vec<i32>, Vec<(u64, i32)>) = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &mut rings[rt.grid_index];
    let piano: Vec<i32> = gr.store.iter(Reason::Edit).collect();
    let chords: Vec<(u64, i32)> =
      gr.chord.live.iter().filter(|(_, v)| v.edited).map(|(s, v)| (*s, v.pitch)).collect();
    if piano.is_empty() && chords.is_empty() {
      return false;
    }
    for p in &piano {
      gr.store.discard(Reason::Edit, *p);
      gr.store.discard(Reason::Sustain, *p);
    }
    for p in &piano {
      gr.store.add(Reason::Edit, *p + delta);
      gr.store.add(Reason::Sustain, *p + delta);
    }
    for (seq, pitch) in &chords {
      if let Some(v) = gr.chord.live.get_mut(seq) {
        v.pitch = *pitch + delta;
      }
    }
    (piano, chords)
  };
  let glide = rt.knobs.slide_duration_secs;
  // Fingered edited voices glide in place (they are cell-keyed) and re-file `held`.
  let moved_cells: Vec<((i32, i32), i32)> =
    held.iter().filter(|(_, p)| piano.contains(p)).map(|(c, p)| (*c, *p)).collect();
  for (c, p) in &moved_cells {
    rt.sink.glide_voice_to(*c, *p + delta, glide);
    held.insert(*c, *p + delta);
  }
  // Drones are PITCH-keyed, so a chain of edited drones an octave apart must
  // re-key starting from the end the move points at (highest first going up,
  // lowest first going down), or one drone lands on the next one's key.
  let fingered: HashSet<i32> = moved_cells.iter().map(|(_, p)| *p).collect();
  let mut drones: Vec<i32> = piano.iter().copied().filter(|p| !fingered.contains(p)).collect();
  drones.sort_unstable();
  if delta > 0 {
    drones.reverse();
  }
  for p in drones {
    rt.sink.glide_sustained_to(p, p + delta, glide);
  }
  for (seq, pitch) in chords {
    rt.sink.glide_chord_voice(seq, pitch + delta, glide);
  }
  true
}

/// The one release path (finger up, or a mono cut): a note in the sustained set --
/// or released under a live accreting condition -- keeps ringing (its voice moves to
/// the sustain register); anything else releases normally and becomes a slide
/// candidate (sustained notes don't: they are still audible, and sliding "from" a
/// ringing drone would double it).
pub(super) fn release_cell(rt: &mut GridThread, held: &mut HashMap<(i32, i32), i32>, cell: (i32, i32)) {
  let Some(pitch) = held.get(&cell).copied() else {
    rt.sink.note_off(cell);
    return;
  };
  // A note rings without a finger only if it is sustained (pedal, per-note button, or
  // -- since editing implies sustaining, edited ⊆ sustained -- because it is being
  // edited). So the single sustained check answers it; `note_released_sustains` writes
  // through to the store (joining/leaving the sustained set per the accrete condition).
  let sustains = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &mut rings[rt.grid_index];
    gr.accrete.note_released_sustains(pitch, &mut gr.store)
  };
  if sustains {
    rt.sink.sustain_note(cell, pitch);
    // The voice just moved from its cell key to its pitch key. If the pedal is sliding
    // it, the pairing has to follow -- otherwise it addresses a key the voice map no
    // longer has and the note freezes mid-glide (the reverted build's disease, wearing
    // a different hat).
    became_a_drone(rt, cell, pitch);
  } else {
    rt.sink.note_off(cell);
    let mono = rt.shared.mono_on[rt.grid_index].load(Ordering::Relaxed);
    rt.slide.note_released(pitch, Instant::now(), mono);
  }
  held.remove(&cell);
}

/// Mono's cut of the note a slide is about to glide from: like `release_cell`,
/// but a note that would release audibly is NOT released -- its voice is left
/// sounding at `cell` for `note_on_legato` to steal, and it never becomes a slide
/// candidate (it keeps sounding). A note that sustains (accrete) becomes a drone
/// as usual and cannot be stolen. Returns true if the voice awaits stealing.
pub(super) fn cut_for_legato(
  rt: &mut GridThread,
  held: &mut HashMap<(i32, i32), i32>,
  cell: (i32, i32),
) -> bool {
  let Some(pitch) = held.get(&cell).copied() else {
    return false;
  };
  let keep = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &mut rings[rt.grid_index];
    gr.accrete.note_released_sustains(pitch, &mut gr.store)
  };
  held.remove(&cell);
  if keep {
    rt.sink.sustain_note(cell, pitch);
    became_a_drone(rt, cell, pitch);
    return false;
  }
  true
}

/// A fingered voice at `cell` just became the drone at `pitch` (`sustain_note`). Tell
/// the pedal-slide engine, so a slide in flight keeps addressing the voice that exists.
/// Inert when the pedal is not sliding this voice, which is nearly always.
fn became_a_drone(rt: &mut GridThread, cell: (i32, i32), pitch: i32) {
  let grid = rt.grid_index;
  rt.pedal_slide.rekey_voice(
    VoiceSource::SurfaceFinger { grid, cell },
    VoiceSource::SurfaceDrone { grid, pitch },
  );
}

/// The mirror: a finger landed on a sliding DRONE and took it over (`cut_sustained` +
/// `note_on_continuing`), so the voice is cell-keyed now. Same reason as
/// `became_a_drone` -- a retrigger is an envelope event, and it must not quietly
/// detach the note from the pedal that is sliding it.
fn drone_became_fingered(rt: &mut GridThread, pitch: i32, cell: (i32, i32)) {
  let grid = rt.grid_index;
  rt.pedal_slide.rekey_voice(
    VoiceSource::SurfaceDrone { grid, pitch },
    VoiceSource::SurfaceFinger { grid, cell },
  );
}

/// Mirror this grid's held map into the shared per-grid registry (for accrete's
/// capture-on-activation, which the pedal hook must reach from outside this thread).
pub(super) fn publish_held(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  index: usize,
  held: &HashMap<(i32, i32), i32>,
) {
  let mut all = held_all.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(slot) = all.get_mut(index) {
    *slot = held.clone();
  }
}

/// The accreting condition just turned on for this grid's bank: add every voice
/// sounding on this grid -- the notes currently held AND the pitches in edit mode
/// (queue.org: "accrete-sustain should add every fingered voice and every
/// edit-mode voice"; an edited drone is a sounding voice like any other). Snapshot
/// each registry first, then feed the bank -- short, non-nested locks.
pub(super) fn capture_grid_held(rt: &GridThread) {
  capture_grid_held_into(&rt.shared.held_all, &rt.shared.ring, rt.grid_index);
}

/// `capture_grid_held` for callers that aren't a grid thread (the pedal hook).
pub(super) fn capture_grid_held_into(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  ring: &Arc<Mutex<Vec<GridRing>>>,
  grid: usize,
) {
  // Snapshot the held (fingered) pitches under their own lock first, then feed the
  // bank -- short, non-nested locks. Every sounding voice joins: the fingered notes
  // AND the pitches ringing only because they are in edit mode.
  let held = held_pitches(held_all, grid);
  let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(gr) = rings.get_mut(grid) {
    let edited: Vec<i32> = gr.store.iter(Reason::Edit).collect();
    gr.accrete.capture_held(held.into_iter().chain(edited), &mut gr.store);
  }
}

/// The erase mirror of `capture_grid_held`: when the erasing condition turns on,
/// the notes currently held on this grid leave the sustained set (they keep
/// sounding under their fingers).
pub(super) fn erase_grid_held(rt: &GridThread) {
  erase_grid_held_into(&rt.shared.held_all, &rt.shared.ring, rt.grid_index);
}

pub(super) fn erase_grid_held_into(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  ring: &Arc<Mutex<Vec<GridRing>>>,
  grid: usize,
) {
  let snapshot = held_pitches(held_all, grid);
  let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(gr) = rings.get_mut(grid) {
    gr.accrete.erase_held(snapshot, &mut gr.store);
  }
}

/// Snapshot one grid's currently-held pitches (its own lock; never nested).
pub(super) fn held_pitches(
  held_all: &Arc<Mutex<Vec<HashMap<(i32, i32), i32>>>>,
  grid: usize,
) -> Vec<i32> {
  let all = held_all.lock().unwrap_or_else(|e| e.into_inner());
  all.get(grid).map(|m| m.values().copied().collect()).unwrap_or_default()
}

/// Every cell of this grid's play surface that sounds exactly `pitch` under the
/// current register. Usually one, but a grid can hold the same pitch twice (with
/// `x_step = 9`, `(x, y)` and `(x+1, y-9)` collide), and Jeff wants both to dance.
pub(super) fn cells_for_pitch(rt: &GridThread, register: i32, pitch: i32) -> Vec<(i32, i32)> {
  let [ex0, ey0, ex1, ey1] = rt.overlays.edo_rect;
  let mut out = vec![];
  for y in ey0..=ey1 {
    for x in ex0..=ex1 {
      if step_for_cell(rt.tuning.x_step, rt.tuning.y_step, register, x, y) == pitch {
        out.push((x, y));
      }
    }
  }
  out
}

/// The edit-mode half of a play-cell press. Returns whether the press should fall
/// through to the ordinary play path.
///
/// Runs first because both of its outcomes are SILENT: the cell under a sounding note
/// is an edit trigger and never sounds, and while anything on this grid is being
/// edited every other press drags instead of playing (2_discussion 2b/2c).
///
/// `cell_above` is geometric, not pitch-derived: Jeff pinned the gesture to the cell
/// physically below a note so it doesn't move with the tuning. A note on the bottom
/// row therefore has no trigger cell and cannot be edited.
pub(super) fn handle_edit_press(
  rt: &mut GridThread,
  held: &mut HashMap<(i32, i32), i32>,
  cell: (i32, i32),
  pitch: i32,
  register: &i32,
) -> bool {
  // Both triggers act on a NEIGHBOUR of the pressed cell, and both are geometric
  // rather than pitch-defined -- Jeff pinned them to physical position so they don't
  // move when the tuning changes. Press BELOW a note to edit it, ABOVE it to sustain
  // it. A note on the top or bottom row therefore has no trigger cell on that side.
  let on_grid = |y: i32| y >= rt.overlays.edo_rect[1] && y <= rt.overlays.edo_rect[3];
  let neighbour = |dy: i32| {
    on_grid(cell.1 + dy)
      .then(|| step_for_cell(rt.tuning.x_step, rt.tuning.y_step, *register, cell.0, cell.1 + dy))
  };
  let edit_target = neighbour(-1); // the note above the pressed cell
  let sustain_target = neighbour(1); // the note below it

  // Decide under the ring lock, act after it drops: the voice touches below happen
  // after the lock, and this module's rule is no nested locks. One lock covers
  // sustain, edit, AND the chord layer (they share the ring), so there is no
  // ordering to get wrong. A note is audible if a finger holds it, it is sustained,
  // or a CHORD-layer voice sounds it (the handles work on chord voices too).
  enum Act {
    Play,
    // Enter edit / exit edit / an inert handle: silent, and ends no voice. Any
    // store / chord-flag mutation already happened under the lock.
    Silent,
    Sustain(i32, bool, Vec<u64>),
    Dragged { from: i32, to: i32, piano_moved: bool, chord_seqs: Vec<u64> },
    /// Pedal slide (TODO/pedal-slide): a press on a FREE pitch while this grid's slide
    /// mode is on PICKS a target instead of dragging or sounding. Silent -- the pedal
    /// does the moving, which is the whole point.
    SlidePick(i32),
  }
  // While pedal slide is on, the play surface is a target-picker, exactly as edit mode
  // already makes it a drag-picker. The gestures that SELECT what to slide (the edit
  // handles) and the retrigger of an already-sounding pitch are untouched -- only the
  // outcomes that would have moved or started a note become picks.
  let slide_mode = rt.pedal_slide.mode();
  let act = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &mut rings[rt.grid_index];
    let sustained: HashSet<i32> = gr.store.iter(Reason::Sustain).collect();
    let chord_pitches: HashSet<i32> = gr.chord.live_pitches().collect();
    // The full edit SELECTION: the piano layer's edited pitches (⊆ sustained)
    // unioned with the edit-flagged chord voices' pitches (NOT sustained -- Jeff's
    // round-2 correction: editing a chord voice adds no sustain reason).
    let mut edited_union: HashSet<i32> = gr.store.iter(Reason::Edit).collect();
    edited_union.extend(gr.chord.live.values().filter(|v| v.edited).map(|v| v.pitch));
    let is_sounding =
      |p: i32| held.values().any(|h| *h == p) || sustained.contains(&p) || chord_pitches.contains(&p);
    // Branch-3 queue item 6: a press only drags the nearest edited note when the
    // pressed pitch is free of the editing set and the sustain set; otherwise it
    // retriggers (classify also consults `edited_union` for the chord half).
    let is_sustained = |p: i32| sustained.contains(&p);
    let press =
      gr.edit.classify(edit_target, sustain_target, pitch, is_sounding, is_sustained, &edited_union);
    // The pedal-slide interception. A Drag (something is edited, this pitch is free)
    // becomes a pick of that pitch as a TARGET; a plain Play on a genuinely free pitch
    // becomes a pick too. Everything else falls through unchanged: a press on a
    // SOUNDING pitch still retriggers, and the edit handles still enter/exit/sustain,
    // because those are how you choose what the pedal will slide.
    let slide_pick = if slide_mode {
      match press {
        edit::Press::Drag { to, .. } => Some(to),
        edit::Press::Play if !is_sounding(pitch) => Some(pitch),
        _ => None,
      }
    } else {
      None
    };
    if let Some(target) = slide_pick {
      Act::SlidePick(target)
    } else {
    match press {
      edit::Press::Play => Act::Play,
      edit::Press::EnterEdit { pitch } => {
        // One press takes BOTH layers at this pitch into the selection (Jeff: "if
        // there is a sustained or fingered voice at the same key, they both enter
        // edit mode"). The piano layer enters only when it has a voice here --
        // `enter` adds a sustain reason, which a voiceless pitch must not get; the
        // chord voices just flip their flags (no sustain implied).
        if held.values().any(|h| *h == pitch) || sustained.contains(&pitch) {
          gr.edit.enter(pitch, &mut gr.store);
        }
        for v in gr.chord.live.values_mut() {
          if v.pitch == pitch {
            v.edited = true;
          }
        }
        Act::Silent
      }
      edit::Press::ExitEdit { pitch } => {
        // Pure deselection in both layers: the piano note stays sustained, the chord
        // voice keeps its chord reason -- leaving edit mode ends no voice.
        gr.edit.exit(pitch, &mut gr.store);
        for v in gr.chord.live.values_mut() {
          if v.pitch == pitch {
            v.edited = false;
          }
        }
        Act::Silent
      }
      edit::Press::ToggleSustain { pitch } => {
        // The local end-sustain control is ORIGIN-BLIND (queues/branch-2.org:
        // "local end sustain should also end a chord voice there -- for those
        // controls, the origin doesn't matter"): toggling a sustained pitch OFF
        // also ends the chord voices ringing at it, and on a pitch where ONLY
        // chord voices ring the press ends them (there is no piano voice to
        // sustain, and "adding sustain" to an already-ringing voice would mean
        // nothing). Only the ON direction -- a fingered, unsustained pitch --
        // touches no chord voice. This supersedes the round-2 "does nothing"
        // ruling for the chord-only case.
        let has_piano = held.values().any(|h| *h == pitch) || sustained.contains(&pitch);
        if has_piano && !sustained.contains(&pitch) {
          Act::Sustain(pitch, true, Vec::new())
        } else {
          let chord_seqs = gr.chord.end_at_pitch(pitch);
          if has_piano || !chord_seqs.is_empty() {
            Act::Sustain(pitch, false, chord_seqs)
          } else {
            Act::Silent
          }
        }
      }
      edit::Press::Drag { from, to } => {
        // Move every EDITED voice at the nearest edited pitch, in both layers. The
        // piano layer re-files the pitch in both reason sets (so a clear can't miss
        // it under the old name); each edit-flagged chord voice re-files its
        // registry pitch and glides in place -- a chord voice is never re-homed
        // onto the drag finger (it stays a chord voice; the press is a control
        // gesture, and its release must release nothing).
        let piano_moved = gr.store.has(Reason::Edit, from);
        if piano_moved {
          gr.store.note_moved(from, to);
        }
        let mut chord_seqs = Vec::new();
        for (seq, v) in gr.chord.live.iter_mut() {
          if v.edited && v.pitch == from {
            v.pitch = to;
            chord_seqs.push(*seq);
          }
        }
        Act::Dragged { from, to, piano_moved, chord_seqs }
      }
    }
    }
  };

  let (release_secs, sample_rate) = rt.sink.release_params();
  match act {
    Act::Play => return true,
    // Enter, exit, the editmode clear, and the inert sustain handle all end nothing.
    Act::Silent => {}
    Act::Sustain(pitch, on, chord_seqs) => {
      if on {
        // Switching sustain ON starts nothing at the voice level: a fingered note has
        // no drone yet (it gets one when the finger lifts), and a note ringing because
        // it is edited already has one. Just add the reason.
        rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
          .store
          .add(Reason::Sustain, pitch);
      } else {
        // Remove the SUSTAIN reason and end the drone iff no finger still holds it. This
        // also deselects the pitch (`remove_sustain` cascades edit membership away:
        // nothing silent may stay selected), so toggling sustain off an edited note ends
        // AND deselects it. (A chord-only pitch was never in the set -- the removal is
        // then a harmless no-op and only the chord half below acts.)
        let ended = {
          let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
          rings[rt.grid_index].store.remove_sustain([pitch], |p| finger_count(held, p))
        };
        synth::end_drones_at(
          &rt.shared.voices, rt.grid_index, &ended.into_iter().collect(), release_secs, sample_rate,
        );
        // The chord half of the origin-blind local end-sustain: the registry was
        // already pruned (and emptied slots untoggled) under the ring lock.
        synth::end_chord_voices(
          &rt.shared.voices, rt.grid_index, &chord_seqs, release_secs, sample_rate,
        );
      }
    }
    Act::Dragged { from, to, piano_moved, chord_seqs } => {
      if piano_moved {
        // The finger that pressed `cell` now holds the piano-layer voice, so re-home
        // it to `cell` as a fingered voice. It was either fingered at some old cell,
        // or a drone (its original finger having lifted while edited). Either way,
        // adopting it here is what fixes Jeff's bug: a voice dragged and then taken
        // out of edit mode used to die, because it stayed a drone that the exit cut
        // while the drag finger -- invisible to `held` -- was still down.
        let old_cell = held.iter().find(|(_, p)| **p == from).map(|(c, _)| *c);
        rt.sink.rehome_to_cell(old_cell, from, cell, to, rt.knobs.slide_duration_secs);
        if let Some(oc) = old_cell {
          held.remove(&oc);
        }
        held.insert(cell, to);
      }
      // The chord half: glide each moved chord voice in place (key unchanged; the
      // registry pitch was re-filed under the lock). A chord-only drag touches
      // `held` not at all -- releasing the drag finger must release nothing.
      for seq in chord_seqs {
        rt.sink.glide_chord_voice(seq, to, rt.knobs.slide_duration_secs);
      }
      // The registries were re-filed under the new pitch during classify; the trail
      // tracks pitches too, so add the new one or it would keep showing a pitch
      // that moved.
      push_trail(
        &rt.shared.trail, to.rem_euclid(rt.tuning.edo), rt.tuning.edo,
        rt.knobs.trail_clobber_radius, rt.knobs.trails_max,
      );
    }
    Act::SlidePick(target) => slide_pick(rt, held, target),
  }
  false
}

/// A pedal-slide pick (`Act::SlidePick`): make `target` the new far goalpost of the
/// nearest managed-set candidate. Silent by construction -- the press chooses a
/// destination, the pedal travels there.
///
/// The candidates are this grid's EDIT-MODE voices, keyed the way the sink keys them: a
/// voice still under a finger by its cell, a fingerless (drone) one by its pitch. That
/// keying is the pairing's identity for the rest of the flight, and it changes only
/// through a confirmed re-file.
fn slide_pick(rt: &mut GridThread, held: &HashMap<(i32, i32), i32>, target: i32) {
  let grid = rt.grid_index;
  let candidates: Vec<(VoiceSource, i32)> = {
    let rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    rings[grid]
      .store
      .iter(Reason::Edit)
      .map(|p| {
        let key = match held.iter().find(|(_, hp)| **hp == p) {
          Some((c, _)) => VoiceSource::SurfaceFinger { grid, cell: *c },
          None => VoiceSource::SurfaceDrone { grid, pitch: p },
        };
        (key, p)
      })
      .collect()
  };
  let outcome = rt.pedal_slide.pick(target, &candidates);
  // Nothing to pair with: the pick becomes a SWELL into a brand-new voice, born silent
  // and raised by the pedal (`2_discussion`: "it makes the pedal a swell into any
  // pitch, and it is exactly the chord case with a one-note chord"). It joins the
  // sustain set, so it is a real drone the clears and handles can reach.
  for pitch in outcome.spawn_fade_ins {
    let slot = rt.timbres[current_slot(&rt.shared.selected, grid)];
    let timbre = Timbre {
      waveform: slot.waveform,
      gain: slot.amplitude,
      am: slot.am,
      fm: slot.fm,
      rel_am: slot.rel_am,
      rel_fm: slot.rel_fm,
    };
    if let Some(key) = rt.sink.spawn_slide_swell(pitch, timbre) {
      rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[grid]
        .store
        .add(Reason::Sustain, pitch);
      rt.pedal_slide.register_fade(key, pitch, pedal_slide::FadeDir::In);
    }
  }
  // Apply this round of drives now rather than waiting for the next repaint: with the
  // pedal parked away from home the pick has an immediate audible consequence, and it
  // should land on the press, not up to a repaint later.
  let drives = rt.pedal_slide.drives();
  let frozen = rt.sink.frozen_grid_gain();
  synth::apply_slide_drives(&rt.shared.voices, &drives, frozen, rt.tuning.fund, rt.tuning.edo);
}

/// How many held cells sound exactly `pitch` right now -- the derived FINGER count
/// (never stored in the ring). Two colliding cells finger one pitch, so this is a
/// count, not a boolean: `remove_sustain` uses it to spare a pitch a finger still holds.
pub(super) fn finger_count(held: &HashMap<(i32, i32), i32>, pitch: i32) -> usize {
  held.values().filter(|&&p| p == pitch).count()
}

pub(super) fn in_overlay(rect: [i32; 4], cell: (i32, i32)) -> bool {
  let [x0, y0, x1, y1] = rect;
  cell.0 >= x0 && cell.0 <= x1 && cell.1 >= y0 && cell.1 <= y1
}

/// Apply a volume-strip press at absolute column `pressed_x`: set the controlled grid's
/// position + gain and rescale its live voices (the fader is *live*, per Jeff).
pub(super) fn set_volume(rt: &GridThread, pressed_x: i32) {
  let cells = volume_cells(rt.overlays.volume_rect);
  if cells <= 0 {
    return;
  }
  let pos = (pressed_x - rt.overlays.volume_rect[0]).clamp(0, cells - 1);
  let gain = volume_gain_for_pos(pos, cells, VOLUME_DB_RANGE);
  let target = rt.knobs.volume_controls_index;
  {
    let mut vp = rt.shared.volume_pos.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = vp.get_mut(target) {
      *slot = pos;
    }
  }
  {
    let mut g = rt.shared.gains.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = g.get_mut(target) {
      *slot = gain;
    }
  }
  // Assign the sounding voices' fader component directly -- their timbre slot's
  // amplitude lives in a separate field and is untouched.
  set_grid_fader_gain(&rt.shared.voices, target, gain);
}

/// Record a just-pressed pitch class in the shared trail. The trail holds up to
/// `trails_max` *distinct* classes, newest first. Playing a note first clears its own
/// class (dedup -- so hammering one note, in any octave, never floods or erases the
/// trail) and every trailed class within `edo / clobber_radius` steps of it (neighbour
/// suppression), then adds it at the front. Both knobs come from the `[trail]` table.
pub(super) fn push_trail(
  trail: &Arc<Mutex<VecDeque<i32>>>,
  class: i32,
  edo: i32,
  clobber_radius: i32,
  trails_max: usize,
) {
  let mut t = trail.lock().unwrap_or_else(|e| e.into_inner());
  // Keep only classes strictly outside the suppression radius (this also drops the new
  // class itself, since its distance is 0).
  t.retain(|&c| pitch_class_distance(c, class, edo) * clobber_radius > edo);
  t.push_front(class);
  while t.len() > trails_max {
    t.pop_back();
  }
}

/// The distance between two pitch classes on the octave circle, in EDO steps (0..=edo/2).
pub(super) fn pitch_class_distance(a: i32, b: i32, edo: i32) -> i32 {
  let d = (a - b).rem_euclid(edo);
  d.min(edo - d)
}
