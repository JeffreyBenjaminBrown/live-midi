//! The on-screen factored-pulse readout window (`TODO/many/3_plan.org` phase 9; see
//! `TODO/many/2_discussion.org` 3c for what it shows and why it exists at all).
//! The `=1` LED that showed a grid's factored-pulse switch, and the tap cell that
//! blinked the tapped tempo, both left the grid for the feet (the softstep pedals
//! now drive tap/tempo-factor/`=1` -- see `polyrhythm.rs` and
//! `TODO/many/1_vision.org`), so there is nowhere left on either monome to see this
//! state. This window is where it goes instead -- exactly the four things Jeff
//! asked for, nothing more ("The rest would be too much for me to read live"):
//! 1. the global tapped tempo as BPM to one decimal place (`readout::bpm`);
//! 2. each grid's tempo factor as `2^x * 3^y` (`readout::grid_line`);
//! 3. whether each grid's factored pulse is on at all (also `readout::grid_line`);
//! 4. a blinker at the UNFACTORED tapped tempo (`PolyrhythmState::tap_phase`).
//!
//! Modeled on `edo12n_gui.rs`: `minifb`, a framebuffer, the shared bitmap font
//! (`crate::bitmap_font`), ~20 fps, Escape to quit. The difference is what drives
//! the redraw: `edo12n_gui` reacts to an mpsc channel of note events, but there is
//! no discrete "state changed" event here, so this window just polls
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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use minifb::{Key, Window, WindowOptions};

use crate::bitmap_font::{self, GLYPH_H};

use crate::types::VoiceMap;

use super::polyrhythm::PolyrhythmState;
use super::readout;
use super::STOP;

/// Wide enough for the pedal-slide line, which is the longest thing here
/// (`LOM  pedal 072.4  low +0013.45`) at the shared font's 20px cell.
const WIN_W: usize = 780;
const MARGIN: usize = 14;
/// A little breathing room below each text line's glyph height.
const LINE_H: usize = GLYPH_H + 12;
const TEXT_COLOR: u32 = 0xFFFFFF;
const BG_COLOR: u32 = 0x000000;
/// The blinker's lit color -- distinct from the plain white text, so it reads as
/// the one thing in the window that moves on its own.
const BLINK_COLOR: u32 = 0xFF3020;
/// The pedal-slide lines' colour -- readable, and distinct from the tempo block above
/// so the eye can find the number it is hunting without reading the labels.
const SLIDE_COLOR: u32 = 0x60D0FF;

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

/// The window's height: one BPM line, one factored-pulse line per grid, one
/// PEDAL SLIDE line per grid, and the blinker -- each `LINE_H` tall, with a margin
/// top/bottom and a second margin's gap setting the blinker apart from the text.
fn win_h(num_grids: usize) -> usize {
  MARGIN * 3 + LINE_H * (2 * num_grids + 2)
}

/// Spawn the factored-pulse window on its own thread and return immediately --
/// opening (or failing to open) the window never blocks or fails the caller.
pub fn spawn(poly: Arc<Mutex<PolyrhythmState>>, num_grids: usize, slide: SlideReadout) {
  std::thread::spawn(move || run(poly, num_grids, slide));
}

/// What the pedal-slide lines read from: each grid's published pedal position (the
/// same atomic the grid threads drive their slides from -- NaN until that pedal has
/// ever reported) and the shared voice map, plus the tuning needed to say a frequency
/// as EDO steps.
///
/// Read-only and lock-light on purpose: this is a display thread, and it must never be
/// able to perturb what it is displaying.
#[derive(Clone)]
pub struct SlideReadout {
  pub frac: Arc<Vec<AtomicU32>>,
  pub voices: Arc<Mutex<VoiceMap>>,
  pub fund: f64,
  pub edo: i32,
}

