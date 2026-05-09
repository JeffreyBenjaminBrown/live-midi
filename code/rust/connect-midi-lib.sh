#!/usr/bin/env bash

find_casio_client() {
  aconnect -l | grep -B1 'CASIO USB-MIDI MIDI 1' | head -1 | grep -oP 'client \K\d+'
}

find_alsa_client() {
  local name="$1"
  aconnect -l | grep "$name" | grep -oP 'client \K\d+'
}

connect_keyboard_to_alsa_client() {
  local label="$1"
  local client_name="$2"
  local casio
  local client

  echo "=== ALSA Sequencer (keyboard -> $label) ==="

  casio=$(find_casio_client)
  client=$(find_alsa_client "$client_name")

  echo "  CASIO: $casio, $client_name: $client"

  if [[ -n "$casio" && -n "$client" ]]; then
    aconnect "$casio:0" "$client:0" && echo "  Connected: keyboard -> $label"
  else
    echo "  Warning: Could not find ALSA ports"
  fi
}

connect_pipewire_midi() {
  local source="$1"
  local dest="$2"
  local label="$3"

  pw-link "$source" "$dest" && echo "  Connected: $label"
}
