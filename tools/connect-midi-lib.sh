#!/usr/bin/env bash

find_casio_client() {
  aconnect -l | grep -B1 'CASIO USB-MIDI MIDI 1' | head -1 | grep -oP 'client \K\d+'
}

# Kernel-type ALSA clients that are hardware MIDI devices but NOT a piano-style
# keyboard: the ALSA plumbing itself plus the known pedal boards. "MPC-20" (not
# the broader "MPC") so a borrowed Akai MPC-series keyboard is not excluded.
KEYBOARD_EXCLUDE_REGEX="client 0:|'System'|Midi Through|SSCOM|SoftStep|MPC-20"

# Every plausible keyboard, one ALSA client number per line, the Casio first when
# present (it keeps its historical priority). The caller decides what each count
# means: one keyboard is the classic single-piano setup, two feed the runtime's two
# port pairs, zero or 3+ get a warning.
find_keyboard_clients() {
  local casio candidates
  casio=$(find_casio_client) || true
  candidates=$(aconnect -l \
    | grep -P "^client \d+: .*type=kernel" \
    | grep -Ev "$KEYBOARD_EXCLUDE_REGEX" \
    | grep -oP 'client \K\d+') || true
  if [[ -n "$casio" ]]; then
    echo "$casio"
    grep -vx "$casio" <<< "$candidates" | grep . || true
  else
    grep . <<< "$candidates" || true
  fi
}

# Connect one keyboard client to one of the rig's -in clients, explaining failures.
connect_one_keyboard() {
  local keyboard="$1"
  local client_name="$2"
  local what="$3"
  local client

  client=$(find_alsa_client "$client_name")
  if [[ -z "$client" ]]; then
    echo "  Warning: '$client_name' not found -- is the rig runtime running (and new enough to open it)? Start it, then rerun this script."
    return 1
  fi
  if aconnect "$keyboard:0" "$client:0" 2>/tmp/connect-midi-aconnect.err; then
    echo "  Connected: keyboard $keyboard -> $what"
  elif grep -q 'Connection is already subscribed' /tmp/connect-midi-aconnect.err; then
    echo "  Already connected: keyboard $keyboard -> $what"
  else
    cat /tmp/connect-midi-aconnect.err
    echo "  To do it by hand: aconnect $keyboard:0 $client:0"
    return 1
  fi
}

# Connect every discovered keyboard (passed as arguments after label and client
# name). One -> the rig's -in, exactly as always. Two -> the first to -in (58-edo 1)
# and the second to -in-2 (58-edo 2); ALSA client numbers follow plug order and say
# NOTHING about physical placement, so remind the player about the runtime's 'l'
# identification. Zero or 3+ -> explain and leave it to the player.
connect_keyboards_to_rig() {
  local label="$1"
  local client_name="$2"
  shift 2
  local -a keyboards=("$@")

  echo "=== ALSA Sequencer (keyboards -> $label) ==="
  case ${#keyboards[@]} in
    0)
      echo "  Warning: no keyboard found (aconnect -l shows no hardware MIDI device that isn't a known non-keyboard). Plug it in and rerun, or connect by hand: aconnect <keyboard-client>:0 <rig-client>:0"
      return 1
      ;;
    1)
      echo "  keyboard: client ${keyboards[0]}"
      connect_one_keyboard "${keyboards[0]}" "$client_name" "$label"
      ;;
    2)
      echo "  two keyboards: clients ${keyboards[0]} and ${keyboards[1]}"
      local failed=0
      connect_one_keyboard "${keyboards[0]}" "$client_name" "$client_name (58-edo 1)" || failed=1
      connect_one_keyboard "${keyboards[1]}" "${client_name}-2" "${client_name}-2 (58-edo 2)" || failed=1
      echo "  Left/right is not knowable from here: in the runtime, type 'l' Enter, then"
      echo "  play a note on the LEFT keyboard -- it becomes 58-edo 1."
      return $failed
      ;;
    *)
      echo "  Warning: ${#keyboards[@]} hardware MIDI devices could be keyboards:"
      aconnect -l | grep -P "^client \d+: .*type=kernel" \
        | grep -Ev "$KEYBOARD_EXCLUDE_REGEX" | sed 's/^/    /'
      echo "  The runtime has ports for two. Connect by hand: aconnect <client>:0 <rig-client>:0"
      return 1
      ;;
  esac
}

