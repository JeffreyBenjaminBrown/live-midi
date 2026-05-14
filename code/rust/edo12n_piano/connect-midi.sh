# Connect MIDI ports for edo12n_piano setup
# Uses aconnect for ALSA sequencer (keyboard->edo12n_piano)
# Uses pw-link for JACK MIDI (edo12n_piano->Reaper)

set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../connect-midi-lib.sh"

connect_keyboard_to_alsa_client "edo12n_piano" "edo12n_piano-in"

echo ""
echo "=== JACK MIDI (edo12n_piano -> Reaper) ==="

# edo12n_piano-out -> REAPER MIDI Input 1
connect_pipewire_midi \
  "Midi-Bridge:edo12n_piano-out:(capture_0) out" \
  "REAPER:MIDI Input 1" \
  "edo12n_piano-out -> REAPER MIDI Input 1"

echo ""
echo "Done!"
