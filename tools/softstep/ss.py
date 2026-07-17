#!/usr/bin/env python3
"""Shared SoftStep (KMSS) helpers: the *working* rawmidi send path + mode control,
now MULTI-DEVICE aware (we have two SoftSteps on this rig).

KEY DISCOVERY (2026-06-30): the prior write-ups concluded "we can't send to the
device." That was tested only through the ALSA *sequencer* (aseqsend), which the
SoftStep ignores. The ALSA *rawmidi* path WORKS: `amidi -p hw:X,0,0` reaches the
device's USB-MIDI IN. Proof -- the firmware query gets a reply, and the
standalone<->tether mode switch takes visible effect (the device starts emitting
Control Change data, which standalone Program-Change mode never does).

So everything here drives `amidi` on the rawmidi port (no python-midi libs needed).

TWO SOFTSTEPS (2026-07-15): there are now two units, and they enumerate DIFFERENTLY --
this is how code tells them apart (see `list_softsteps`):

  original unit  ALSA card "SSCOM"    (KESUMO, LLC)         ports "SSCOM MIDI 1/2"
                 control/data port = "SSCOM MIDI 1"         (older firmware, build 91)
  newer unit     ALSA card "SoftStep" (KMI Music, Inc.)     ports "SoftStep Control
                 control/data port = "SoftStep Control Surface"  Surface" / "TRS MIDI
                                                             Out" / "CV Out"

The ALSA CARD NUMBER is assigned at plug-in and is NOT stable (the original unit was
`hw:1,...` when there was only one; with both plugged in it is now `hw:3,...`). So we
match by the port NAME, never by a hardcoded `hw:1,0,0`. Both units accept the SAME
tether/standalone SysEx and stream sensors as Control Change ch0 in the SAME CC range
(40..79 pads, 80..83 nav, 86 expression jack) -- verified on 2026-07-15.

Mode switching is REVERSIBLE and does NOT touch firmware -- it cannot brick the
device (only an interrupted firmware *flash* can; we never flash). See
learnings/keith-mcmillen-softstep.org.
"""
import re
import subprocess
import sys
import time

# --- control SysEx, bytes verbatim from KMI's open-source editor -------------
# github.com/Muse-Kinetics/softstep_editor. Mode/backlight commands use KMI
# manufacturer id 00 01 5F + magic 7A; the firmware query uses id 00 1B 48.
IDENTITY_REQUEST = "F0 7E 7F 06 01 F7"                      # universal MIDI identity request
FW_QUERY = "f0 00 1b 48 7a 01 " + "00 " * 55 + "01 00 00 00 00 04 40 00 30 f7"

STANDALONE_OFF = "f000015f7a010000000000000000000000010009000b2b3a001003000000000000000050070000000002f7"
STANDALONE_ON  = "f000015f7a010000000000000000000000010009000b2b3a001003010000000000000068660000000000f7"
TETHER_ON      = "f000015f7a010000000000000000000000010009000b2b3a00100401000000000000002f7e0000000002f7"
TETHER_OFF     = "f000015f7a010000000000000000000000010009000b2b3a0010040000000000000000171f0000000000f7"
SCENE_PERSIST  = "f000015f7a010000000000000000000000010009000b2b3a00100801010000000000007b690000000002f7"
NAV_PERSIST    = "f000015f7a010000000000000000000000010009000b2b3a00100901010000000000003c3a0000000006f7"
BACKLIGHT_ON   = "f000015f7a01000000000000000000000001000400050825012000007b2c0000000cf7"
BACKLIGHT_OFF  = "f000015f7a01000000000000000000000001000400050825002000004c1c0000000cf7"


# --------------------------------------------------------------------------- discovery
# A control/data port name identifies each unit's Port-1 -- the one that answers the
# query, accepts the mode SysEx, and streams the tether sensor data.
#   original unit ("SSCOM"):     "SSCOM MIDI 1"
#   newer unit ("SoftStep"):     "SoftStep Control Surface"
_CONTROL_PORT_KINDS = [
    # (kind, short display label, predicate on the amidi port Name)
    ("sscom",    "SSCOM",    lambda n: n.strip() == "SSCOM MIDI 1"),
    ("softstep", "SoftStep", lambda n: "SoftStep Control Surface" in n),
]


