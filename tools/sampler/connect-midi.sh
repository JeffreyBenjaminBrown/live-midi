# Connect MIDI ports for sampler setup
# Uses aconnect for ALSA sequencer (keyboard->sampler)
# Uses pw-link for JACK MIDI (sampler->Reaper)

set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../connect-midi-lib.sh"

connect_keyboard_to_alsa_client "sampler" "sampler-in"

echo ""
echo "=== JACK MIDI (sampler -> Reaper) ==="

# sampler-immediate -> REAPER MIDI Input 1
connect_pipewire_midi \
  "Midi-Bridge:sampler-immediate:(capture_0) immediate-out" \
  "REAPER:MIDI Input 1" \
  "immediate-out -> REAPER MIDI Input 1"

# sampler-sample -> REAPER MIDI Input 4
connect_pipewire_midi \
  "Midi-Bridge:sampler-sample:(capture_0) sample-out" \
  "REAPER:MIDI Input 4" \
  "sample-out -> REAPER MIDI Input 4"

echo ""
echo "Done!"
