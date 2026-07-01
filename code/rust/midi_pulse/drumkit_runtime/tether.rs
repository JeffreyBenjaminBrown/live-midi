//! Tether-mode control for the SoftStep: switch the device into hosted/streaming
//! mode and reliably back to standalone on exit.
//!
//! The mode-switch SysEx must go over ALSA *rawmidi* (`amidi`), NOT the sequencer:
//! the device ignores sequencer sends but honors rawmidi (see `learnings/keith-mcmillen-softstep.org`).
//! We read the sensor stream through midir/seq as usual; only these short control
//! messages go via `amidi`. None of this touches firmware -- it cannot brick the
//! device; it only flips a runtime mode that we always restore.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Override the rawmidi port for the mode-switch SysEx. Normally auto-detected from
/// `amidi -l`; set this if detection picks the wrong device.
const RAWMIDI_PORT_ENV: &str = "MIDI_PULSE_KMSS_RAWMIDI";
const DEFAULT_RAWMIDI_PORT: &str = "hw:1,0,0";

// Control SysEx, verbatim from KMI's editor (mirrors code/python/softstep/ss.py).
const STANDALONE_OFF: &str =
  "f000015f7a010000000000000000000000010009000b2b3a001003000000000000000050070000000002f7";
const STANDALONE_ON: &str =
  "f000015f7a010000000000000000000000010009000b2b3a001003010000000000000068660000000000f7";
const TETHER_ON: &str =
  "f000015f7a010000000000000000000000010009000b2b3a00100401000000000000002f7e0000000002f7";
const TETHER_OFF: &str =
  "f000015f7a010000000000000000000000010009000b2b3a0010040000000000000000171f0000000000f7";

/// The rawmidi port to drive: env override, else the SSCOM performance port parsed
/// from `amidi -l`, else a sensible default.
fn rawmidi_port() -> String {
  if let Ok(p) = std::env::var(RAWMIDI_PORT_ENV) {
    return p;
  }
  if let Ok(out) = Command::new("amidi").arg("-l").output() {
    let text = String::from_utf8_lossy(&out.stdout);
    let mut any_sscom: Option<String> = None;
    for line in text.lines() {
      if !line.contains("SSCOM") {
        continue;
      }
      if let Some(tok) = line.split_whitespace().find(|t| t.starts_with("hw:")) {
        if line.contains("MIDI 1") {
          return tok.to_string(); // the performance port (cable 0)
        }
        any_sscom.get_or_insert_with(|| tok.to_string());
      }
    }
    if let Some(p) = any_sscom {
      return p;
    }
  }
  DEFAULT_RAWMIDI_PORT.to_string()
}

fn amidi_send(messages: &[&str]) -> Result<(), String> {
  let hex = messages.join(" ");
  let output = Command::new("amidi")
    .args(["-p", &rawmidi_port(), "-i", "150", "-S", &hex])
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

fn enter_tether() -> Result<(), String> {
  amidi_send(&[STANDALONE_OFF, TETHER_ON])
}

fn restore_standalone() -> Result<(), String> {
  amidi_send(&[TETHER_OFF, STANDALONE_ON])
}

/// A handle that restores standalone mode exactly once -- on `Drop` (normal exit,
/// `?`, or panic unwind) and on a caught SIGINT/SIGTERM. Create with [`arm`] before
/// spawning audio/MIDI threads, then call [`TetherSession::enter`].
pub struct TetherSession {
  active: Arc<AtomicBool>,
}

/// Block SIGINT/SIGTERM process-wide and start the restore-on-signal waiter, then
/// return a session whose restoration is initially DISARMED. Call this as early as
/// possible (before any other thread spawns) so the signal mask is inherited by all
/// threads; a signal arriving before [`TetherSession::enter`] simply exits cleanly.
pub fn arm() -> TetherSession {
  let active = Arc::new(AtomicBool::new(false));
  install_signal_restore(Arc::clone(&active));
  TetherSession { active }
}

/// A `TetherSession` with NO signal handling installed -- restoration happens only
/// on `Drop`. For a host runtime (e.g. the surfaces runtime) that owns its own
/// SIGINT/SIGTERM handling and drives teardown itself, so it must not have the
/// drumkit's own signal thread call `process::exit` out from under it. The caller
/// is responsible for blocking the signals early (before spawning threads) and for
/// dropping this session on exit.
pub fn session() -> TetherSession {
  TetherSession { active: Arc::new(AtomicBool::new(false)) }
}

impl TetherSession {
  /// Enter tether mode and arm restoration.
  pub fn enter(&self) -> Result<(), String> {
    enter_tether()?;
    self.active.store(true, Ordering::SeqCst);
    Ok(())
  }
}

impl Drop for TetherSession {
  fn drop(&mut self) {
    if self.active.swap(false, Ordering::SeqCst) {
      match restore_standalone() {
        Ok(()) => println!("Restored standalone mode."),
        Err(e) => eprintln!("warning: could not restore standalone mode: {e}"),
      }
    }
  }
}

/// Block SIGINT/SIGTERM in the calling (main) thread -- so every thread spawned
/// afterward inherits the block -- and wait for them on a dedicated thread, which
/// restores standalone mode and exits. A default Ctrl-C would kill the process
/// without running `Drop`, leaving the device stuck streaming in tether mode.
fn install_signal_restore(active: Arc<AtomicBool>) {
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
    if active.swap(false, Ordering::SeqCst) {
      let _ = restore_standalone();
      eprintln!("\nRestored standalone mode (caught signal). Exiting.");
    }
    std::process::exit(130);
  });
}
