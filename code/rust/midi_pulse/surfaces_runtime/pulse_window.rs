//! The on-screen pulse readout window (`TODO/many/3_plan.org` phase 9; see
//! `TODO/many/2_discussion.org` 3c for what it shows and why it exists at all).
//! The `=1` LED that showed a grid's pulse switch, and the tap cell that blinked
//! the tapped tempo, both left the grid for the feet (the softstep pedals now
//! drive tap/factor/`=1` -- see `polyrhythm.rs` and `TODO/many/1_vision.org`), so
//! there is nowhere left on either monome to see this state. This window is where
//! it goes instead -- exactly the four things Jeff asked for, nothing more
//! ("The rest would be too much for me to read live"):
//! 1. the global tapped tempo as BPM to one decimal place (`readout::bpm`);
//! 2. each grid's factor as `2^x * 3^y` (`readout::grid_line`);
//! 3. whether each grid's pulse is on at all (also `readout::grid_line`);
//! 4. a blinker at the UNFACTORED tapped tempo (`PolyrhythmState::tap_blink`).
//!
//! Modeled on `edo12n_gui.rs`: `minifb`, a framebuffer, the shared bitmap font
//! (`crate::bitmap_font`), ~20 fps, Escape to quit. The difference is what drives
//! the redraw: `edo12n_gui` reacts to an mpsc channel of note events, but there is
//! no discrete "pulse changed" event here, so this window just polls
//! `PolyrhythmState` once per frame instead.
//!
//! Optional and non-fatal by design (like the rest of this runtime is robust to
//! missing gear, `TODO.org` "robust to missing gear"): opening a `minifb` window
//! needs a working, authorized X11 connection, which is exactly the thing this
//! container cannot always get (`TODO/many/2_discussion.org` 3c -- it needs
//! `xhost +local:` on the host). `spawn` never blocks its caller and `run` prints
//! one clear warning and returns if the window can't open, so a headless run --
//! or a host that hasn't run `xhost` yet -- never takes the instrument down with
//! it; the grids and audio don't depend on this thread existing.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use minifb::{Key, Window, WindowOptions};

use crate::bitmap_font::{self, GLYPH_H};

use super::polyrhythm::PolyrhythmState;
use super::readout;
use super::STOP;

const WIN_W: usize = 640;
const MARGIN: usize = 14;
/// A little breathing room below each text line's glyph height.
const LINE_H: usize = GLYPH_H + 12;
const TEXT_COLOR: u32 = 0xFFFFFF;
const BG_COLOR: u32 = 0x000000;
/// The blinker's lit color -- distinct from the plain white text, so it reads as
/// the one thing in the window that moves on its own.
const BLINK_COLOR: u32 = 0xFF3020;

/// Jeff's names for the two grids this rig actually has (`TODO/many/3_plan.org`
/// "gear and naming": LOM = left/older monome, RNM = right/newer). A hypothetical
/// third grid falls back to a plain label instead of panicking -- nothing here
/// depends on there being exactly two.
fn grid_name(index: usize) -> String {
  match index {
    0 => "LOM".to_string(),
    1 => "RNM".to_string(),
    n => format!("g{n}"),
  }
}

/// The window's height for `num_grids` grid lines: one BPM line, one line per
/// grid, and the blinker, each `LINE_H` tall, with a margin top/bottom and a
/// second margin's gap setting the blinker apart from the text above it.
fn win_h(num_grids: usize) -> usize {
  MARGIN * 3 + LINE_H * (num_grids + 2)
}

/// Spawn the pulse window on its own thread and return immediately -- opening (or
/// failing to open) the window never blocks or fails the caller.
pub fn spawn(poly: Arc<Mutex<PolyrhythmState>>, num_grids: usize) {
  std::thread::spawn(move || run(poly, num_grids));
}

