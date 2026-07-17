//! Distinct-device assignment for multi-grid runtimes (the looper and the surfaces
//! runtime both bind two grids of the same size).
//!
//! `monome::discover_devices` dedupes only by *port*, and serialosc can report one
//! device id on several ports where only the newest is live (RUNTIME-NOTES.org,
//! "Ghost tty devices"). So we group by id, keep the newest (last-replied) port
//! per id, and hand each rig monome a distinct live id in rig order. We
//! never hand two ports of the same id to two grids. This is additive: the
//! existing single-grid `discover_device`/`discover_configured_device` are
//! untouched.

use std::collections::HashMap;

use crate::monome::DeviceInfo;
use crate::rig::MonomeSelect;

/// Does this device satisfy every predicate the rig monome declared? All provided
/// `select.*` must match (an omitted predicate matches anything).
fn matches(device: &DeviceInfo, select: &MonomeSelect) -> bool {
  select.size.is_none_or(|[w, h]| [device.grid_w, device.grid_h] == [w, h])
    && select.type_contains.as_ref().is_none_or(|t| device.type_name.contains(t))
    && select.id_contains.as_ref().is_none_or(|i| device.id.contains(i))
}

/// Is this selector *discriminating* -- i.e. does it name a particular device rather
/// than merely a shape? Size alone matches every grid of that shape.
fn is_pinned(select: &MonomeSelect) -> bool {
  select.id_contains.is_some() || select.type_contains.is_some()
}

/// Assign one live device per rig monome, honoring each monome's FULL `select`
/// (`id_contains` / `type_contains`, not just `size`), tolerating absent gear the way
/// [`assign_available_devices`] does.
///
/// Pinned monomes are resolved FIRST, then unpinned ones fill from what's left. Order
/// matters: with an unpinned monome `a` and a `b` pinned to a serial, filling in rig
/// order would let `a` claim the very device `b` names, and `b` would find nothing.
///
/// Why this exists: `select.id_contains` has been in the rig schema all along and was
/// never read, so two same-size grids were handed out in *enumeration* order -- which
/// flips when they re-enumerate. The looper doesn't care which grid is which, but a
/// rig whose pedals target "the left monome" very much does: a swap silently inverts
/// the whole spatial layout.
pub fn assign_selected_devices(
  devices: &[DeviceInfo],
  selects: &[MonomeSelect],
) -> Vec<Option<DeviceInfo>> {
  // Distinct live ids, newest port each. Size is per-select now, so filter here only
  // by "is a grid at all" and let `matches` do the rest.
  let pool = distinct_newest_any(devices);
  let mut slots: Vec<Option<DeviceInfo>> = vec![None; selects.len()];
  let mut claimed: Vec<&str> = Vec::new();

  for pass_pinned in [true, false] {
    for (slot, select) in selects.iter().enumerate() {
      if slots[slot].is_some() || is_pinned(select) != pass_pinned {
        continue;
      }
      if let Some(device) = pool
        .iter()
        .find(|d| !claimed.contains(&d.id.as_str()) && matches(d, select))
      {
        claimed.push(device.id.as_str());
        slots[slot] = Some(device.clone());
      }
    }
  }
  slots
}