def _amidi_l():
    """Raw `amidi -l` text (rawmidi device list). '' if amidi is missing/errors."""
    try:
        r = subprocess.run(["amidi", "-l"], capture_output=True, text=True, timeout=5)
        return r.stdout
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return ""


def list_softsteps():
    """Discover every connected SoftStep's control/data port, in `amidi -l` order.

    Returns a list of dicts: {"kind", "label", "port", "name", "card"}. `kind` is
    "sscom" or "softstep" (the two ways the two units enumerate -- how the synth code
    tells them apart); `label` is a short human tag for display; `port` is the
    `hw:card,dev,sub` string to hand `amidi -p`; `name` is the ALSA port name.

    Robust to the ALSA card number changing between plug-ins: we match by port name.
    """
    found = []
    for line in _amidi_l().splitlines():
        # Columns: Dir  Device(hw:c,d,s)  Name(may contain spaces)
        m = re.match(r"\s*[IO]+\s+(hw:(\d+),\d+,\d+)\s+(.*\S)\s*$", line)
        if not m:
            continue
        port, card, name = m.group(1), int(m.group(2)), m.group(3)
        for kind, label, pred in _CONTROL_PORT_KINDS:
            if pred(name):
                found.append({"kind": kind, "label": label, "port": port,
                              "name": name, "card": card})
                break
    return found


def default_port():
    """The first discovered SoftStep control port, or the legacy `hw:1,0,0` fallback."""
    devs = list_softsteps()
    return devs[0]["port"] if devs else "hw:1,0,0"


# Back-compat: single-device tools still read `ss.PORT`. Computed once at import from
# discovery (was a hardcoded "hw:1,0,0"; that is now the NEWER unit, so discovery is
# the correct default). Multi-device callers pass an explicit `port=` instead.
PORT = default_port()


def send(*msgs, port=None, gap_ms=150, retries=8):
    """Send one or more hex SysEx messages to a device, gap_ms apart.

    `port` defaults to `PORT` (the first discovered SoftStep). Retries on a busy port
    -- amidi -d (the sensor reader) holds the rawmidi node, and it can take a moment to
    free after we terminate it, so the next send may transiently get EBUSY. Returns
    True on success."""
    port = port or PORT
    hexstr = " ".join(msgs)
    for attempt in range(retries):
        r = subprocess.run(["amidi", "-p", port, "-i", str(gap_ms), "-S", hexstr],
                           capture_output=True, text=True)
        if r.returncode == 0:
            return True
        if "busy" in (r.stderr + r.stdout).lower() and attempt < retries - 1:
            time.sleep(0.25)
            continue
        sys.stderr.write(f"send failed on {port}: {(r.stderr or r.stdout).strip()}\n")
        return False
    return False


def enter_tether(port=None):
    """Switch device into hosted/tether streaming mode (reversible, non-persistent)."""
    return send(STANDALONE_OFF, TETHER_ON, port=port)


def restore_standalone(port=None):
    """Return device to its normal standalone Program-Change mode."""
    return send(TETHER_OFF, STANDALONE_ON, port=port)


def query(timeout=2, port=None):
    """Send the firmware query, return the raw reply hex (or '' if no reply)."""
    port = port or PORT
    try:
        r = subprocess.run(["amidi", "-p", port, "-S", FW_QUERY, "-d", "-t", str(int(timeout))],
                           capture_output=True, text=True, timeout=timeout + 4)
        return r.stdout.strip()
    except subprocess.TimeoutExpired:
        return ""


if __name__ == "__main__":
    # Quick self-test: enumerate the connected SoftSteps and query each one.
    devs = list_softsteps()
    if not devs:
        print("no SoftStep found (amidi -l shows no 'SSCOM MIDI 1' / 'SoftStep Control Surface' port)")
        sys.exit(1)
    print(f"discovered {len(devs)} SoftStep(s):")
    for d in devs:
        print(f"  [{d['label']:8}] kind={d['kind']:8} port={d['port']:10} name={d['name']!r}")
    for d in devs:
        print(f"\n{d['label']} ({d['port']}) firmware query reply:")
        print(" ", query(port=d["port"]) or "(no reply -- newer 'SoftStep' firmware answers the "
              "universal identity request instead; see probe.py)")
