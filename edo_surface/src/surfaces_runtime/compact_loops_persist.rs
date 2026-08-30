//! Versioned pitch/timing-only persistence for `compact_loop_block`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::compact_loops::{LoopInterval, LoopSlot, SLOTS};

pub type AllSlots = BTreeMap<String, Vec<Option<LoopSlot>>>;

#[derive(Debug, Deserialize, Serialize)]
struct FileV1 {
  version: u32,
  monomes: BTreeMap<String, MonomeFile>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MonomeFile {
  slots: Vec<SlotFile>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct SlotFile {
  #[serde(default)]
  duration_ns: u64,
  #[serde(default)]
  intervals: Vec<LoopInterval>,
}

pub fn path_for(rig_id: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../state/compact-loops")
    .join(format!("{rig_id}.toml"))
}

pub fn encode(all: &AllSlots) -> String {
  let monomes = all
    .iter()
    .map(|(id, slots)| {
      let slots = (0..SLOTS)
        .map(|index| match slots.get(index).and_then(Option::as_ref) {
          Some(slot) => SlotFile {
            duration_ns: slot.duration_ns,
            intervals: slot.intervals.clone(),
          },
          None => SlotFile::default(),
        })
        .collect();
      (id.clone(), MonomeFile { slots })
    })
    .collect();
  toml::to_string_pretty(&FileV1 { version: 1, monomes }).unwrap_or_default()
}

pub fn decode(text: &str) -> Result<AllSlots, String> {
  let file: FileV1 = toml::from_str(text).map_err(|e| e.to_string())?;
  if file.version != 1 {
    return Err(format!("unknown compact-loop file version {}", file.version));
  }
  Ok(file
    .monomes
    .into_iter()
    .map(|(id, monome)| {
      let mut slots: Vec<Option<LoopSlot>> = monome
        .slots
        .into_iter()
        .map(|slot| {
          LoopSlot { duration_ns: slot.duration_ns, intervals: slot.intervals }.normalized()
        })
        .collect();
      slots.resize_with(SLOTS, || None);
      slots.truncate(SLOTS);
      (id, slots)
    })
    .collect())
}

pub fn save(path: &Path, all: &AllSlots) {
  let write = || -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(encode(all).as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
  };
  if let Err(error) = write() {
    eprintln!(
      "\x1b[1;31mcompact loops: could not write {}: {error}\x1b[0m",
      path.display()
    );
  }
}

pub fn load(path: &Path) -> AllSlots {
  let text = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return AllSlots::new(),
    Err(error) => {
      eprintln!(
        "\x1b[1;31mcompact loops: could not read {}: {error}\x1b[0m",
        path.display()
      );
      return AllSlots::new();
    }
  };
  match decode(&text) {
    Ok(all) => all,
    Err(error) => {
      eprintln!(
        "\x1b[1;31mcompact loops: {} did not parse ({error}); starting empty\x1b[0m",
        path.display()
      );
      AllSlots::new()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trip_stores_only_pitch_and_timing_and_pads_slots() {
    let mut all = AllSlots::new();
    all.insert(
      "physical-grid".to_string(),
      vec![Some(LoopSlot {
        duration_ns: 1_000,
        intervals: vec![LoopInterval { pitch: 7, start_ns: 900, duration_ns: Some(300) }],
      })],
    );
    let text = encode(&all);
    for forbidden in ["timbre", "gain", "phase_mode", "playback", "crowded"] {
      assert!(!text.contains(forbidden));
    }
    let decoded = decode(&text).unwrap();
    assert_eq!(decoded["physical-grid"].len(), SLOTS);
    assert_eq!(decoded["physical-grid"][0], all["physical-grid"][0]);
  }

  #[test]
  fn invalid_slots_normalize_to_empty() {
    let text = r#"
version = 1
[monomes.grid]
[[monomes.grid.slots]]
duration_ns = 0
intervals = [{ pitch = 2, start_ns = 0, duration_ns = 2 }]
"#;
    assert!(decode(text).unwrap()["grid"][0].is_none());
  }
}
