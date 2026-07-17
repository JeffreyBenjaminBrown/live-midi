//! A hand-rolled 5x7 bitmap font: digits + minus (the original set, drawn by
//! `edo12n_gui`'s per-cell shift numbers) plus the letters/symbols the surfaces
//! pulse window needs (`surfaces_runtime::pulse_window`, `TODO/many/3_plan.org`
//! phase 9). Shared so the pixel-fiddling -- plotting one glyph's bits into a
//! `minifb` framebuffer -- lives in exactly one place instead of being copied
//! per window. Unsupported characters draw nothing rather than panic: callers are
//! expected to only pass in-vocabulary text, and a missing glyph should read as a
//! gap, not crash a running instrument.

/// Each glyph pixel is drawn as a `SCALE x SCALE` screen-pixel block -- the size
/// `edo12n_gui` used, kept as the one shared default so every window using this
/// font looks the same weight.
pub const SCALE: usize = 4;
pub const GLYPH_W: usize = 5 * SCALE;
pub const GLYPH_H: usize = 7 * SCALE;

/// One glyph's bit pattern: 7 rows, the low 5 bits of each row are its columns
/// left-to-right (bit 4 = column 0 ... bit 0 = column 4). `None` for characters
/// outside the small vocabulary this instrument actually prints.
fn glyph_bits(ch: char) -> Option<[u8; 7]> {
  Some(match ch {
    // Digits + minus: byte-identical to the original `edo12n_gui` table, so
    // moving it here changes no pixel of the piano's per-cell shift numbers.
    '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    '2' => [0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111],
    '3' => [0b01110, 0b10001, 0b00001, 0b00110, 0b00001, 0b10001, 0b01110],
    '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    '6' => [0b01110, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b01110],
    '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110],
    '-' => [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],

    // Added for the pulse window's readout lines (BPM / factor / on-off / names).
    ' ' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
    '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100],
    '^' => [0b00100, 0b01010, 0b10001, 0b00000, 0b00000, 0b00000, 0b00000],
    '*' => [0b00000, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0b00000],

    'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
    'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
    'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
    'N' => [0b10001, 0b11001, 0b10101, 0b10101, 0b10011, 0b10001, 0b10001],
    'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
    'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
    'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],

    'e' => [0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b10001, 0b01110],
    'f' => [0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000],
    'l' => [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    'm' => [0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10101, 0b10101],
    'n' => [0b00000, 0b00000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001],
    'o' => [0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110],
    'p' => [0b00000, 0b00000, 0b11110, 0b10001, 0b11110, 0b10000, 0b10000],
    's' => [0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
    't' => [0b00100, 0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00011],
    'u' => [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101],

    _ => return None,
  })
}

/// Plot one glyph at `(x0, y0)` into `buf` (`buf_w` x `buf_h`), clipping silently
/// at the buffer edge (as the original `edo12n_gui::draw_glyph` did) rather than
/// panicking on an off-screen line.
pub fn draw_glyph(buf: &mut [u32], buf_w: usize, buf_h: usize, ch: char, x0: usize, y0: usize, color: u32) {
  let Some(glyph) = glyph_bits(ch) else { return };
  for row in 0..7 {
    for col in 0..5 {
      if glyph[row] & (1 << (4 - col)) != 0 {
        for sy in 0..SCALE {
          for sx in 0..SCALE {
            let px: usize = x0 + col * SCALE + sx;
            let py: usize = y0 + row * SCALE + sy;
            if px < buf_w && py < buf_h {
              buf[py * buf_w + px] = color;
            }
          }
        }
      }
    }
  }
}

/// Plot `text` left-to-right starting at `(x0, y0)`, one glyph after another with
/// the same 1px gap `edo12n_gui` used between digits.
pub fn draw_text(buf: &mut [u32], buf_w: usize, buf_h: usize, text: &str, x0: usize, y0: usize, color: u32) {
  let mut x = x0;
  for ch in text.chars() {
    draw_glyph(buf, buf_w, buf_h, ch, x, y0, color);
    x += GLYPH_W + 1;
  }
}

/// The pixel width `draw_text` will occupy for `text` -- for centering it in a
/// cell (as `edo12n_gui`'s shift numbers do) or sizing a window to fit it.
pub fn text_width(text: &str) -> usize {
  let n = text.chars().count();
  if n == 0 {
    0
  } else {
    n * (GLYPH_W + 1) - 1
  }
}
