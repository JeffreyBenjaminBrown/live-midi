//! A framebuffer to draw the editor into: filled and translucent rectangles,
//! lines, and small anti-aliased text in the instrument's one embedded font
//! (`bitmap_font`). Colors are 0xRRGGBB.

use std::collections::HashMap;

use fontdue::Metrics;

use crate::bitmap_font::{blend, font};

/// The editor's text size, in pixels.
pub const TEXT_PX: f32 = 13.0;
/// The height of a line of text, and the width of one character (the font
/// is monospaced).
pub const LINE_H: i32 = 16;

pub struct Canvas {
  pub width: i32,
  pub height: i32,
  pub pixels: Vec<u32>,
  /// Rasterized glyphs, by character and size in tenths of a pixel.
  glyphs: HashMap<(char, u32), (Metrics, Vec<u8>)>,
  char_w: i32,
}

impl Canvas {
  pub fn new(
    width: i32,
    height: i32,
  ) -> Canvas {
    let char_w: i32 = font().metrics('M', TEXT_PX).advance_width.round() as i32;
    Canvas {
      width, height, pixels: vec![0; (width * height).max(0) as usize],
      glyphs: HashMap::new(), char_w, } }

  pub fn resize(
    &mut self,
    width: i32,
    height: i32,
  ) {
    if (width, height) == (self.width, self.height) { return; }
    self.width = width;
    self.height = height;
    self.pixels = vec![0; (width * height).max(0) as usize]; }

  pub fn char_width(&self) -> i32 { self.char_w }

  pub fn text_width(
    &self,
    text: &str,
  ) -> i32 {
    self.char_w * text.chars().count() as i32 }

  pub fn fill(
    &mut self,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
  ) {
    self.shade(x, y, w, h, color, 255); }

  /// Lay `color` over the rectangle at `alpha` (0..=255).
  pub fn shade(
    &mut self,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
    alpha: u8,
  ) {
    let (x0, y0) = (x.max(0), y.max(0));
    let (x1, y1) = ((x + w).min(self.width), (y + h).min(self.height));
    for py in y0..y1 {
      for px in x0..x1 {
        let index: usize = (py * self.width + px) as usize;
        self.pixels[index] = blend(self.pixels[index], color, alpha); } } }

  pub fn vline(
    &mut self,
    x: i32,
    y: i32,
    h: i32,
    color: u32,
  ) {
    self.fill(x, y, 1, h, color); }

  pub fn hline(
    &mut self,
    x: i32,
    y: i32,
    w: i32,
    color: u32,
  ) {
    self.fill(x, y, w, 1, color); }

  /// The width of one character at `px` pixels.
  pub fn char_width_at(
    &self,
    px: f32,
  ) -> i32 {
    font().metrics('M', px).advance_width.round() as i32 }

  /// Draw `text` with its line's top-left at (x, y), clipped to `max_w`
  /// pixels of width.
  pub fn text(
    &mut self,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    max_w: i32,
  ) {
    self.text_at_size(x, y, text, color, max_w, TEXT_PX); }

  /// `text`, at `px` pixels instead of the editor's usual size. The line
  /// is `px * 1.25` tall.
  pub fn text_at_size(
    &mut self,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    max_w: i32,
    px: f32,
  ) {
    let line_h: i32 = (px * 1.25).round() as i32;
    let baseline: i32 = y + line_h - (px * 0.3).round() as i32;
    let char_w: i32 = if px == TEXT_PX { self.char_w } else { self.char_width_at(px) };
    let size_key: u32 = (px * 10.0).round() as u32;
    let mut pen: i32 = x;
    for ch in text.chars() {
      if pen + char_w > x + max_w { break; }
      let (metrics, mask) = self.glyphs.entry((ch, size_key))
        .or_insert_with(|| font().rasterize(ch, px)).clone();
      let gx: i32 = pen + metrics.xmin;
      let gy: i32 = baseline - metrics.height as i32 - metrics.ymin;
      for row in 0..metrics.height as i32 {
        for col in 0..metrics.width as i32 {
          let coverage: u8 = mask[(row * metrics.width as i32 + col) as usize];
          let (px, py) = (gx + col, gy + row);
          if coverage == 0 || px < 0 || py < 0 || px >= self.width || py >= self.height {
            continue; }
          let index: usize = (py * self.width + px) as usize;
          self.pixels[index] = blend(self.pixels[index], color, coverage); } }
      pen += char_w; } }

  /// The picture as a PNG file's bytes (uncompressed), for looking at a
  /// frame without a display.
  pub fn png(&self) -> Vec<u8> {
    let mut raw: Vec<u8> = Vec::with_capacity(((self.width * 3 + 1) * self.height) as usize);
    for row in self.pixels.chunks(self.width as usize) {
      raw.push(0);
      for pixel in row {
        raw.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]); } }
    let mut ihdr: Vec<u8> = vec![];
    ihdr.extend_from_slice(&(self.width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(self.height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut out, b"IEND", &[]);
    out }
}

fn png_chunk(
  out: &mut Vec<u8>,
  kind: &[u8; 4],
  data: &[u8],
) {
  out.extend_from_slice(&(data.len() as u32).to_be_bytes());
  let start: usize = out.len();
  out.extend_from_slice(kind);
  out.extend_from_slice(data);
  let crc: u32 = crc32(&out[start..]);
  out.extend_from_slice(&crc.to_be_bytes()); }

/// A zlib stream of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
  let mut out: Vec<u8> = vec![0x78, 0x01];
  let blocks: Vec<&[u8]> = data.chunks(65535).collect();
  for (index, block) in blocks.iter().enumerate() {
    out.push(if index + 1 == blocks.len() { 1 } else { 0 });
    let len: u16 = block.len() as u16;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(block); }
  if blocks.is_empty() { out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]); }
  let (mut a, mut b): (u32, u32) = (1, 0);
  for byte in data {
    a = (a + *byte as u32) % 65521;
    b = (b + a) % 65521; }
  out.extend_from_slice(&((b << 16) | a).to_be_bytes());
  out }

fn crc32(data: &[u8]) -> u32 {
  let mut crc: u32 = 0xFFFF_FFFF;
  for byte in data {
    crc ^= *byte as u32;
    for _ in 0..8 {
      crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 }; } }
  !crc }
