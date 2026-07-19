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
use crate::rig::EditmodeControlKind;

use crate::types::Timbre;

use super::grid::{slot_for_selector_cell, volume_cells, volume_gain_for_pos};
use super::hooks::{editmode_press, factored_pulse_press};
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
  // Selector: a press sets the *controlled* grid's timbre slot (radio; future notes).
  if let Some(slot) = slot_for_selector_cell(rt.overlays.selector_rect, cell) {
    if press {
      set_slot(&rt.shared.selected, rt.knobs.controls_index, slot);
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
  // Feet-accrete toggle: key-down flips THIS grid's switch (does the softstep
  // mirror this monome's accrete bank?); key-up does nothing.
  if in_overlay(rt.overlays.feet_accrete_rect, cell) {
    if press {
      let _ = rt.shared.feet_accrete_on[rt.grid_index].fetch_xor(true, Ordering::Relaxed);
    }
    return;
  }
  // The editmode buttons, through the same `editmode_press` the softstep pedals
  // run: clear empties THIS grid's edit mode (edit-only drones ring out; sustained
  // and fingered voices keep their other reasons), accrete puts every sounding
  // voice into it. Key-down only; key-up douses the LED.
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
        let (release_secs, sample_rate) = rt.sink.release_params();
        editmode_press(
          rt.grid_index, control,
          &rt.shared.ring, &rt.shared.held_all, &rt.shared.voices,
          release_secs, sample_rate,
        );
      }
      return;
    }
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
    if press {
      // Sustain clear removes the SUSTAIN reason from every sustained pitch and ends
      // exactly the drones that had no other reason -- edited drones keep ringing
      // (audibly, so it is not the old silent "dancing ghost") until editmode clear
      // (or the exit gesture) takes that reason too, and a fingered pitch's finger is
      // never touched. One `remove_reason` call replaces the old flush + keep-set
      // dance.
      let ended = {
        let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        let gr = &mut rings[rt.grid_index];
        gr.accrete.press_clear();
        let all: Vec<i32> = gr.store.iter(Reason::Sustain).collect();
        gr.store.remove_reason(Reason::Sustain, all, |p| finger_count(held, p))
      };
      let (release_secs, sample_rate) = rt.sink.release_params();
      synth::end_drones_at(
        &rt.shared.voices, rt.grid_index, &ended.into_iter().collect(), release_secs, sample_rate,
      );
    } else {
      rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index].accrete.release_clear();
    }
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
    rt.sink.cut_sustained(pitch);
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
        None => rt.sink.note_on(cell, pitch, timbre, factored_pulse),
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
  // A note rings without a finger for either of two independent reasons: it is
  // sustained (pedal, or the per-note button), or it is being edited. Either keeps it.
  // One ring lock answers both, and `note_released_sustains` writes through to the
  // store (joining/leaving the sustained set per the accrete condition).
  let (sustains, editing) = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &mut rings[rt.grid_index];
    let sustains = gr.accrete.note_released_sustains(pitch, &mut gr.store);
    let editing = gr.store.has(Reason::Edit, pitch);
    (sustains, editing)
  };
  if sustains || editing {
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
  // after the lock, and this module's rule is no nested locks. One lock now covers
  // sustain and edit both (they share the store), so there is no ordering to get
  // wrong. A note rings for any of three independent reasons -- a finger on it, a
  // sustain, or being edited -- and both triggers ask only "is it audible", so all
  // three count in `is_sounding`.
  enum Act {
    Play,
    Entered,
    Exited(i32),
    Sustain(i32, bool),
    Dragged(i32, i32),
  }
  let act = {
    let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let gr = &mut rings[rt.grid_index];
    let sustained: HashSet<i32> = gr.store.iter(Reason::Sustain).collect();
    let editing: HashSet<i32> = gr.store.iter(Reason::Edit).collect();
    let is_sounding = |p: i32| {
      held.values().any(|h| *h == p) || sustained.contains(&p) || editing.contains(&p)
    };
    match gr.edit.classify(edit_target, sustain_target, pitch, is_sounding, &gr.store) {
      edit::Press::Play => Act::Play,
      edit::Press::EnterEdit { pitch } => {
        gr.edit.enter(pitch, &mut gr.store);
        Act::Entered
      }
      // The edit reason is removed by `remove_reason` in the act below, so it can end
      // exactly the reason-less drone in the same breath.
      edit::Press::ExitEdit { pitch } => Act::Exited(pitch),
      edit::Press::ToggleSustain { pitch } => Act::Sustain(pitch, !sustained.contains(&pitch)),
      edit::Press::Drag { from, to } => {
        // A drag re-files the pitch in BOTH reason sets at once (the note may be
        // sustained, edited, or both), so a clear can't miss it under the old name and
        // the trail keeps up.
        gr.store.note_moved(from, to);
        Act::Dragged(from, to)
      }
    }
  };

  let (release_secs, sample_rate) = rt.sink.release_params();
  match act {
    Act::Play => return true,
    // Entering needs nothing else: being edited is itself a reason to ring, so the
    // note simply keeps sounding when the finger lifts (`release_cell` asks).
    Act::Entered => {}
    Act::Exited(pitch) => {
      // Take away the EDIT reason and end the drone iff nothing else holds the pitch up
      // -- a sustain, or a finger (a finger has its own voice, so a fingered pitch's
      // finger is never ended here). `end_drones_at` only touches drones, so "exit
      // while still holding the note" is naturally a no-op: that note has a finger, no
      // drone. This is the exit-edit half of the one `remove_reason` operation.
      let ended = {
        let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        rings[rt.grid_index].store.remove_reason(Reason::Edit, [pitch], |p| finger_count(held, p))
      };
      synth::end_drones_at(
        &rt.shared.voices, rt.grid_index, &ended.into_iter().collect(), release_secs, sample_rate,
      );
    }
    Act::Sustain(pitch, on) => {
      if on {
        // Switching sustain ON starts nothing at the voice level: a fingered note has
        // no drone yet (it gets one when the finger lifts), and a note ringing because
        // it is edited already has one. Just add the reason.
        rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner())[rt.grid_index]
          .store
          .add(Reason::Sustain, pitch);
      } else {
        // The mirror of leaving edit mode: remove the SUSTAIN reason, end the drone iff
        // nothing else (edit, or a finger) still holds it.
        let ended = {
          let mut rings = rt.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
          rings[rt.grid_index].store.remove_reason(Reason::Sustain, [pitch], |p| finger_count(held, p))
        };
        synth::end_drones_at(
          &rt.shared.voices, rt.grid_index, &ended.into_iter().collect(), release_secs, sample_rate,
        );
      }
    }
    Act::Dragged(from, to) => {
      // The finger that pressed `cell` now holds this voice, so re-home the voice to
      // `cell` as a fingered voice. It was either fingered at some old cell, or a
      // drone (its original finger having lifted while edited). Either way, adopting
      // it here is what fixes Jeff's bug: a voice dragged and then taken out of edit
      // mode used to die, because it stayed a drone that the exit cut while the drag
      // finger -- invisible to `held` -- was still down.
      let old_cell = held.iter().find(|(_, p)| **p == from).map(|(c, _)| *c);
      rt.sink.rehome_to_cell(old_cell, from, cell, to, rt.knobs.slide_duration_secs);
      if let Some(oc) = old_cell {
        held.remove(&oc);
      }
      held.insert(cell, to);
      // The store was re-filed under the new pitch during classify; the trail tracks
      // pitches too, so add the new one or it would keep showing a pitch that moved.
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
/// count, not a boolean: `remove_reason` uses it to spare a pitch a finger still holds.
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
