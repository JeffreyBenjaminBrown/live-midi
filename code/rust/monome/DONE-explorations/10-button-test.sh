#!/usr/bin/env bash
# Step 10: verify we can receive button-press events, from inside the
# Docker container.
#
# No oscsend/oscdump inside the container (liblo tools not installed),
# so we use a small Rust program instead (./button_test/). It also
# serves as scaffolding for step 11.
#
# The program:
#   1. Binds UDP :9000 in the container. Because docker.sh uses
#      --network host, that's the same :9000 on the host.
#   2. Asks serialoscd (on the host, :12002) to list devices.
#   3. Registers itself with the first device via /sys/host, /sys/port,
#      /sys/prefix (prefix = /256-1-cable).
#   4. Prints every OSC message it receives for 15 seconds.
#
# Press buttons on the grid during those 15 seconds. Expected lines:
#     press   x= 3 y= 5  (/256-1-cable/grid/key)
#     release x= 3 y= 5  (/256-1-cable/grid/key)
#
# Output: ./10-button-test.out

set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/10-button-test.out"
PROJECT="$HERE/button_test"
TARGET_DIR="${CARGO_TARGET_DIR:-/home/ubuntu/target}"

exec 3>&1
progress() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&3; }

progress "starting; full log -> $OUT"

{
  echo "================================================================"
  echo "=== BUTTON TEST    $(date -Iseconds)"
  echo "================================================================"
} >"$OUT"

progress "cargo build --release (first run fetches rosc; ~30-60 s)"
(cd "$PROJECT" && cargo build --release 2>&1) | tee -a "$OUT"

progress "press buttons on the grid; listening for 15 s on UDP 9000"
echo
echo "### PRESS SOME BUTTONS ON THE GRID NOW (15 seconds) ###"
echo

# Run the binary directly (faster) and tee so you can watch events
# arrive live AND have them captured in the log.
"$TARGET_DIR/release/button_test" 2>&1 | tee -a "$OUT"

{
  echo
  echo "=== END BUTTON TEST ==="
} >>"$OUT"

progress "done; full log in $OUT"
