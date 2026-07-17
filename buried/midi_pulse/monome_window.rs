pub type MonomeCell = (i32, i32);
pub type Rect = (MonomeCell, MonomeCell);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Window<Id> {
  pub id: Id,
  pub rect: Rect,
}

pub fn rect_contains(rect: &Rect, cell: MonomeCell) -> bool {
  let ((x0, y0), (x1, y1)) = *rect;
  let (x, y) = cell;
  x0 <= x && x <= x1 && y0 <= y && y <= y1
}

pub fn window_for_cell<Id: Copy>(windows: &[Window<Id>], cell: MonomeCell) -> Option<Id> {
  windows
    .iter()
    .rev()
    .find(|window| rect_contains(&window.rect, cell))
    .map(|window| window.id)
}

pub fn visible<Id: Copy + Eq>(windows: &[Window<Id>], from: Id, cell: MonomeCell) -> bool {
  for window in windows.iter().rev() {
    if window.id == from {
      return rect_contains(&window.rect, cell);
    }
    if rect_contains(&window.rect, cell) {
      return false;
    }
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  enum TestWindow {
    Front,
    Back,
  }

  fn test_windows() -> Vec<Window<TestWindow>> {
    vec![
      Window { id: TestWindow::Back, rect: ((0, 0), (3, 3)) },
      Window { id: TestWindow::Front, rect: ((1, 1), (2, 2)) },
    ]
  }

  #[test]
  fn dispatch_uses_last_matching_window() {
    let windows = test_windows();

    assert_eq!(window_for_cell(&windows, (1, 1)), Some(TestWindow::Front));
    assert_eq!(window_for_cell(&windows, (0, 0)), Some(TestWindow::Back));
    assert_eq!(window_for_cell(&windows, (4, 4)), None);
  }

  #[test]
  fn back_window_is_not_visible_under_front_window() {
    let windows = test_windows();

    assert!(!visible(&windows, TestWindow::Back, (1, 1)));
    assert!(visible(&windows, TestWindow::Back, (0, 0)));
    assert!(visible(&windows, TestWindow::Front, (1, 1)));
  }
}
