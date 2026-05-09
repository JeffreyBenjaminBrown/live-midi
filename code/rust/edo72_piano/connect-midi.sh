# Connect MIDI ports for edo72_piano setup
# Uses aconnect for ALSA sequencer (keyboard->edo72_piano)
# Uses pw-link for JACK MIDI (edo72_piano->Reaper)

set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../connect-midi-lib.sh"

connect_keyboard_to_alsa_client "edo72_piano" "edo72_piano-in"

echo ""
echo "=== JACK MIDI (edo72_piano -> Reaper) ==="

# edo72_piano-out -> REAPER MIDI Input 1
connect_pipewire_midi \
  "Midi-Bridge:edo72_piano-out:(capture_0) out" \
  "REAPER:MIDI Input 1" \
  "edo72_piano-out -> REAPER MIDI Input 1"

echo ""
echo "Done!"
