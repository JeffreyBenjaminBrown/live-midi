//! Distinct-device assignment for multi-grid runtimes (the looper and the surfaces
//! runtime both bind two grids of the same size).
//!
//! `monome::discover_devices` dedupes only by *port*, and serialosc can report one
//! device id on several ports where only the newest is live (RUNTIME-NOTES.org,
//! "Ghost tty devices"). So we group by id, keep the newest (last-replied) port
//! per id, and hand each config monome a distinct live id in config order. We
//! never hand two ports of the same id to two grids. This is additive: the
//! existing single-grid `discover_device`/`discover_configured_device` are
//! untouched.

use std::collections::HashMap;

use crate::monome::DeviceInfo;

/// The distinct live grids of `size` in a discovery reply, in first-appearance
/// order, keeping the newest port per id (serialosc can report one id on several
/// ports; RUNTIME-NOTES.org "Ghost tty devices"). The shared core of both the
/// strict and the tolerant assignment below.
fn distinct_newest(devices: &[DeviceInfo], size: [i32; 2]) -> Vec<DeviceInfo> {
  let mut order: Vec<String> = vec![];
  let mut newest: HashMap<String, DeviceInfo> = HashMap::new();
  for device in devices {
    if [device.grid_w, device.grid_h] != size {
      continue;
    }
    if !newest.contains_key(&device.id) {
      order.push(device.id.clone());
    }
    // Reply order is oldest-first, so the last write per id is the newest port.
    newest.insert(device.id.clone(), device.clone());
  }
  order.iter().map(|id| newest[id].clone()).collect()
}

/// Pick `count` distinct grids of `size` from a discovery reply list, in
/// first-appearance order, taking the newest port for each id. Errs (loudly,
/// naming what was seen) if fewer than `count` distinct live ids of that size
/// were found.
pub fn assign_distinct_devices(
  devices: &[DeviceInfo],
  size: [i32; 2],
  count: usize,
) -> Result<Vec<DeviceInfo>, String> {
  let chosen = distinct_newest(devices, size);
  if chosen.len() < count {
    let seen: Vec<(&str, u16)> = chosen.iter().map(|d| (d.id.as_str(), d.port)).collect();
    let all: Vec<(&str, u16, i32, i32)> = devices
      .iter()
      .map(|d| (d.id.as_str(), d.port, d.grid_w, d.grid_h))
      .collect();
    return Err(format!(
      "need {count} distinct {size:?} grids; found {} of that size ({seen:?}). \
       serialosc reported: {all:?}",
      chosen.len(),
    ));
  }
  Ok(chosen.into_iter().take(count).collect())
}

/// Like [`assign_distinct_devices`], but tolerant of missing gear: returns a slot for
/// each of the `count` requested grids -- `Some(device)` for the grids that have a
/// live device to bind (the distinct ids of `size`, newest port each, in first-
/// appearance order, filling the low indices first) and `None` for the grids with no
/// device present. Never errors: absent gear is the caller's to report and route
/// around, so a runtime can load the surfaces it *can* bring up (the "robust to
/// missing gear" path -- see the surfaces runtime). With every grid present this is
/// exactly `assign_distinct_devices` wrapped in `Some`, so the all-connected behaviour
/// is unchanged.
pub fn assign_available_devices(
  devices: &[DeviceInfo],
  size: [i32; 2],
  count: usize,
) -> Vec<Option<DeviceInfo>> {
  let mut chosen = distinct_newest(devices, size);
  chosen.truncate(count);
  let mut slots: Vec<Option<DeviceInfo>> = chosen.into_iter().map(Some).collect();
  slots.resize(count, None);
  slots
}

#[cfg(test)]
mod tests {
  use super::*;

  fn dev(id: &str, port: u16, w: i32, h: i32) -> DeviceInfo {
    DeviceInfo {
      id: id.to_string(),
      type_name: "monome 256".to_string(),
      port,
      grid_w: w,
      grid_h: h,
    }
  }

  #[test]
  fn two_distinct_grids_assign_in_order() {
    let devices = [dev("a", 9000, 16, 16), dev("b", 9001, 16, 16)];
    let got = assign_distinct_devices(&devices, [16, 16], 2).unwrap();
    assert_eq!(got.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
  }

  #[test]
  fn one_id_on_two_ports_is_not_two_grids() {
    // A single device id reported on two ports (a ghost) must NOT satisfy a
    // two-grid request.
    let devices = [dev("a", 9000, 16, 16), dev("a", 9005, 16, 16)];
    let err = assign_distinct_devices(&devices, [16, 16], 2).expect_err("one id is not two grids");
    assert!(err.contains("distinct"), "{err}");
  }

  #[test]
  fn newest_port_per_id_is_chosen() {
    // id "a" appears on 9000 then 9005; 9005 (last = newest) wins.
    let devices = [dev("a", 9000, 16, 16), dev("a", 9005, 16, 16), dev("b", 9001, 16, 16)];
    let got = assign_distinct_devices(&devices, [16, 16], 2).unwrap();
    assert_eq!(got[0].id, "a");
    assert_eq!(got[0].port, 9005, "newest port for a ghosted id");
    assert_eq!(got[1].id, "b");
  }

  #[test]
  fn wrong_size_devices_are_ignored() {
    let devices = [dev("a", 9000, 16, 8), dev("b", 9001, 16, 16), dev("c", 9002, 16, 16)];
    let got = assign_distinct_devices(&devices, [16, 16], 2).unwrap();
    assert_eq!(got.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["b", "c"]);
  }

  #[test]
  fn extra_grids_are_truncated_to_count() {
    let devices = [dev("a", 1, 16, 16), dev("b", 2, 16, 16), dev("c", 3, 16, 16)];
    let got = assign_distinct_devices(&devices, [16, 16], 2).unwrap();
    assert_eq!(got.len(), 2);
  }

  #[test]
  fn available_fills_present_grids_and_leaves_the_rest_none() {
    // One grid of a two-grid request is connected: slot 0 gets it, slot 1 is None
    // (no error). This is the "one grid unplugged" path the surfaces runtime loads
    // around.
    let devices = [dev("a", 9000, 16, 16)];
    let got = assign_available_devices(&devices, [16, 16], 2);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].as_ref().map(|d| d.id.as_str()), Some("a"));
    assert!(got[1].is_none(), "the second grid is absent");
  }

  #[test]
  fn available_all_present_matches_the_strict_assignment() {
    let devices = [dev("a", 9000, 16, 16), dev("b", 9001, 16, 16)];
    let got = assign_available_devices(&devices, [16, 16], 2);
    assert_eq!(
      got.iter().map(|d| d.as_ref().map(|d| d.id.clone())).collect::<Vec<_>>(),
      [Some("a".to_string()), Some("b".to_string())],
    );
  }

  #[test]
  fn available_with_no_devices_is_all_none() {
    let got = assign_available_devices(&[], [16, 16], 2);
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|d| d.is_none()), "nothing connected -> every slot None");
  }

  #[test]
  fn available_keeps_newest_port_per_ghosted_id() {
    let devices = [dev("a", 9000, 16, 16), dev("a", 9005, 16, 16)];
    let got = assign_available_devices(&devices, [16, 16], 2);
    assert_eq!(got[0].as_ref().map(|d| d.port), Some(9005), "newest port for a ghosted id");
    assert!(got[1].is_none(), "one id is still one grid");
  }
}
