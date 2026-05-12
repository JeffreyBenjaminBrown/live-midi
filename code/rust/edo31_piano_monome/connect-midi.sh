# Connect MIDI ports for edo31_piano_monome setup
# Uses aconnect for ALSA sequencer (keyboard->edo31_piano_monome)
# Uses pw-link for JACK MIDI (edo31_piano_monome->Reaper)

set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../connect-midi-lib.sh"

connect_keyboard_to_alsa_client "edo31_piano_monome" "edo31_piano_monome-in"

echo ""
echo "=== JACK MIDI (edo31_piano_monome -> Reaper) ==="

connect_pipewire_midi \
  "Midi-Bridge:edo31_piano_monome-out:(capture_0) out" \
  "REAPER:MIDI Input 1" \
  "edo31_piano_monome-out -> REAPER MIDI Input 1"

echo ""
echo "Done!"
