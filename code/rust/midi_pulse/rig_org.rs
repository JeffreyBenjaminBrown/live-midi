//! The `.org` rig loader (TODO.org "change .toml to .org"). A rig can be written as
//! an org-mode outline instead of TOML: each parameter's documentation lives in the
//! headline tree with it. The data model is unchanged -- this parses the outline into
//! the very same [`Rig`](crate::rig::Rig) via the existing serde derives + validation,
//! so nothing downstream of loading differs between an `.org` rig and a `.toml` one.
//!
//! Syntax (headline *titles*; see rigs/README.org and TODO/toml-to-org-rig-format.org):
//! - `PARAM <key> = <value>` -- a scalar leaf. `<key>` may be dotted (`select.size`);
//!   `<value>` is ordinary TOML value syntax, so inline arrays/tables just work.
//! - `TABLE <key>` -- a (sub)table; its descendants are its contents. TABLEs nest.
//! - `ELEM <key>` -- one element of the array-of-tables `<key>` (one `[[key]]`);
//!   repeat sibling `ELEM <key>` headlines to grow the array, in order.
//! - any other title -- a pure *grouping* headline: transparent to the data (its
//!   descendants attach to the nearest enclosing TABLE/ELEM), there only to organize
//!   and document.
//! - headline bodies (lines under a headline) are documentation; the parser ignores
//!   them.
//!
//! Implementation: the outline builds a `toml::Table`, which is re-emitted as TOML and
//! fed to [`crate::rig::parse_rig`]. That reuses TOML's exact value grammar (for the
//! `PARAM` right-hand sides and dotted keys) and the whole typed/validated path.

use crate::rig::{parse_rig, Rig};

/// One open TABLE/ELEM (or the root) while walking the outline: the org depth of its
/// headline and the table being accumulated for it.
struct Frame {
  depth: usize,
  kind: FrameKind,
  table: toml::Table,
}

enum FrameKind {
  /// The document root (depth 0); its table is the final result.
  Root,
  /// A `TABLE key`: merged into its parent under `key` when it closes.
  Table(String),
  /// An `ELEM key`: appended to its parent's `key` array when it closes.
  Elem(String),
}

/// Parse an `.org` rig into a validated [`Rig`].
pub fn parse_org_rig(source: &str) -> Result<Rig, String> {
  let table = build_table(source)?;
  // Re-emit as TOML and reuse the existing parse+validate path, so `.org` and `.toml`
  // rigs are byte-for-byte the same struct (unknown keys still rejected, etc.).
  let text = toml::to_string(&toml::Value::Table(table))
    .map_err(|e| format!("org rig -> toml: {e}"))?;
  parse_rig(&text)
}

/// Build the `toml::Table` for an outline (no validation -- `parse_org_rig` does that).
fn build_table(source: &str) -> Result<toml::Table, String> {
  let mut stack: Vec<Frame> =
    vec![Frame { depth: 0, kind: FrameKind::Root, table: toml::Table::new() }];

  for (i, raw) in source.lines().enumerate() {
    let lineno = i + 1;
    let Some((depth, title)) = headline(raw) else {
      continue; // body / blank / non-headline: documentation, ignored.
    };
    // Close every open frame that this headline is not nested inside (depth-based
    // dedent). A grouping headline pushes nothing, so its descendants keep attaching
    // to the enclosing TABLE/ELEM -- transparency falls out of this.
    while stack.len() > 1 && stack.last().unwrap().depth >= depth {
      let child = stack.pop().unwrap();
      close_frame(child, stack.last_mut().unwrap(), lineno)?;
    }

    match classify(title) {
      Line::Param { key, value } => {
        // Parse "<key> = <value>" as a one-line TOML doc: this handles dotted keys and
        // the full TOML value grammar (inline arrays/tables, strings, numbers, bools).
        let parsed: toml::Table = toml::from_str(&format!("{key} = {value}"))
          .map_err(|e| format!("line {lineno}: PARAM {key:?}: {e}"))?;
        let top = stack.last_mut().unwrap();
        deep_merge(&mut top.table, parsed);
      }
      Line::Table(key) => {
        stack.push(Frame { depth, kind: FrameKind::Table(key), table: toml::Table::new() });
      }
      Line::Elem(key) => {
        stack.push(Frame { depth, kind: FrameKind::Elem(key), table: toml::Table::new() });
      }
      Line::Grouping => { /* transparent: no frame */ }
    }
  }

  // Close everything back down to the root.
  while stack.len() > 1 {
    let child = stack.pop().unwrap();
    close_frame(child, stack.last_mut().unwrap(), source.lines().count())?;
  }
  Ok(stack.pop().unwrap().table)
}

