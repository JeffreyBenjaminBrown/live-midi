//! Chord-slot persistence (chord-storage-v2, 2_discussion "persistence"): the slots
//! survive a restart. One TOML file per rig id under `state/chord-slots/` at the
//! repo root (gitignored), written ATOMICALLY (tmp + rename) on every successful
//! save-to-slot and loaded once at bring-up. Toggle/armed state is deliberately not
//! stored: a restart comes up silent with contents intact.
//!
//! Snapshots embed their full timbre parameters, so later edits to the rig's
//! `[[timbres]]` table never corrupt a stored chord. A missing file is simply empty
//! slots; an unparsable one prints one red line and plays on -- persistence must
//! never kill the instrument.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::{Am, Fm, RelAm, RelFm, Timbre, Waveform};

use super::chords::{StoredChord, StoredVoice, SLOTS};

/// The state file for `rig_id`: `state/chord-slots/<rig id>.toml`, resolved like
/// `rig_dir()` resolves `rigs/` (compile-time manifest dir, so per-worktree).
pub fn path_for(rig_id: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../state/chord-slots")
    .join(format!("{rig_id}.toml"))
}

/// Everything one save writes: each monome's nine slots, keyed by monome id
/// (BTreeMap for stable file output).
pub type AllSlots = BTreeMap<String, Vec<Option<StoredChord>>>;

#[derive(Serialize, Deserialize)]
struct FileV1 {
  version: u32,
  monomes: BTreeMap<String, MonomeFile>,
}

#[derive(Serialize, Deserialize)]
struct MonomeFile {
  /// Exactly [`SLOTS`] entries; an empty `voices` list is an empty slot. Defensive
  /// on load: shorter files pad, longer truncate.
  slots: Vec<SlotFile>,
}

#[derive(Serialize, Deserialize, Default)]
struct SlotFile {
  #[serde(default)]
  voices: Vec<VoiceFile>,
}

#[derive(Serialize, Deserialize)]
struct VoiceFile {
  pitch: i32,
  waveform: String,
  gain: f32,
  abs_am_depth: f32,
  abs_am_hz: f32,
  am_shape: f32,
  abs_fm_depth_cents: f32,
  abs_fm_hz: f32,
  rel_am_depth: f32,
  rel_am_freq: f32,
  rel_fm_depth: f32,
  rel_fm_freq: f32,
  fader_gain: f32,
  pedal_gain: f32,
  osc_phase: f32,
  pulse_factor: f32,
  pulse_phase: f32,
}

fn waveform_name(w: Waveform) -> &'static str {
  match w {
    Waveform::Sine => "sine",
    Waveform::Triangle => "triangle",
    Waveform::Square => "square",
    Waveform::Saw => "saw",
  }
}

fn waveform_from(name: &str) -> Option<Waveform> {
  match name {
    "sine" => Some(Waveform::Sine),
    "triangle" => Some(Waveform::Triangle),
    "square" => Some(Waveform::Square),
    "saw" => Some(Waveform::Saw),
    _ => None,
  }
}

impl VoiceFile {
  fn from_stored(v: &StoredVoice) -> Self {
    VoiceFile {
      pitch: v.pitch,
      waveform: waveform_name(v.timbre.waveform).to_string(),
      gain: v.timbre.gain,
      abs_am_depth: v.timbre.am.depth,
      abs_am_hz: v.timbre.am.freq,
      am_shape: v.timbre.am.shape,
      abs_fm_depth_cents: v.timbre.fm.depth_cents,
      abs_fm_hz: v.timbre.fm.freq,
      rel_am_depth: v.timbre.rel_am.depth,
      rel_am_freq: v.timbre.rel_am.freq,
      rel_fm_depth: v.timbre.rel_fm.depth,
      rel_fm_freq: v.timbre.rel_fm.freq,
      fader_gain: v.fader_gain,
      pedal_gain: v.pedal_gain,
      osc_phase: v.osc_phase,
      pulse_factor: v.pulse_factor,
      pulse_phase: v.pulse_phase,
    }
  }

  fn to_stored(&self) -> Option<StoredVoice> {
    Some(StoredVoice {
      pitch: self.pitch,
      timbre: Timbre {
        waveform: waveform_from(&self.waveform)?,
        gain: self.gain,
        am: Am { depth: self.abs_am_depth, freq: self.abs_am_hz, shape: self.am_shape },
        fm: Fm { depth_cents: self.abs_fm_depth_cents, freq: self.abs_fm_hz },
        rel_am: RelAm { depth: self.rel_am_depth, freq: self.rel_am_freq },
        rel_fm: RelFm { depth: self.rel_fm_depth, freq: self.rel_fm_freq },
      },
      fader_gain: self.fader_gain,
      pedal_gain: self.pedal_gain,
      osc_phase: self.osc_phase,
      pulse_factor: self.pulse_factor,
      pulse_phase: self.pulse_phase,
    })
  }
}

/// Encode every monome's slots as the file's TOML text (pure; the I/O is `save`).
pub fn encode(all: &AllSlots) -> String {
  let monomes = all
    .iter()
    .map(|(id, slots)| {
      let slots = slots
        .iter()
        .map(|slot| SlotFile {
          voices: slot
            .as_ref()
            .map(|c| c.voices.iter().map(VoiceFile::from_stored).collect())
            .unwrap_or_default(),
        })
        .collect();
      (id.clone(), MonomeFile { slots })
    })
    .collect();
  let file = FileV1 { version: 1, monomes };
  toml::to_string_pretty(&file).unwrap_or_default()
}

