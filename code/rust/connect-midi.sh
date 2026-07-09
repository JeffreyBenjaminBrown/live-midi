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
if [[ "$rig_path" != *.toml ]]; then
  rig_path="$rig_path.toml"
fi
if [[ ! -f "$rig_path" ]]; then
  echo "rig not found: $rig_path" >&2
  exit 1
fi

table_string() {
  local table="$1"
  local key="$2"
  awk -v table="$table" -v key="$key" '
    $0 == "[" table "]" { in_table = 1; next }
    /^\[/ { in_table = 0 }
    in_table && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
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
