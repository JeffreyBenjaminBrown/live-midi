//! Rig -> runtime settings resolution: `Settings` / `GridSettings`, the bring-up
//! plan (`BringUp` / `plan_bringup`) that decides what loads around missing gear,
//! and the timbre-slot table (`TimbreSlot` + its resolve/default helpers and the
//! currently-selected-slot accessors).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use midi_pulse::rig::{
  AccreteControlKind, AmShapeFamilyRig, EditmodeControlKind, MonomeWindowRig, Rig, SinkRig,
  SoftstepWindowRig, WaveformChoice,
};

use crate::types::{Am, AmShapeFamily, Fm, RelAm, RelFm, Waveform};
use crate::voices::Distortion;

use super::grid::SELECTOR_CELLS;
use super::pedal_volume::PedalVolumeCurve;
use super::NO_RECT;

/// One selectable timbre behind a selector cell: the resolved form of a rig
/// `[[timbres]]` entry (or a plain-waveform default). `amplitude` multiplies below
/// the grid's volume fader at note-on.
#[derive(Clone, Copy)]
pub(crate) struct TimbreSlot {
  pub(super) waveform: Waveform,
  pub(super) amplitude: f32,
  pub(super) am: Am,
  pub(super) fm: Fm,
  pub(super) rel_am: RelAm,
  pub(super) rel_fm: RelFm,
}

/// The startup-selected slot: 1 = triangle in the default slots, matching the old
/// `Waveform::default()` behavior (and any custom `[[timbres]]` layout's cell 1).
pub(super) const DEFAULT_SLOT: usize = 1;

/// The pre-`[[timbres]]` behavior: the four plain waveforms, everything else off.
pub(super) fn default_timbre_slots() -> [TimbreSlot; SELECTOR_CELLS] {
  let plain = |waveform| TimbreSlot {
    waveform,
    amplitude: 1.0,
    am: Am::default(),
    fm: Fm::default(),
    rel_am: RelAm::default(),
    rel_fm: RelFm::default(),
  };
  [
    plain(Waveform::Sine),
    plain(Waveform::Triangle),
    plain(Waveform::Square),
    plain(Waveform::Saw),
  ]
}

/// Every overlay rect a play grid can carry, `NO_RECT` where absent. Built once per
/// grid in `resolve_settings` and stored WHOLE in both `GridSettings` (the resolved
/// rig) and `GridThread` (the running thread) -- so adding a new overlay touches this
/// struct and its one resolve site, not three separate field lists (cleaning phase 4,
/// `TODO/cleaning/2_plan.org`).
#[derive(Clone, Copy)]
pub(super) struct Overlays {
  pub(super) edo_rect: [i32; 4],
  pub(super) scroll_rect: [i32; 4],
  pub(super) selector_rect: [i32; 4],
  pub(super) volume_rect: [i32; 4],
  /// The accrete (sustain) buttons' cells, `NO_RECT` when absent. Each grid's
  /// trio (+ optional erase) drives its own per-monome accrete bank.
  pub(super) clear_rect: [i32; 4],
  pub(super) needs_holding_rect: [i32; 4],
  pub(super) accrete_rect: [i32; 4],
  pub(super) erase_rect: [i32; 4],
  /// The global-distortion toggle's cell, `NO_RECT` when absent (one shared switch).
  pub(super) distortion_rect: [i32; 4],
  /// The slide / mono toggles' cells, `NO_RECT` when absent (global switches too).
  pub(super) slide_rect: [i32; 4],
  pub(super) mono_rect: [i32; 4],
  /// The feet-accrete (softstep-accretes) toggle's cell, `NO_RECT` when absent.
  /// Per-grid: it enables the pedal mirror for THIS grid's accrete bank.
  pub(super) feet_accrete_rect: [i32; 4],
  /// The 3x2 polyrhythm pad's rect (x3/x2/tap over /3//2/=1), `NO_RECT` when absent.
  pub(super) poly_rect: [i32; 4],
  /// The editmode_control buttons' cells, `NO_RECT` when absent: clear empties
  /// this grid's edit mode, accrete fills it with every sounding voice.
  pub(super) editmode_clear_rect: [i32; 4],
  pub(super) editmode_accrete_rect: [i32; 4],
}

