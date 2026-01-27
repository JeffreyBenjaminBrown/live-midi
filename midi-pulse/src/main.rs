//! MIDI Pulse - outputs a quiet high note every 300ms
//!
//! # How to run
//!
//! ```sh
//! cd midi-pulse
//! cargo run
//! ```
//!
//! # Where to see it in QJackCtl
//!
//! 1. Open QJackCtl and click the "Connect" button (or press Ctrl+P)
//! 2. Go to the "ALSA" tab (not "Audio" or "MIDI" - those are for JACK routing)
//! 3. Look for "midi-pulse" in the left "Readable Clients" pane
//! 4. Connect it to a synthesizer or MIDI monitor in the right "Writable Clients" pane
//!
//! Alternatively, use the command line:
//! ```sh
//! aconnect -l          # list MIDI ports
//! aconnect 128:0 129:0 # connect midi-pulse to another port (adjust numbers)
//! ```

use midir::MidiOutput;
use midir::os::unix::VirtualOutput;
use std::{thread, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let midi_out = MidiOutput::new("midi-pulse")?;

    // Create a virtual output port (appears in ALSA/JACK)
    let mut conn = midi_out.create_virtual("pulse-out")?;

    println!("Created virtual MIDI port 'midi-pulse:pulse-out'");
    println!("Look for 'midi-pulse' in QJackCtl's ALSA tab or aconnect -l");
    println!("Sending note 96 (C7), velocity 10, every 300ms. Ctrl+C to stop.");

    let note: u8 = 96;      // C7 - high note
    let velocity: u8 = 10;  // quiet
    let channel: u8 = 0;    // channel 1

    loop {
        // Note on: 0x90 + channel, note, velocity
        conn.send(&[0x90 | channel, note, velocity])?;

        thread::sleep(Duration::from_millis(100));

        // Note off: 0x80 + channel, note, velocity
        conn.send(&[0x80 | channel, note, 0])?;

        thread::sleep(Duration::from_millis(200));
    }
}
