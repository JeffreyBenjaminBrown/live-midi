//! Window dispatch and the LED compositor.
//!
//! Windows are rectangular cell regions, ordered front-to-back. Each
//! event lands in the first window whose rect contains the cell. LED
//! commands from a window are forwarded only if no front-er window
//! occludes the target cell.

use std::net::{SocketAddr, UdpSocket};
use rosc::OscType;

use crate::leds::low_res_brightness;
use crate::osc::send_osc;
use crate::types::{Brightness, MonomeKey, Rect, Window, WindowId};

pub fn rect_contains(rect: &Rect, cell: MonomeKey) -> bool {
  let ((x0, y0), (x1, y1)) = *rect;
  let (x, y) = cell;
  x0 <= x && x <= x1 && y0 <= y && y <= y1
}

// Front-to-back: returns the first window whose rect contains `cell`.
pub fn window_for_cell(windows: &[Window], cell: MonomeKey) -> Option<WindowId> {
  windows.iter().find(|w| rect_contains(&w.rect, cell)).map(|w| w.id)
}

// True iff window `from` "owns" `cell` — i.e. `from`'s rect contains
// `cell` AND no earlier (front-er) window's rect does. This is the
// compositor's only decision; pulled out as a pure fn for testing.
//
// PITFALL: the PitchLedReasons map is global state that every window
// writes into. Without this filter the EDO grid would quietly stomp
// the LEDs that control windows are managing. If you move windows
// around, also update what cells the EDO grid claims.
pub fn visible(windows: &[Window], from: WindowId, cell: MonomeKey) -> bool {
  for w in windows {
    if w.id == from {
      return rect_contains(&w.rect, cell);
    }
    if rect_contains(&w.rect, cell) {
      return false;
    }
  }
  false
}

// LED-command compositor wrapper around send_osc. Drops writes for
// cells the calling window doesn't own. Uses /grid/led/level/set
// (variable brightness); binary on/off is just the Off/Bright cases
// of Brightness.
pub fn set_led(
  windows: &[Window], from: WindowId, cell: MonomeKey, b: Brightness,
  sock: &UdpSocket, device: SocketAddr, led_level_set: &str,
) {
  if !visible(windows, from, cell) { return; }
  send_osc(sock, device, led_level_set, vec![
    OscType::Int(cell.0), OscType::Int(cell.1),
    OscType::Int(low_res_brightness(b)),
  ]);
}

#[cfg(test)]
mod tests {
  use super::*;

  fn standard_windows() -> Vec<Window> {
    vec![
      Window { id: WindowId::ControlsTop,    rect: ((0, 14), (3, 14)) },
      Window { id: WindowId::ControlsBottom, rect: ((0, 15), (15, 15)) },
      Window { id: WindowId::Edo,            rect: ((0, 0),  (15, 15)) },
    ]
  }

  #[test]
  fn event_in_top_row_left_goes_to_controls_top() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 14)), Some(WindowId::ControlsTop));
    assert_eq!(window_for_cell(&ws, (3, 14)), Some(WindowId::ControlsTop));
  }

  #[test]
  fn event_in_bottom_row_goes_to_controls_bottom() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 15)),  Some(WindowId::ControlsBottom));
    assert_eq!(window_for_cell(&ws, (15, 15)), Some(WindowId::ControlsBottom));
  }

  #[test]
  fn event_outside_controls_goes_to_edo_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 0)),   Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (8, 8)),   Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (15, 13)), Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (4, 14)),  Some(WindowId::Edo)); // just past ControlsTop
  }

  #[test]
  fn edo_writing_into_controls_cell_is_dropped() {
    let ws = standard_windows();
    assert!(!visible(&ws, WindowId::Edo, (0, 14)));
    assert!(!visible(&ws, WindowId::Edo, (3, 14)));
    assert!(!visible(&ws, WindowId::Edo, (0, 15)));
    assert!(!visible(&ws, WindowId::Edo, (15, 15)));
  }

  #[test]
  fn edo_writing_into_owned_cell_passes() {
    let ws = standard_windows();
    assert!(visible(&ws, WindowId::Edo, (0, 0)));
    assert!(visible(&ws, WindowId::Edo, (4, 14)));   // past ControlsTop
    assert!(visible(&ws, WindowId::Edo, (15, 13))); // above ControlsBottom
  }

  #[test]
  fn controls_top_writes_pass_for_its_own_cells() {
    let ws = standard_windows();
    assert!(visible(&ws, WindowId::ControlsTop, (0, 14)));
    assert!(visible(&ws, WindowId::ControlsTop, (3, 14)));
    // …but not outside its rect.
    assert!(!visible(&ws, WindowId::ControlsTop, (4, 14)));
    assert!(!visible(&ws, WindowId::ControlsTop, (0, 15)));
  }
}
