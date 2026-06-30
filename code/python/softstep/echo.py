#!/usr/bin/env python3
"""Human-readable echo of everything a Keith McMillen SoftStep sends.

The SoftStep enumerates as the ALSA client "SSCOM" with two ports:
  20:0  "SSCOM MIDI 1"  -- performance MIDI (what we listen to)
  20:1  "SSCOM MIDI 2"  -- editor / firmware COM channel (normally silent)

We don't have python-rtmidi/mido in this container, so rather than fight C
bindings we drive `aseqdump` (line-buffered) as a subprocess and translate its
text into a friendlier form. aseqdump ignores SIGTERM, so we SIGKILL it on exit.

Usage:
    python3 echo.py                 # listen on 20:0 (the SSCOM performance port)
    python3 echo.py 20:0 20:1       # listen on several ports at once
    python3 echo.py --raw           # also print the underlying aseqdump line

What you'll see depends entirely on which on-device preset is loaded. The
factory "preset switcher" makes each big pedal send a Program Change equal to
its printed number and the nav arrows send nothing. A "notes" preset makes the
pedals send Note On/Off (with velocity) plus continuous key-pressure CCs.
"""

import re
import select
import signal
import subprocess
import sys

# --- SoftStep knowledge -----------------------------------------------------

# This device's loaded preset-set is a pure Program-Change bank switcher. The
# 13 on-device presets (display 0..12) and the 10 pedals combine as:
#     Program Change = preset * 10 + pedal_label
# so pedal labeled P on preset K sends PC (10*K + P), giving 130 distinct
# momentary triggers (PC 0..129) -- but no velocity, pressure, or note data in
# ANY preset. The nav arrows only scroll presets locally (no MIDI).
def program_change_origin(program):
    preset, pedal = divmod(program, 10)
    return f"preset {preset}, pedal labeled {pedal}"

# KMI documents the 10 keys' continuous pressure as CC 110..119 (key 1..10).
def cc_origin(controller):
    if 110 <= controller <= 119:
        return f"key {controller - 110 + 1} pressure"
    return f"controller {controller}"

# When a preset sends notes, we don't yet know the note->pedal map for *this*
# device, so we just report the note number. Fill this in once we learn it.
NOTE_TO_PEDAL = {}  # e.g. {60: "big pedal 1"}

def note_origin(note):
    if note in NOTE_TO_PEDAL:
        return NOTE_TO_PEDAL[note]
    return f"note {note}"


# --- aseqdump line parsing --------------------------------------------------
# aseqdump data lines look like:
#   " 20:0   Note on                 0, note 60, velocity 100"
#   " 20:0   Program change          0, program 5"
#   " 20:0   Control change          0, controller 110, value 64"
#   " 20:0   Channel aftertouch      0, value 80"
#   " 20:0   Pitchbend               0, value 1234"

LINE = re.compile(r"^\s*(\d+:\d+)\s+(.+?)\s{2,}(.*)$")

def fields(data):
    """Parse 'controller 110, value 64' -> {'controller': 110, 'value': 64}."""
    out = {}
    for part in data.split(","):
        part = part.strip()
        m = re.match(r"([a-z ]+?)\s+(-?\d+)$", part)
        if m:
            out[m.group(1).strip()] = int(m.group(2))
    return out

def render(port, event, data):
    f = fields(data)
    ev = event.lower()
    ch = f.get("note", None)  # placeholder; real channel parsed below

    # aseqdump puts the channel as the first bare number; grab it.
    chan = None
    first = data.split(",")[0].strip()
    if first.isdigit() or (first.startswith("-") and first[1:].isdigit()):
        chan = int(first) + 1  # show 1-based MIDI channel

    lines = [f"  port: {port}"]
    if chan is not None:
        lines.append(f"  channel: {chan}")

    if "note on" in ev and f.get("velocity", 0) > 0:
        lines = [f"  event origin: {note_origin(f.get('note'))}",
                 "  event type: note on",
                 f"  velocity: {f.get('velocity')}"] + lines[1:]
    elif "note off" in ev or ("note on" in ev and f.get("velocity", 1) == 0):
        lines = [f"  event origin: {note_origin(f.get('note'))}",
                 "  event type: note off"] + lines[1:]
    elif "program change" in ev:
        lines = [f"  event origin: {program_change_origin(f.get('program'))}",
                 "  event type: program change",
                 f"  program number: {f.get('program')}"] + lines[1:]
    elif "control change" in ev:
        lines = [f"  event origin: {cc_origin(f.get('controller'))}",
                 "  event type: control change (continuous)",
                 f"  value: {f.get('value')}"] + lines[1:]
    elif "aftertouch" in ev:  # channel or poly pressure
        origin = note_origin(f["note"]) if "note" in f else "whole pedalboard"
        lines = [f"  event origin: {origin}",
                 "  event type: aftertouch (pressure)",
                 f"  value: {f.get('value')}"] + lines[1:]
    elif "pitchbend" in ev:
        lines = ["  event origin: pitch bend",
                 "  event type: pitch bend",
                 f"  value: {f.get('value')}"] + lines[1:]
    else:
        lines = [f"  event type: {event}", f"  data: {data}"] + lines[1:]

    return "\n".join(lines)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    show_raw = "--raw" in sys.argv
    ports = args or ["20:0"]

    cmd = ["stdbuf", "-oL", "aseqdump"]
    for p in ports:
        cmd += ["-p", p]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, bufsize=1)

    print(f"listening on {', '.join(ports)} (Ctrl-C to stop)\n", flush=True)
    try:
        for raw in proc.stdout:
            raw = raw.rstrip("\n")
            m = LINE.match(raw)
            if not m:
                continue  # banners / header
            port, event, data = m.groups()
            if show_raw:
                print(f"  [raw] {raw.strip()}")
            print(render(port, event, data) + "\n", flush=True)
    except KeyboardInterrupt:
        pass
    finally:
        proc.send_signal(signal.SIGKILL)  # aseqdump ignores SIGTERM


if __name__ == "__main__":
    main()
