#!/usr/bin/env bash

# Connect Reaper's stereo output to whichever audio sink is currently
# available, preferring Headphones over Speaker.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../rust/connect-midi-lib.sh"

connect_reaper_to_primary_audio_out

echo "Done."
