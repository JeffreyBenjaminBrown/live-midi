#!/usr/bin/env bash

# Connect Reaper's stereo output to whichever audio sink is
# currently available:
# - Headphones when cable is plugged in
# - Speaker when it's not
# Run after plugging/unplugging the audio cable, or after starting Reaper.

set -e

REAPER_L="REAPER:out1"
REAPER_R="REAPER:out2"

SINK_PREFIX="alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic"
HEADPHONES="${SINK_PREFIX}.HiFi__Headphones__sink"
SPEAKER="${SINK_PREFIX}.HiFi__Speaker__sink"

# Find whichever sink exists right now.
if pw-link -i 2>/dev/null | grep -q "${HEADPHONES}:playback_FL"; then
  SINK="$HEADPHONES"
  echo "Found: Headphones"
elif pw-link -i 2>/dev/null | grep -q "${SPEAKER}:playback_FL"; then
  SINK="$SPEAKER"
  echo "Found: Speaker"
else
  echo "Error: neither Headphones nor Speaker sink found."
  echo "Available sinks:"
  pw-link -i 2>/dev/null | grep 'sink:playback' | sort -u
  exit 1
fi

# Disconnect any existing Reaper output links (ignore errors
# if they don't exist).
for port in $(pw-link -i 2>/dev/null | grep 'sink:playback'); do
  pw-link -d "$REAPER_L" "$port" 2>/dev/null || true
  pw-link -d "$REAPER_R" "$port" 2>/dev/null || true
done

# Connect Reaper to the active sink.
pw-link "$REAPER_L" "${SINK}:playback_FL" && echo "  $REAPER_L -> ${SINK##*.}:playback_FL"
pw-link "$REAPER_R" "${SINK}:playback_FR" && echo "  $REAPER_R -> ${SINK##*.}:playback_FR"

echo "Done."
