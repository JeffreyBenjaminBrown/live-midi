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
    if aconnect "$casio:0" "$client:0" 2>/tmp/connect-midi-aconnect.err; then
      echo "  Connected: keyboard -> $label"
    elif grep -q 'Connection is already subscribed' /tmp/connect-midi-aconnect.err; then
      echo "  Already connected: keyboard -> $label"
    else
      cat /tmp/connect-midi-aconnect.err
      return 1
    fi
  else
    echo "  Warning: Could not find ALSA ports"
  fi
}

connect_pipewire_midi() {
  local source="$1"
  local dest="$2"
  local label="$3"

  if pw-link "$source" "$dest" 2>/tmp/connect-midi-pw-link.err; then
    echo "  Connected: $label"
  elif pw-link -l | grep -A20 -Fx "$source" | grep -Fxq "  |-> $dest"; then
    echo "  Already connected: $label"
  else
    cat /tmp/connect-midi-pw-link.err
    return 1
  fi
}
