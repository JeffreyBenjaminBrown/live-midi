use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{evenly_spaced_map, load_config, EdoConfig, RemapIdiom};
use crate::layout::{edo_local_cell, grid_step, map_rect, undo_cell, window_for_cell, GridRect, WindowId};
use crate::midi_runtime::{edo31_instruction, update_sounding};
use crate::monome_runtime::{apply_monome_key, apply_monome_press, PreimageRowState};
use crate::remap::{apply_grid_press, move_delta, undo_remap};
use crate::render::{
  next_render_wait, render_led_cols, render_led_levels, render_led_levels_with_preimage_row, Color,
  ColorClock, LedPhases, ANCHOR_COLOR, IMAGE_COLOR, SOUNDING_COLOR, PREIMAGE_ROW_FLASH_COLOR,
};
use crate::state::{Edo31State, LooseState, SoundingState};
use crate::{
  DEFAULT_EDO, DEFAULT_GRID_H, DEFAULT_GRID_W, DEFAULT_LOWEST_HZ, DEFAULT_X_STEP, DEFAULT_Y_STEP,
  LED_LEVEL_FULL, LED_LEVEL_IMAGE, LED_LEVEL_OFF, LED_LEVEL_UNDO, MIN_CHANNEL_OUT, MIN_NOTE_OUT,
  MONOME_REFRESH, PREIMAGE_ROW_FLASH_MIN,
};

fn test_config() -> EdoConfig {
  EdoConfig::new(
    DEFAULT_LOWEST_HZ,
    DEFAULT_EDO,
    DEFAULT_X_STEP,
    DEFAULT_Y_STEP,
    RemapIdiom::Loose,
    DEFAULT_GRID_W,
    DEFAULT_GRID_H,
  )
}

fn test_state() -> Edo31State {
  Edo31State::new(test_config())
}

fn snap_state() -> Edo31State {
  Edo31State::new(EdoConfig::default())
}

fn test_state_arc() -> Arc<Mutex<Edo31State>> {
  Arc::new(Mutex::new(test_state()))
}

fn no_sounding() -> Vec<u16> {
  vec![0; test_config().edo as usize]
}

fn render_with_preimage_row(state: &Edo31State, preimage_row: &PreimageRowState, now: Instant) -> Vec<u8> {
  render_led_levels_with_preimage_row(
    state,
    &vec![0; state.config.edo as usize],
    &preimage_row.counts,
    &preimage_row.flash_until,
    now,
    phases(true, true, true),
  )
}

fn phases(sounding_on: bool, anchor_on: bool, image_on: bool) -> LedPhases {
  LedPhases {
    sounding_on,
    anchor_on,
    image_on,
    preimage_row_flash_on: true,
  }
}

fn encoded_output_step(channel: i16, note: i16, edo: i16) -> i16 {
  (channel - MIN_CHANNEL_OUT as i16) * edo + (note - MIN_NOTE_OUT as i16)
}

#[test]
fn grid_geometry_matches_requested_axes() {
  let config = test_config();
  assert_eq!(grid_step(&config, 0, 0), 0);
  assert_eq!(grid_step(&config, 1, 0), 6);
  assert_eq!(grid_step(&config, 0, 1), 1);
}

#[test]
fn map_rect_reserves_preimage_row_and_uses_remaining_rows() {
  let config = EdoConfig::new(80.0, 58, 8, 1, RemapIdiom::Snap, 16, 16);

  assert_eq!(
    map_rect(&config),
    GridRect { x0: 0, y0: 1, x1: 10, y1: 16 },
  );
  assert_eq!(edo_local_cell(&config, 0, 0), None);
  assert_eq!(edo_local_cell(&config, 0, 1), Some((0, 0)));
  assert_eq!(edo_local_cell(&config, 9, 15), Some((9, 14)));
  assert_eq!(edo_local_cell(&config, 10, 15), None);
  assert_eq!(window_for_cell(&config, 15, 15), Some(WindowId::Undo));
}

#[test]
fn initial_map_matches_even_31_edo_spacing() {
  assert_eq!(
    test_state().map,
    [0, 3, 5, 8, 10, 13, 16, 18, 21, 23, 26, 28],
  );
}

