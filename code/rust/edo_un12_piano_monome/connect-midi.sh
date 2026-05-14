# Connect MIDI ports for edo_un12_piano_monome setup
# Uses aconnect for ALSA sequencer (keyboard->edo_un12_piano_monome)
# Uses pw-link for JACK MIDI (edo_un12_piano_monome->Reaper)
# Also routes REAPER audio to the best available PipeWire sink.

set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../connect-midi-lib.sh"

connect_keyboard_to_alsa_client "edo_un12_piano_monome" "edo_un12_piano_monome-in"

echo ""
echo "=== JACK MIDI (edo_un12_piano_monome -> Reaper) ==="

connect_pipewire_midi \
  "Midi-Bridge:edo_un12_piano_monome-out:(capture_0) out" \
  "REAPER:MIDI Input 1" \
  "edo_un12_piano_monome-out -> REAPER MIDI Input 1"

echo ""
echo "=== JACK audio (Reaper -> output) ==="

find_audio_sink_port() {
  local channel="$1"
  local pattern

  for pattern in \
    'HiFi__Headphones__sink' \
    'HiFi__Speaker__sink' \
    'Headphones' \
    'Speaker'
  do
    pw-link -i |
      grep 'alsa_output.*:playback_'"$channel"'$' |
      grep "$pattern" |
      head -1
  done | head -1
}

disconnect_reaper_audio_output() {
  local source="$1"
  pw-link -l |
    awk -v source="$source" '
      $0 == source { in_source = 1; next }
      /^[^[:space:]]/ { in_source = 0 }
      in_source && /[|]->/ { sub(/^[[:space:]]*[|]->[[:space:]]*/, ""); print }
    ' |
    while IFS= read -r dest; do
      pw-link -d "$source" "$dest" 2>/dev/null || true
    done
}

left_sink="$(find_audio_sink_port FL)"
right_sink="$(find_audio_sink_port FR)"

if [[ -n "$left_sink" && -n "$right_sink" ]]; then
  disconnect_reaper_audio_output "REAPER:out1"
  disconnect_reaper_audio_output "REAPER:out2"
  pw-link "REAPER:out1" "$left_sink" && echo "  Connected: REAPER out1 -> $left_sink"
  pw-link "REAPER:out2" "$right_sink" && echo "  Connected: REAPER out2 -> $right_sink"
else
  echo "  Warning: no Headphones/Speaker PipeWire sink is currently available."
  echo "  Available playback sink ports:"
  pw-link -i | grep 'alsa_output.*:playback_' | sed 's/^/    /' || true
fi

echo ""
echo "Done!"
