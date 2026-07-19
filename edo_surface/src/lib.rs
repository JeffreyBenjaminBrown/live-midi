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

pub mod tuning;
pub mod voice;

// The live instrument core, unburied here in cleaning phase 7 (everything the
// `2-monomes_2-softsteps` rig uses). `buried/` (the old `midi_pulse` crate) now depends
// on this crate for these; the reverse never happens.

// The rig loader + its org-mode parser, and the surface plumbing.
pub mod rig;
pub mod rig_org;
pub mod device_assign;
pub mod edo_play;
pub mod expression_pedals;
pub mod midi;
pub mod mock_monome;
pub mod monome;
pub mod monome_brightness;

// The shared 5x7 bitmap font for the on-screen readouts.
pub mod bitmap_font;

// The sawwave synth engine, mounted flat (as it was in the old bin) so the
// runtimes here and in `buried/` reach it as `edo_surface::{types, voices, ...}`.
#[path = "sawwave/consts.rs"]
pub mod consts;
#[path = "sawwave/diagnostics.rs"]
pub mod diagnostics;
#[path = "sawwave/leds.rs"]
pub mod leds;
#[path = "sawwave/osc.rs"]
pub mod osc;
#[path = "sawwave/pitch.rs"]
pub mod pitch;
#[path = "sawwave/state.rs"]
pub mod state;
#[path = "sawwave/types.rs"]
pub mod types;
#[path = "sawwave/voices.rs"]
pub mod voices;
#[path = "sawwave/windows.rs"]
pub mod windows;

// The runtimes the latest rig runs.
pub mod drumkit_runtime;
pub mod surfaces_runtime;
