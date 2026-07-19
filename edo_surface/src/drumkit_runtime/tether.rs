//! Tether-mode control for the SoftStep: switch each device into hosted/streaming
//! mode and reliably back to standalone on exit.
//!
//! The mode-switch SysEx must go over ALSA *rawmidi* (`amidi`), NOT the sequencer:
//! the device ignores sequencer sends but honors rawmidi (see `learnings/keith-mcmillen-softstep.org`).
//! We read the sensor stream through midir/seq as usual; only these short control
//! messages go via `amidi`. None of this touches firmware -- it cannot brick the
//! device; it only flips a runtime mode that we always restore.
//!
//! *Per device.* One [`TetherSession`] covers every declared board: [`TetherSession::enter`]
//! is called once per SoftStep with that board's own `select.name_contains`, and each
//! resolves its own rawmidi port from `amidi -l`. This matters because a board left in
//! standalone streams NO sensors -- it is silent, not merely degraded -- so with two
//! boards, missing one means half the instrument does nothing.
//!
//! Every restore path (`Drop`, and the SIGINT/SIGTERM thread) drains the whole board
//! list and attempts each independently, because the failure mode to avoid is a
//! *half*-restored pair: a board still in tether mode after exit streams CCs forever
//! with nothing listening, and only a replug or another run will clear it.

use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::midi;

/// Override the rawmidi port for the mode-switch SysEx. Normally auto-detected from
/// `amidi -l`; set this if detection picks the wrong device.
///
/// NOTE this override is process-wide: it forces EVERY board onto one port, so it is
/// only meaningful when a single SoftStep is declared. With two boards, let detection
/// do its job (it matches each board's own `select.name_contains`).
const RAWMIDI_PORT_ENV: &str = "MIDI_PULSE_KMSS_RAWMIDI";
const DEFAULT_RAWMIDI_PORT: &str = "hw:1,0,0";

// Control SysEx, verbatim from KMI's editor (mirrors tools/softstep/ss.py).
const STANDALONE_OFF: &str =
  "f000015f7a010000000000000000000000010009000b2b3a001003000000000000000050070000000002f7";
const STANDALONE_ON: &str =
  "f000015f7a010000000000000000000000010009000b2b3a001003010000000000000068660000000000f7";
const TETHER_ON: &str =
  "f000015f7a010000000000000000000000010009000b2b3a00100401000000000000002f7e0000000002f7";
const TETHER_OFF: &str =
  "f000015f7a010000000000000000000000010009000b2b3a0010040000000000000000171f0000000000f7";

/// Pick the rawmidi port for the board whose `amidi -l` line contains
/// `select_substring` -- the SAME selector the rig uses to bind that board's sensor
/// stream, so the two always agree. Among that board's ports, prefer its DATA port
/// (`midi::SOFTSTEP_DATA_PORT_PREFERENCE`): "SSCOM MIDI 1" on the original board,
/// "SoftStep Control Surface" on the newer one.
///
/// Pure, so it can be tested against real `amidi -l` output without a device.
/// `None` when no line matches.
pub fn parse_rawmidi_port(amidi_l: &str, select_substring: &str) -> Option<String> {
  let mine: Vec<&str> = amidi_l.lines().filter(|l| l.contains(select_substring)).collect();
  let hw = |line: &str| -> Option<String> {
    line.split_whitespace().find(|t| t.starts_with("hw:")).map(str::to_string)
  };
  for want in midi::SOFTSTEP_DATA_PORT_PREFERENCE {
    if let Some(p) = mine.iter().find(|l| l.contains(want)).and_then(|l| hw(l)) {
      return Some(p);
    }
  }
  mine.iter().find_map(|l| hw(l))
}

/// The rawmidi port to drive for the board matching `select_substring`: env override
/// (process-wide -- see [`RAWMIDI_PORT_ENV`]), else parsed from `amidi -l`, else a
/// sensible default.
fn rawmidi_port(select_substring: &str) -> String {
  if let Ok(p) = std::env::var(RAWMIDI_PORT_ENV) {
    return p;
  }
  if let Ok(out) = Command::new("amidi").arg("-l").output() {
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(p) = parse_rawmidi_port(&text, select_substring) {
      return p;
    }
  }
  DEFAULT_RAWMIDI_PORT.to_string()
}

fn amidi_send(port: &str, messages: &[&str]) -> Result<(), String> {
  let hex = messages.join(" ");
  let output = Command::new("amidi")
    .args(["-p", port, "-i", "150", "-S", &hex])
    .output()
    .map_err(|e| format!("running `amidi` (install alsa-utils?): {e}"))?;
  if !output.status.success() {
    return Err(format!(
      "amidi exited {}: {}",
      output.status,
      String::from_utf8_lossy(&output.stderr).trim()
    ));
  }
  Ok(())
}

