//! The on-screen text for this instrument's windows (`edo12n_gui`'s per-cell shift
//! numbers, `surfaces_runtime::pulse_window`'s pulse readout). Shared so the
//! pixel-fiddling -- plotting a glyph into a `minifb` framebuffer -- lives in exactly
//! one place instead of being copied per window.
//!
//! *Anti-aliased, from a real font.* This was a hand-rolled 5x7 bitmap blown up 4x,
//! which meant every "pixel" was a 4x4 square and the edges were visibly stepped.
//! Jeff asked for smoother letters and explicitly wanted everything else kept:
//! monospaced, the same size, the same thick strokes. So the glyphs now come from
//! *DejaVu Sans Mono Bold*, rasterized by `fontdue` into a coverage mask and blended
//! against the background -- monospaced and bold by construction, same cell metrics as
//! before, just no staircase.
//!
//! *Why this font.* Monospaced (Jeff: "unispaced is good"), a genuine Bold weight
//! rather than a synthesised one (Jeff: "thick lines in the letters are good"), and
//! designed for screen legibility. It is embedded rather than loaded from the system
//! because this container has *no fonts installed at all* -- so anything resolved at
//! runtime would work on my machine and not on the next one. Licence is permissive
//! (Bitstream Vera derivative; see `fonts/LICENSE`).
//!
//! Unsupported characters draw nothing rather than panic: a missing glyph should read
//! as a gap, not crash a running instrument.

use std::sync::OnceLock;

use fontdue::{Font, FontSettings};

/// DejaVu Sans Mono Bold. Embedded (see the module note): resolving a system font at
/// runtime is not an option in a container with none.
const FONT_BYTES: &[u8] = include_bytes!("fonts/DejaVuSansMono-Bold.ttf");

/// The px size to rasterize at, picked by measuring rather than by eye: it makes this
/// font's glyphs the same SIZE as the ones it replaces, which is what Jeff asked for
/// ("size is good ... just need smoother edges").
///
/// The old 5x7-at-4x font inked caps 28px tall and 20px wide into a 21px cell. At 36px
/// DejaVu Sans Mono Bold inks caps 27px tall and 20px wide, advancing 21.7 -- so the
/// letters land within a pixel of their old size and keep the old 1px gap. Going
/// bigger overflows the cell (at 38px an `M` is 21px wide and letters touch); smaller
/// visibly shrinks them (at 34px caps are only 25px, ~10% down).
const FONT_PX: f32 = 36.0;

/// The cell a glyph occupies, unchanged from the bitmap font it replaces
/// (5 * 4 wide, 7 * 4 tall), so callers' layout arithmetic still holds.
pub const SCALE: usize = 4;
pub const GLYPH_W: usize = 5 * SCALE;
pub const GLYPH_H: usize = 7 * SCALE;

/// The rasterizer, built once. Parsing the font on every glyph would be absurd at
/// 20 fps, and `Font` is immutable once built.
fn font() -> &'static Font {
  static FONT: OnceLock<Font> = OnceLock::new();
  FONT.get_or_init(|| {
    Font::from_bytes(FONT_BYTES, FontSettings::default())
      .expect("the embedded DejaVu Sans Mono Bold is valid (it ships with this binary)")
  })
}

