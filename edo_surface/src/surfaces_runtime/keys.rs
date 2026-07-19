//! Key handling: `handle_key` (the overlay dispatch), the per-voice edit-mode
//! Act logic (`handle_edit_press`), the release path (`release_cell`,
//! `cut_for_legato`), and the shared per-grid registries it publishes into
//! (`capture_grid_held(_into)` / `erase_grid_held(_into)`, `held_pitches`,
//! `publish_held`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::edo_play::{register_delta, shift_for_cell, step_for_cell};
use crate::rig::{AccreteControlKind, EditmodeControlKind};

use crate::types::{Timbre, VoiceSource, VoiceState};

use super::chords;
use super::grid::{slot_for_selector_cell, volume_cells, volume_gain_for_pos};
use super::hooks::{drive_accrete, editmode_press, factored_pulse_press};
use super::paint::publish_sounding;
use super::polyrhythm::TempoFactorButton;
use super::ring::{GridRing, Reason};
use super::settings::{current_slot, set_slot};
use super::synth::{self, set_grid_fader_gain};
use super::{edit, GridThread, VOLUME_DB_RANGE};

/// Route one debounced key edge by which overlay (if any) it falls in.
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
        let ts = rt.timbres[slot];
        let timbre = Timbre {
          waveform: ts.waveform,
          gain: ts.amplitude,
          am: ts.am,
          fm: ts.fm,
          rel_am: ts.rel_am,
          rel_fm: ts.rel_fm,
        };
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
  // The accrete (sustain) buttons. Each grid's trio acts on ITS OWN bank (misc.org
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
  // Scroll pad: a press moves THIS grid's play register.
  if let Some(shift) = shift_for_cell(rt.overlays.scroll_rect, cell) {
    if press {
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
    // Per-voice edit mode, BEFORE the play path: this press may be an edit trigger or
    // a pitch drag rather than a note, and in both of those cases it must not sound.
    if !handle_edit_press(rt, held, cell, pitch, register) {
      // A trigger or a drag: no note sounds, but `held` may have MOVED (a drag
      // re-pitches a finger's voice), so the shared maps still have to be republished
      // or the other grid reflects a pitch that is no longer sounding.
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
    // `cut_sustained` hands back the outgoing drone's oscillator phase (`None` when
    // no drone rang here) so the plain-note_on branch below can continue it instead
    // of restarting at 0 (branch-3 queue item 5: retrigger must not reset phase).
    let retrigger_phase = rt.sink.cut_sustained(pitch);
    let slot = rt.timbres[current_slot(&rt.shared.selected, rt.grid_index)];
    let timbre = Timbre {
      waveform: slot.waveform,
      // The timbre slot's amplitude alone -- the fader is a separate, stored
      // component (`VoiceState::fader_gain`), stamped by `SurfaceSink::note_on`
      // from the shared per-grid fader gain and multiplied in at render time, not
      // baked in here.
      gain: slot.amplitude,
      am: slot.am,
      fm: slot.fm,
      rel_am: slot.rel_am,
      rel_fm: slot.rel_fm,
    };
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
        // The ordinary retrigger-in-place case: carry the just-cut drone's phase
        // forward (0.0 when there was none to cut, i.e. an ordinary fresh note).
        None => rt.sink.note_on_with_phase(
          cell, pitch, timbre, factored_pulse, retrigger_phase.unwrap_or(0.0),
        ),
      }
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
    release_cell(rt, held, cell);
  }
  publish_held(&rt.shared.held_all, rt.grid_index, held);
  publish_sounding(&rt.shared.sounding, rt.grid_index, held, rt.tuning.edo);
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
    return false;
  }
  true
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
    Sustain(i32, bool),
    Dragged { from: i32, to: i32, piano_moved: bool, chord_seqs: Vec<u64> },
  }
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
    match gr.edit.classify(edit_target, sustain_target, pitch, is_sounding, is_sustained, &edited_union)
    {
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
        // The sustain handle acts on the PIANO layer only. On a pitch where only
        // chord voices sound it is consumed but inert ("pressing the add-to-sustain
        // key above a chord voice does nothing -- it's already ringing"): adding a
        // sustain reason with no voice behind it would sustain nothing and still
        // paint/square-dance as if it did.
        if held.values().any(|h| *h == pitch) || sustained.contains(&pitch) {
          Act::Sustain(pitch, !sustained.contains(&pitch))
        } else {
          Act::Silent
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
  };

  let (release_secs, sample_rate) = rt.sink.release_params();
  match act {
    Act::Play => return true,
    // Enter, exit, the editmode clear, and the inert sustain handle all end nothing.
    Act::Silent => {}
    Act::Sustain(pitch, on) => {
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
        // AND deselects it.
        let ended = {
          let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
          rings[rt.grid_index].store.remove_sustain([pitch], |p| finger_count(held, p))
        };
        synth::end_drones_at(
          &rt.shared.voices, rt.grid_index, &ended.into_iter().collect(), release_secs, sample_rate,
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
  }
  false
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