fn enter_tether(port: &str) -> Result<(), String> {
  amidi_send(port, &[STANDALONE_OFF, TETHER_ON])
}

fn restore_standalone(port: &str) -> Result<(), String> {
  amidi_send(port, &[TETHER_OFF, STANDALONE_ON])
}

/// Restore every board still in tether mode, and empty the list so a later Drop or
/// signal cannot double-restore. Each board is attempted independently: one failure
/// must not strand the others in tether mode, which is exactly the half-restored
/// state this whole module exists to prevent.
fn restore_all(boards: &Mutex<Vec<String>>, announce: bool) {
  let ports: Vec<String> = match boards.lock() {
    Ok(mut g) => std::mem::take(&mut *g),
    // A panic elsewhere poisoned the lock; the boards still need restoring.
    Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
  };
  for port in &ports {
    match restore_standalone(port) {
      Ok(()) if announce => println!("Restored standalone mode ({port})."),
      Ok(()) => {}
      Err(e) => eprintln!("warning: could not restore standalone mode on {port}: {e}"),
    }
  }
}

/// A handle that restores standalone mode exactly once -- on `Drop` (normal exit,
/// `?`, or panic unwind) and on a caught SIGINT/SIGTERM. Create with [`arm`] before
/// spawning audio/MIDI threads, then call [`TetherSession::enter`] once per board.
///
/// ONE session covers ALL boards: `enter` appends, and every restore path drains the
/// whole list. Two SoftSteps must both come back, and a board left in tether mode
/// streams sensor CCs forever with nothing listening -- so partial restoration is the
/// failure this type exists to make impossible.
pub struct TetherSession {
  /// Ports currently in tether mode. Empty once restored, so restoring twice is a
  /// no-op rather than a double-send.
  boards: Arc<Mutex<Vec<String>>>,
}

/// Block SIGINT/SIGTERM process-wide and start the restore-on-signal waiter, then
/// return a session whose restoration is initially DISARMED. Call this as early as
/// possible (before any other thread spawns) so the signal mask is inherited by all
/// threads; a signal arriving before [`TetherSession::enter`] simply exits cleanly.
pub fn arm() -> TetherSession {
  let boards = Arc::new(Mutex::new(Vec::new()));
  install_signal_restore(Arc::clone(&boards));
  TetherSession { boards }
}

/// A `TetherSession` with NO signal handling installed -- restoration happens only
/// on `Drop`. For a host runtime (e.g. the surfaces runtime) that owns its own
/// SIGINT/SIGTERM handling and drives teardown itself, so it must not have the
/// drumkit's own signal thread call `process::exit` out from under it. The caller
/// is responsible for blocking the signals early (before spawning threads) and for
/// dropping this session on exit.
pub fn session() -> TetherSession {
  TetherSession { boards: Arc::new(Mutex::new(Vec::new())) }
}

impl TetherSession {
  /// Put the board matching `select_substring` into tether mode and arm its
  /// restoration. Call once per declared SoftStep; `select_substring` is that board's
  /// own `select.name_contains`, so each board resolves to its own rawmidi port.
  ///
  /// The board is registered for restoration BEFORE the mode switch is attempted: a
  /// send that fails halfway (the device may have taken `STANDALONE_OFF` and missed
  /// `TETHER_ON`) still needs restoring, and restoring a board that never entered is
  /// harmless.
  pub fn enter(&self, select_substring: &str) -> Result<(), String> {
    let port = rawmidi_port(select_substring);
    match self.boards.lock() {
      Ok(mut g) => {
        if !g.contains(&port) {
          g.push(port.clone());
        }
      }
      Err(poisoned) => {
        let mut g = poisoned.into_inner();
        if !g.contains(&port) {
          g.push(port.clone());
        }
      }
    }
    enter_tether(&port)
  }
}

impl Drop for TetherSession {
  fn drop(&mut self) {
    restore_all(&self.boards, true);
  }
}

