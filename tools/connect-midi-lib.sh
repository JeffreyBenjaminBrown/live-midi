#!/usr/bin/env bash

find_casio_client() {
  aconnect -l | grep -B1 'CASIO USB-MIDI MIDI 1' | head -1 | grep -oP 'client \K\d+'
}

# Kernel-type ALSA clients that are hardware MIDI devices but NOT a piano-style
# keyboard: the ALSA plumbing itself plus the known pedal boards.
KEYBOARD_EXCLUDE_REGEX="client 0:|'System'|Midi Through|SSCOM|SoftStep|MPC"

# The keyboard need not be the Casio: find the hardware MIDI client that isn't a
# known non-keyboard. Prefers the Casio when present (exact legacy behavior);
# otherwise, if exactly one candidate remains, that's the keyboard. Zero or
# several candidates print nothing (the caller warns / the user connects by hand).
find_keyboard_client() {
  local casio candidates
  casio=$(find_casio_client)
  if [[ -n "$casio" ]]; then
    echo "$casio"
    return
  fi
  candidates=$(aconnect -l \
    | grep -P "^client \d+: .*type=kernel" \
    | grep -Ev "$KEYBOARD_EXCLUDE_REGEX" \
    | grep -oP 'client \K\d+')
  if [[ $(wc -w <<< "$candidates") -eq 1 ]]; then
    echo "$candidates"
  elif [[ -n "$candidates" ]]; then
    echo "  Warning: several hardware MIDI devices could be the keyboard:" >&2
    aconnect -l | grep -P "^client \d+: .*type=kernel" \
      | grep -Ev "$KEYBOARD_EXCLUDE_REGEX" | sed 's/^/    /' >&2
    echo "  Connect by hand: aconnect <client>:0 <rig-client>:0" >&2
  fi
}

find_alsa_client() {
  local name="$1"
  aconnect -l | grep "$name" | grep -oP 'client \K\d+'
}

connect_keyboard_to_alsa_client() {
  local label="$1"
  local client_name="$2"
  local keyboard
  local client

  echo "=== ALSA Sequencer (keyboard -> $label) ==="

  keyboard=$(find_keyboard_client)
  client=$(find_alsa_client "$client_name")

  echo "  keyboard: $keyboard, $client_name: $client"

  if [[ -n "$keyboard" && -n "$client" ]]; then
    if aconnect "$keyboard:0" "$client:0" 2>/tmp/connect-midi-aconnect.err; then
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

pipewire_port_exists() {
  local port="$1"

  pw-link -i 2>/dev/null | grep -Fxq "$port"
}

connect_reaper_to_primary_audio_out() {
  local reaper_l="REAPER:out1"
  local reaper_r="REAPER:out2"
  local sink_prefix="alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic"
  local headphones="${sink_prefix}.HiFi__Headphones__sink"
  local speaker="${sink_prefix}.HiFi__Speaker__sink"
  local sink
  local sink_label
  local port

  echo "=== JACK/PipeWire audio (Reaper -> output) ==="

  if pipewire_port_exists "${headphones}:playback_FL"; then
    sink="$headphones"
    sink_label="Headphones"
  elif pipewire_port_exists "${speaker}:playback_FL"; then
    sink="$speaker"
    sink_label="Speaker"
  else
    echo "  Warning: neither Headphones nor Speaker sink found."
    echo "  Available sinks:"
    pw-link -i 2>/dev/null | grep 'sink:playback' | sort -u | sed 's/^/    /' || true
    return 0
  fi

  echo "  Found: $sink_label"

  # Remove old Reaper-to-sink links first, so rerunning after plugging or
  # unplugging the audio jack moves Reaper to the currently active sink.
  while IFS= read -r port; do
    pw-link -d "$reaper_l" "$port" 2>/dev/null || true
    pw-link -d "$reaper_r" "$port" 2>/dev/null || true
  done < <(pw-link -i 2>/dev/null | grep 'sink:playback' || true)

  if pw-link "$reaper_l" "${sink}:playback_FL" 2>/tmp/connect-midi-pw-link-audio.err; then
    echo "  Connected: $reaper_l -> ${sink_label}:playback_FL"
  elif pw-link -l | grep -A20 -Fx "$reaper_l" | grep -Fxq "  |-> ${sink}:playback_FL"; then
    echo "  Already connected: $reaper_l -> ${sink_label}:playback_FL"
  else
    cat /tmp/connect-midi-pw-link-audio.err
    return 1
  fi

  if pw-link "$reaper_r" "${sink}:playback_FR" 2>/tmp/connect-midi-pw-link-audio.err; then
    echo "  Connected: $reaper_r -> ${sink_label}:playback_FR"
  elif pw-link -l | grep -A20 -Fx "$reaper_r" | grep -Fxq "  |-> ${sink}:playback_FR"; then
    echo "  Already connected: $reaper_r -> ${sink_label}:playback_FR"
  else
    cat /tmp/connect-midi-pw-link-audio.err
    return 1
  fi
}
