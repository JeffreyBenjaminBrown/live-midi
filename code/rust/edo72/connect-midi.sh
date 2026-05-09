# Connect MIDI ports for edo72 setup
# Uses aconnect for ALSA sequencer (keyboard->edo72)
# Uses pw-link for JACK MIDI (edo72->Reaper)

set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../connect-midi-lib.sh"

connect_keyboard_to_alsa_client "edo72" "edo72-in"

echo ""
echo "=== JACK MIDI (edo72 -> Reaper) ==="

# edo72-out -> REAPER MIDI Input 1
connect_pipewire_midi \
  "Midi-Bridge:edo72-out:(capture_0) out" \
  "REAPER:MIDI Input 1" \
  "edo72-out -> REAPER MIDI Input 1"

echo ""
echo "Done!"
