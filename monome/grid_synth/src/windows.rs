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
      Window { id: WindowId::Accretion2x2,  rect: ((0, 14), (1, 15)) },
      Window { id: WindowId::EmitToggle1x1, rect: ((2, 15), (2, 15)) },
      Window { id: WindowId::Edo,           rect: ((0, 0),  (15, 15)) },
    ]
  }

  #[test]
  fn event_in_2x2_goes_to_accretion_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 14)), Some(WindowId::Accretion2x2));
    assert_eq!(window_for_cell(&ws, (1, 15)), Some(WindowId::Accretion2x2));
  }

  #[test]
  fn event_in_1x1_goes_to_emit_toggle_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (2, 15)), Some(WindowId::EmitToggle1x1));
  }

  #[test]
  fn event_outside_smalls_goes_to_edo_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 0)),  Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (8, 8)),  Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (15, 15)), Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (3, 15)), Some(WindowId::Edo)); // just past 1x1
  }

  #[test]
  fn edo_writing_into_2x2_cell_is_dropped() {
    let ws = standard_windows();
    assert!(!visible(&ws, WindowId::Edo, (0, 14)));
    assert!(!visible(&ws, WindowId::Edo, (1, 15)));
  }

  #[test]
  fn edo_writing_into_owned_cell_passes() {
    let ws = standard_windows();
    assert!(visible(&ws, WindowId::Edo, (0, 0)));
    assert!(visible(&ws, WindowId::Edo, (15, 14)));
  }

  #[test]
  fn accretion_window_writes_pass_for_its_own_cells() {
    let ws = standard_windows();
    assert!(visible(&ws, WindowId::Accretion2x2, (0, 14)));
    assert!(visible(&ws, WindowId::Accretion2x2, (1, 15)));
    // …but not into cells outside its rect.
    assert!(!visible(&ws, WindowId::Accretion2x2, (2, 15)));
    assert!(!visible(&ws, WindowId::Accretion2x2, (5, 5)));
  }
}
