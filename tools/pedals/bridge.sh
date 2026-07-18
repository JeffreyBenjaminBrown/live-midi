#!/usr/bin/env bash
# Run the MPC-20 pedal bridge (bridge.py) on the HOST, wrapped in the nix shell
# that has its dependencies (bridge.nix). See README.org "Run the bridge".
#
# Two traps this wrapper exists to avoid:
# - nix-shell's --run takes ONE argument. An unquoted `--run sudo python3
#   bridge.py` runs only `sudo`, and nix-shell then treats bridge.py as a
#   script of its own -- bash executes the Python docstring and prints
#   nonsense like "aconnect: command not found".
# - sudo resets PATH, so `sudo python3` inside the shell can silently pick the
#   bare system python (no pyusb). We resolve python3 to its /nix/store path
#   INSIDE the shell first, and hand sudo that absolute path.
#
# Root is needed to detach the MPC-20's USB interface from snd-usb-audio.
set -euo pipefail
cd "$(dirname "$0")"
exec nix-shell bridge.nix --run 'exec sudo "$(command -v python3)" bridge.py'
