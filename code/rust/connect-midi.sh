#!/usr/bin/env bash

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
RIG_NAME="${1:-}"

if [[ -z "$RIG_NAME" ]]; then
  echo "usage: $0 RIG_NAME" >&2
  echo "example: $0 edo-un12_58-8-1_snap_recording" >&2
  exit 2
fi

source "$HERE/connect-midi-lib.sh"

rig_path="$ROOT/code/rust/rigs/$RIG_NAME"
if [[ "$rig_path" != *.org && "$rig_path" != *.toml ]]; then
  # Rigs are .org now (TOML-in-org); fall back to .toml for anything not yet migrated.
  if [[ -f "$rig_path.org" ]]; then
    rig_path="$rig_path.org"
  else
    rig_path="$rig_path.toml"
  fi
fi
if [[ ! -f "$rig_path" ]]; then
  echo "rig not found: $rig_path" >&2
  exit 1
fi

# Read e.g. [midi.input].virtual_name from either a .toml rig or our .org rig
# (`*** TABLE input` / `**** PARAM virtual_name = "..."`), matching the table's LEAF
# segment (input/output). Both forms carry the value as `virtual_name = "..."`.
table_string() {
  local table="$1"   # e.g. midi.input
  local key="$2"     # e.g. virtual_name
  local leaf="${table##*.}"
  awk -v table="$table" -v leaf="$leaf" -v key="$key" '
    $0 == "[" table "]"        { in_table = 1; next }   # .toml form
    $0 ~ "^\\*+ TABLE " leaf "$" { in_table = 1; next }  # .org form
    /^\[/                       { in_table = 0 }
    /^\*+ (TABLE|ELEM) /        { if ($0 !~ "^\\*+ TABLE " leaf "$") in_table = 0 }
    in_table && $0 ~ ("(^[[:space:]]*|PARAM[[:space:]]+)" key "[[:space:]]*=") {
      value = $0
      sub(/^[^=]*=[[:space:]]*"/, "", value)
      sub(/"[[:space:]]*$/, "", value)
      print value
      exit
    }
  ' "$rig_path"
}

midi_input="$(table_string "midi.input" "virtual_name")"
midi_output="$(table_string "midi.output" "virtual_name")"

echo "=== midi_pulse rig: $RIG_NAME ==="

if [[ -n "$midi_input" ]]; then
  connect_keyboard_to_alsa_client "$RIG_NAME" "$midi_input"
else
  echo "No [midi.input] virtual_name in rig; skipping keyboard input."
fi

if [[ -n "$midi_output" ]]; then
  echo ""
  echo "=== JACK/PipeWire MIDI ($midi_output -> Reaper) ==="
  connect_pipewire_midi \
    "Midi-Bridge:$midi_output:(capture_0) out" \
    "REAPER:MIDI Input 1" \
    "$midi_output -> REAPER MIDI Input 1"
else
  echo "No [midi.output] virtual_name in rig; skipping MIDI output."
fi

echo ""
connect_reaper_to_primary_audio_out

echo ""
echo "Done!"