/// One play grid's resolved rig: its monome binding + its overlay rects + which
/// grid its selector re-timbres.
pub(super) struct GridSettings {
  pub(super) monome_id: String,
  /// This grid's FULL device selector, not just its size. A rig that pins grids by
  /// serial (`select.id_contains`) makes the left/right assignment independent of
  /// serialosc's enumeration order -- which matters the moment anything off-grid
  /// (a foot pedal) targets "the left monome" by name.
  pub(super) select: midi_pulse::rig::MonomeSelect,
  pub(super) listen_port: u16,
  pub(super) prefix: String,
  /// The grid index this grid's waveform selector sets (self if it has no selector).
  pub(super) controls_index: usize,
  /// The grid index this grid's volume strip sets (self if it has no volume strip).
  pub(super) volume_controls_index: usize,
  pub(super) overlays: Overlays,
}

pub(super) struct Settings {
  pub(super) grids: Vec<GridSettings>,
  pub(super) size: [i32; 2],
  pub(super) grid_w: i32,
  pub(super) grid_h: i32,
  pub(super) x_step: i32,
  pub(super) y_step: i32,
  pub(super) edo: i32,
  pub(super) fund: f64,
  pub(super) sample_rate: u32,
  pub(super) buffer_frames: u32,
  /// The synth master gain -- the single "synth volume" knob, from the cpal_synth sink's
  /// `amplitude` (applies to both grids; the per-grid volume strips trim below it).
  pub(super) amplitude: f32,
  pub(super) oversample: u32,
  pub(super) attack: f32,
  pub(super) release: f32,
  /// The pluck envelope (cpal_synth `sustain_level` / `decay_secs`): fresh strikes
  /// peak, then decay toward the sustain so they ring out over held notes.
  pub(super) sustain_level: f32,
  pub(super) decay_secs: f32,
  /// The four selectable timbres (rig `[[timbres]]`, or the plain waveforms).
  pub(super) timbres: [TimbreSlot; SELECTOR_CELLS],
  /// The instrument-wide AM LFO morph family (rig `[am]`).
  pub(super) am_shape_family: AmShapeFamily,
  /// The global distortion's curve (scale + shape from the cpal_synth sink); the
  /// on/off lives in a shared AtomicBool flipped by the distortion_toggle windows.
  pub(super) distortion: Distortion,
  /// The distortion's loudness compensation (cpal_synth `distortion_makeup` /
  /// `distortion_auto_makeup`): the trim, and whether to restore the clean bus's RMS.
  pub(super) distortion_makeup: f32,
  pub(super) distortion_auto_makeup: bool,
  /// Lag on the applied makeup (cpal_synth `distortion_makeup_slew_ms`). 0 = exact.
  pub(super) distortion_makeup_slew_secs: f32,
  pub(super) has_drums: bool,
  /// Trail clobber radius as a divisor of the octave (`[surfaces].trail_clobber_radius`):
  /// a played note clears trailed classes within `edo / this` steps of it.
  pub(super) trail_clobber_radius: i32,
  /// Max distinct pitch classes in the shared trail (`[surfaces].trails_max`).
  pub(super) trails_max: usize,
  /// The slide feature's knobs (`[surfaces]`): how recently a note must have been
  /// released to be a slide source, and how long the glide takes.
  pub(super) slide_window: Duration,
  pub(super) slide_duration_secs: f32,
  /// The tap-tempo pairing window (`[surfaces].tap_tempo_window_ms`).
  pub(super) tap_window: Duration,
  /// Echo each fingered note to stderr (top-level `echo_input`). Off by default so the
  /// startup red report of components that couldn't load stays on screen instead of
  /// scrolling away under key echoes as you play.
  pub(super) echo_input: bool,
  /// The EX-P volume pedals (`[[expression_pedals]]`), resolved to
  /// `(pedal index = MPC-20 channel - 1, grid index, taper)`.
  pub(super) expression_pedals: Vec<(usize, usize, PedalVolumeCurve)>,
}

