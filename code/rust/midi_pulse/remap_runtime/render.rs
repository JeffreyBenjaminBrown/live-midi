use midi_pulse::{monome, monome_window};
use std::net::{SocketAddr, UdpSocket};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::config::RemapConfig;
use super::layout::{map_rect, monome_windows, record_control_cells, undo_cell, WindowId};
use super::record::{RecordControl, RecordRuntime};
use super::remap::preimage_for_step;
use super::state::{RemappableEdoState, SoundingPitchCounts};
use super::{
  ANCHOR_PITCH_CLASSES, LED_LEVEL_FULL, LED_LEVEL_IMAGE, LED_LEVEL_OFF,
  LED_LEVEL_UNDO, LED_TRACE_ENV, MONOME_REFRESH, PREIMAGE_ROW_FLASH_FRACTION_ON,
  PREIMAGE_ROW_FLASH_WAVELENGTH,
  PREIMAGE_ROW_Y, WHITE_KEYS,
};

pub(crate) const SOUNDING_COLOR: Color = Color::Duty {
  period: Duration::from_micros(300_000),
  fraction_on: 0.3,
};
pub(crate) const ANCHOR_COLOR: Color = Color::Duty {
  period: Duration::from_micros(300_000),
  fraction_on: 0.3,
};
pub(crate) const IMAGE_COLOR: Color = Color::AlwaysOn;
pub(crate) const PREIMAGE_ROW_FLASH_COLOR: Color = Color::Duty {
  period: PREIMAGE_ROW_FLASH_WAVELENGTH,
  fraction_on: PREIMAGE_ROW_FLASH_FRACTION_ON,
};

static LED_TRACE_STARTED: OnceLock<Instant> = OnceLock::new();

#[derive(Clone, Copy)]
pub(crate) enum Color {
  AlwaysOn,
  Duty {
    period: Duration,
    fraction_on: f64,
  },
}

impl Color {
  pub(crate) fn is_initially_on(self) -> bool {
    match self {
      Color::AlwaysOn => true,
      Color::Duty { .. } => self
        .duty_durations()
        .map(|(on, _)| !on.is_zero())
        .unwrap_or(false),
    }
  }

  pub(crate) fn phase_duration(self, on: bool) -> Option<Duration> {
    let (on_duration, off_duration) = self.duty_durations()?;
    Some(if on { on_duration } else { off_duration })
  }

  pub(crate) fn duty_durations(self) -> Option<(Duration, Duration)> {
    let (period_micros, on_micros) = self.duty_micros()?;
    Some((
      Duration::from_micros(on_micros as u64),
      Duration::from_micros((period_micros - on_micros) as u64),
    ))
  }

  fn duty_micros(self) -> Option<(u128, u128)> {
    match self {
      Color::AlwaysOn => None,
      Color::Duty {
        period,
        fraction_on,
      } => {
        let period_micros = period.as_micros();
        if period_micros == 0 {
          return None;
        }
        let fraction_on = fraction_on.clamp(0.0, 1.0);
        let on_micros = (period_micros as f64 * fraction_on) as u128;
        Some((period_micros, on_micros))
      }
    }
  }
}

#[derive(Clone, Copy)]
pub(crate) struct ColorClock {
  color: Color,
  on: bool,
  next_transition: Option<Instant>,
}

impl ColorClock {
  pub(crate) fn new(color: Color, now: Instant) -> Self {
    let Some((on_duration, off_duration)) = color.duty_durations() else {
      return ColorClock {
        color,
        on: true,
        next_transition: None,
      };
    };
    if on_duration.is_zero() {
      return ColorClock {
        color,
        on: false,
        next_transition: None,
      };
    }
    if off_duration.is_zero() {
      return ColorClock {
        color,
        on: true,
        next_transition: None,
      };
    }
    let on = color.is_initially_on();
    let next_transition = Some(now + on_duration);
    ColorClock {
      color,
      on,
      next_transition,
    }
  }

  pub(crate) fn is_on(self) -> bool {
    match self.color {
      Color::AlwaysOn => true,
      Color::Duty { .. } => self.on,
    }
  }

  pub(crate) fn advance_if_due(&mut self, now: Instant) -> bool {
    let Some(next_transition) = self.next_transition else {
      return false;
    };
    if now < next_transition {
      return false;
    }
    self.on = !self.on;
    self.next_transition = self
      .color
      .phase_duration(self.on)
      .filter(|duration| !duration.is_zero())
      .map(|duration| now + duration);
    true
  }

