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

// === Chord storage =====================================================

// Number of chord slots, including silence at index 0. Chord buttons
// fill the entire y=15 row.
pub const N_CHORDS:                 usize = 16;
pub const SILENCE_CHORD:            usize = 0;
pub const INITIALLY_ACCRETING_CHORD: usize = 1;

// === Layout: cell positions of every control-window button ============

pub const CELL_WIPE:                 (i32, i32) = (0, 14);
pub const CELL_ACCRETE_ON:           (i32, i32) = (1, 14);
pub const CELL_EMIT_IS_TOGGLE:       (i32, i32) = (2, 14);
pub const CELL_SET_ACCRETION_TARGET: (i32, i32) = (3, 14);
// Silence at (0,15); chord N at (N, 15) for N in 1..=15.

pub const CONTROLS_TOP_RECT:    ((i32, i32), (i32, i32)) = ((0, 14), (3, 14));
pub const CONTROLS_BOTTOM_RECT: ((i32, i32), (i32, i32)) = ((0, 15), (15, 15));
pub const EDO_RECT:             ((i32, i32), (i32, i32)) = ((0,  0), (15, 15));

// === Diagnostics =======================================================

// How often the main loop reads the audio thread's atomic counters
// and prints one [hb] summary line. Unrelated to anything in the
// control flow — purely for spotting audio-stall issues. See commit
// 74cee11 for context.
pub const HEARTBEAT_SECS: f64 = 1.0;

// === UI feedback ======================================================

// Set-accretion-target LED flash period: half on, half off, this many
// milliseconds per phase (so the full cycle is 2× this). The main
// loop's 50 ms recv timeout dominates the worst-case visible jitter.
pub const FLASH_PHASE_MS: u128 = 150;
