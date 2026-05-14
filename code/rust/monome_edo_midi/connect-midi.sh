#!/usr/bin/env bash

# Connect MIDI ports for monome_edo_midi setup.
# Uses pw-link for JACK/PipeWire MIDI (monome_edo_midi -> Reaper).

set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../connect-midi-lib.sh"

echo "=== JACK MIDI (monome_edo_midi -> Reaper) ==="

connect_pipewire_midi \
  "Midi-Bridge:monome_edo_midi-out:(capture_0) out" \
  "REAPER:MIDI Input 1" \
  "monome_edo_midi-out -> REAPER MIDI Input 1"

echo ""
echo "Done!"