/// Blend `color` over `under` by `coverage` (0..=255). This is what buys the smooth
/// edge: a partly-covered pixel becomes a partly-mixed colour rather than being
/// forced fully on or fully off.
fn blend(under: u32, color: u32, coverage: u8) -> u32 {
  if coverage == 0 {
    return under;
  }
  if coverage == 255 {
    return color;
  }
  let a = coverage as u32;
  let mix = |shift: u32| {
    let c = (color >> shift) & 0xFF;
    let u = (under >> shift) & 0xFF;
    ((c * a + u * (255 - a)) / 255) & 0xFF
  };
  (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// Plot one glyph at `(x0, y0)` into `buf` (`buf_w` x `buf_h`), clipping silently at
/// the buffer edge rather than panicking on an off-screen line.
///
/// `(x0, y0)` is the top-left of the glyph's CELL, as it was for the bitmap font, so
/// callers position text exactly as before. The glyph is placed within that cell from
/// the font's own metrics: horizontally centred (a monospace advance is not the same
/// as the inked width -- `l` is narrow, `m` is wide) and sat on a common baseline, so
/// letters line up instead of each floating to its own height.
pub fn draw_glyph(buf: &mut [u32], buf_w: usize, buf_h: usize, ch: char, x0: usize, y0: usize, color: u32) {
  // A character the font lacks draws NOTHING, not the `.notdef` tofu box fontdue
  // would otherwise rasterize. That keeps the contract the callers were written
  // against, and a stray box in a glanceable readout is worse noise than a gap.
  if font().lookup_glyph_index(ch) == 0 {
    return;
  }
  let (metrics, mask) = font().rasterize(ch, FONT_PX);
  if metrics.width == 0 || metrics.height == 0 {
    return; // a space, or a glyph with no ink
  }
  // Baseline: the cell is GLYPH_H tall and the text is caps/digits-dominant, so sit
  // the baseline near the bottom of the cell with a little room for descenders.
  let baseline = y0 as i32 + GLYPH_H as i32 - SCALE as i32;
  let gx = x0 as i32 + (GLYPH_W as i32 - metrics.width as i32) / 2;
  let gy = baseline - metrics.height as i32 - metrics.ymin;

  for row in 0..metrics.height {
    for col in 0..metrics.width {
      let coverage = mask[row * metrics.width + col];
      if coverage == 0 {
        continue;
      }
      let px = gx + col as i32;
      let py = gy + row as i32;
      if px < 0 || py < 0 || px as usize >= buf_w || py as usize >= buf_h {
        continue;
      }
      let i = py as usize * buf_w + px as usize;
      buf[i] = blend(buf[i], color, coverage);
    }
  }
}

/// Plot `text` left-to-right starting at `(x0, y0)`, one glyph after another with the
/// same 1px gap between cells the bitmap font used. The advance is the fixed cell,
/// not the font's own, so the columns stay aligned no matter what is printed -- which
/// is the whole point of a monospaced readout.
pub fn draw_text(buf: &mut [u32], buf_w: usize, buf_h: usize, text: &str, x0: usize, y0: usize, color: u32) {
  let mut x = x0;
  for ch in text.chars() {
    draw_glyph(buf, buf_w, buf_h, ch, x, y0, color);
    x += GLYPH_W + 1;
  }
}

/// The pixel width `draw_text` will occupy for `text` -- for centering it in a cell
/// (as `edo12n_gui`'s shift numbers do) or sizing a window to fit it.
pub fn text_width(text: &str) -> usize {
  let n = text.chars().count();
  if n == 0 {
    0
  } else {
    n * (GLYPH_W + 1) - 1
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const FG: u32 = 0x00FF_FFFF;
  const BG: u32 = 0x0000_0000;

  fn render(text: &str) -> (Vec<u32>, usize, usize) {
    let w = text_width(text) + 8;
    let h = GLYPH_H + 8;
    let mut buf = vec![BG; w * h];
    draw_text(&mut buf, w, h, text, 4, 4, FG);
    (buf, w, h)
  }

  #[test]
  fn the_embedded_font_parses() {
    // If this fails the binary shipped a broken font and every window is blank.
    assert!(font().rasterize('M', FONT_PX).0.width > 0);
  }

  /// The point of the change: edges must be ANTI-ALIASED. A hand-rolled bitmap can
  /// only ever produce fully-on or fully-off pixels, so the presence of intermediate
  /// values is exactly what distinguishes this from what it replaced.
  #[test]
  fn glyphs_have_partly_covered_edge_pixels() {
    let (buf, _, _) = render("S");
    let partial = buf.iter().filter(|p| **p != BG && **p != FG).count();
    assert!(partial > 0, "no intermediate pixels -- the glyph is not anti-aliased");
  }

  #[test]
  fn a_glyph_actually_inks_pixels() {
    let (buf, _, _) = render("M");
    assert!(buf.iter().any(|p| *p != BG), "M drew nothing");
  }

  /// Jeff asked for the size to stay put, and `edo12n_gui` centres its numbers with
  /// `text_width` -- a changed cell would silently shift them.
  #[test]
  fn the_cell_metrics_are_unchanged_from_the_bitmap_font_they_replaced() {
    assert_eq!(GLYPH_W, 20);
    assert_eq!(GLYPH_H, 28);
    assert_eq!(text_width("123"), 3 * 21 - 1);
    assert_eq!(text_width(""), 0);
  }

  /// Monospaced: every glyph advances one fixed cell, whatever its natural width.
  #[test]
  fn narrow_and_wide_letters_advance_by_the_same_cell() {
    assert_eq!(text_width("lll"), text_width("mmm"));
  }

  #[test]
  fn a_space_draws_nothing_but_still_advances() {
    let (buf, _, _) = render(" ");
    assert!(buf.iter().all(|p| *p == BG), "a space inked something");
    assert_eq!(text_width("a b"), 3 * 21 - 1);
  }

  /// A glyph outside the font must not crash a running instrument.
  #[test]
  fn an_unsupported_character_draws_nothing_rather_than_panicking() {
    let (buf, _, _) = render("\u{10FFFF}");
    assert!(buf.iter().all(|p| *p == BG));
  }

  /// Drawing off the edge clips instead of panicking -- a window can be resized
  /// smaller than its text.
  #[test]
  fn drawing_past_the_buffer_edge_clips_silently() {
    let mut buf = vec![BG; 10 * 10];
    draw_text(&mut buf, 10, 10, "MMMM", 8, 8, FG);
    draw_glyph(&mut buf, 10, 10, 'M', 100, 100, FG);
  }

  /// Not an assertion -- a way to LOOK at the result. Tests can say "there are
  /// intermediate pixels"; they cannot say "that is legible". Run with
  /// `MIDI_PULSE_FONT_PNG=/tmp/font.png cargo test -p midi_pulse --bin midi_pulse font_sample`
  /// and open the file.
  #[test]
  fn font_sample_png() {
    let Some(path) = std::env::var_os("MIDI_PULSE_FONT_PNG") else { return };
    let lines = ["BPM 137.4", "LOM  pulse ON   2^1 * 3^-1", "RNM  pulse off  2^0 * 3^0", "no tempo"];
    let w = lines.iter().map(|l| text_width(l)).max().unwrap() + 16;
    let h = lines.len() * (GLYPH_H + 8) + 16;
    let mut buf = vec![0x0011_1111u32; w * h];
    for (i, line) in lines.iter().enumerate() {
      draw_text(&mut buf, w, h, line, 8, 8 + i * (GLYPH_H + 8), 0x00E8_E8E8);
    }
    // Minimal uncompressed-ish PNG via a plain PPM->PNG-free path: write a PPM, which
    // any viewer (and I) can read, rather than pulling in an encoder crate for a test.
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in &buf {
      out.push(((px >> 16) & 0xFF) as u8);
      out.push(((px >> 8) & 0xFF) as u8);
      out.push((px & 0xFF) as u8);
    }
    std::fs::write(&path, out).expect("write the sample");
  }

  #[test]
  fn blending_is_a_linear_mix_between_the_two_colours() {
    assert_eq!(blend(0x000000, 0xFFFFFF, 0), 0x000000, "no coverage = untouched");
    assert_eq!(blend(0x000000, 0xFFFFFF, 255), 0xFFFFFF, "full coverage = the colour");
    let half = blend(0x000000, 0xFFFFFF, 128);
    let r = (half >> 16) & 0xFF;
    assert!((0x7E..=0x82).contains(&r), "half coverage should sit mid-grey, got {r:#x}");
  }

  /// The letters must sit on a common baseline; otherwise each floats to its own
  /// height and the line looks ransom-note.
  #[test]
  fn letters_share_a_baseline() {
    fn lowest_inked_row(text: &str) -> usize {
      let (buf, w, h) = render(text);
      (0..h).rev().find(|y| (0..w).any(|x| buf[y * w + x] != BG)).expect("ink")
    }
    // 'n' and 'o' have no descender and differ in shape; their feet should agree
    // within a pixel or two of overshoot (round letters overshoot by design).
    let n = lowest_inked_row("n") as i32;
    let o = lowest_inked_row("o") as i32;
    assert!((n - o).abs() <= 2, "baselines disagree: n={n}, o={o}");
  }
}
