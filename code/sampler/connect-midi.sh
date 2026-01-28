# Connect MIDI ports for sampler setup
# Uses aconnect for ALSA sequencer (keyboard->sampler)
# Uses pw-link for JACK MIDI (sampler->Reaper)

set -e

echo "=== ALSA Sequencer (keyboard -> sampler) ==="

casio=$(aconnect -l | grep -B1 'CASIO USB-MIDI MIDI 1' | head -1 | grep -oP 'client \K\d+')
sampler_in=$(aconnect -l | grep 'sampler-in' | grep -oP 'client \K\d+')

echo "  CASIO: $casio, sampler-in: $sampler_in"

if [[ -n "$casio" && -n "$sampler_in" ]]; then
  aconnect "$casio:0" "$sampler_in:0" && echo "  Connected: keyboard -> sampler"
else
  echo "  Warning: Could not find ALSA ports"
fi

echo ""
echo "=== JACK MIDI (sampler -> Reaper) ==="

# sampler-immediate -> REAPER MIDI Input 1
pw-link "Midi-Bridge:sampler-immediate:(capture_0) immediate-out" "REAPER:MIDI Input 1" \
  && echo "  Connected: immediate-out -> REAPER MIDI Input 1"

# sampler-sample -> REAPER MIDI Input 4
pw-link "Midi-Bridge:sampler-sample:(capture_0) sample-out" "REAPER:MIDI Input 4" \
  && echo "  Connected: sample-out -> REAPER MIDI Input 4"

echo ""
echo "Done!"