pub(super) fn resolve_settings(rig: &Rig) -> Result<Settings, Box<dyn std::error::Error>> {
  // The play grids: every declared monome that carries an edo_note_grid, in rig
  // order (grid index = position here).
  let grid_monomes: Vec<&str> = rig
    .monomes
    .iter()
    .map(|m| m.id.as_str())
    .filter(|id| {
      rig.monome_windows.iter().any(|w| {
        matches!(w, MonomeWindowRig::EdoNoteGrid { monome, .. } if monome == id)
      })
    })
    .collect();
  if grid_monomes.is_empty() {
    return Err("a surfaces rig needs at least one edo_note_grid".into());
  }
  let index_of = |monome_id: &str| grid_monomes.iter().position(|m| *m == monome_id);

  // The tuning + sink come from the first edo grid (all grids share them here).
  let (tuning_id, sink_id) = rig
    .monome_windows
    .iter()
    .find_map(|w| match w {
      MonomeWindowRig::EdoNoteGrid { tuning, sink, .. } => Some((tuning.clone(), sink.clone())),
      _ => None,
    })
    .ok_or("a surfaces rig needs an edo_note_grid")?;
  let tuning = rig
    .tunings
    .iter()
    .find(|t| t.id == tuning_id)
    .ok_or("edo_note_grid references an unknown tuning")?;
  let sink = rig
    .sinks
    .iter()
    .find(|s| s.id() == sink_id)
    .ok_or("edo_note_grid references an unknown sink")?;
  let SinkRig::CpalSynth {
    sample_rate, buffer_frames, amplitude, attack_secs, release_secs, oversample,
    distortion_scale, distortion_shape, distortion_makeup, distortion_auto_makeup,
    distortion_makeup_slew_ms, sustain_level, decay_secs, ..
  } = sink
  else {
    return Err("surfaces requires a cpal_synth sink for the play grids".into());
  };

  let rect_on = |monome_id: &str, pred: fn(&MonomeWindowRig) -> bool| {
    rig
      .monome_windows
      .iter()
      .find(|w| w.monome() == monome_id && pred(w))
      .map(|w| w.rect())
  };

  let mut grids = Vec::new();
  for monome_id in &grid_monomes {
    let monome_cfg = rig
      .monomes
      .iter()
      .find(|m| m.id == *monome_id)
      .ok_or("a play-grid monome is not declared")?;
    let edo_rect = rect_on(monome_id, |w| matches!(w, MonomeWindowRig::EdoNoteGrid { .. }))
      .ok_or("a play grid lost its edo_note_grid")?;
    let scroll_rect =
      rect_on(monome_id, |w| matches!(w, MonomeWindowRig::EdoShiftPad { .. })).unwrap_or(NO_RECT);
    // The selector rect + which grid it controls.
    let selector = rig.monome_windows.iter().find_map(|w| match w {
      MonomeWindowRig::WaveformSelector { monome, rect, controls, .. } if monome == monome_id => {
        Some((*rect, controls.clone()))
      }
      _ => None,
    });
    let (selector_rect, controls_index) = match selector {
      Some((rect, controls)) => {
        let idx = index_of(&controls)
          .ok_or("waveform_selector controls a monome that is not a play grid")?;
        (rect, idx)
      }
      None => (NO_RECT, index_of(monome_id).unwrap()),
    };
    // The volume strip rect + which grid it sets the loudness of (like the selector).
    let volume = rig.monome_windows.iter().find_map(|w| match w {
      MonomeWindowRig::VolumeStrip { monome, rect, controls, .. } if monome == monome_id => {
        Some((*rect, controls.clone()))
      }
      _ => None,
    });
    let (volume_rect, volume_controls_index) = match volume {
      Some((rect, controls)) => {
        let idx = index_of(&controls)
          .ok_or("volume_strip controls a monome that is not a play grid")?;
        (rect, idx)
      }
      None => (NO_RECT, index_of(monome_id).unwrap()),
    };
    // The accrete (sustain) buttons, one rect per control kind (validation already
    // guarantees a monome has either the whole trio or none of it).
    let accrete_rect_on = |kind: AccreteControlKind| {
      rig
        .monome_windows
        .iter()
        .find_map(|w| match w {
          MonomeWindowRig::AccreteControl { monome, rect, control, .. }
            if monome == monome_id && *control == kind =>
          {
            Some(*rect)
          }
          _ => None,
        })
        .unwrap_or(NO_RECT)
    };
    let distortion_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::DistortionToggle { monome, rect, .. } if monome == monome_id => {
          Some(*rect)
        }
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let slide_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::SlideToggle { monome, rect, .. } if monome == monome_id => Some(*rect),
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let mono_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::MonoToggle { monome, rect, .. } if monome == monome_id => Some(*rect),
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let feet_accrete_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::SoftstepAccretesToggle { monome, rect, .. } if monome == monome_id => {
          Some(*rect)
        }
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let poly_rect = rig
      .monome_windows
      .iter()
      .find_map(|w| match w {
        MonomeWindowRig::TapTempoPad { monome, rect, .. } if monome == monome_id => Some(*rect),
        _ => None,
      })
      .unwrap_or(NO_RECT);
    let editmode_rect_on = |wanted: EditmodeControlKind| {
      rig
        .monome_windows
        .iter()
        .find_map(|w| match w {
          MonomeWindowRig::EditmodeControl { monome, rect, control, .. }
            if monome == monome_id && *control == wanted =>
          {
            Some(*rect)
          }
          _ => None,
        })
        .unwrap_or(NO_RECT)
    };
    let overlays = Overlays {
      edo_rect,
      scroll_rect,
      selector_rect,
      volume_rect,
      clear_rect: accrete_rect_on(AccreteControlKind::Clear),
      needs_holding_rect: accrete_rect_on(AccreteControlKind::NeedsHolding),
      accrete_rect: accrete_rect_on(AccreteControlKind::Accrete),
      erase_rect: accrete_rect_on(AccreteControlKind::Erase),
      distortion_rect,
      slide_rect,
      mono_rect,
      feet_accrete_rect,
      poly_rect,
      editmode_clear_rect: editmode_rect_on(EditmodeControlKind::Clear),
      editmode_accrete_rect: editmode_rect_on(EditmodeControlKind::Accrete),
    };
    grids.push(GridSettings {
      monome_id: monome_id.to_string(),
      select: monome_cfg.select.clone(),
      listen_port: monome_cfg.listen_port,
      prefix: monome_cfg.prefix.clone(),
      controls_index,
      volume_controls_index,
      overlays,
    });
  }

  let size = rig
    .monomes
    .iter()
    .find(|m| m.id == grids[0].monome_id)
    .and_then(|m| m.select.size)
    .unwrap_or([16, 16]);

  // The `[surfaces]` table (trail knobs); absent -> defaults, so unchanged behaviour.
  let surfaces = rig.surfaces.unwrap_or_default();

  // The EX-P volume pedals, by grid index (= position in `grids`). Validation
  // already pinned channel to 1|2, the monome to a play grid, and the curve
  // parameters to their legal ranges.
  let expression_pedals = rig
    .expression_pedals
    .iter()
    .filter_map(|p| {
      grids.iter().position(|g| g.monome_id == p.monome).map(|grid| {
        let curve = PedalVolumeCurve {
          lin_frac: p.curve_initial_lin_frac,
          exp_db: p.curve_remainder_exp_db,
        };
        (p.channel as usize - 1, grid, curve)
      })
    })
    .collect();

  Ok(Settings {
    grids,
    size,
    grid_w: size[0],
    grid_h: size[1],
    x_step: tuning.x_step as i32,
    y_step: tuning.y_step as i32,
    edo: tuning.edo as i32,
    fund: tuning.fundamental_hz,
    sample_rate: *sample_rate,
    buffer_frames: *buffer_frames,
    amplitude: *amplitude,
    oversample: *oversample,
    attack: *attack_secs,
    release: *release_secs,
    sustain_level: *sustain_level,
    decay_secs: *decay_secs,
    timbres: resolve_timbre_slots(rig),
    am_shape_family: match rig.am.as_ref().map(|a| a.shape.family).unwrap_or_default() {
      AmShapeFamilyRig::SinToSquare => AmShapeFamily::SinToSquare,
      AmShapeFamilyRig::TriToSquare => AmShapeFamily::TriToSquare,
    },
    distortion: Distortion { scale: *distortion_scale, shape: *distortion_shape },
    distortion_makeup: *distortion_makeup,
    distortion_auto_makeup: *distortion_auto_makeup,
    distortion_makeup_slew_secs: *distortion_makeup_slew_ms / 1000.0,
    has_drums: !rig.softstep_windows.is_empty(),
    trail_clobber_radius: surfaces.trail_clobber_radius,
    trails_max: surfaces.trails_max,
    slide_window: Duration::from_millis(surfaces.slide_candidate_window_ms),
    slide_duration_secs: surfaces.slide_duration_ms as f32 / 1000.0,
    tap_window: Duration::from_millis(surfaces.tap_tempo_window_ms),
    echo_input: rig.echo_input,
    expression_pedals,
  })
}