/// If `line` is an org headline (`^\*+\s+<title>`), return (depth, title). Depth is the
/// number of leading `*`.
fn headline(line: &str) -> Option<(usize, &str)> {
  let stars = line.len() - line.trim_start_matches('*').len();
  if stars == 0 {
    return None;
  }
  let rest = &line[stars..];
  // A real headline has whitespace after the stars (so a line like "**bold**" is body).
  let trimmed = rest.trim_start();
  if rest.len() == trimmed.len() {
    return None;
  }
  let title = trimmed.trim_end();
  if title.is_empty() {
    return None;
  }
  Some((stars, title))
}

enum Line {
  Param { key: String, value: String },
  Table(String),
  Elem(String),
  Grouping,
}

/// Classify a headline title by its first word.
fn classify(title: &str) -> Line {
  let mut it = title.splitn(2, char::is_whitespace);
  let head = it.next().unwrap_or("");
  let rest = it.next().unwrap_or("").trim();
  match head {
    "PARAM" => match rest.split_once('=') {
      Some((k, v)) => Line::Param { key: k.trim().to_string(), value: v.trim().to_string() },
      // A PARAM with no '=' is malformed; treat it as grouping so parse_rig's
      // "missing field" error points at the real problem rather than us panicking.
      None => Line::Grouping,
    },
    "TABLE" if !rest.is_empty() => Line::Table(rest.to_string()),
    "ELEM" if !rest.is_empty() => Line::Elem(rest.to_string()),
    _ => Line::Grouping,
  }
}

/// Merge a just-closed frame into its parent: a `TABLE` under its key, an `ELEM` onto
/// its key's array.
fn close_frame(child: Frame, parent: &mut Frame, lineno: usize) -> Result<(), String> {
  match child.kind {
    FrameKind::Root => Ok(()), // never on the stack above index 0
    FrameKind::Table(key) => {
      match parent.table.get_mut(&key) {
        Some(toml::Value::Table(dt)) => deep_merge(dt, child.table),
        Some(_) => {
          return Err(format!("line {lineno}: TABLE {key:?} conflicts with a non-table value"))
        }
        None => {
          parent.table.insert(key, toml::Value::Table(child.table));
        }
      }
      Ok(())
    }
    FrameKind::Elem(key) => {
      let slot = parent.table.entry(key.clone()).or_insert_with(|| toml::Value::Array(vec![]));
      match slot {
        toml::Value::Array(a) => {
          a.push(toml::Value::Table(child.table));
          Ok(())
        }
        _ => Err(format!("line {lineno}: ELEM {key:?} conflicts with a non-array value")),
      }
    }
  }
}