/// Decode a state file's text (pure; the I/O is `load`). `Err` carries a
/// human-readable reason; an unknown waveform in one voice drops THAT VOICE only
/// (loudly would be better than silently, but a whole-file refusal for one bad
/// voice would cost eight good slots).
pub fn decode(text: &str) -> Result<AllSlots, String> {
  let file: FileV1 = toml::from_str(text).map_err(|e| e.to_string())?;
  if file.version != 1 {
    return Err(format!("unknown chord-slots file version {}", file.version));
  }
  let mut all = AllSlots::new();
  for (id, monome) in file.monomes {
    let mut slots: Vec<Option<StoredChord>> = monome
      .slots
      .iter()
      .map(|s| {
        let voices: Vec<StoredVoice> = s.voices.iter().filter_map(|v| v.to_stored()).collect();
        (!voices.is_empty()).then_some(StoredChord { voices })
      })
      .collect();
    slots.resize(SLOTS, None);
    slots.truncate(SLOTS);
    all.insert(id, slots);
  }
  Ok(all)
}

/// Write `all` to `path` atomically: the bytes land in a sibling tmp file first and
/// rename into place, so a crash mid-write can never leave a truncated state file.
/// Errors are reported (red) and swallowed -- a failed save must not stop the music.
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
    eprintln!("\x1b[1;31mchord slots: could not write {}: {e}\x1b[0m", path.display());
  }
}

/// Read `path`. Missing file = empty slots (first run); an unreadable or
/// unparsable one prints a red line and plays on with empty slots.
pub fn load(path: &Path) -> AllSlots {
  let text = match std::fs::read_to_string(path) {
    Ok(t) => t,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AllSlots::new(),
    Err(e) => {
      eprintln!("\x1b[1;31mchord slots: could not read {}: {e}\x1b[0m", path.display());
      return AllSlots::new();
    }
  };
  match decode(&text) {
    Ok(all) => all,
    Err(e) => {
      eprintln!(
        "\x1b[1;31mchord slots: {} did not parse ({e}); starting with empty slots\x1b[0m",
        path.display(),
      );
      AllSlots::new()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn chord(pitches: &[i32]) -> StoredChord {
    StoredChord {
      voices: pitches
        .iter()
        .map(|p| StoredVoice {
          pitch: *p,
          timbre: Timbre { waveform: Waveform::Saw, gain: 0.8, ..Timbre::default() },
          fader_gain: 0.5,
          pedal_gain: 0.25,
          osc_phase: 0.4,
          pulse_factor: 1.5,
          pulse_phase: 0.3,
        })
        .collect(),
    }
  }

  fn slots_with(chords: &[(usize, StoredChord)]) -> Vec<Option<StoredChord>> {
    let mut slots = vec![None; SLOTS];
    for (i, c) in chords {
      slots[*i] = Some(c.clone());
    }
    slots
  }

  #[test]
  fn encode_decode_round_trips_every_field() {
    let mut all = AllSlots::new();
    all.insert("a".to_string(), slots_with(&[(0, chord(&[10, 20])), (8, chord(&[5]))]));
    all.insert("b".to_string(), slots_with(&[(3, chord(&[-7]))]));
    let text = encode(&all);
    let back = decode(&text).expect("round trip parses");
    assert_eq!(back, all);
  }

  #[test]
  fn a_missing_file_is_just_empty_slots() {
    let path = Path::new("/nonexistent/chord-slots/never.toml");
    assert!(load(path).is_empty());
  }

  #[test]
  fn garbage_decodes_to_an_error_not_a_panic() {
    assert!(decode("this is not toml [[[").is_err());
    assert!(decode("version = 99\n[monomes]\n").is_err(), "unknown version rejected");
  }

  #[test]
  fn an_unknown_waveform_drops_that_voice_only() {
    let mut all = AllSlots::new();
    all.insert("a".to_string(), slots_with(&[(0, chord(&[10, 20]))]));
    let text = encode(&all).replace("waveform = \"saw\"", "waveform = \"theremin\"");
    let back = decode(&text).expect("still parses");
    assert!(back["a"][0].is_none(), "both voices dropped -> the slot reads empty");
  }

  #[test]
  fn save_and_load_round_trip_on_disk_atomically() {
    let dir = std::env::temp_dir().join(format!("chord-slots-test-{}", std::process::id()));
    let path = dir.join("test-rig.toml");
    let mut all = AllSlots::new();
    all.insert("a".to_string(), slots_with(&[(4, chord(&[1, 2, 3]))]));
    save(&path, &all);
    assert_eq!(load(&path), all, "what was saved comes back");
    assert!(!path.with_extension("toml.tmp").exists(), "the tmp file was renamed away");
    // Overwrite with new content; the old file is replaced whole.
    let mut all2 = AllSlots::new();
    all2.insert("a".to_string(), slots_with(&[(0, chord(&[9]))]));
    save(&path, &all2);
    assert_eq!(load(&path), all2);
    let _ = std::fs::remove_dir_all(&dir);
  }
}