/// The rig's `[[timbres]]` mapped onto the four selector slots (validation
/// guarantees exactly four when present); absent = the plain waveforms.
pub(super) fn resolve_timbre_slots(rig: &Rig) -> [TimbreSlot; SELECTOR_CELLS] {
  if rig.timbres.is_empty() {
    return default_timbre_slots();
  }
  let mut slots = default_timbre_slots();
  for (slot, t) in slots.iter_mut().zip(&rig.timbres) {
    *slot = TimbreSlot {
      waveform: match t.waveform {
        WaveformChoice::Sine => Waveform::Sine,
        WaveformChoice::Triangle => Waveform::Triangle,
        WaveformChoice::Square => Waveform::Square,
        WaveformChoice::Saw => Waveform::Saw,
      },
      amplitude: t.amplitude,
      am: Am { depth: t.abs_am_depth, freq: t.abs_am_hz, shape: t.am_shape },
      fm: Fm { depth_cents: t.abs_fm_depth_cents, freq: t.abs_fm_hz },
      rel_am: RelAm { depth: t.rel_am_depth, freq: t.rel_am_freq },
      rel_fm: RelFm { depth: t.rel_fm_depth, freq: t.rel_fm_freq },
    };
  }
  slots
}

/// The bring-up decision for one run: given which configured grids have a live device
/// (and whether the SoftStep is present), what loads and what is skipped for a missing
/// dependency. The clean statement of the independence rule: *a component loads iff
/// every device it functionally depends on is present.* Pure -- unit-tested without
/// hardware (see `plan_*` tests).
pub(super) struct BringUp {
  /// Per grid index: does this grid have a live device to bind?
  present: Vec<bool>,
  /// Per grid index: drop this grid's waveform selector (it cross-controls an absent
  /// grid, so it would do nothing -- its cells become plain play cells).
  pub(super) drop_selector: Vec<bool>,
  /// Per grid index: drop this grid's volume strip (same reason).
  pub(super) drop_volume: Vec<bool>,
  /// Bring up the KMSS drumkit? (its SoftStep is present).
  pub(super) drums: bool,
  /// Human-readable red lines: every component skipped for a missing dependency, in
  /// the player's terms. Empty when everything the rig declares came up.
  pub(super) report: Vec<String>,
}

