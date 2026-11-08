//! The four timbres a `waveform_selector` strip chooses among, read from a
//! rig's `[[timbres]]` (or the plain four waveforms when it has none).
//!
//! A runtime-independent piece: the editor and the surfaces runtime both
//! need it.

use crate::rig::{Rig, TimbreRig, WaveformChoice};
use crate::types::{Am, Fm, RelAm, RelFm, Timbre, Waveform};

/// How many timbres a selector strip offers.
pub const TIMBRE_SLOTS: usize = 4;

/// The rig's four selectable timbres, left to right. Each `Timbre.gain` is
/// that timbre's own amplitude.
pub fn timbre_slots(rig: &Rig) -> [Timbre; TIMBRE_SLOTS] {
  let mut slots: [Timbre; TIMBRE_SLOTS] = plain_waveform_slots();
  for (slot, timbre_rig) in slots.iter_mut().zip(&rig.timbres) {
    *slot = timbre_of(timbre_rig); }
  slots }

/// The four plain waveforms (sine, triangle, square, saw) at unity gain.
pub fn plain_waveform_slots() -> [Timbre; TIMBRE_SLOTS] {
  [Waveform::Sine, Waveform::Triangle, Waveform::Square, Waveform::Saw]
    .map(|waveform| Timbre { waveform, ..Timbre::default() }) }

pub fn timbre_of(timbre_rig: &TimbreRig) -> Timbre {
  Timbre {
    waveform: waveform_of(timbre_rig.waveform),
    gain: timbre_rig.amplitude,
    am: Am {
      depth: timbre_rig.abs_am_depth,
      freq: timbre_rig.abs_am_hz,
      shape: timbre_rig.am_shape, },
    fm: Fm { depth_cents: timbre_rig.abs_fm_depth_cents, freq: timbre_rig.abs_fm_hz },
    rel_am: RelAm { depth: timbre_rig.rel_am_depth, freq: timbre_rig.rel_am_freq },
    rel_fm: RelFm { depth: timbre_rig.rel_fm_depth, freq: timbre_rig.rel_fm_freq }, } }

pub fn waveform_of(choice: WaveformChoice) -> Waveform {
  match choice {
    WaveformChoice::Sine => Waveform::Sine,
    WaveformChoice::Triangle => Waveform::Triangle,
    WaveformChoice::Square => Waveform::Square,
    WaveformChoice::Saw => Waveform::Saw, } }
