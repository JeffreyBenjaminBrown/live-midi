#!/usr/bin/env bash
# Step 11: build + run the grid_synth.
#
# Runs forever until Ctrl-C. Stdout/stderr go to both the terminal
# (so you can watch press events live) and ./grid-synth.out.
#
# Prerequisite: steps 9 and 10 both worked.
#
# Usage (args are passed through to the binary):
#   bash ./grid-synth.sh                         # default: fund=220 edo=46 x=9 y=1
#   bash ./grid-synth.sh 440                     # 440 Hz root, other defaults
#   bash ./grid-synth.sh 220 72                  # 72-EDO, default steps
#   bash ./grid-synth.sh 220 72 7 1              # 72-EDO, x=+7 steps, y=+1 step
#   bash ./grid-synth.sh 220 12 7 5              # vanilla 12-TET-ish
#   bash ./grid-synth.sh 220 53 31 9             # 53-EDO, fifth + whole tone
#
# Audio buffer size can be set via the BUF env var (affects latency).
# "default" (or unset) → cpal's default; positive integer → fixed size.
#   BUF=256  bash ./grid-synth.sh
#   BUF=1024 bash ./grid-synth.sh
#   BUF=default bash ./grid-synth.sh   # explicit, same as unset

set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/grid-synth.out"
PROJECT="$HERE/grid_synth"
TARGET_DIR="${CARGO_TARGET_DIR:-/home/ubuntu/target}"

exec 3>&1
progress() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&3; }

progress "cargo build --release (first run fetches cpal + deps)"
(cd "$PROJECT" && cargo build --release 2>&1) | tee "$OUT"
build_status=${PIPESTATUS[0]}
if [ "$build_status" -ne 0 ]; then
  progress "cargo build failed; not running grid_synth"
  exit "$build_status"
fi

progress "running grid_synth; press keys on the grid; Ctrl-C to quit"
echo
echo "### GRID SYNTH LIVE — press keys on the grid; Ctrl-C to quit ###"
echo

stdbuf -oL -eL "$TARGET_DIR/release/grid_synth" "$@" 2>&1 | tee -a "$OUT"
