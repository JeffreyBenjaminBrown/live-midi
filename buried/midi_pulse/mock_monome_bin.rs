//! `mock_monome` -- a standalone virtual monome rig.
//!
//! Runs a fake serialoscd + N fake 16x16 grids on localhost. Point the looper at
//! it and you can play the instrument with no hardware, beside your real session:
//!
//!     # terminal A:
//!     cargo run --bin mock_monome -- --detector-port 17002
//!     # terminal B (a rig whose listen ports/prefixes don't collide -- e.g.
//!     # monome-looper-58-8-1-mock):
//!     MIDI_PULSE_DETECTOR_PORT=17002 cargo run --bin midi_pulse -- monome-looper-58-8-1-mock
//!
//! The rig prints each grid's LEDs whenever they change, and reads key presses
//! from stdin. Commands (one per line):
//!
//!     <grid> <x> <y>         tap (press+release)   e.g.  edo 0 0    or  0 7 7
//!     <grid> <x> <y> down    press and hold
//!     <grid> <x> <y> up      release
//!     render | r             reprint all grids
//!     ids                    show grid ids/ports
//!     help | h               this help
//!     quit | q               exit
//!
//! <grid> is an index (0,1,...) or a name: edo=0, loops=1.

use midi_pulse::mock_monome::{GridSpec, MockRig};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let mut detector_port: u16 = 17002;
  let mut ids: Vec<String> = vec!["mock-256-a".into(), "mock-256-b".into()];

  let mut args = std::env::args().skip(1);
  while let Some(a) = args.next() {
    match a.as_str() {
      "--detector-port" | "-p" => {
        detector_port = args.next().ok_or("--detector-port needs a value")?.parse()?;
      }
      "--grids" | "-g" => {
        ids = args
          .next()
          .ok_or("--grids needs a comma-separated id list")?
          .split(',')
          .map(|s| s.trim().to_string())
          .filter(|s| !s.is_empty())
          .collect();
      }
      "--help" | "-h" => {
        print_help();
        return Ok(());
      }
      other => return Err(format!("unknown argument: {other}").into()),
    }
  }

  let specs: Vec<GridSpec> = ids.iter().map(|id| GridSpec::grid_256(id)).collect();
  let rig = MockRig::start(detector_port, &specs)?;
  let detector_port = rig.detector_port();

  println!("mock_monome: virtual serialosc on detector port {detector_port}");
  for (i, g) in rig.grids().iter().enumerate() {
    let (w, h) = g.dims();
    println!("  grid {i}: id={:?} {w}x{h} device-port={}", g.id(), g.port());
  }
  println!();
  println!("point the looper at it with:");
  println!("  MIDI_PULSE_DETECTOR_PORT={detector_port} cargo run --bin midi_pulse -- <mock-rig>");
  println!();
  print_help();

  // stdin blocks, so read lines on their own thread and feed them over a channel;
  // the main thread polls the channel and watches LED changes, never blocking.
  let stop = Arc::new(AtomicBool::new(false));
  let (tx, rx) = std::sync::mpsc::channel::<String>();
  let reader_stop = Arc::clone(&stop);
  let reader = thread::spawn(move || {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
      let Ok(line) = line else { break };
      if tx.send(line).is_err() {
        break;
      }
    }
    reader_stop.store(true, Ordering::SeqCst); // EOF on stdin -> shut down
  });

  let mut last_gen: Vec<u64> = rig.grids().iter().map(|g| g.generation()).collect();
  loop {
    if stop.load(Ordering::SeqCst) {
      break;
    }
    // Drain any input lines.
    while let Ok(line) = rx.try_recv() {
      if handle_line(&rig, &line) == Flow::Quit {
        stop.store(true, Ordering::SeqCst);
        break;
      }
    }
    // Reprint changed grids.
    for (i, g) in rig.grids().iter().enumerate() {
      let gen = g.generation();
      if gen != last_gen[i] {
        last_gen[i] = gen;
        println!("\n[grid {i} {:?}]\n{}", g.id(), g.render());
        let _ = io::stdout().flush();
      }
    }
    thread::sleep(Duration::from_millis(40));
  }

  stop.store(true, Ordering::SeqCst);
  let _ = reader.join();
  rig.shutdown();
  println!("mock_monome: stopped.");
  Ok(())
}

#[derive(PartialEq)]
enum Flow {
  Continue,
  Quit,
}

fn handle_line(rig: &MockRig, line: &str) -> Flow {
  let line = line.trim();
  if line.is_empty() {
    return Flow::Continue;
  }
  let tok: Vec<&str> = line.split_whitespace().collect();
  match tok[0] {
    "quit" | "q" | "exit" => return Flow::Quit,
    "help" | "h" | "?" => {
      print_help();
      return Flow::Continue;
    }
    "render" | "r" => {
      for (i, g) in rig.grids().iter().enumerate() {
        println!("[grid {i} {:?}]\n{}", g.id(), g.render());
      }
      return Flow::Continue;
    }
    "ids" => {
      for (i, g) in rig.grids().iter().enumerate() {
        println!("  grid {i}: id={:?} port={}", g.id(), g.port());
      }
      return Flow::Continue;
    }
    _ => {}
  }

  // <grid> <x> <y> [down|up|1|0]
  if tok.len() < 3 {
    eprintln!("?? expected: <grid> <x> <y> [down|up]   (try `help`)");
    return Flow::Continue;
  }
  let Some(gi) = grid_index(tok[0]) else {
    eprintln!("?? unknown grid {:?} (use an index or edo/loops)", tok[0]);
    return Flow::Continue;
  };
  let (Ok(x), Ok(y)) = (tok[1].parse::<i32>(), tok[2].parse::<i32>()) else {
    eprintln!("?? x and y must be integers");
    return Flow::Continue;
  };
  let Some(g) = rig.grids().get(gi) else {
    eprintln!("?? no grid {gi} (have {})", rig.grids().len());
    return Flow::Continue;
  };
  let sent = match tok.get(3).copied() {
    None => g.tap(x, y),
    Some("down") | Some("1") | Some("press") => g.press(x, y),
    Some("up") | Some("0") | Some("release") => g.release(x, y),
    Some(other) => {
      eprintln!("?? unknown state {other:?} (use down/up)");
      return Flow::Continue;
    }
  };
  if !sent {
    eprintln!("(grid {gi} not registered yet -- is the looper running and pointed here?)");
  }
  Flow::Continue
}

fn grid_index(s: &str) -> Option<usize> {
  match s.to_ascii_lowercase().as_str() {
    "edo" | "e" => Some(0),
    "loops" | "loop" | "l" => Some(1),
    n => n.parse().ok(),
  }
}

fn print_help() {
  println!(
    "commands:\n  \
     <grid> <x> <y>         tap (press+release)   e.g.  edo 0 0   |  1 14 3\n  \
     <grid> <x> <y> down    press and hold\n  \
     <grid> <x> <y> up      release\n  \
     render | r             reprint all grids\n  \
     ids                    show grid ids/ports\n  \
     help | h               this help\n  \
     quit | q               exit\n  \
     (grid = index 0,1,... or name: edo=0, loops=1)"
  );
}