#[test]
fn initial_map_generalizes_to_58_edo() {
  assert_eq!(
    evenly_spaced_map(58),
    [0, 5, 10, 15, 19, 24, 29, 34, 39, 44, 48, 53],
  );
}

#[test]
fn default_config_load_58_8_5_snap() {
  let config = load_config("default").unwrap();

  assert_eq!(config.lowest_hz, 80.0);
  assert_eq!(config.edo, 58);
  assert_eq!(config.x_step, 8);
  assert_eq!(config.y_step, 5);
  assert_eq!(config.remap_idiom, RemapIdiom::Snap);
}

#[test]
fn loose_config_accept_loose_alias() {
  let config = load_config("31-loose.toml").unwrap();

  assert_eq!(config.edo, 31);
  assert_eq!(config.x_step, 6);
  assert_eq!(config.y_step, 1);
  assert_eq!(config.remap_idiom, RemapIdiom::Loose);
}

#[test]
fn lit_press_makes_preimage_loose_without_moving() {
  let mut state = test_state();
  let initial_map = state.config.initial_map;
  let y0 = map_rect(&state.config).y0;

  assert!(apply_grid_press(&mut state, 0, y0));

  assert_eq!(state.loose[0], LooseState::Loose);
  assert_eq!(state.map, initial_map);
}

#[test]
fn dark_press_has_no_effect_when_no_preimage_is_loose() {
  let mut state = test_state();
  let y = map_rect(&state.config).y0 + 1;

  assert!(!apply_grid_press(&mut state, 0, y));

  assert_eq!(state.map[0], 0);
}

#[test]
fn dark_press_moves_loose_neighbor_and_fixes_it() {
  let mut state = test_state();
  state.loose[0] = LooseState::Loose;
  let y = map_rect(&state.config).y0 + 1;

  assert!(apply_grid_press(&mut state, 0, y));

  assert_eq!(state.map[0], 1);
  assert_eq!(state.deltas[0], 1);
  assert_eq!(state.loose[0], LooseState::Fixed);
}

#[test]
fn dark_press_moves_farther_neighbor_if_only_farther_neighbor_is_loose() {
  let mut state = test_state();
  state.loose[1] = LooseState::Loose;
  let y = map_rect(&state.config).y0 + 1;

  assert!(apply_grid_press(&mut state, 0, y));

  assert_eq!(state.map[0], 0);
  assert_eq!(state.map[1], 1);
  assert_eq!(state.deltas[1], -2);
  assert_eq!(state.loose[1], LooseState::Fixed);
}

#[test]
fn dark_press_chooses_higher_neighbor_on_tie() {
  let mut state = test_state();
  state.loose[0] = LooseState::Loose;
  state.loose[1] = LooseState::Loose;
  let y = map_rect(&state.config).y0 + 2;

  assert!(apply_grid_press(&mut state, 0, y));

  assert_eq!(state.map[0], 0);
  assert_eq!(state.map[1], 2);
  assert_eq!(state.deltas[1], -1);
  assert_eq!(state.loose[0], LooseState::Loose);
  assert_eq!(state.loose[1], LooseState::Fixed);
}

#[test]
fn snap_dark_press_moves_nearest_image_without_loose_state() {
  let mut state = snap_state();
  let y = map_rect(&state.config).y0 + 1;

  assert!(apply_grid_press(&mut state, 0, y));

  assert_eq!(state.map[0], 1);
  assert_eq!(state.deltas[0], 1);
  assert_eq!(state.loose[0], LooseState::Fixed);
}

#[test]
fn snap_tie_moves_higher_image_down() {
  let mut state = snap_state();
  state.map[1] = 4;
  let y = map_rect(&state.config).y0 + 2;

  assert!(apply_grid_press(&mut state, 0, y));

  assert_eq!(state.map[0], 0);
  assert_eq!(state.map[1], 2);
  assert_eq!(state.deltas[1], -2);
}

