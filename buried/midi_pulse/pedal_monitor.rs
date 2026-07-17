//! `pedal_monitor` -- prove Rust can SEE the DOREMiDi MPC-20 expression pedals.
//!
//! Connects to the "MPC-20 pedals" virtual ALSA-seq port and draws each pedal's live
//! normalized position (0.0..1.0) plus a bar and the raw CC. This is only a demo of
//! `midi_pulse::expression_pedals::PedalReader`; how the pedals will actually be USED is not
//! designed yet.
//!
//! PREREQUISITE: the host-side bridge must be running (the pedals have no ALSA device of
//! their own -- see tools/pedals/README.org):
//!
//!   # on the HOST:
//!   nix-shell -p 'python3.withPackages(p: [p.pyusb p.python-rtmidi])' libusb1 \
//!     --run 'sudo python3 tools/pedals/bridge.py'
//!
//!   # then in this container:
//!   cargo run --bin pedal_monitor
//!   cargo run --bin pedal_monitor -- 'other port substring'   # override the port name
//!
//! Ctrl-C to quit (a plain MIDI input -- nothing to restore).

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use midi_pulse::expression_pedals::{PedalReader, DEFAULT_PORT, PEDAL_RAW_BOTTOM, PEDAL_RAW_TOP};

const BAR_W: usize = 40;

fn main() {
  let substring = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_PORT.to_string());
  let reader = match PedalReader::connect(&substring) {
    Ok(r) => r,
    Err(e) => {
      eprintln!("{e}");
      std::process::exit(1);
    }
  };
  println!("bound MIDI input {:?}. Move the pedals; Ctrl-C to quit.\n", reader.port_name());
  loop {
    let pedals = reader.pedals();
    let mut out = String::from("\x1b[2J\x1b[H");
    out.push_str("  MPC-20 expression-pedal monitor (Rust) -- normalized 0.0..1.0 via the bridge\n");
    out.push_str(&format!(
      "  raw {PEDAL_RAW_BOTTOM}..{PEDAL_RAW_TOP} -> 0..1 (needs tools/pedals/bridge.py on the host)\n\n"
    ));
    for (i, p) in pedals.iter().enumerate() {
      let fill = (p.norm * BAR_W as f32).round() as usize;
      let bar: String = (0..BAR_W).map(|j| if j < fill { '#' } else { '-' }).collect();
      let seen = if p.updates == 0 { "  (no CC yet)" } else { "" };
      out.push_str(&format!(
        "  pedal {i} (MIDI ch{})  {:.3}  [{bar}]  raw {:3}  {} msgs{seen}\n",
        i + 1,
        p.norm,
        p.raw,
        p.updates,
      ));
    }
    out.push_str("\n  (Ctrl-C to quit)\n");
    print!("{out}");
    let _ = io::stdout().flush();
    thread::sleep(Duration::from_millis(30));
  }
}