impl SlideReadout {
  /// One grid's `(pedal position on its own 0..120 scale, lowest sounding voice in EDO
  /// steps)`. Either is `None` when there is nothing honest to say: no CC yet from that
  /// pedal, or no voice on that grid.
  fn read(&self, grid: usize) -> (Option<f32>, Option<f32>) {
    let f = self.frac.get(grid).map(|a| f32::from_bits(a.load(Ordering::Relaxed)));
    let pedal = f.filter(|f| !f.is_nan()).map(|f| f * 120.0);
    let voices = self.voices.lock().unwrap_or_else(|e| e.into_inner());
    // The LOWEST voice by frequency, which is what the ear picks out of a slide and
    // what Jeff asked to watch. Release tails are excluded -- a dying voice is not what
    // the pedal is moving. Chord and fingered voices count: any of them can be slid.
    let lowest = voices
      .iter()
      .filter(|(src, _)| {
        matches!(src,
          crate::types::VoiceSource::SurfaceFinger { grid: g, .. }
          | crate::types::VoiceSource::SurfaceDrone { grid: g, .. }
          | crate::types::VoiceSource::SurfaceChord { grid: g, .. } if *g == grid)
      })
      .map(|(_, v)| v.freq)
      .filter(|hz| hz.is_finite() && *hz > 0.0)
      .fold(f32::INFINITY, f32::min);
    drop(voices);
    let pitch = readout::steps_from_hz(lowest, self.fund, self.edo);
    (pedal, pitch)
  }
}