/// Block SIGINT/SIGTERM in the calling (main) thread -- so every thread spawned
/// afterward inherits the block -- and wait for them on a dedicated thread, which
/// restores standalone mode and exits. A default Ctrl-C would kill the process
/// without running `Drop`, leaving the device stuck streaming in tether mode.
fn install_signal_restore(boards: Arc<Mutex<Vec<String>>>) {
  unsafe {
    let mut set: libc::sigset_t = std::mem::zeroed();
    libc::sigemptyset(&mut set);
    libc::sigaddset(&mut set, libc::SIGINT);
    libc::sigaddset(&mut set, libc::SIGTERM);
    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
  }
  std::thread::spawn(move || {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut sig: libc::c_int = 0;
    unsafe {
      libc::sigemptyset(&mut set);
      libc::sigaddset(&mut set, libc::SIGINT);
      libc::sigaddset(&mut set, libc::SIGTERM);
      libc::sigwait(&set, &mut sig);
    }
    restore_all(&boards, false);
    eprintln!("\nRestored standalone mode on every board (caught signal). Exiting.");
    std::process::exit(130);
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Real `amidi -l` shape: a header, then "Dir  Device  Name" rows.
  const BOTH_BOARDS: &str = "\
Dir Device    Name
IO  hw:1,0,0  SSCOM MIDI 1
IO  hw:1,0,1  SSCOM MIDI 2
IO  hw:2,0,0  SoftStep Control Surface
IO  hw:2,0,1  SoftStep TRS MIDI Out
IO  hw:2,0,2  SoftStep CV Out
";

  #[test]
  fn original_board_resolves_to_its_own_data_port() {
    assert_eq!(parse_rawmidi_port(BOTH_BOARDS, "SSCOM"), Some("hw:1,0,0".into()));
  }

  /// The regression: the newer board's data port is "Control Surface", and the old
  /// hardcoded "SSCOM" + "MIDI 1" logic could never have found it -- it would have
  /// fallen through to the hw:1,0,0 default, i.e. the OTHER board.
  #[test]
  fn newer_board_resolves_to_control_surface_not_trs_or_cv() {
    assert_eq!(parse_rawmidi_port(BOTH_BOARDS, "SoftStep"), Some("hw:2,0,0".into()));
  }

  /// The whole point of phase 1: with both boards present the two selectors must
  /// resolve to DIFFERENT cards, or one board never enters tether mode.
  #[test]
  fn the_two_boards_resolve_to_different_ports() {
    let a = parse_rawmidi_port(BOTH_BOARDS, "SSCOM").unwrap();
    let b = parse_rawmidi_port(BOTH_BOARDS, "SoftStep").unwrap();
    assert_ne!(a, b);
  }

  #[test]
  fn no_matching_board_is_none() {
    assert_eq!(parse_rawmidi_port(BOTH_BOARDS, "Launchpad"), None);
  }

  /// Verbatim `amidi -l` from Jeff's rig with both boards connected (2026-07-16).
  /// Kept because it is the real thing and it is *adversarial by accident*: the newer
  /// board enumerated FIRST, on card 1 -- which is exactly `DEFAULT_RAWMIDI_PORT`.
  /// So the old code's fallback would have looked correct on the newer board while
  /// actually being a coincidence, and the older board (card 3) is the one it named.
  const JEFFS_RIG: &str = "\
Dir Device    Name
IO  hw:1,0,0  SoftStep Control Surface
 O  hw:1,0,1  SoftStep TRS MIDI Out
 O  hw:1,0,2  SoftStep CV Out
IO  hw:3,0,0  SSCOM MIDI 1
IO  hw:3,0,1  SSCOM MIDI 2
";

  #[test]
  fn real_hardware_both_boards_resolve_to_their_own_data_port() {
    assert_eq!(parse_rawmidi_port(JEFFS_RIG, "SSCOM"), Some("hw:3,0,0".into()));
    assert_eq!(parse_rawmidi_port(JEFFS_RIG, "SoftStep"), Some("hw:1,0,0".into()));
  }

  /// The card numbers are not stable (they follow plug order), so nothing may assume
  /// "SSCOM is card 1". The old hardcoded default did exactly that.
  #[test]
  fn real_hardware_the_default_port_is_not_the_sscom_board() {
    assert_ne!(
      parse_rawmidi_port(JEFFS_RIG, "SSCOM"),
      Some(DEFAULT_RAWMIDI_PORT.to_string()),
      "on this hardware the hw:1,0,0 default is the OTHER board",
    );
  }

  /// A board with no preferred name still resolves, rather than silently vanishing.
  #[test]
  fn falls_back_to_the_boards_first_port_when_no_preference_matches() {
    let odd = "IO  hw:3,0,0  SSCOM Mystery Port\n";
    assert_eq!(parse_rawmidi_port(odd, "SSCOM"), Some("hw:3,0,0".into()));
  }

  #[test]
  fn restore_all_drains_so_a_second_restore_is_a_no_op() {
    let boards = Mutex::new(vec!["hw:9,9,9".to_string()]);
    // `amidi` is absent or will fail against a bogus port; either way restore_all
    // must not panic, and must clear the list so Drop cannot double-send.
    restore_all(&boards, false);
    assert!(boards.lock().unwrap().is_empty());
    restore_all(&boards, false);
    assert!(boards.lock().unwrap().is_empty());
  }

  #[test]
  fn a_session_with_no_boards_restores_nothing_and_does_not_panic() {
    drop(session());
  }
}
