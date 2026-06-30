#!/usr/bin/env python3
"""Send SoftStep mode-control SysEx (standalone <-> tether/hosted, backlight).

  *** STATUS: on the device tested (2026-06-30) NONE of these commands had any
  observable effect -- the SoftStep ignored every one (no mode change, no
  backlight blink, no query reply), even though aseqsend demonstrably delivers
  MIDI through the sequencer (a Note looped back through Midi Through fine).
  Most likely a firmware-vs-editor-version mismatch and/or a container write-path
  issue. Kept as a faithful record of the protocol for a future attempt. See
  learnings/keith-mcmillen-softstep.org. ***

The bytes are lifted verbatim from KMI/Muse-Kinetics' own open-source editor
(github.com/Muse-Kinetics/softstep_editor, shared/sysexmessages.h), with the
#define constants substituted: SysEx start F0 / stop F7, KMI manufacturer id
00 01 5F, magic 7A.

Usage:
    python3 ssmode.py tether       20:0     # try to enter hosted/streaming mode
    python3 ssmode.py standalone   20:0     # restore normal preset mode
    python3 ssmode.py backlight_on 20:0     # visible reachability test
    python3 ssmode.py query        20:0     # firmware query (expect a reply)

The 4-message mode sequences and their order mirror the editor's
SysExComposer::slotHostedOnOff(). The firmware "requires delay between
messages", so we sleep 100 ms between sends.
"""
import subprocess, sys, time

# name -> raw bytes (hex), parsed from the editor's sysexmessages.h
MSG = {
    "_fw_query_syx_softstep":        "f0001b487a01" + "00"*55 + "010000000004400030f7",
    "_fw_standalone_off":            "f000015f7a010000000000000000000000010009000b2b3a001003000000000000000050070000000002f7",
    "_fw_standalone_on":             "f000015f7a010000000000000000000000010009000b2b3a001003010000000000000068660000000000f7",
    "_fw_nav_standalone_off":        "f000015f7a010000000000000000000000010009000b2b3a0010090000000000000000417b0000000000f7",
    "_fw_nav_standalone_on":         "f000015f7a010000000000000000000000010009000b2b3a0010090100000000000000791a0000000002f7",
    "_fw_tether_on":                 "f000015f7a010000000000000000000000010009000b2b3a00100401000000000000002f7e0000000002f7",
    "_fw_tether_off":                "f000015f7a010000000000000000000000010009000b2b3a0010040000000000000000171f0000000000f7",
    "_fw_scenechange_on_persist":    "f000015f7a010000000000000000000000010009000b2b3a00100801010000000000007b690000000002f7",
    "_fw_nav_standalone_on_persist": "f000015f7a010000000000000000000000010009000b2b3a00100901010000000000003c3a0000000006f7",
    "_backlight_on":                 "f000015f7a01000000000000000000000001000400050825012000007b2c0000000cf7",
    "_backlight_off":                "f000015f7a01000000000000000000000001000400050825002000004c1c0000000cf7",
}

# Sequences, verbatim from sysexcomposer.cpp slotHostedOnOff() (order matters).
SEQUENCES = {
    "tether":        ["_fw_standalone_off", "_fw_scenechange_on_persist",
                      "_fw_nav_standalone_on_persist", "_fw_tether_on"],
    "standalone":    ["_fw_tether_off", "_fw_standalone_on",
                      "_fw_scenechange_on_persist", "_fw_nav_standalone_on_persist"],
    "backlight_on":  ["_backlight_on"],
    "backlight_off": ["_backlight_off"],
    "query":         ["_fw_query_syx_softstep"],
}

def send(name, port):
    data = bytes.fromhex(MSG[name])
    open("/tmp/_ss_msg.syx", "wb").write(data)
    r = subprocess.run(["aseqsend", "-v", "-p", port, "-s", "/tmp/_ss_msg.syx"],
                       capture_output=True, text=True)
    time.sleep(0.1)
    return f"{len(data)}B -> {(r.stdout + r.stderr).strip()}"

def main():
    seq  = sys.argv[1] if len(sys.argv) > 1 else "tether"
    port = sys.argv[2] if len(sys.argv) > 2 else "20:0"
    print(f"sending '{seq}' to {port}:")
    for name in SEQUENCES[seq]:
        print(f"  {name:34s} {send(name, port)}")

if __name__ == "__main__":
    main()
