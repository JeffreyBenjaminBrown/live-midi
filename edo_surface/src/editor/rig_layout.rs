//! What the editor takes from a rig: how each monome's surface is laid out
//! (`GridLayout`), and how the rig sounds (`RigSound`).
//!
//! The editor plays the plain pieces of a rig: an `edo_note_grid`, a
//! `waveform_selector`, a `volume_delta` and an `edo_shift_pad`. Any other
//! window is inert here (its cells do nothing and stay dark), and each one is
//! reported once, so a richer rig like the volpedal one still plays, minus
//! its extras.

use crate::rig::{MonomeRig, MonomeWindowRig, Rig, SinkRig};
use crate::timbre_slots::{timbre_slots, TIMBRE_SLOTS};
use crate::tuning::Tuning;
use crate::types::Timbre;

use super::track::TrackTuning;

/// How a rig sounds, apart from its pitches: what a track playing through it
/// needs. Every voice of the rig shares one envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct RigSound {
  pub timbres: [Timbre; TIMBRE_SLOTS],
  pub envelope: Envelope,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Envelope {
  /// The sink's master amplitude; the editor folds it into each voice.
  pub amplitude: f32,
  pub attack_secs: f32,
  pub release_secs: f32,
  pub sustain_level: f32,
  pub decay_secs: f32,
}

/// The volume keys' steps and limits, in dB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolumeKeys {
  pub coarse_db: f32,
  pub fine_db: f32,
  pub initial_db: f32,
  pub min_db: f32,
  pub max_db: f32,
}

impl VolumeKeys {
  /// The change the `index`th key (from the left) makes: -coarse, -fine,
  /// +fine, +coarse.
  pub fn delta_db(
    &self,
    index: usize,
  ) -> f32 {
    match index {
      0 => -self.coarse_db,
      1 => -self.fine_db,
      2 => self.fine_db,
      _ => self.coarse_db, } }
}

/// What one window of a grid does in the editor.
#[derive(Clone, Debug, PartialEq)]
pub enum PieceKind {
  Notes,
  Shift,
  /// Chooses the timbre of grid number `controls`.
  Timbre { controls: usize },
  /// Moves the volume of grid number `controls`.
  Volume { controls: usize, keys: VolumeKeys },
  /// A window the editor does not play.
  Inert,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Piece {
  /// Inclusive: [left x, top y, right x, bottom y].
  pub rect: [i32; 4],
  pub kind: PieceKind,
}

impl Piece {
  pub fn contains(
    &self,
    (x, y): (i32, i32),
  ) -> bool {
    let [x0, y0, x1, y1] = self.rect;
    x >= x0 && x <= x1 && y >= y0 && y <= y1 }

  /// Where `cell` sits in the piece, counting left to right, then down.
  pub fn index_of(
    &self,
    (x, y): (i32, i32),
  ) -> usize {
    let [x0, y0, x1, _] = self.rect;
    ((y - y0) * (x1 - x0 + 1) + (x - x0)) as usize }
}

/// One monome of a rig, as the editor plays it.
#[derive(Clone, Debug, PartialEq)]
pub struct GridLayout {
  pub monome: MonomeRig,
  pub tuning: Tuning,
  /// Back to front, as in the rig: a later piece covers an earlier one.
  pub pieces: Vec<Piece>,
  pub sound: RigSound,
  /// The timbre slot and volume a grid starts with.
  pub initial_timbre: usize,
  pub initial_volume_db: f32,
}

impl GridLayout {
  pub fn track_tuning(&self) -> TrackTuning {
    TrackTuning { edo: self.tuning.edo, fundamental_hz: self.tuning.fundamental_hz } }

  /// The front piece at `cell`, if any.
  pub fn piece_at(
    &self,
    cell: (i32, i32),
  ) -> Option<&Piece> {
    self.pieces.iter().rev().find(|piece| piece.contains(cell)) }
}

/// The triangle, as in the surfaces runtime.
pub const INITIAL_TIMBRE_SLOT: usize = 1;

/// A rig as the editor plays it: one layout per monome with a note grid, and
/// a line for each thing the editor leaves out.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorRig {
  pub name: String,
  pub grids: Vec<GridLayout>,
  pub ignored: Vec<String>,
}

pub fn editor_rig(
  name: &str,
  rig: &Rig,
) -> Result<EditorRig, String> {
  let mut ignored: Vec<String> = vec![];
  if !rig.expression_pedals.is_empty() { ignored.push("the expression pedals".into()); }
  if !rig.softsteps.is_empty() { ignored.push("the SoftSteps".into()); }
  let grid_monomes: Vec<&MonomeRig> = rig.monomes.iter()
    .filter(|monome| rig.monome_windows.iter().any(|window|
      window.monome() == monome.id && matches!(window, MonomeWindowRig::EdoNoteGrid { .. })))
    .collect();
  let grid_index = |monome_id: &str| -> Option<usize> {
    grid_monomes.iter().position(|monome| monome.id == monome_id) };
  let mut grids: Vec<GridLayout> = vec![];
  for monome in &grid_monomes {
    grids.push(grid_layout(rig, monome, &grid_index, &mut ignored)?); }
  if grids.is_empty() {
    return Err(format!("rig {name} has no edo_note_grid for the editor to play")); }
  Ok(EditorRig { name: name.to_string(), grids, ignored }) }

