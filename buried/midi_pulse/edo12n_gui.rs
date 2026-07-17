use minifb::{Window, WindowOptions, Key};
use std::sync::MutexGuard;
use std::collections::HashMap;
use std::sync::mpsc;

use super::{GRID_ROWS, GRID_COLS, GRID_ANCHOR, GRID_ROW_STEP,
            WHITE_KEYS, pitch_class_shifts};
use crate::bitmap_font::{self, GLYPH_H};

pub const CELL_W: usize = 90;
pub const CELL_H: usize = 75;
pub const BORDER_W: usize = 7;
pub const WIN_W: usize = GRID_COLS * CELL_W;
pub const WIN_H: usize = GRID_ROWS * CELL_H;

// The 5x7 bitmap font (digits + minus) now lives in `bitmap_font`, shared with the
// surfaces pulse window (`TODO/many/3_plan.org` phase 9) so the glyph-plotting code
// is written once. `draw_number` below draws pixel-identical output to before.
fn draw_number(buf: &mut [u32], buf_w: usize,
               cell_x: usize, cell_y: usize, value: i8, color: u32) {
  let s: String = format!("{}", value);
  let total_w: usize = bitmap_font::text_width(&s);
  let x0: usize = cell_x + (CELL_W.saturating_sub(total_w)) / 2;
  let y0: usize = cell_y + (CELL_H.saturating_sub(GLYPH_H)) / 2;
  bitmap_font::draw_text(buf, buf_w, WIN_H, &s, x0, y0, color); }

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
      "edo12n_piano", WIN_W, WIN_H,
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