/// Every distinct live device, newest port per id, in first-appearance order.
fn distinct_newest_any(devices: &[DeviceInfo]) -> Vec<DeviceInfo> {
  let mut order: Vec<String> = vec![];
  let mut newest: HashMap<String, DeviceInfo> = HashMap::new();
  for device in devices {
    if !newest.contains_key(&device.id) {
      order.push(device.id.clone());
    }
    newest.insert(device.id.clone(), device.clone());
  }
  order.iter().map(|id| newest[id].clone()).collect()
}

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

  // ---- assign_selected_devices: honoring the full select ----

  fn sel(size: Option<[i32; 2]>, id: Option<&str>) -> MonomeSelect {
    MonomeSelect {
      size,
      type_contains: None,
      id_contains: id.map(str::to_string),
    }
  }

  fn ids(slots: &[Option<DeviceInfo>]) -> Vec<Option<&str>> {
    slots.iter().map(|s| s.as_ref().map(|d| d.id.as_str())).collect()
  }

  /// The point of the whole function: a rig pins each grid by serial, so the
  /// assignment does NOT depend on enumeration order. Here the devices arrive in the
  /// opposite order to the rig's monomes.
  #[test]
  fn pinned_monomes_bind_their_own_serial_regardless_of_enumeration_order() {
    let devices = [dev("m0000102", 9000, 16, 16), dev("m256-282", 9001, 16, 16)];
    let selects = [sel(Some([16, 16]), Some("m256-282")), sel(Some([16, 16]), Some("m0000102"))];
    assert_eq!(ids(&assign_selected_devices(&devices, &selects)), [Some("m256-282"), Some("m0000102")]);
    // ...and the same rig against the other enumeration order gives the same answer.
    let flipped = [dev("m256-282", 9001, 16, 16), dev("m0000102", 9000, 16, 16)];
    assert_eq!(ids(&assign_selected_devices(&flipped, &selects)), [Some("m256-282"), Some("m0000102")]);
  }

  /// The ordering trap that forces the pinned-first pass: an unpinned monome listed
  /// FIRST must not swallow the device a later monome pins by name.
  #[test]
  fn an_unpinned_monome_does_not_steal_a_pinned_monomes_device() {
    let devices = [dev("m256-282", 9000, 16, 16), dev("m0000102", 9001, 16, 16)];
    let selects = [sel(Some([16, 16]), None), sel(Some([16, 16]), Some("m256-282"))];
    assert_eq!(
      ids(&assign_selected_devices(&devices, &selects)),
      [Some("m0000102"), Some("m256-282")],
      "the pinned monome takes its serial; the unpinned one takes what's left",
    );
  }

  #[test]
  fn a_pinned_monome_whose_grid_is_absent_gets_none_and_does_not_grab_another() {
    let devices = [dev("m0000102", 9000, 16, 16)];
    let selects = [sel(Some([16, 16]), Some("m256-282")), sel(Some([16, 16]), Some("m0000102"))];
    assert_eq!(
      ids(&assign_selected_devices(&devices, &selects)),
      [None, Some("m0000102")],
      "an absent pinned grid stays absent rather than binding the wrong board",
    );
  }

  #[test]
  fn selected_never_hands_one_id_to_two_monomes() {
    let devices = [dev("a", 9000, 16, 16), dev("a", 9005, 16, 16)];
    let selects = [sel(Some([16, 16]), None), sel(Some([16, 16]), None)];
    let got = assign_selected_devices(&devices, &selects);
    assert_eq!(got[0].as_ref().map(|d| d.port), Some(9005), "newest port for a ghosted id");
    assert!(got[1].is_none(), "one id is still one grid");
  }

  #[test]
  fn selected_respects_size() {
    let devices = [dev("small", 9000, 8, 8), dev("big", 9001, 16, 16)];
    let selects = [sel(Some([16, 16]), None)];
    assert_eq!(ids(&assign_selected_devices(&devices, &selects)), [Some("big")]);
  }

  /// With no discriminating predicate this must behave exactly like the old
  /// size-and-count assignment, so unpinned rigs (the looper) are unaffected.
  #[test]
  fn selected_with_only_sizes_matches_assign_available_devices() {
    let devices = [dev("a", 9000, 16, 16), dev("b", 9001, 16, 16), dev("c", 9002, 8, 8)];
    let selects = [sel(Some([16, 16]), None), sel(Some([16, 16]), None)];
    assert_eq!(
      ids(&assign_selected_devices(&devices, &selects)),
      ids(&assign_available_devices(&devices, [16, 16], 2)),
    );
  }
}
