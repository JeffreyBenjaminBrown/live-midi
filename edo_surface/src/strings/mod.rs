//! The strings engine: an extended Karplus-Strong string per note,
//! started by a deterministic pluck or strike, every string coupled
//! through one shared bridge, the sum radiated through a body. Rigs
//! select it with a `cpal_strings` sink; the sawwave engine is the
//! other renderer of the same voice map.
//!
//! `params` holds every parameter and the curves of decay time and
//! inharmonicity over pitch. `loop_dsp` is one delay-line loop, built
//! of the filters in `first_order`. `voice` is a note's audio state
//! of either kind: a string voice (a note's loops and the excitation
//! it streams into them) or a modal voice (`modal`: a few resonators
//! for a note above the crossover). `junction` is the bridge that
//! couples every loop and modal voice; `howl` is the optional active
//! path beside it. `body` designs and runs
//! the bank of body resonators. `exciter` builds the tables that
//! start a note. `energy` is the energy pedal's part: the pedal's
//! history, the window around key-down, the pre-charge a string
//! holds at birth and the drive that feeds it afterward. `noise` is the deterministic random source the
//! exciter and the body share. `renderer` runs all of it over the
//! voice map, block by block. `scripted_voices` builds `VoiceState`s
//! as the surfaces runtime does, for the harness and the tests.
//! `pedal_targets` holds which parameters an expression pedal may
//! move and computes the parameters a block renders with.

pub mod body;
pub mod energy;
pub mod exciter;
pub mod first_order;
pub mod howl;
pub mod junction;
pub mod loop_dsp;
pub mod modal;
pub mod noise;
pub mod params;
pub mod pedal_targets;
pub mod renderer;
pub mod scripted_voices;
pub mod voice;

pub use params::StringsParams;
pub use renderer::StringsRenderer;

#[cfg(test)]
mod tests;
