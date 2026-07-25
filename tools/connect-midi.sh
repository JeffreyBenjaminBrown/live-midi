#!/usr/bin/env bash

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
RIG_NAME="${1:-}"

if [[ -z "$RIG_NAME" ]]; then
  echo "usage: $0 RIG_NAME" >&2
  echo "example: $0 edo-un12_58-8-1_snap_recording" >&2
  exit 2
fi

source "$HERE/connect-midi-lib.sh"

rig_path="$ROOT/rigs/$RIG_NAME"
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

# Robustness: each connection step runs even if an earlier one failed, and a
# failed step explains itself (and how to do it by hand) rather than killing
# the script -- so a missing keyboard still gets you the Reaper wiring, and
# vice versa. FAILED_STEPS collects what to look at.
FAILED_STEPS=()

# Discover the keyboards once; the count also decides whether the second
# input/output pair (two-keyboard piano rigs) gets wired.
KEYBOARDS=()
mapfile -t KEYBOARDS < <(find_keyboard_clients)

if [[ -n "$midi_input" ]]; then
  connect_keyboards_to_rig "$RIG_NAME" "$midi_input" "${KEYBOARDS[@]}" \
    || FAILED_STEPS+=("keyboard(s) -> $midi_input")
else
  echo "No [midi.input] virtual_name in rig; skipping keyboard input."
fi

if [[ -n "$midi_output" ]]; then
  echo ""
  echo "=== JACK/PipeWire MIDI ($midi_output -> Reaper) ==="
  if aconnect -l 2>/dev/null | grep -qF "Midi Through Port-1"; then
    # Kernel-thru route (make-2-midi-inputs.sh): snd-seq-dummy has 2 ports,
    # which Reaper reads directly -- they are the only port kind its Linux
    # MIDI-device list shows (kernel clients get port.physical, user-space
    # ports never do). Port-0 = 58-edo 1 (left), Port-1 = 58-edo 2.
    echo "  kernel-thru route (Midi Through has two ports; Reaper reads them directly)."
    connect_alsa_by_names "$midi_output" 0 "Midi Through" 0 \
      "$midi_output -> Midi Through Port-0 (58-edo 1)" \
      || FAILED_STEPS+=("$midi_output -> Midi Through Port-0")
    if [[ ${#KEYBOARDS[@]} -ge 2 ]]; then
      connect_alsa_by_names "$midi_output-2" 0 "Midi Through" 1 \
        "$midi_output-2 -> Midi Through Port-1 (58-edo 2)" \
        || FAILED_STEPS+=("$midi_output-2 -> Midi Through Port-1")
    fi
  else
    # Prefer a Reaper port named for the rig ("58-edo 1"), else the first MIDI input.
    dest1="$(find_reaper_midi_input "58-edo 1" 1)"
    if [[ -z "$dest1" ]]; then
      echo "  Warning: no REAPER MIDI input port found -- is Reaper running? Start it and rerun."
      FAILED_STEPS+=("$midi_output -> Reaper MIDI")
    else
      connect_pipewire_midi \
        "Midi-Bridge:$midi_output:(capture_0) out" \
        "$dest1" \
        "$midi_output -> ${dest1#REAPER:}" \
        || FAILED_STEPS+=("$midi_output -> Reaper MIDI")
    fi
    if [[ ${#KEYBOARDS[@]} -ge 2 ]]; then
      dest2="$(find_reaper_midi_input "58-edo 2" 2)"
      if [[ -z "$dest2" || "$dest2" == "$dest1" ]]; then
        echo "  Warning: no second REAPER MIDI input for 58-edo 2 -- enable (or rename) it in Reaper, then rerun."
        echo "  Once it exists: pw-link \"Midi-Bridge:$midi_output-2:(capture_0) out\" \"REAPER:<its name>\""
        FAILED_STEPS+=("$midi_output-2 -> Reaper MIDI")
      else
        connect_pipewire_midi \
          "Midi-Bridge:$midi_output-2:(capture_0) out" \
          "$dest2" \
          "$midi_output-2 -> ${dest2#REAPER:}" \
          || FAILED_STEPS+=("$midi_output-2 -> Reaper MIDI")
      fi
    fi
  fi
else
  echo "No [midi.output] virtual_name in rig; skipping MIDI output."
fi

echo ""
connect_reaper_to_primary_audio_out \
  || FAILED_STEPS+=("Reaper -> audio out")

echo ""
if [[ ${#FAILED_STEPS[@]} -eq 0 ]]; then
  echo "Done!"
else
  echo "Done, but ${#FAILED_STEPS[@]} step(s) could not connect (details above):"
  printf '  - %s\n' "${FAILED_STEPS[@]}"
  echo "Fix the cause (or connect by hand as shown) and rerun; already-made connections are kept."
  exit 1
fi
