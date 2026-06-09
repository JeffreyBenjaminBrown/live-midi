//! The relayout pass (6_plan 4.2): resolve every window's per-edge `RectSpecConfig`
//! into an absolute `[x0, y0, x1, y1]`, following relative references
//! (`other.edge + offset`) in dependency order. Re-run whenever a window's height
//! changes (a fold/unfold), so anchored windows reflow. Pure; no I/O.
//!
//! Wired into the loop-display reflow in C5c; the engine + its tests land here in
//! C5a (6_plan's "main new engine mechanism").

use std::collections::HashMap;

use midi_pulse::config::{EdgeName, EdgeRef, RectSpecConfig, ResolvedEdge};

/// Resolve all windows' rects to absolute corners `[left, top, right, bottom]`.
/// `specs` is `(id, rect)` in any order; references may point forward or backward.
/// Errors on an unknown reference target or a reference cycle.
pub fn resolve_layout(
  specs: &[(String, RectSpecConfig)],
  grid_w: i32,
  grid_h: i32,
) -> Result<HashMap<String, [i32; 4]>, String> {
  let index: HashMap<&str, &RectSpecConfig> = specs.iter().map(|(id, s)| (id.as_str(), s)).collect();
  let mut memo: HashMap<(String, EdgeName), i32> = HashMap::new();
  let mut out = HashMap::new();
  for (id, _) in specs {
    let mut stack = Vec::new();
    let left = resolve_edge(id, EdgeName::Left, &index, grid_w, grid_h, &mut memo, &mut stack)?;
    let top = resolve_edge(id, EdgeName::Top, &index, grid_w, grid_h, &mut memo, &mut stack)?;
    let right = resolve_edge(id, EdgeName::Right, &index, grid_w, grid_h, &mut memo, &mut stack)?;
    let bottom = resolve_edge(id, EdgeName::Bottom, &index, grid_w, grid_h, &mut memo, &mut stack)?;
    out.insert(id.clone(), [left, top, right, bottom]);
  }
  Ok(out)
}

fn is_vertical(edge: EdgeName) -> bool {
  matches!(edge, EdgeName::Top | EdgeName::Bottom)
}

/// Absolute value of an edge integer: a negative counts from the far edge, so -1 =
/// last row/col, -2 = second-to-last.
fn absolute(v: i32, edge: EdgeName, grid_w: i32, grid_h: i32) -> i32 {
  let far = if is_vertical(edge) { grid_h - 1 } else { grid_w - 1 };
  if v < 0 {
    far + 1 + v
  } else {
    v
  }
}

#[allow(clippy::too_many_arguments)]
fn resolve_edge(
  id: &str,
  edge: EdgeName,
  index: &HashMap<&str, &RectSpecConfig>,
  grid_w: i32,
  grid_h: i32,
  memo: &mut HashMap<(String, EdgeName), i32>,
  stack: &mut Vec<(String, EdgeName)>,
) -> Result<i32, String> {
  let key = (id.to_string(), edge);
  if let Some(v) = memo.get(&key) {
    return Ok(*v);
  }
  if stack.contains(&key) {
    return Err(format!("window layout has a reference cycle through {id:?}.{edge:?}"));
  }
  let spec = *index
    .get(id)
    .ok_or_else(|| format!("window rect references unknown window {id:?}"))?;
  stack.push(key.clone());
  let value = match spec.resolved_edge(edge)? {
    ResolvedEdge::Absolute(v) => absolute(v, edge, grid_w, grid_h),
    ResolvedEdge::Ref(EdgeRef { target, edge: tedge, offset }) => {
      resolve_edge(&target, tedge, index, grid_w, grid_h, memo, stack)? + offset
    }
  };
  stack.pop();
  memo.insert(key, value);
  Ok(value)
}

#[cfg(test)]
mod tests {
  use super::*;
  use midi_pulse::config::EdgeSpecConfig;

  fn abs(rect: [i32; 4]) -> RectSpecConfig {
    RectSpecConfig::Absolute(rect)
  }

  /// A rect whose top is `top_expr` and whose other edges are absolute.
  fn anchored(top_expr: &str, bottom: i32, left: i32, right: i32) -> RectSpecConfig {
    RectSpecConfig::PerEdge {
      top: EdgeSpecConfig::Expr(top_expr.to_string()),
      bottom: EdgeSpecConfig::Absolute(bottom),
      left: EdgeSpecConfig::Absolute(left),
      right: EdgeSpecConfig::Absolute(right),
    }
  }

  #[test]
  fn absolute_rects_pass_through_with_far_edges() {
    let specs = vec![("a".to_string(), abs([0, 0, -1, -1]))];
    let out = resolve_layout(&specs, 16, 16).unwrap();
    assert_eq!(out["a"], [0, 0, 15, 15], "-1 = last col/row");
  }

  #[test]
  fn negative_counts_from_the_far_edge() {
    let specs = vec![("a".to_string(), abs([0, 0, -1, -2]))];
    let out = resolve_layout(&specs, 16, 16).unwrap();
    assert_eq!(out["a"], [0, 0, 15, 14], "-2 = second-to-last row");
  }

  #[test]
  fn an_anchored_window_moves_when_its_target_height_changes() {
    // The C5a "Done": a window anchored to another's edge reflows when that other's
    // height changes. Editor unfolded (bottom 6) vs folded (bottom 0); the display
    // sits at editor.bottom + 1.
    let display = anchored("editor.bottom + 1", -1, 0, -1);
    let unfolded = vec![
      ("editor".to_string(), abs([0, 0, 15, 6])),
      ("display".to_string(), display.clone()),
    ];
    let folded = vec![
      ("editor".to_string(), abs([0, 0, 15, 0])),
      ("display".to_string(), display),
    ];
    assert_eq!(resolve_layout(&unfolded, 16, 16).unwrap()["display"], [0, 7, 15, 15]);
    assert_eq!(resolve_layout(&folded, 16, 16).unwrap()["display"], [0, 1, 15, 15]);
  }

  #[test]
  fn transitive_references_resolve() {
    let specs = vec![
      ("a".to_string(), abs([0, 0, 15, 2])),
      ("b".to_string(), anchored("a.bottom + 1", 5, 0, 15)),
      ("c".to_string(), anchored("b.bottom + 2", 15, 0, 15)),
    ];
    let out = resolve_layout(&specs, 16, 16).unwrap();
    assert_eq!(out["b"][1], 3, "b.top = a.bottom(2) + 1");
    assert_eq!(out["c"][1], 7, "c.top = b.bottom(5) + 2");
  }

  #[test]
  fn unknown_target_errors() {
    let specs = vec![("a".to_string(), anchored("ghost.bottom + 1", -1, 0, -1))];
    let err = resolve_layout(&specs, 16, 16).unwrap_err();
    assert!(err.contains("unknown window"), "{err}");
  }

  #[test]
  fn reference_cycle_errors() {
    // a.top -> b.top -> a.top is a genuine cycle (both depend on the other's top).
    let specs = vec![
      ("a".to_string(), anchored("b.top + 1", 5, 0, 15)),
      ("b".to_string(), anchored("a.top + 1", 5, 0, 15)),
    ];
    let err = resolve_layout(&specs, 16, 16).unwrap_err();
    assert!(err.contains("cycle"), "{err}");
  }
}
