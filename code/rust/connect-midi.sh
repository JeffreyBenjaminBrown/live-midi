#!/usr/bin/env bash

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
CONFIG_NAME="${1:-}"

if [[ -z "$CONFIG_NAME" ]]; then
  echo "usage: $0 CONFIG_NAME" >&2
  echo "example: $0 edo-un12-58-8-1-snap" >&2
  exit 2
fi

source "$HERE/connect-midi-lib.sh"

config_path="$ROOT/code/rust/configs/$CONFIG_NAME"
if [[ "$config_path" != *.toml ]]; then
  config_path="$config_path.toml"
fi
if [[ ! -f "$config_path" ]]; then
  echo "config not found: $config_path" >&2
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
  ' "$config_path"
}

midi_input="$(table_string "midi.input" "virtual_name")"
midi_output="$(table_string "midi.output" "virtual_name")"

echo "=== midi_pulse config: $CONFIG_NAME ==="

if [[ -n "$midi_input" ]]; then
  connect_keyboard_to_alsa_client "$CONFIG_NAME" "$midi_input"
else
  echo "No [midi.input] virtual_name in config; skipping keyboard input."
fi

if [[ -n "$midi_output" ]]; then
  echo ""
  echo "=== JACK/PipeWire MIDI ($midi_output -> Reaper) ==="
  connect_pipewire_midi \
    "Midi-Bridge:$midi_output:(capture_0) out" \
    "REAPER:MIDI Input 1" \
    "$midi_output -> REAPER MIDI Input 1"
else
  echo "No [midi.output] virtual_name in config; skipping MIDI output."
fi

echo ""
connect_reaper_to_primary_audio_out

echo ""
echo "Done!"