fn run(poly: Arc<Mutex<PolyrhythmState>>, num_grids: usize) {
  // minifb talks X11; on Wayland this ensures DISPLAY is set for XWayland, the
  // same fixup `edo12n_gui` applies (a spawned process doesn't always inherit it).
  if std::env::var("DISPLAY").is_err() {
    std::env::set_var("DISPLAY", ":0");
  }
  let win_h = win_h(num_grids);
  let mut window = match Window::new("surfaces pulse", WIN_W, win_h, WindowOptions::default()) {
    Ok(w) => w,
    Err(e) => {
      // Non-fatal: the instrument plays fine without this window. Name the one
      // fix Jeff actually needs (TODO/many/2_discussion.org 3c) rather than just
      // printing the raw minifb error.
      eprintln!(
        "surfaces: the pulse window did not open ({e}) -- likely fix: run \
         `xhost +local:` on the host so this container's X11 connection is \
         authorized. Playing without the on-screen pulse readout."
      );
      return;
    }
  };
  window.set_target_fps(20);
  let mut buf = vec![0u32; WIN_W * win_h];

  // Escape or the window's own close button end only this thread -- the window is
  // a display aid, not something the rest of the instrument depends on, so
  // closing it must not stop the grids or audio. STOP (Ctrl-C / normal exit)
  // ends it from the other direction, so this thread never outlives the run.
  while window.is_open() && !window.is_key_down(Key::Escape) && !STOP.load(Ordering::SeqCst) {
    render(&mut buf, WIN_W, win_h, &poly, num_grids);
    if window.update_with_buffer(&buf, WIN_W, win_h).is_err() {
      // The X connection dropped mid-run (server restart, etc.): stop rather than
      // spin against a broken window.
      break;
    }
  }
}

/// Redraw the whole window from a fresh snapshot of `poly`. The lock is held only
/// long enough to read the four values out as owned data -- never across the
/// drawing calls below or a `window.update_with_buffer` -- per this module's
/// no-nested-locks rule (`surfaces_runtime/mod.rs`'s header comment); this thread
/// never touches any other lock, so there is nothing to nest it with anyway.
fn render(buf: &mut [u32], win_w: usize, win_h: usize, poly: &Arc<Mutex<PolyrhythmState>>, num_grids: usize) {
  let now = Instant::now();
  let (bpm, blink, lines) = {
    let p = poly.lock().unwrap_or_else(|e| e.into_inner());
    let bpm = readout::bpm(p.tapped_hz());
    let blink = p.tap_blink(now);
    let lines: Vec<String> = (0..num_grids)
      .map(|g| {
        let (two_exp, three_exp) = p.factor_exponents(g);
        readout::grid_line(&grid_name(g), p.pulse_on(g), two_exp, three_exp)
      })
      .collect();
    (bpm, blink, lines)
  }; // poly unlocked here -- everything below only touches `buf`.

  buf.fill(BG_COLOR);
  let mut y = MARGIN;
  let bpm_text = match bpm {
    Some(s) => format!("BPM {s}"),
    // No tempo has been tapped yet: say so plainly rather than printing "0.0",
    // which would read as a stopped clock rather than an absent one.
    None => "BPM no tempo".to_string(),
  };
  bitmap_font::draw_text(buf, win_w, win_h, &bpm_text, MARGIN, y, TEXT_COLOR);
  y += LINE_H;

  for line in &lines {
    bitmap_font::draw_text(buf, win_w, win_h, line, MARGIN, y, TEXT_COLOR);
    y += LINE_H;
  }

  // The blinker: the unfactored global tempo, 10% duty, phase-anchored to the
  // taps -- `tap_blink` already implements all of that, so this only paints what
  // it says, drawn as a filled rectangle rather than re-deriving the timing.
  y += MARGIN;
  if blink {
    fill_rect(buf, win_w, win_h, MARGIN, y, win_w - 2 * MARGIN, LINE_H, BLINK_COLOR);
  }
}

fn fill_rect(buf: &mut [u32], buf_w: usize, buf_h: usize, x0: usize, y0: usize, w: usize, h: usize, color: u32) {
  for y in y0..(y0 + h).min(buf_h) {
    for x in x0..(x0 + w).min(buf_w) {
      buf[y * buf_w + x] = color;
    }
  }
}