/// Recursively merge `src` into `dst`: matching sub-tables merge, everything else is
/// inserted/overwritten. Lets dotted `PARAM` keys and sibling TABLEs accumulate.
fn deep_merge(dst: &mut toml::Table, src: toml::Table) {
  for (k, v) in src {
    match (dst.get_mut(&k), v) {
      (Some(toml::Value::Table(dt)), toml::Value::Table(sv)) => deep_merge(dt, sv),
      (_, v) => {
        dst.insert(k, v);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::rig::{MonomeWindowRig, SinkRig};

  const SAWWAVE_ORG: &str = r#"
* a monome EDO sawwave rig
Top-level grouping headline: transparent, just documents the whole rig.
** PARAM version = 1
** PARAM id = "org-sawwave"
** PARAM title = "Org Sawwave"
** the tuning
A grouping headline; its ELEM child attaches to the root.
*** ELEM tunings
**** PARAM id = "main"
**** PARAM edo = 46
**** PARAM x_step = 9
**** PARAM y_step = 1
**** PARAM fundamental_hz = 220
** ELEM sinks
*** PARAM id = "saw"
*** PARAM kind = "cpal_synth"
*** PARAM sample_rate = 48000
*** PARAM buffer_frames = 128
*** PARAM amplitude = 0.15
*** PARAM attack_secs = 0.003
*** PARAM release_secs = 0.05
*** PARAM accretion_level = 0.5
** ELEM monomes
*** PARAM id = "big"
*** PARAM listen_port = 9000
*** PARAM prefix = "/256-1"
*** PARAM select.size = [16, 16]
** ELEM monome_windows
*** PARAM id = "edo-grid"
*** PARAM monome = "big"
*** PARAM kind = "edo_note_grid"
*** PARAM rect = [0, 0, 15, 13]
*** PARAM tuning = "main"
*** PARAM sink = "saw"
"#;

  #[test]
  fn parses_a_sawwave_rig_from_org() {
    let rig = parse_org_rig(SAWWAVE_ORG).expect("org rig parses");
    assert_eq!(rig.id, "org-sawwave");
    assert_eq!(rig.title, "Org Sawwave");
    assert_eq!(rig.tunings.len(), 1);
    assert_eq!(rig.tunings[0].edo, 46);
    assert_eq!(rig.tunings[0].x_step, 9);
    // Dotted PARAM key -> nested table (select.size).
    assert_eq!(rig.monomes.len(), 1);
    assert_eq!(rig.monomes[0].select.size, Some([16, 16]));
    assert_eq!(rig.monomes[0].listen_port, 9000);
    // The one window came through as an edo_note_grid with its inline-array rect.
    assert_eq!(rig.monome_windows.len(), 1);
    match &rig.monome_windows[0] {
      MonomeWindowRig::EdoNoteGrid { rect, tuning, sink, .. } => {
        assert_eq!(*rect, [0, 0, 15, 13]);
        assert_eq!(tuning, "main");
        assert_eq!(sink, "saw");
      }
      other => panic!("expected edo_note_grid, got {other:?}"),
    }
    // The tagged sink parsed.
    assert!(matches!(rig.sinks[0], SinkRig::CpalSynth { .. }));
    assert!(!rig.echo_input, "echo_input defaults off");
  }

  #[test]
  fn repeated_elem_headlines_grow_the_array_in_order() {
    let org = r#"
* rig
** PARAM version = 1
** PARAM id = "r"
** PARAM title = "R"
** ELEM tunings
*** PARAM id = "a"
*** PARAM edo = 12
*** PARAM x_step = 1
*** PARAM y_step = 1
*** PARAM fundamental_hz = 440
** ELEM tunings
*** PARAM id = "b"
*** PARAM edo = 31
*** PARAM x_step = 6
*** PARAM y_step = 1
*** PARAM fundamental_hz = 80
"#;
    let rig = parse_org_rig(org).expect("parses");
    assert_eq!(rig.tunings.len(), 2);
    assert_eq!(rig.tunings[0].id, "a");
    assert_eq!(rig.tunings[1].id, "b");
    assert_eq!(rig.tunings[1].edo, 31);
  }

  #[test]
  fn nested_tables_and_grouping_are_transparent() {
    // [surfaces] as a TABLE, its knobs under a transparent grouping headline.
    let org = r#"
* rig
** PARAM version = 1
** PARAM id = "r"
** PARAM title = "R"
** TABLE surfaces
The shared trail knobs.
*** the knobs
A grouping headline inside the TABLE: its PARAMs still land in [surfaces].
**** PARAM trails_max = 5
**** PARAM trail_clobber_radius = 20
"#;
    let rig = parse_org_rig(org).expect("parses");
    let s = rig.surfaces.expect("surfaces table present");
    assert_eq!(s.trails_max, 5);
    assert_eq!(s.trail_clobber_radius, 20);
  }

  #[test]
  fn unknown_key_is_still_rejected() {
    let org = r#"
* rig
** PARAM version = 1
** PARAM id = "r"
** PARAM title = "R"
** PARAM bogus_field = 3
"#;
    assert!(parse_org_rig(org).is_err(), "deny_unknown_fields still applies through the org path");
  }

  /// A hand conversion of the shipped `rigs/monome-edo-sawwave.toml`, for the
  /// round-trip below. Exercises real complexity: dotted select keys, a tagged
  /// cpal_synth sink, and six tagged monome_windows (one with a bool field).
  const SAWWAVE_REAL_ORG: &str = r#"
* monome EDO sawwave
** PARAM version = 1
** PARAM id = "monome-edo-sawwave"
** PARAM title = "Monome EDO Sawwave"
** ELEM tunings
*** PARAM id = "main"
*** PARAM edo = 46
*** PARAM x_step = 9
*** PARAM y_step = 1
*** PARAM fundamental_hz = 220
** ELEM monomes
*** PARAM id = "big"
*** PARAM listen_port = 9000
*** PARAM prefix = "/256-1-cable"
*** PARAM select.size = [16, 16]
*** PARAM select.type_contains = "256"
** ELEM sinks
*** PARAM id = "saw"
*** PARAM kind = "cpal_synth"
*** PARAM sample_rate = 48000
*** PARAM buffer_frames = 128
*** PARAM amplitude = 0.15
*** PARAM attack_secs = 0.003
*** PARAM release_secs = 0.050
*** PARAM accretion_level = 0.5
** the play grid
*** ELEM monome_windows
**** PARAM id = "edo-grid"
**** PARAM monome = "big"
**** PARAM kind = "edo_note_grid"
**** PARAM rect = [0, 0, 15, 13]
**** PARAM tuning = "main"
**** PARAM sink = "saw"
** the chord controls
*** ELEM monome_windows
**** PARAM id = "wipe"
**** PARAM monome = "big"
**** PARAM kind = "chord_wipe_button"
**** PARAM rect = [0, 14, 0, 14]
*** ELEM monome_windows
**** PARAM id = "accrete"
**** PARAM monome = "big"
**** PARAM kind = "chord_accrete_toggle"
**** PARAM rect = [1, 14, 1, 14]
*** ELEM monome_windows
**** PARAM id = "emit-mode"
**** PARAM monome = "big"
**** PARAM kind = "chord_emit_mode_toggle"
**** PARAM rect = [2, 14, 2, 14]
**** PARAM emit_is_toggle_initially = true
*** ELEM monome_windows
**** PARAM id = "target"
**** PARAM monome = "big"
**** PARAM kind = "chord_target_button"
**** PARAM rect = [3, 14, 3, 14]
*** ELEM monome_windows
**** PARAM id = "chords"
**** PARAM monome = "big"
**** PARAM kind = "chord_slot_buttons"
**** PARAM rect = [0, 15, 15, 15]
**** PARAM initial_accretion_target = 1
"#;

  #[test]
  fn round_trips_a_real_shipped_rig() {
    use crate::rig::load_named_rig;
    // The .org conversion must produce the exact same typed Rig as the shipped .toml,
    // even across grouping headlines at varying depths and interleaved ELEM arrays.
    let from_toml = load_named_rig("monome-edo-sawwave").expect("the shipped .toml loads");
    let from_org = parse_org_rig(SAWWAVE_REAL_ORG).expect("the org conversion loads");
    assert_eq!(from_org, from_toml, "the .org conversion == the .toml rig");
  }

  #[test]
  fn bodies_and_blank_lines_are_ignored() {
    let org = "\n\n* rig\nsome prose\n  indented prose\n** PARAM version = 1\nmore prose\n** PARAM id = \"r\"\n** PARAM title = \"R\"\n";
    let rig = parse_org_rig(org).expect("parses despite the prose");
    assert_eq!(rig.id, "r");
  }
}