  pub(crate) fn wait(self, now: Instant) -> Option<Duration> {
    self.next_transition.map(|transition| {
      if transition > now {
        transition.duration_since(now)
      } else {
        Duration::ZERO
      }
    })
  }
}

#[derive(Clone, Copy)]
pub(crate) struct LedPhases {
  pub(crate) sounding_on: bool,
  pub(crate) anchor_on: bool,
  pub(crate) image_on: bool,
  pub(crate) preimage_row_flash_on: bool,
}

pub(crate) fn blank_rendered_cols(config: &RemapConfig) -> Vec<u8> {
  vec![0; (config.grid_w * config.grid_h) as usize]
}

#[cfg(test)]
pub(crate) fn col_bank_count(config: &RemapConfig) -> usize {
  ((config.grid_h + 7) / 8) as usize
}

pub(crate) fn led_phases(
  sounding_clock: ColorClock,
  anchor_clock: ColorClock,
  image_clock: ColorClock,
  preimage_row_flash_clock: ColorClock,
) -> LedPhases {
  LedPhases {
    sounding_on: sounding_clock.is_on(),
    anchor_on: anchor_clock.is_on(),
    image_on: image_clock.is_on(),
    preimage_row_flash_on: preimage_row_flash_clock.is_on(),
  }
}

pub(crate) fn next_render_wait(
  now: Instant,
  sounding_clock: ColorClock,
  anchor_clock: ColorClock,
  image_clock: ColorClock,
  preimage_row_flash_clock: ColorClock,
  preimage_row_flash_until: &[Option<Instant>; 12],
) -> Duration {
  let next_transition = [
    sounding_clock.wait(now),
    anchor_clock.wait(now),
    image_clock.wait(now),
    preimage_row_flash_clock.wait(now),
    preimage_row_flash_until
      .iter()
      .flatten()
      .filter(|deadline| **deadline > now)
      .map(|deadline| deadline.duration_since(now))
      .min(),
  ]
  .into_iter()
  .flatten()
  .min();
  next_transition
    .map(|transition| transition.min(MONOME_REFRESH))
    .unwrap_or(MONOME_REFRESH)
}

pub(crate) fn render_to_monome(
  sock: &UdpSocket,
  device: SocketAddr,
  state: &RemappableEdoState,
  sounding: &SoundingPitchCounts,
  recorder: &RecordRuntime,
  preimage_row_counts: &[u16; 12],
  preimage_row_flash_until: &[Option<Instant>; 12],
  now: Instant,
  prefix: &str,
  phases: LedPhases,
  rendered_cols: &mut Vec<u8>,
) {
  let levels = render_led_levels_with_preimage_row(
    state,
    sounding,
    recorder,
    preimage_row_counts,
    preimage_row_flash_until,
    now,
    phases,
  );
  let trace_leds = led_trace_enabled();
  if rendered_cols.len() != levels.len() {
    *rendered_cols = vec![0; levels.len()];
  }
  for (i, level) in levels.iter().enumerate() {
    if rendered_cols[i] != *level {
      let x = i as i32 % state.config.grid_w;
      let y = i as i32 / state.config.grid_w;
      if trace_leds {
        let trace_started = LED_TRACE_STARTED.get_or_init(Instant::now);
        eprintln!(
          "led {:.3}ms x={x:02} y={y:02} level={level:02} old={:02}",
          trace_started.elapsed().as_secs_f64() * 1000.0,
          rendered_cols[i],
        );
      }
      monome::send_led_level_set(sock, device, prefix, x, y, *level as i32);
      rendered_cols[i] = *level;
    }
  }
}

fn led_trace_enabled() -> bool {
  std::env::var_os(LED_TRACE_ENV).is_some()
}

#[cfg(test)]
pub(crate) fn render_led_cols(
  state: &RemappableEdoState,
  sounding: &SoundingPitchCounts,
  recorder: &RecordRuntime,
  phases: LedPhases,
) -> Vec<u8> {
  let banks = col_bank_count(&state.config);
  let mut cols = vec![0u8; state.config.grid_w as usize * banks];
  let levels = render_led_levels(state, sounding, recorder, phases);
  for y in 0..state.config.grid_h {
    for x in 0..state.config.grid_w {
      if levels[(y * state.config.grid_w + x) as usize] > 0 {
        let i = x as usize * banks + (y as usize / 8);
        cols[i] |= 1u8 << (y % 8);
      }
    }
  }
  cols
}

pub(crate) fn render_led_levels(
  state: &RemappableEdoState,
  sounding: &SoundingPitchCounts,
  recorder: &RecordRuntime,
  phases: LedPhases,
) -> Vec<u8> {
  render_led_levels_with_preimage_row(
    state,
    sounding,
    recorder,
    &[0; 12],
    &[None; 12],
    Instant::now(),
    phases,
  )
}