#[test]
fn successful_remap_can_be_undone() {
  let mut state = snap_state();

  assert!(apply_grid_press(&mut state, 0, 2));
  assert_eq!(state.map[0], 1);
  assert_eq!(state.history.len(), 1);

  assert!(undo_remap(&mut state));

  assert_eq!(state.map[0], 0);
  assert_eq!(state.deltas[0], 0);
  assert!(state.history.is_empty());
}

#[test]
fn undo_button_uses_bottom_right_cell() {
  let mut state = snap_state();
  state.config = state.config.with_grid_size(16, 16);
  apply_grid_press(&mut state, 0, 2);

  assert_eq!(undo_cell(&state.config), Some((15, 15)));
  assert!(apply_monome_press(&mut state, 15, 15));
  assert_eq!(state.map[0], 0);
}

#[test]
fn undo_window_occludes_edo_on_small_grids() {
  let config = EdoConfig::new(80.0, 31, 6, 1, RemapIdiom::Snap, 8, 8);
  let mut state = Edo31State::new(config);
  state.history.push(state.snapshot());

  assert_eq!(window_for_cell(&state.config, 7, 7), Some(WindowId::Undo));
  assert_eq!(edo_local_cell(&state.config, 7, 7), None);

  let levels = render_led_levels(
    &state,
    &vec![0; state.config.edo as usize],
    phases(true, true, true),
  );
  assert_eq!(levels[(7 * state.config.grid_w + 7) as usize], LED_LEVEL_UNDO);
}

#[test]
fn edo31_note_uses_current_pitch_class_mapping() {
  let state = test_state_arc();
  {
    let mut state = state.lock().unwrap();
    state.map[0] = 1;
    state.deltas[0] = 1;
  }

  let (_channel, note) = edo31_instruction(60, &state);

  assert_eq!(note, MIN_NOTE_OUT as i16 + 1);
}

#[test]
fn move_delta_uses_shorter_arc_before_checking_blockers() {
  let initial_map = test_config().initial_map;
  assert_eq!(move_delta(0, 30, &initial_map, 31), Some(-1));
  assert_eq!(move_delta(0, 4, &initial_map, 31), None);
}

#[test]
fn lowering_c_across_display_boundary_lowers_one_31_edo_step() {
  let state = test_state_arc();
  let edo = test_config().edo;
  let (default_channel, default_note) = edo31_instruction(60, &state);
  {
    let mut state = state.lock().unwrap();
    state.map[0] = 30;
    state.deltas[0] = -1;
  }

  let (channel, note) = edo31_instruction(60, &state);

  assert_eq!(
    encoded_output_step(channel, note, edo),
    encoded_output_step(default_channel, default_note, edo) - 1,
  );
}

#[test]
fn default_middle_octave_mapping_is_monotone_across_c_boundary() {
  let state = test_state_arc();
  let encoded: Vec<i16> = (60..72)
    .map(|note| {
      let (channel, midi_note) = edo31_instruction(note, &state);
      encoded_output_step(channel, midi_note, test_config().edo)
    })
    .collect();

  assert_eq!(encoded, vec![93, 96, 98, 101, 103, 106, 109, 111, 114, 116, 119, 121]);
}

#[test]
fn output_residue_matches_displayed_pitch_class_mapping() {
  let config = EdoConfig::new(80.0, 58, 8, 1, RemapIdiom::Snap, 16, 16);
  let state = Arc::new(Mutex::new(Edo31State::new(config.clone())));

  for note in 48..60 {
    let pitch_class = (note % 12) as usize;
    let (channel, midi_note) = edo31_instruction(note, &state);
    let residue = encoded_output_step(channel, midi_note, config.edo).rem_euclid(config.edo);

    assert_eq!(residue, config.initial_map[pitch_class]);
  }
}

#[test]
fn eb_to_bb_output_interval_matches_58_edo_display_interval() {
  let config = EdoConfig::new(80.0, 58, 8, 1, RemapIdiom::Snap, 16, 16);
  let state = Arc::new(Mutex::new(Edo31State::new(config.clone())));
  let (eb_channel, eb_note) = edo31_instruction(51, &state);
  let (bb_channel, bb_note) = edo31_instruction(58, &state);
  let eb = encoded_output_step(eb_channel, eb_note, config.edo);
  let bb = encoded_output_step(bb_channel, bb_note, config.edo);
  let output_interval = (bb - eb).rem_euclid(config.edo);
  let display_interval = (config.initial_map[10] - config.initial_map[3]).rem_euclid(config.edo);

  assert_eq!(output_interval, display_interval);
}

