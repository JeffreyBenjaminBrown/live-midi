//! The instrument that the `2-monomes_2-softsteps` rig runs: two 16x16 monome grids as
//! 46-EDO play surfaces, two SoftStep boards carrying sustain, tap tempo and the pulse,
//! an on-screen readout, and per-voice editing.
//!
//! It is a separate crate from `midi_pulse` on purpose. The old library grew names that
//! stopped matching what they held -- a `chord` field that named no chord, a `pitch`
//! field holding a monomekey, a `Fingered` variant that fingered notes did not use --
//! and the documentation compensated with a paragraph per lie. Two of the bugs that
//! reached Jeff's hands came out of exactly that gap.
//!
//! So this crate depends on nothing from `midi_pulse`. What the instrument needs gets
//! cannibalized across and renamed to say what it is; what it does not need cannot leak
//! in. The plan and the terms are in `TODO/new-library/`.

pub mod voice;