fn grid_layout(
  rig: &Rig,
  monome: &MonomeRig,
  grid_index: &dyn Fn(&str) -> Option<usize>,
  ignored: &mut Vec<String>,
) -> Result<GridLayout, String> {
  let mut pieces: Vec<Piece> = vec![];
  let mut note_grid: Option<(&str, &str)> = None; // its tuning and sink ids
  let mut initial_volume_db: f32 = 0.0;
  for window in rig.monome_windows.iter().filter(|window| window.monome() == monome.id) {
    let kind: PieceKind = match window {
      MonomeWindowRig::EdoNoteGrid { tuning, sink, .. } => {
        note_grid = Some((tuning, sink));
        PieceKind::Notes }
      MonomeWindowRig::EdoShiftPad { .. } => PieceKind::Shift,
      MonomeWindowRig::WaveformSelector { controls, .. } => match grid_index(controls) {
        Some(controls) => PieceKind::Timbre { controls },
        None => PieceKind::Inert, },
      MonomeWindowRig::VolumeDelta {
        controls, volume_coarse, volume_fine, initial_db, min_db, max_db, .. } => {
        initial_volume_db = *initial_db;
        match grid_index(controls) {
          Some(controls) => PieceKind::Volume { controls, keys: VolumeKeys {
            coarse_db: *volume_coarse, fine_db: *volume_fine, initial_db: *initial_db,
            min_db: *min_db, max_db: *max_db, } },
          None => PieceKind::Inert, } }
      other => {
        let line: String = format!("{} ({})", other.kind_name(), other.id());
        if !ignored.contains(&line) { ignored.push(line); }
        PieceKind::Inert } };
    pieces.push(Piece { rect: window.rect(), kind }); }
  let (tuning_id, sink_id) = note_grid.expect("only monomes with a note grid get here");
  let tuning_rig = rig.tunings.iter().find(|tuning| tuning.id == tuning_id)
    .ok_or(format!("no tuning {tuning_id}"))?;
  let tuning: Tuning = Tuning {
    edo: tuning_rig.edo as i32,
    x_step: tuning_rig.x_step as i32,
    y_step: tuning_rig.y_step as i32,
    fundamental_hz: tuning_rig.fundamental_hz, };
  let sound: RigSound = rig_sound(rig, sink_id)?;
  Ok(GridLayout {
    monome: monome.clone(), tuning, pieces, sound,
    initial_timbre: INITIAL_TIMBRE_SLOT, initial_volume_db, }) }

fn rig_sound(
  rig: &Rig,
  sink_id: &str,
) -> Result<RigSound, String> {
  let sink: &SinkRig = rig.sinks.iter().find(|sink| sink_id_of(sink) == sink_id)
    .ok_or(format!("no sink {sink_id}"))?;
  let SinkRig::CpalSynth {
    amplitude, attack_secs, release_secs, sustain_level, decay_secs, .. } = sink else {
    return Err(format!("the editor plays only cpal_synth sinks, and {sink_id} is not one")); };
  Ok(RigSound {
    timbres: timbre_slots(rig),
    envelope: Envelope {
      amplitude: *amplitude,
      attack_secs: *attack_secs,
      release_secs: *release_secs,
      sustain_level: *sustain_level,
      decay_secs: *decay_secs, }, }) }

fn sink_id_of(sink: &SinkRig) -> &str {
  match sink {
    SinkRig::Midi { id }
    | SinkRig::CpalSynth { id, .. }
    | SinkRig::CpalSampler { id, .. }
    | SinkRig::CpalStrings { id, .. } => id, } }

#[cfg(test)]
mod tests {
  use super::*;
  use crate::rig::load_named_rig;

  const PLAIN: &str = "timbres-volume-scroll_41edo";

  #[test]
  fn the_plain_rig_lays_out_every_grid_with_nothing_ignored() {
    let rig: Rig = load_named_rig(PLAIN).unwrap();
    let editor: EditorRig = editor_rig(PLAIN, &rig).unwrap();
    assert!(editor.ignored.is_empty(), "{:?}", editor.ignored);
    assert_eq!(editor.grids.len(), rig.monomes.len());
    for grid in &editor.grids {
      assert!(grid.pieces.iter().any(|piece| piece.kind == PieceKind::Notes));
      assert!(grid.pieces.iter().any(|piece| piece.kind == PieceKind::Shift)); } }

  #[test]
  fn the_volpedal_rig_plays_minus_its_extras() {
    let name: &str = "volpedal-newnotes-and-global_granular-sustain";
    let editor: EditorRig = editor_rig(name, &load_named_rig(name).unwrap()).unwrap();
    assert!(editor.ignored.iter().any(|line| line.contains("pedals")));
    assert!(editor.ignored.iter().any(|line| line.contains("accrete_control"))); }

  #[test]
  fn a_control_covers_the_note_grid_beneath_it() {
    let rig: Rig = load_named_rig(PLAIN).unwrap();
    let grid: GridLayout = editor_rig(PLAIN, &rig).unwrap().grids.remove(0);
    let timbre: &Piece = grid.pieces.iter()
      .find(|piece| matches!(piece.kind, PieceKind::Timbre { .. })).unwrap();
    let corner: (i32, i32) = (timbre.rect[0], timbre.rect[1]);
    assert_eq!(grid.piece_at(corner), Some(timbre)); }
}
