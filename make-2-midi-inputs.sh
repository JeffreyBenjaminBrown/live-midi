#!/usr/bin/env bash
# The ONE host-side setup for the edo-un12 piano rigs. Run this FIRST, then
# start Reaper, then the synth (docker), then tools/connect-midi.sh -- that
# order, so nothing depends on anything that starts later.
#
# THE KEY FACT (verified 2026-07-25 via pw-dump): Reaper's Linux MIDI-device
# list shows only ports flagged `port.physical = True`, and only KERNEL ALSA
# clients get that flag -- every user-space port (the rig runtime's, rtmidi's,
# the pedal bridge's) is invisible to it, no matter the naming. So the two
# Reaper inputs must be KERNEL thru ports: snd-seq-dummy loaded with ports=2,
# giving "Midi Through Port-0" (= 58-edo 1) and "Midi Through Port-1"
# (= 58-edo 2). Reaper scans devices only at startup, and kernel ports also
# survive runtime restarts -- both lifetime problems solved at once.
#
# What this script does:
# 1. Ensures Midi Through has TWO ports (sudo modprobe -r snd_seq_dummy &&
#    modprobe snd_seq_dummy ports=2). To make that permanent on NixOS instead:
#      boot.extraModprobeConfig = "options snd-seq-dummy ports=2";
#    after which this step self-skips.
# 2. Runs a2jmidid (exports the ports under the a2j naming Reaper lists).
# 3. Runs the MPC-20 pedal bridge (EX-P sustain; sudo asked up front;
#    harmless with the MPC-20 unplugged -- it retries).
#
# In Reaper, ONCE (persists): Preferences > MIDI Devices -> enable inputs
# "Midi Through Port-0" (58-edo 1, LEFT keyboard) and "Midi Through Port-1"
# (58-edo 2). A stale "Port-1 (not found)" entry from an old setup springs
# back to life by itself once the port exists.
#
# tools/connect-midi.sh wires the runtime in: rig out -> Port-0, out-2 ->
# Port-1 (it detects Port-1 and takes this route automatically).
#
# Each piece is skipped if already up, so rerunning is safe. Ctrl-C stops the
# daemons this run started (the kernel ports stay -- they are free).
set -euo pipefail
cd "$(dirname "$0")"

have_seq_port() { aconnect -l 2>/dev/null | grep -qF "$1"; }

command -v a2jmidid >/dev/null || { echo "a2jmidid not found on PATH. On NixOS: nix-shell -p a2jmidid, or add it to the system config." >&2; exit 1; }

NEED_PORTS=
have_seq_port "Midi Through Port-1" || NEED_PORTS=1
NEED_BRIDGE=1
have_seq_port "MPC-20 pedals" && NEED_BRIDGE=

# sudo up front (module reload now, pedal bridge in the background later).
if [[ -n "$NEED_PORTS" || -n "$NEED_BRIDGE" ]]; then
  echo "(the sudo password is for the Midi Through reload and the pedal bridge)"
  sudo -v
fi

# --- 1. two kernel Midi Through ports --------------------------------------
if [[ -z "$NEED_PORTS" ]]; then
  echo "Midi Through Port-1 already exists -- skipping the module reload."
else
  echo "reloading snd-seq-dummy with ports=2..."
  if sudo modprobe -r snd_seq_dummy && sudo modprobe snd_seq_dummy ports=2; then
    if have_seq_port "Midi Through Port-1"; then
      echo "Midi Through Port-0 and Port-1 are up."
    else
      echo "Warning: reload succeeded but Port-1 is not visible; check aconnect -l." >&2
    fi
  else
    echo "Warning: could not reload snd-seq-dummy (in use?). Options:" >&2
    echo "  - close subscribers (a2jmidid, the rig runtime) and rerun, or" >&2
    echo "  - make it permanent in the NixOS config and reboot:" >&2
    echo "      boot.extraModprobeConfig = \"options snd-seq-dummy ports=2\";" >&2
  fi
fi

# Everything backgrounded below shares this script's process group, so Ctrl-C
# from the terminal reaches every piece directly -- including the bridge's
# root-owned python, which a plain kill from this user could not stop. The
# EXIT trap is a fallback for the user-owned pieces.
PIDS=()
trap '((${#PIDS[@]})) && kill "${PIDS[@]}" 2>/dev/null || true' EXIT

# --- 2. a2jmidid ------------------------------------------------------------
if pw-link -o 2>/dev/null | grep -q '^a2j:'; then
  echo "a2jmidid already running (a2j ports present) -- skipping."
else
  echo "starting a2jmidid (ALSA -> JACK export, the naming Reaper lists)..."
  a2jmidid &
  PIDS+=($!)
fi

# --- 3. the pedal bridge ----------------------------------------------------
if [[ -z "$NEED_BRIDGE" ]]; then
  echo "pedal bridge already running ('MPC-20 pedals' exists) -- skipping."
else
  command -v nix-shell >/dev/null || { echo "Warning: nix-shell not found; skipping the pedal bridge (EX-P sustain off)." >&2; }
  if command -v nix-shell >/dev/null; then
    echo "starting the MPC-20 pedal bridge (EX-P sustain)..."
    tools/pedals/bridge.sh &
    PIDS+=($!)
  fi
fi

echo ""
echo "Host setup is up. Now, in order:"
echo "  1. start Reaper  (once, in Preferences > MIDI Devices: enable inputs"
echo "     'Midi Through Port-0' = 58-edo 1/left and 'Midi Through Port-1' ="
echo "     58-edo 2/right -- that choice persists)"
echo "  2. start the synth in docker:"
echo "       cargo run --bin midi_pulse -- edo-un12_58-8-1_snap_save-scales"
echo "  3. run ./tools/connect-midi.sh <rig>  (host or container, either works)"
if ((${#PIDS[@]})); then
  echo "Leave this terminal running; Ctrl-C stops the daemons it started."
  wait
else
  echo "(Every daemon was already running elsewhere; nothing for this run to own.)"
fi
