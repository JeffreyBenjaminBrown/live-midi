//! Pitch-only persistence for `momentary_chord_block` rigs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::momentary_chords::{normalized, SLOTS};

pub type AllSlots = BTreeMap<String, Vec<Option<Vec<i32>>>>;

#[derive(Debug, Serialize, Deserialize)]
struct FileV1 {
  version: u32,
  monomes: BTreeMap<String, MonomeFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MonomeFile {
  slots: Vec<SlotFile>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SlotFile {
  #[serde(default)]
  pitches: Vec<i32>,
}

pub fn path_for(rig_id: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../state/chord-slots")
    .join(format!("{rig_id}.toml"))
}

pub fn encode(all: &AllSlots) -> String {
  let monomes = all
    .iter()
    .map(|(id, slots)| {
      let slots = (0..SLOTS)
        .map(|i| SlotFile {
          pitches: slots.get(i).and_then(Option::as_ref).cloned().unwrap_or_default(),
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
    return Err(format!("unknown momentary chord-slot file version {}", file.version));
  }
  Ok(file
    .monomes
    .into_iter()
    .map(|(id, monome)| {
      let mut slots: Vec<Option<Vec<i32>>> = monome
        .slots
        .into_iter()
        .map(|slot| {
          let pitches = normalized(&slot.pitches);
          (!pitches.is_empty()).then_some(pitches)
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
    std::fs::write(&tmp, encode(all))?;
    std::fs::rename(&tmp, path)
  };
  if let Err(e) = write() {
    eprintln!("\x1b[1;31mmomentary chord slots: could not write {}: {e}\x1b[0m", path.display());
  }
}

pub fn load(path: &Path) -> AllSlots {
  let text = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AllSlots::new(),
    Err(e) => {
      eprintln!("\x1b[1;31mmomentary chord slots: could not read {}: {e}\x1b[0m", path.display());
      return AllSlots::new();
    }
  };
  match decode(&text) {
    Ok(all) => all,
    Err(e) => {
      eprintln!(
        "\x1b[1;31mmomentary chord slots: {} did not parse ({e}); starting empty\x1b[0m",
        path.display(),
      );
      AllSlots::new()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trip_is_pitch_only_and_normalizes_to_eight_slots() {
    let mut all = AllSlots::new();
    all.insert("m256-282".to_string(), vec![Some(vec![9, 5, 9])]);
    let text = encode(&all);
    for forbidden in ["waveform", "gain", "phase", "pulse", "timbre"] {
      assert!(!text.contains(forbidden), "pitch-only format leaked {forbidden}: {text}");
    }
    let decoded = decode(&text).unwrap();
    assert_eq!(decoded["m256-282"].len(), SLOTS);
    assert_eq!(decoded["m256-282"][0].as_deref(), Some([5, 9].as_slice()));
  }

  #[test]
  fn physical_ids_keep_spaces_separate() {
    let text = r#"
version = 1
[monomes.a]
slots = [{ pitches = [1] }]
[monomes.b]
slots = [{ pitches = [2] }]
"#;
    let got = decode(text).unwrap();
    assert_eq!(got["a"][0].as_deref(), Some([1].as_slice()));
    assert_eq!(got["b"][0].as_deref(), Some([2].as_slice()));
  }

  #[test]
  fn malformed_or_unknown_versions_degrade_to_empty_state() {
    let nonce = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let malformed = std::env::temp_dir().join(format!(
      "midi-pulse-momentary-chords-{}-{nonce}.toml",
      std::process::id()
    ));
    std::fs::write(&malformed, "this is not toml = [").unwrap();
    assert!(load(&malformed).is_empty());
    std::fs::remove_file(&malformed).unwrap();
    assert!(decode("version = 99\n[monomes]\n").is_err());
    assert!(load(&malformed).is_empty());
  }

  #[test]
  fn save_round_trips_atomically() {
    let nonce = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let path = std::env::temp_dir().join(format!(
      "midi-pulse-momentary-chords-save-{}-{nonce}.toml",
      std::process::id()
    ));
    let mut all = AllSlots::new();
    all.insert("physical-grid".to_string(), vec![Some(vec![7, 3, 7])]);
    save(&path, &all);
    assert_eq!(load(&path)["physical-grid"][0].as_deref(), Some([3, 7].as_slice()));
    assert!(!path.with_extension("toml.tmp").exists(), "the temporary file was renamed away");
    std::fs::remove_file(path).unwrap();
  }
}
