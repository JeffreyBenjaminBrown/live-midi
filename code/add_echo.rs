//! Add Echo - MIDI pass-through with delayed echo
//!
//! # How to run
//!
//! ```sh
//! cargo run --bin add_echo
//! ```
//!
//! Creates three virtual MIDI ports:
//! - "midi-in": Input port - connect your MIDI source here
//! - "immediate-out": Outputs MIDI immediately (pass-through)
//! - "echo-out": Outputs MIDI delayed by 300ms

use midir::{MidiInput, MidiOutput, MidiInputConnection, MidiOutputConnection};
use midir::os::unix::{VirtualInput, VirtualOutput};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{thread, io};

struct DelayedMessage {
    data: Vec<u8>,
    send_at: Instant,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let midi_in: MidiInput = MidiInput::new("add-echo-in")?;
    let midi_out_immediate: MidiOutput = MidiOutput::new("add-echo-immediate")?;
    let midi_out_echo: MidiOutput = MidiOutput::new("add-echo-echo")?;

    // Create virtual output ports
    let mut conn_immediate: MidiOutputConnection =
        midi_out_immediate.create_virtual("immediate-out")?;
    let conn_echo: MidiOutputConnection =
        midi_out_echo.create_virtual("echo-out")?;

    // Channel for sending messages to the delay thread
    let (tx_immediate, rx_immediate): (
        mpsc::Sender<Vec<u8>>,
        mpsc::Receiver<Vec<u8>>,
    ) = mpsc::channel();
    let (tx_echo, rx_echo): (
        mpsc::Sender<Vec<u8>>,
        mpsc::Receiver<Vec<u8>>,
    ) = mpsc::channel();

    // Spawn thread for immediate output
    let _immediate_thread: thread::JoinHandle<()> = thread::spawn(move || {
        while let Ok(data) = rx_immediate.recv() {
            let _ = conn_immediate.send(&data);
        }
    });

    // Spawn thread for delayed echo output
    let _echo_thread: thread::JoinHandle<()> = thread::spawn(move || {
        let mut conn: MidiOutputConnection = conn_echo;
        let mut queue: Vec<DelayedMessage> = Vec::new();
        let delay: Duration = Duration::from_millis(300);

        loop {
            // Check for new messages (non-blocking)
            while let Ok(data) = rx_echo.try_recv() {
                let msg: DelayedMessage = DelayedMessage {
                    data,
                    send_at: Instant::now() + delay,
                };
                queue.push(msg);
            }

            // Send any messages whose time has come
            let now: Instant = Instant::now();
            let mut i: usize = 0;
            while i < queue.len() {
                if queue[i].send_at <= now {
                    let msg: DelayedMessage = queue.remove(i);
                    let _ = conn.send(&msg.data);
                } else {
                    i += 1;
                }
            }

            // Sleep briefly to avoid busy-waiting
            thread::sleep(Duration::from_millis(1));
        }
    });

    // Create virtual input port with callback
    let _conn_in: MidiInputConnection<()> = midi_in.create_virtual(
        "midi-in",
        move |_timestamp: u64, message: &[u8], _: &mut ()| {
            let data: Vec<u8> = message.to_vec();
            let _ = tx_immediate.send(data.clone());
            let _ = tx_echo.send(data);
        },
        (),
    )?;

    println!("MIDI Echo processor started!");
    println!();
    println!("Virtual ports created:");
    println!("  - 'add-echo-in:midi-in' (input)");
    println!("  - 'add-echo-immediate:immediate-out' (pass-through)");
    println!("  - 'add-echo-echo:echo-out' (300ms delay)");
    println!();
    println!("Use 'aconnect -l' to see ports, 'aconnect <src> <dst>' to connect.");
    println!("Press Enter to exit...");

    let mut input: String = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(())
}
