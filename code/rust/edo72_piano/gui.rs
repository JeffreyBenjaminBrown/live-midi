use minifb::{Window, WindowOptions, Key};
use std::sync::MutexGuard;
use std::collections::HashMap;
use std::sync::mpsc;

use crate::{GRID_ROWS, GRID_COLS, GRID_ANCHOR, GRID_ROW_STEP,
            WHITE_KEYS, pitch_class_shifts};

pub const CELL_W: usize = 90;
pub const CELL_H: usize = 75;
pub const BORDER_W: usize = 7;
pub const WIN_W: usize = GRID_COLS * CELL_W;
pub const WIN_H: usize = GRID_ROWS * CELL_H;

// 5x7 bitmap font for digits 0-9 and minus sign.
// Each glyph is [u8; 7], lower 5 bits per row.
const GLYPHS: [[u8; 7]; 11] = [
  [ 0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110 ], // 0
  [ 0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110 ], // 1
  [ 0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111 ], // 2
  [ 0b01110, 0b10001, 0b00001, 0b00110, 0b00001, 0b10001, 0b01110 ], // 3
  [ 0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010 ], // 4
  [ 0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110 ], // 5
  [ 0b01110, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b01110 ], // 6
  [ 0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000 ], // 7
  [ 0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110 ], // 8
  [ 0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110 ], // 9
  [ 0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000 ], // minus (index 10)
];

const SCALE: usize = 4; // each glyph pixel = 4x4 screen pixels
const GLYPH_W: usize = 5 * SCALE; // 20
const GLYPH_H: usize = 7 * SCALE; // 28

fn draw_glyph(buf: &mut [u32], buf_w: usize,
              glyph_idx: usize, x0: usize, y0: usize, color: u32) {
  let glyph: &[u8; 7] = &GLYPHS[glyph_idx];
  for row in 0..7 {
    for col in 0..5 {
      if glyph[row] & (1 << (4 - col)) != 0 {
        for sy in 0..SCALE {
          for sx in 0..SCALE {
            let px: usize = x0 + col * SCALE + sx;
            let py: usize = y0 + row * SCALE + sy;
            if px < buf_w && py < WIN_H {
              buf[py * buf_w + px] = color; }} }} }} }

fn draw_number(buf: &mut [u32], buf_w: usize,
               cell_x: usize, cell_y: usize, value: i8, color: u32) {
  let s: String = format!("{}", value);
  let glyphs: Vec<usize> = s.chars().map(|c: char| match c {
    '-' => 10,
    d   => (d as usize) - ('0' as usize),
  }).collect();
  let total_w: usize = glyphs.len() * GLYPH_W
                        + (glyphs.len().saturating_sub(1)); // 1px gap
  let x0: usize = cell_x + (CELL_W.saturating_sub(total_w)) / 2;
  let y0: usize = cell_y + (CELL_H.saturating_sub(GLYPH_H)) / 2;
  for (i, &gi) in glyphs.iter().enumerate() {
    draw_glyph(buf, buf_w, gi, x0 + i * (GLYPH_W + 1), y0, color); }}

fn render_grid(buf: &mut [u32],
               held_count: &[u8; 12]) {
  let shifts: MutexGuard<'_, HashMap<u8, i8>> =
    pitch_class_shifts().lock().unwrap();
  for row in 0..GRID_ROWS {
    for col in 0..GRID_COLS {
      // Row 0 in the grid is the top of the window,
      // but in the music grid row 0 is the bottom.
      let music_row: usize = GRID_ROWS - 1 - row;
      let pc: u8 =
        ((GRID_ANCHOR + GRID_ROW_STEP * music_row + col) % 12) as u8;
      let is_white: bool = WHITE_KEYS[pc as usize];
      let bg: u32 = if is_white { 0xFFFFFF } else { 0x000000 };
      let fg: u32 = if is_white { 0x000000 } else { 0xFFFFFF };
      let held: bool = held_count[pc as usize] > 0;
      let cell_x: usize = col * CELL_W;
      let cell_y: usize = row * CELL_H;
      // Fill background
      for y in cell_y..cell_y + CELL_H {
        for x in cell_x..cell_x + CELL_W {
          buf[y * WIN_W + x] = bg; }}
      // Red border while any note of this pitch class is held
      if held {
        let red: u32 = 0xFF0000;
        for y in cell_y..cell_y + CELL_H {
          for x in cell_x..cell_x + CELL_W {
            let in_top: bool    = y < cell_y + BORDER_W;
            let in_bottom: bool = y >= cell_y + CELL_H - BORDER_W;
            let in_left: bool   = x < cell_x + BORDER_W;
            let in_right: bool  = x >= cell_x + CELL_W - BORDER_W;
            if in_top || in_bottom || in_left || in_right {
              buf[y * WIN_W + x] = red; }} }}
      // Draw shift number
      let shift: i8 = shifts.get(&pc).copied().unwrap_or(0);
      draw_number(buf, WIN_W, cell_x, cell_y, shift, fg); }} }

pub fn run_display_thread(rx: mpsc::Receiver<(u8, bool)>) {
  // minifb uses X11; on Wayland, ensure DISPLAY is set for XWayland.
  if std::env::var("DISPLAY").is_err() {
    std::env::set_var("DISPLAY", ":0"); }
  let mut window: Window = Window::new(
      "edo72_piano", WIN_W, WIN_H,
      WindowOptions::default(),
    ).expect("Failed to create window");
  window.set_target_fps(20);
  let mut buf: Vec<u32> = vec![0; WIN_W * WIN_H];
  let mut held_count: [u8; 12] = [0; 12];
  loop {
    if !window.is_open() || window.is_key_down(Key::Escape) {
      std::process::exit(0); }
    // Drain pending note on/off events
    while let Ok((pc, is_on)) = rx.try_recv() {
      if is_on {
        held_count[pc as usize] =
          held_count[pc as usize].saturating_add(1);
      } else {
        held_count[pc as usize] =
          held_count[pc as usize].saturating_sub(1); }}
    render_grid(&mut buf, &held_count);
    window.update_with_buffer(&buf, WIN_W, WIN_H)
      .expect("Failed to update window"); }}