# The Reaper MIDI input port to feed: prefer a port whose name contains $1 (e.g.
# "58-edo 1" -- Reaper device renames, when they reach PipeWire), else the $2-th
# (1-based) of REAPER's MIDI inputs in sorted order. REAPER's audio ins are
# in1/in2; every other REAPER port in `pw-link -i` is a MIDI input. Empty output
# when Reaper is not up or has fewer MIDI inputs than $2.
find_reaper_midi_input() {
  local prefer="$1"
  local ordinal="$2"
  local ports hit
  ports=$(pw-link -i 2>/dev/null | grep '^REAPER:' | grep -Ev '^REAPER:in[0-9]+$' | sort) || true
  if [[ -n "$prefer" ]]; then
    hit=$(grep -F "$prefer" <<< "$ports" | head -1) || true
    if [[ -n "$hit" ]]; then
      echo "$hit"
      return 0
    fi
  fi
  sed -n "${ordinal}p" <<< "$ports"
  return 0
}

find_alsa_client() {
  local name="$1"
  # head -1: if two runtime instances are up, connect to the first rather than
  # producing a two-line value that breaks the aconnect call below.
  aconnect -l | grep "$name" | grep -oP 'client \K\d+' | head -1
}

# Exact-name ALSA client lookup: matches the FULL quoted client name, so
# "edo_un12_piano_monome-out" cannot accidentally resolve to "...-out-2".
find_alsa_client_exact() {
  local name="$1"
  aconnect -l 2>/dev/null | grep -F "'$name'" | grep -oP 'client \K\d+' | head -1
}

# Connect one ALSA client's port into another's, clients by exact name.
connect_alsa_by_names() {
  local src_name="$1"
  local src_port="$2"
  local dest_name="$3"
  local dest_port="$4"
  local label="$5"
  local src dest
  src=$(find_alsa_client_exact "$src_name")
  dest=$(find_alsa_client_exact "$dest_name")
  if [[ -z "$src" || -z "$dest" ]]; then
    [[ -z "$src" ]] && echo "  Warning: ALSA client '$src_name' not found -- is its program running?"
    [[ -z "$dest" ]] && echo "  Warning: ALSA client '$dest_name' not found -- is make-2-midi-inputs.sh running?"
    return 1
  fi
  if aconnect "$src:$src_port" "$dest:$dest_port" 2>/tmp/connect-midi-aconnect.err; then
    echo "  Connected: $label"
  elif grep -q 'Connection is already subscribed' /tmp/connect-midi-aconnect.err; then
    echo "  Already connected: $label"
  else
    cat /tmp/connect-midi-aconnect.err
    echo "  To do it by hand: aconnect $src:$src_port $dest:$dest_port"
    return 1
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
    echo "  Could not connect $label."
    echo "  (Is the source's program running? Is Reaper up? pw-link -o / pw-link -i list the live ports.)"
    echo "  To do it by hand once both ends exist: pw-link \"$source\" \"$dest\""
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
    echo "  To do it by hand: pw-link \"$reaper_l\" <sink>:playback_FL ; pw-link \"$reaper_r\" <sink>:playback_FR"
    # return 1 so the caller's failed-steps summary reports the audio as unconnected.
    return 1
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
    echo "  To do it by hand: pw-link $reaper_l ${sink}:playback_FL"
    return 1
  fi

  if pw-link "$reaper_r" "${sink}:playback_FR" 2>/tmp/connect-midi-pw-link-audio.err; then
    echo "  Connected: $reaper_r -> ${sink_label}:playback_FR"
  elif pw-link -l | grep -A20 -Fx "$reaper_r" | grep -Fxq "  |-> ${sink}:playback_FR"; then
    echo "  Already connected: $reaper_r -> ${sink_label}:playback_FR"
  else
    cat /tmp/connect-midi-pw-link-audio.err
    echo "  To do it by hand: pw-link $reaper_r ${sink}:playback_FR"
    return 1
  fi
}