#[test]
fn render_hides_anchor_preimages_during_anchor_off_phase() {
  let state = test_state();
  let sounding = no_sounding();

  let on_cols = render_led_cols(&state, &sounding, phases(true, true, true));
  let off_cols = render_led_cols(&state, &sounding, phases(true, false, true));

  assert_ne!(on_cols[0] & (1 << 1), 0);
  assert_eq!(off_cols[0] & (1 << 1), 0);
  assert_ne!(on_cols[2] & (1 << 2), 0);
  assert_eq!(off_cols[2] & (1 << 2), 0);
  assert_ne!(on_cols[3] & (1 << 1), 0);
  assert_eq!(off_cols[3] & (1 << 1), 0);
}

#[test]
fn render_hides_non_anchor_image_during_image_off_phase() {
  let state = test_state();
  let sounding = no_sounding();

  let off_cols = render_led_cols(&state, &sounding, phases(true, true, false));

  assert_eq!(off_cols[1] & (1 << 3), 0);
}

#[test]
fn render_keeps_non_anchor_image_during_image_on_phase() {
  let state = test_state();
  let sounding = no_sounding();

  let on_cols = render_led_cols(&state, &sounding, phases(true, true, true));

  assert_ne!(on_cols[1] & (1 << 3), 0);
}

#[test]
fn preimage_row_shows_black_keys_dim_and_white_keys_off_at_rest() {
  let state = test_state();
  let preimage_row = PreimageRowState::new();
  let levels = render_with_preimage_row(&state, &preimage_row, Instant::now());

  assert_eq!(levels[0], LED_LEVEL_OFF);
  assert_eq!(levels[1], LED_LEVEL_IMAGE);
  assert_eq!(levels[2], LED_LEVEL_OFF);
  assert_eq!(levels[3], LED_LEVEL_IMAGE);
}

#[test]
fn grid_key_down_flashes_preimage_for_at_least_300ms_after_short_press() {
  let mut state = test_state();
  let mut preimage_row = PreimageRowState::new();
  let start = Instant::now();

  assert!(apply_monome_key(&mut state, &mut preimage_row, 0, 1, 1, start));
  assert!(apply_monome_key(
    &mut state,
    &mut preimage_row,
    0,
    1,
    0,
    start + Duration::from_millis(20),
  ));

  let still_flashing = render_with_preimage_row(
    &state,
    &preimage_row,
    start + PREIMAGE_ROW_FLASH_MIN - Duration::from_millis(1),
  );
  let after_flash = render_with_preimage_row(&state, &preimage_row, start + PREIMAGE_ROW_FLASH_MIN);

  assert_eq!(still_flashing[0], LED_LEVEL_FULL);
  assert_eq!(after_flash[0], LED_LEVEL_OFF);
}

#[test]
fn grid_key_mellows_to_on_until_key_up() {
  let mut state = test_state();
  let mut preimage_row = PreimageRowState::new();
  let start = Instant::now();

  assert!(apply_monome_key(&mut state, &mut preimage_row, 0, 1, 1, start));

  let held = render_with_preimage_row(
    &state,
    &preimage_row,
    start + PREIMAGE_ROW_FLASH_MIN + Duration::from_millis(1),
  );
  assert_eq!(held[0], LED_LEVEL_FULL);

  assert!(apply_monome_key(
    &mut state,
    &mut preimage_row,
    0,
    1,
    0,
    start + PREIMAGE_ROW_FLASH_MIN + Duration::from_millis(2),
  ));
  let released = render_with_preimage_row(
    &state,
    &preimage_row,
    start + PREIMAGE_ROW_FLASH_MIN + Duration::from_millis(3),
  );

  assert_eq!(released[0], LED_LEVEL_OFF);
}