impl BringUp {
  pub(super) fn any_grid(&self) -> bool {
    self.present.iter().any(|p| *p)
  }
}

/// Work out [`BringUp`] from the resolved grids, which ones are present, and the
/// SoftStep's presence. The dependencies:
/// - a grid's own windows depend on that grid's device;
/// - a waveform selector / volume strip that *cross-controls* another grid also
///   depends on that grid (the "one monome controls the other's timbre" case);
/// - the drumkit depends only on its SoftStep -- never on any grid (it is a
///   standalone sample-trigger surface).
pub(super) fn plan_bringup(
  grids: &[GridSettings],
  present: &[bool],
  want_drums: bool,
  softstep_present: bool,
) -> BringUp {
  let n = grids.len();
  let mut drop_selector = vec![false; n];
  let mut drop_volume = vec![false; n];
  let mut report = Vec::new();

  // Absent grids: their whole window set (play surface + every overlay) does not load.
  for (i, g) in grids.iter().enumerate() {
    if !present[i] {
      report.push(format!(
        "monome {:?} is not connected -- its play surface and all its overlays did not load",
        g.monome_id,
      ));
    }
  }
  // Cross-surface functional dependencies. A present grid whose selector/volume strip
  // controls an ABSENT grid can affect nothing, so drop it (reported).
  for (i, g) in grids.iter().enumerate() {
    if !present[i] {
      continue;
    }
    if g.overlays.selector_rect != NO_RECT && !present[g.controls_index] {
      drop_selector[i] = true;
      report.push(format!(
        "monome {:?}'s waveform selector re-timbres monome {:?}, which is not connected \
         -- the selector did not load (those cells play notes instead)",
        g.monome_id, grids[g.controls_index].monome_id,
      ));
    }
    if g.overlays.volume_rect != NO_RECT && !present[g.volume_controls_index] {
      drop_volume[i] = true;
      report.push(format!(
        "monome {:?}'s volume strip sets monome {:?}, which is not connected \
         -- the volume strip did not load",
        g.monome_id, grids[g.volume_controls_index].monome_id,
      ));
    }
  }
  // The drumkit depends only on the SoftStep.
  let drums = want_drums && softstep_present;
  if want_drums && !softstep_present {
    report.push("the SoftStep is not connected -- the drumkit did not load".to_string());
  }

  BringUp { present: present.to_vec(), drop_selector, drop_volume, drums, report }
}