pub(crate) fn render_led_levels_with_preimage_row(
  state: &RemappableEdoState,
  sounding: &SoundingPitchCounts,
  recorder: &RecordRuntime,
  preimage_row_counts: &[u16; 12],
  preimage_row_flash_until: &[Option<Instant>; 12],
  now: Instant,
  phases: LedPhases,
) -> Vec<u8> {
  let mut levels = vec![LED_LEVEL_OFF; (state.config.grid_w * state.config.grid_h) as usize];
  render_preimage_row(
    state,
    preimage_row_counts,
    preimage_row_flash_until,
    now,
    phases,
    &mut levels,
  );
  render_record_controls(state, recorder, now, phases, &mut levels);
  let rect = map_rect(&state.config);
  let windows = monome_windows(&state.config);
  for y in rect.y0..rect.y1 {
    for x in rect.x0..rect.x1 {
      if !monome_window::visible(&windows, WindowId::Edo, (x, y)) {
        continue;
      }
      let step = super::layout::grid_step(&state.config, x - rect.x0, y - rect.y0);
      levels[(y * state.config.grid_w + x) as usize] =
        rendered_level(state, sounding, step, phases);
    }
  }
  if let Some((x, y)) = undo_cell(&state.config) {
    if monome_window::visible(&windows, WindowId::Undo, (x, y)) {
      levels[(y * state.config.grid_w + x) as usize] = if state.history.is_empty() {
        LED_LEVEL_OFF
      } else {
        LED_LEVEL_UNDO
      };
    }
  }
  levels
}

fn render_record_controls(
  state: &RemappableEdoState,
  recorder: &RecordRuntime,
  now: Instant,
  phases: LedPhases,
  levels: &mut [u8],
) {
  for ((x, y), control) in record_control_cells(&state.config) {
    let level = match control {
      RecordControl::Arm => {
        if recorder.armed { LED_LEVEL_FULL } else { LED_LEVEL_OFF }
      }
      RecordControl::Start => {
        if recorder.playback && phases.preimage_row_flash_on {
          LED_LEVEL_FULL
        } else {
          LED_LEVEL_OFF
        }
      }
      RecordControl::Stop => {
        if recorder.stop_flash_until.is_some_and(|deadline| now < deadline) {
          LED_LEVEL_FULL
        } else {
          LED_LEVEL_OFF
        }
      }
      RecordControl::EraseOns => {
        if recorder.erase_ons_held && phases.preimage_row_flash_on {
          LED_LEVEL_FULL
        } else {
          LED_LEVEL_OFF
        }
      }
      RecordControl::Rscm => {
        if recorder.rscm { LED_LEVEL_FULL } else { LED_LEVEL_OFF }
      }
      RecordControl::Loop | RecordControl::EndAll => LED_LEVEL_OFF,
    };
    levels[(y * state.config.grid_w + x) as usize] = level;
  }
}

fn render_preimage_row(
  state: &RemappableEdoState,
  preimage_row_counts: &[u16; 12],
  preimage_row_flash_until: &[Option<Instant>; 12],
  now: Instant,
  phases: LedPhases,
  levels: &mut [u8],
) {
  if PREIMAGE_ROW_Y < 0 || PREIMAGE_ROW_Y >= state.config.grid_h {
    return;
  }
  for pitch_class in 0..12.min(state.config.grid_w as usize) {
    let flashing = preimage_row_flash_until[pitch_class].is_some_and(|deadline| now < deadline);
    let level = if flashing {
      if phases.preimage_row_flash_on {
        LED_LEVEL_FULL
      } else {
        LED_LEVEL_OFF
      }
    } else if preimage_row_counts[pitch_class] > 0 {
      LED_LEVEL_FULL
    } else if !WHITE_KEYS[pitch_class] {
      LED_LEVEL_IMAGE
    } else {
      LED_LEVEL_OFF
    };
    levels[(PREIMAGE_ROW_Y * state.config.grid_w + pitch_class as i32) as usize] = level;
  }
}

