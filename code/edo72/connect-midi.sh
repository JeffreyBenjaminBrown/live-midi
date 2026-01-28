# Connect MIDI ports for edo72 setup
# Uses aconnect for ALSA sequencer (keyboard->edo72)
# Uses pw-link for JACK MIDI (edo72->Reaper)

set -e

echo "=== ALSA Sequencer (keyboard -> edo72) ==="

casio=$(aconnect -l | grep -B1 'CASIO USB-MIDI MIDI 1' | head -1 | grep -oP 'client \K\d+')
edo72_in=$(aconnect -l | grep 'edo72-in' | grep -oP 'client \K\d+')

echo "  CASIO: $casio, edo72-in: $edo72_in"

if [[ -n "$casio" && -n "$edo72_in" ]]; then
  aconnect "$casio:0" "$edo72_in:0" && echo "  Connected: keyboard -> edo72"
else
  echo "  Warning: Could not find ALSA ports"
fi

echo ""
echo "=== JACK MIDI (edo72 -> Reaper) ==="

# edo72-out -> REAPER MIDI Input 1
pw-link "Midi-Bridge:edo72-out:(capture_0) out" "REAPER:MIDI Input 1" \
  && echo "  Connected: edo72-out -> REAPER MIDI Input 1"

echo ""
echo "Done!"
