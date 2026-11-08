//! The multitrack editor: record what a monome plays into text tracks, play
//! them back, overdub their parameters, and cut, paste and delete regions of
//! time. Player reference: rigs/editor_readme.org.
//!
//! The pure layers, bottom up: 'track' (the data), 'track_text' (its file
//! form), 'track_edit' (region edits and merging a take), 'rig_layout' (what
//! the editor takes from a rig), 'live_grid' (key presses and LEDs),
//! 'playback', 'recorder', 'song' (a directory of tracks). 'sound' drives
//! the sawwave engine's voices; 'session' holds it all and is what the
//! window and the monome threads call.

pub mod canvas;
pub mod grid_io;
pub mod live_grid;
pub mod playback;
pub mod recorder;
pub mod rig_layout;
pub mod session;
pub mod song;
pub mod sound;
pub mod track;
pub mod track_edit;
pub mod track_text;
pub mod view;
pub mod window;
