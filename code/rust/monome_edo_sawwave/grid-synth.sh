#!/usr/bin/env bash
# Build + run the config-driven sawwave runtime.
#
# Runs forever until Ctrl-C. Stdout/stderr go to both the terminal
# (so you can watch press events live) and ./grid-synth.out.
#
# Prerequisite: steps 9 and 10 both worked.
#
# Usage:
#   bash ./grid-synth.sh
#
# Audio buffer size can be set via the BUF env var (affects latency).
# "default" (or unset) → cpal's default; positive integer → fixed size.
#   BUF=256  bash ./grid-synth.sh
#   BUF=1024 bash ./grid-synth.sh
#   BUF=default bash ./grid-synth.sh   # explicit, same as unset

set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/grid-synth.out"
PROJECT="$(cd "$HERE/../../.." && pwd)"

exec 3>&1
progress() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&3; }

progress "cargo build --release --bin midi_pulse (first run fetches cpal + deps)"
(cd "$PROJECT" && cargo build --release --bin midi_pulse 2>&1) | tee "$OUT"
build_status=${PIPESTATUS[0]}
if [ "$build_status" -ne 0 ]; then
  progress "cargo build failed; not running midi_pulse"
  exit "$build_status"
fi

progress "running midi_pulse monome-edo-sawwave; press keys on the grid; Ctrl-C to quit"
echo
echo "### MIDI PULSE SAWWAVE LIVE - press keys on the grid; Ctrl-C to quit ###"
echo

stdbuf -oL -eL "$PROJECT/target/release/midi_pulse" monome-edo-sawwave 2>&1 | tee -a "$OUT"
