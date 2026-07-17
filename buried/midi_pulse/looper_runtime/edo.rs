//! The looper's edo-grid play math now lives in the lib (`midi_pulse::edo_play`)
//! so the surfaces runtime's two independent grids can share it. Re-exported here
//! so the looper's `super::edo::{...}` call sites are unchanged.

pub use midi_pulse::edo_play::{register_delta, shift_for_cell, step_for_cell, Shift};