fn rendered_level(
  state: &RemappableEdoState,
  sounding: &SoundingPitchCounts,
  step: i16,
  phases: LedPhases,
) -> u8 {
  if sounding.counts[step as usize] > 0 {
    return if sounding.most_recent_step() == Some(step) || phases.sounding_on {
      LED_LEVEL_FULL
    } else {
      LED_LEVEL_OFF
    };
  }
  if let Some(preimage) = preimage_for_step(state, step) {
    if is_anchor_pitch_class(preimage) && !sounding.has_held_notes() {
      if phases.anchor_on {
        LED_LEVEL_FULL
      } else {
        LED_LEVEL_OFF
      }
    } else if phases.image_on {
      LED_LEVEL_IMAGE
    } else {
      LED_LEVEL_OFF
    }
  } else {
    LED_LEVEL_OFF
  }
}

fn is_anchor_pitch_class(preimage: usize) -> bool {
  ANCHOR_PITCH_CLASSES.contains(&preimage)
}

#[cfg(test)]
mod tests {
  use super::*;
  use super::super::config::RemapIdiom;

  fn test_state() -> RemappableEdoState {
    RemappableEdoState::new(RemapConfig::new(
      80.0,
      12,
      1,
      0,
      RemapIdiom::Snap,
      16,
      8,
    ))
  }

  fn phases(sounding_on: bool, anchor_on: bool) -> LedPhases {
    LedPhases {
      sounding_on,
      anchor_on,
      image_on: true,
      preimage_row_flash_on: false,
    }
  }

  fn level_at_grid_step(state: &RemappableEdoState, levels: &[u8], step: usize) -> u8 {
    levels[state.config.grid_w as usize + step]
  }

  #[test]
  fn anchors_flash_only_when_no_piano_notes_are_held() {
    let state = test_state();
    let mut sounding = SoundingPitchCounts::new(12);
    let recorder = RecordRuntime::new();

    let idle_levels = render_led_levels(&state, &sounding, &recorder, phases(false, true));
    assert_eq!(level_at_grid_step(&state, &idle_levels, 0), LED_LEVEL_FULL);
    assert_eq!(level_at_grid_step(&state, &idle_levels, 5), LED_LEVEL_FULL);
    assert_eq!(level_at_grid_step(&state, &idle_levels, 7), LED_LEVEL_FULL);

    sounding.by_original_note.insert(28, 4);
    sounding.held_order.push(28);
    sounding.counts[4] = 1;
    let held_levels = render_led_levels(&state, &sounding, &recorder, phases(false, true));
    assert_eq!(level_at_grid_step(&state, &held_levels, 0), LED_LEVEL_IMAGE);
    assert_eq!(level_at_grid_step(&state, &held_levels, 5), LED_LEVEL_IMAGE);
    assert_eq!(level_at_grid_step(&state, &held_levels, 7), LED_LEVEL_IMAGE);
  }

  #[test]
  fn most_recent_held_note_is_solid_and_other_held_notes_flash() {
    let state = test_state();
    let mut sounding = SoundingPitchCounts::new(12);
    sounding.by_original_note.insert(28, 4);
    sounding.held_order.push(28);
    sounding.counts[4] = 1;
    sounding.by_original_note.insert(31, 7);
    sounding.held_order.push(31);
    sounding.counts[7] = 1;
    let recorder = RecordRuntime::new();

    let levels = render_led_levels(&state, &sounding, &recorder, phases(false, true));
    assert_eq!(level_at_grid_step(&state, &levels, 4), LED_LEVEL_OFF);
    assert_eq!(level_at_grid_step(&state, &levels, 7), LED_LEVEL_FULL);

    let levels = render_led_levels(&state, &sounding, &recorder, phases(true, true));
    assert_eq!(level_at_grid_step(&state, &levels, 4), LED_LEVEL_FULL);
    assert_eq!(level_at_grid_step(&state, &levels, 7), LED_LEVEL_FULL);
  }

  #[test]
  fn record_control_leds_reflect_runtime_flags() {
    let state = test_state();
    let sounding = SoundingPitchCounts::new(12);
    let mut recorder = RecordRuntime::new();
    recorder.armed = true;
    recorder.playback = true;
    recorder.erase_ons_held = true;
    recorder.rscm = true;
    recorder.stop_flash_until = Some(Instant::now() + Duration::from_millis(50));
    let mut phase = phases(false, false);
    phase.preimage_row_flash_on = true;

    let levels = render_led_levels(&state, &sounding, &recorder, phase);

    assert_eq!(levels[13], LED_LEVEL_FULL);
    assert_eq!(levels[14], LED_LEVEL_FULL);
    assert_eq!(levels[13 + state.config.grid_w as usize], LED_LEVEL_FULL);
    assert_eq!(levels[14 + state.config.grid_w as usize], LED_LEVEL_FULL);
    assert_eq!(levels[15 + 2 * state.config.grid_w as usize], LED_LEVEL_FULL);
  }
}