fn run(poly: Arc<Mutex<PolyrhythmState>>, num_grids: usize, slide: SlideReadout) {
  // minifb talks X11; on Wayland this ensures DISPLAY is set for XWayland, the
  // same fixup `edo12n_gui` applies (a spawned process doesn't always inherit it).
  if std::env::var("DISPLAY").is_err() {
    std::env::set_var("DISPLAY", ":0");
  }
  let win_h = win_h(num_grids);
  let mut window = match Window::new("surfaces readout", WIN_W, win_h, WindowOptions::default()) {
    Ok(w) => w,
    Err(e) => {
      // Non-fatal: the instrument plays fine without this window. Name the one
      // fix Jeff actually needs (TODO/many/2_discussion.org 3c) rather than just
      // printing the raw minifb error.
      eprintln!(
        "surfaces: the factored-pulse window did not open ({e}) -- likely fix: run \
         `xhost +local:` on the host so this container's X11 connection is \
         authorized. Playing without the on-screen factored-pulse readout."
      );
      return;
    }
  };
  // 60, not 20. The blinker is the only thing here that moves fast, and at 20 fps
  // (50 ms frames) a brisk tempo's flash is shorter than a single frame -- see
  // `screen_blink_on`. Redrawing a handful of text lines is cheap.
  window.set_target_fps(60);
  let mut buf = vec![0u32; WIN_W * win_h];

  // Escape or the window's own close button end only this thread -- the window is
  // a display aid, not something the rest of the instrument depends on, so
  // closing it must not stop the grids or audio. STOP (Ctrl-C / normal exit)
  // ends it from the other direction, so this thread never outlives the run.
  while window.is_open() && !window.is_key_down(Key::Escape) && !STOP.load(Ordering::SeqCst) {
    render(&mut buf, WIN_W, win_h, &poly, num_grids, &slide);
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
/// The shortest flash a redraw can reliably show. Below this it is a coin toss
/// whether any frame lands inside the lit window, so the blink reads as erratic --
/// which is exactly what it did at 20 fps with the LED's 10% duty.
const MIN_BLINK_ON: f32 = 0.060;

/// Never let the flash swallow more than half the cycle; past that it stops reading
/// as a pulse and starts reading as a square wave that is mostly on.
const MAX_BLINK_DUTY: f32 = 0.5;

/// Is the on-screen blinker lit, given the tap phase and tempo?
///
/// The LED's 10% duty is right for an LED and wrong for a screen: a monome cell is
/// scanned far faster than it flashes, while a window redraws at tens of Hz. At 120
/// bpm a 10% flash is 50 ms -- one frame at 20 fps, well under one at 240 bpm -- so it
/// renders or not depending on where the frame boundary happens to fall.
///
/// So the screen keeps the LED's *phase* (both are anchored to the taps, both
/// unfactored, so they agree about WHEN the beat is) and widens the *duty* only as far
/// as it must: at slow tempos this is the same 10% spark, and it stretches only once
/// 10% would be too brief to draw.
fn screen_blink_on(phase: Option<f32>, hz: Option<f32>) -> bool {
  let (Some(phase), Some(hz)) = (phase, hz) else { return false };
  if hz <= 0.0 {
    return false;
  }
  let period = 1.0 / hz;
  let duty = (MIN_BLINK_ON / period).max(LED_BLINK_DUTY).min(MAX_BLINK_DUTY);
  phase < duty
}

/// The LED's duty, mirrored here so the two agree at tempos slow enough for both.
const LED_BLINK_DUTY: f32 = 0.1;

fn render(
  buf: &mut [u32],
  win_w: usize,
  win_h: usize,
  poly: &Arc<Mutex<PolyrhythmState>>,
  num_grids: usize,
  slide: &SlideReadout,
) {
  let now = Instant::now();
  let (bpm, blink, lines) = {
    let p = poly.lock().unwrap_or_else(|e| e.into_inner());
    let bpm = readout::bpm(p.tapped_hz());
    let blink = screen_blink_on(p.tap_phase(now), p.tapped_hz());
    let lines: Vec<String> = (0..num_grids)
      .map(|g| {
        let (two_exp, three_exp) = p.tempo_factor_exponents(g);
        readout::grid_line(&grid_name(g), p.factored_pulse_on(g), two_exp, three_exp)
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

  // The pedal-slide lines (TODO/pedal-slide): where each pedal is, and what the lowest
  // voice on its grid is actually sounding. Both are read AFTER the poly lock has been
  // dropped -- this thread holds one lock at a time, never two (the module's rule).
  // A distinct colour so they read as a separate instrument from the tempo block.
  for g in 0..num_grids {
    let (pedal, pitch) = slide.read(g);
    let line = readout::pedal_slide_line(&grid_name(g), pedal, pitch);
    bitmap_font::draw_text(buf, win_w, win_h, &line, MARGIN, y, SLIDE_COLOR);
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

#[cfg(test)]
mod tests {
  use super::*;

  /// The bug Jeff saw: "the tempo flashing on the computer screen is all sorts of
  /// off". At 20 fps a 10% flash is one frame at 120 bpm and HALF a frame at 240, so
  /// whether it drew at all depended on where the frame boundary landed. Whatever the
  /// tempo, the lit window must now span several frames.
  #[test]
  fn the_flash_is_never_shorter_than_a_frame_at_any_musical_tempo() {
    let frame = 1.0 / 60.0;
    for bpm in [40.0f32, 60.0, 120.0, 180.0, 240.0, 300.0] {
      let hz = bpm / 60.0;
      let period = 1.0 / hz;
      // The lit fraction, found by asking the rule where it turns off.
      let duty = (1..=1000)
        .map(|i| i as f32 / 1000.0)
        .take_while(|p| screen_blink_on(Some(*p), Some(hz)))
        .last()
        .expect("some part of the cycle is lit");
      let on_secs = duty * period;
      assert!(
        on_secs >= frame * 2.0,
        "{bpm} bpm: lit for {:.0} ms = {:.1} frames -- too brief to draw reliably",
        on_secs * 1000.0,
        on_secs / frame,
      );
    }
  }

  /// ...but it must stay a PULSE, not creep toward a square wave that is mostly on.
  #[test]
  fn the_flash_never_swallows_more_than_half_the_cycle() {
    for bpm in [120.0f32, 240.0, 600.0, 2000.0] {
      let hz = bpm / 60.0;
      assert!(!screen_blink_on(Some(0.5), Some(hz)), "{bpm} bpm: still lit at half-cycle");
    }
  }

  /// At tempos slow enough for both, the screen keeps the LED's own 10% spark -- the
  /// duty widens only when it must.
  #[test]
  fn a_slow_tempo_keeps_the_leds_ten_percent_duty() {
    let hz = 1.0; // 60 bpm: a 10% flash is 100 ms, plenty of frames
    assert!(screen_blink_on(Some(0.09), Some(hz)));
    assert!(!screen_blink_on(Some(0.11), Some(hz)), "no wider than the LED needs");
  }

  /// The screen and the LED must agree about WHEN the beat is, or the window and the
  /// grid would disagree about the tempo they are both showing.
  #[test]
  fn the_flash_starts_at_the_top_of_the_cycle_like_the_led() {
    assert!(screen_blink_on(Some(0.0), Some(2.0)), "lit at the downbeat");
    assert!(!screen_blink_on(Some(0.99), Some(2.0)), "dark just before it");
  }

  #[test]
  fn there_is_no_blink_before_a_tempo_is_tapped() {
    assert!(!screen_blink_on(None, None));
    assert!(!screen_blink_on(Some(0.0), None));
    assert!(!screen_blink_on(None, Some(2.0)));
  }

  #[test]
  fn a_nonsense_tempo_does_not_blink_or_divide_by_zero() {
    assert!(!screen_blink_on(Some(0.0), Some(0.0)));
    assert!(!screen_blink_on(Some(0.0), Some(-1.0)));
  }
}