#[test]
fn black_grid_press_flashes_preimage_assigned_to_it() {
  let mut state = snap_state();
  let mut preimage_row = PreimageRowState::new();
  let start = Instant::now();

  assert!(apply_monome_key(&mut state, &mut preimage_row, 0, 2, 1, start));

  let levels = render_with_preimage_row(&state, &preimage_row, start);
  assert_eq!(state.map[0], 1);
  assert_eq!(levels[0], LED_LEVEL_FULL);
}

#[test]
fn render_uses_two_led_col_banks_for_16_high_grid() {
  let config = EdoConfig::new(80.0, 58, 8, 1, RemapIdiom::Snap, 16, 16);
  let state = Edo31State::new(config.clone());
  let sounding = vec![0; config.edo as usize];

  let cols = render_led_cols(&state, &sounding, phases(true, true, true));

  assert_eq!(cols.len(), 32);
  assert_ne!(cols[0], 0);
  assert_ne!(cols[1], 0);
}

#[test]
fn duty_clock_schedules_off_from_actual_on_time() {
  let color = Color::Duty {
    period: Duration::from_micros(100_000),
    fraction_on: 0.02,
  };
  let start = Instant::now();
  let mut clock = ColorClock::new(color, start);
  let (on_duration, off_duration) = color.duty_durations().unwrap();

  assert!(clock.is_on());
  assert_eq!(clock.wait(start), Some(on_duration));
  assert!(clock.advance_if_due(start + on_duration + Duration::from_micros(200)));
  assert!(!clock.is_on());
  assert_eq!(
    clock.wait(start + on_duration + Duration::from_micros(200)),
    Some(off_duration),
  );
}

#[test]
fn refresh_wait_uses_next_scheduled_color_transition() {
  let start = Instant::now();
  let sounding_clock = ColorClock::new(SOUNDING_COLOR, start);
  let anchor_clock = ColorClock::new(ANCHOR_COLOR, start);
  let image_clock = ColorClock::new(IMAGE_COLOR, start);
  let preimage_row_flash_clock = ColorClock::new(PREIMAGE_ROW_FLASH_COLOR, start);
  let (anchor_on, _) = ANCHOR_COLOR.duty_durations().unwrap();

  assert_eq!(
    next_render_wait(
      start,
      sounding_clock,
      anchor_clock,
      image_clock,
      preimage_row_flash_clock,
      &[None; 12],
    ),
    anchor_on.min(MONOME_REFRESH),
  );
  assert_eq!(
    next_render_wait(
      start + anchor_on,
      sounding_clock,
      anchor_clock,
      image_clock,
      preimage_row_flash_clock,
      &[None; 12],
    ),
    Duration::ZERO,
  );
}

#[test]
fn sounding_pitch_clobbers_anchor_and_image_off_phases() {
  let mut state = test_state();
  state.map[0] = 30;
  let mut sounding = no_sounding();
  sounding[30] = 1;
  sounding[3] = 1;

  let off_cols = render_led_cols(&state, &sounding, phases(true, false, false));

  assert_ne!(off_cols[5] & (1 << 1), 0);
  assert_ne!(off_cols[0] & (1 << 4), 0);
}

#[test]
fn sounding_state_survives_map_change_until_note_off() {
  let state = test_state_arc();
  let sounding = Arc::new(Mutex::new(SoundingState::new(test_config().edo)));

  update_sounding(&[0x90, 60, 100], &state, &sounding);
  state.lock().unwrap().map[0] = 30;

  assert_eq!(sounding.lock().unwrap().counts[0], 1);
  assert_eq!(sounding.lock().unwrap().counts[30], 0);
  update_sounding(&[0x80, 60, 64], &state, &sounding);
  assert_eq!(sounding.lock().unwrap().counts[0], 0);
}

#[test]
fn sounding_state_uses_displayed_pitch_class_not_output_residue() {
  let state = test_state_arc();
  let sounding = Arc::new(Mutex::new(SoundingState::new(test_config().edo)));

  update_sounding(&[0x90, 62, 100], &state, &sounding);

  assert_eq!(
    sounding.lock().unwrap().counts[test_config().initial_map[2] as usize],
    1,
  );
  assert_eq!(sounding.lock().unwrap().counts[0], 0);
}
