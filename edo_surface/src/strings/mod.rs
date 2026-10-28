//! The strings engine: an extended Karplus-Strong string per note,
//! started by a deterministic pluck or strike. Rigs select it with a
//! `cpal_strings` sink; the sawwave engine is the other renderer of
//! the same voice map.
//!
//! `params` holds every parameter and the curves of decay time and
//! inharmonicity over pitch. `loop_dsp` is one delay-line loop, built
//! of the filters in `first_order`. `body` designs the bank of body
//! resonators. `exciter` builds the table that starts a note.
//! `noise` is the deterministic random source the exciter and the
//! body share.

pub mod body;
pub mod exciter;
pub mod first_order;
pub mod loop_dsp;
pub mod noise;
pub mod params;

#[cfg(test)]
mod tests;
