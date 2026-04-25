//! Auto-capture of system state when the audio callback stalls.
//!
//! Triggered from the main loop when the heartbeat sees STALL twice
//! in a row. Snapshots /proc/self/task/<audio_tid>/status, pw-top,
//! pw-dump, and a filtered ps; writes everything to
//! /tmp/grid_synth-stall-<unix>-<reason>.txt.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn capture_stall_diagnostics(reason: &str, audio_tid: u32) {
  let ts = SystemTime::now().duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs()).unwrap_or(0);
  let path = format!("/tmp/grid_synth-stall-{ts}-{reason}.txt");
  let mut out = String::new();
  out.push_str(&format!("=== grid_synth stall capture ({reason}) at unix={ts} ===\n\n"));

  // 1. Audio thread state from /proc. The interesting fields are
  // State, voluntary_ctxt_switches, nonvoluntary_ctxt_switches,
  // and the State line tells us R(running) vs S(sleeping). If the
  // audio thread is in S with no recent ctxt switches, PipeWire
  // genuinely isn't calling us — not a deadlock on our side.
  out.push_str(&format!("--- audio thread (tid {audio_tid}) ---\n"));
  if audio_tid == 0 {
    out.push_str("(audio TID never recorded — first callback never fired?)\n");
  } else {
    match std::fs::read_to_string(format!("/proc/self/task/{audio_tid}/status")) {
      Ok(s) => out.push_str(&s),
      Err(e) => out.push_str(&format!("(read failed: {e})\n")),
    }
  }
  out.push('\n');

  // 2. PipeWire's view of the graph.
  for (label, cmd, args) in &[
    ("pw-top -n 1 -b", "pw-top",  &["-n", "1", "-b"][..]),
    ("pw-dump",        "pw-dump", &[][..]),
  ] {
    out.push_str(&format!("--- {label} ---\n"));
    match std::process::Command::new(cmd).args(*args).output() {
      Ok(o) => {
        out.push_str(&String::from_utf8_lossy(&o.stdout));
        if !o.stderr.is_empty() {
          out.push_str("(stderr) ");
          out.push_str(&String::from_utf8_lossy(&o.stderr));
        }
      }
      Err(e) => out.push_str(&format!("({cmd} failed: {e})\n")),
    }
    out.push('\n');
  }

  // 3. Process snapshot, filtered to relevant audio + monome processes.
  out.push_str("--- ps (filtered) ---\n");
  match std::process::Command::new("ps").args(["auxf"]).output() {
    Ok(o) => {
      let all = String::from_utf8_lossy(&o.stdout);
      for line in all.lines() {
        if line.starts_with("USER")
          || line.contains("serialosc")
          || line.contains("grid_synth")
          || line.contains("pipewire")
          || line.contains("wireplumber")
          || line.contains("rtkit")
        {
          out.push_str(line);
          out.push('\n');
        }
      }
    }
    Err(e) => out.push_str(&format!("(ps failed: {e})\n")),
  }

  match std::fs::write(&path, &out) {
    Ok(_)  => eprintln!("[stall] captured diagnostics to {path}"),
    Err(e) => eprintln!("[stall] failed to write {path}: {e}\n--- partial ---\n{out}"),
  }
}
