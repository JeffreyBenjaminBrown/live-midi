//! All magic numbers used by the app, in one place.

// === OSC ===============================================================

pub const PREFIX:        &str = "/256-1-cable";
pub const DETECTOR_PORT: u16  = 12002;
pub const LISTEN_PORT:   u16  = 9000;

// === Audio =============================================================

pub const AMPLITUDE:        f32 = 0.15;
pub const ATTACK_SECS:      f32 = 0.003;
pub const RELEASE_SECS:     f32 = 0.050;
// Accretion-born voices play at this fraction of full volume.
pub const ACCRETION_TARGET: f32 = 0.5;

// === Grid geometry =====================================================

// Hardcoded for the 256 (16×16) grid. If smaller/larger grids appear,
// query /sys/size from serialoscd at startup and replace these.
pub const GRID_W: i32 = 16;
pub const GRID_H: i32 = 16;

// === Diagnostics =======================================================

// How often the main loop reads the audio thread's atomic counters
// and prints one [hb] summary line. Unrelated to anything in the
// control flow — purely for spotting audio-stall issues. See commit
// 74cee11 for context.
pub const HEARTBEAT_SECS: f64 = 1.0;