/// Print the red report of components skipped for a missing dependency. Silent when
/// nothing was skipped. Coloured only on a terminal (so a redirected log stays clean).
pub(super) fn print_missing_report(report: &[String]) {
  if report.is_empty() {
    return;
  }
  use std::io::IsTerminal;
  let (red, reset) = if std::io::stderr().is_terminal() {
    ("\x1b[1;31m", "\x1b[0m")
  } else {
    ("", "")
  };
  eprintln!("{red}surfaces: some components did not load (missing gear):{reset}");
  for line in report {
    eprintln!("{red}  - {line}{reset}");
  }
  eprintln!(
    "{red}  (playing what loaded; reconnect the gear and restart to get the rest){reset}"
  );
}

/// Can this grid's `needs_holding` switch be flipped at all -- by an on-grid button or
/// by a rig-declared pedal? If not, the mode is fixed forever at whatever it starts
/// as, which is why the caller makes such a bank momentary rather than toggling.
pub(super) fn grid_has_needs_holding_control(rig: &Rig, grid: &GridSettings) -> bool {
  if grid.overlays.needs_holding_rect != NO_RECT {
    return true;
  }
  rig.softstep_windows.iter().any(|w| {
    matches!(
      w,
      SoftstepWindowRig::AccreteControl { monome, control: AccreteControlKind::NeedsHolding, .. }
        if *monome == grid.monome_id
    )
  })
}

pub(super) fn current_slot(selected: &Arc<Mutex<Vec<usize>>>, index: usize) -> usize {
  let guard = selected.lock().unwrap_or_else(|e| e.into_inner());
  guard.get(index).copied().unwrap_or(DEFAULT_SLOT)
}

pub(super) fn set_slot(selected: &Arc<Mutex<Vec<usize>>>, index: usize, slot: usize) {
  let mut guard = selected.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(entry) = guard.get_mut(index) {
    *entry = slot;
  }
}
