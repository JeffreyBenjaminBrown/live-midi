#!/usr/bin/env bash
# Step 9: verify audio stack end-to-end, from inside the Docker container.
#
# Compiles code/rust/synth/triangle_chord.rs (already on disk) and
# plays a 220 Hz triangle wave for ~2 seconds via pw-play. If you
# hear a tone, sound output works from the container; we can build
# on it.
#
# Run this INSIDE the container (docker.sh starts one that shares
# /run/user/1000/pipewire-0 and /dev/snd, so audio works).
#
# Output: ./09-sound-test.out

set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/09-sound-test.out"
SYNTH_DIR="/home/ubuntu/code/rust/synth"

exec 3>&1
progress() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&3; }

progress "starting; full log -> $OUT"

{
  echo "================================================================"
  echo "=== SOUND TEST    $(date -Iseconds)"
  echo "================================================================"

  progress "compiling triangle_chord.rs (rustc -O; ~10s cold)"
  cd "$SYNTH_DIR"
  rustc -O triangle_chord.rs -o /tmp/triangle_chord 2>&1
  ls -la /tmp/triangle_chord 2>&1

  progress "playing 220 Hz triangle (~2s) — you should hear a tone"
  /tmp/triangle_chord 220 1 2>&1

  echo "=== END SOUND TEST ==="
} >"$OUT" 2>&1

progress "done"
echo "Did you hear a tone? Tell Claude."
