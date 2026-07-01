#!/usr/bin/env python3
"""Shared SoftStep (KMSS) helpers: the *working* rawmidi send path + mode control.

KEY DISCOVERY (2026-06-30): the prior write-ups concluded "we can't send to the
device." That was tested only through the ALSA *sequencer* (aseqsend), which the
SoftStep ignores. The ALSA *rawmidi* path WORKS: `amidi -p hw:1,0,0` reaches the
device's USB-MIDI IN. Proof -- the firmware query gets a reply, and the
standalone<->tether mode switch takes visible effect (the device starts emitting
Control Change data, which standalone Program-Change mode never does).

So everything here drives `amidi` on the rawmidi port (no python-midi libs needed).

Ports: the SoftStep is ALSA card 1, device 0, with two USB-MIDI cables:
  hw:1,0,0  = "SSCOM MIDI 1" (Port 1)  <- answers the query AND accepts control SysEx
  hw:1,0,1  = "SSCOM MIDI 2" (Port 2)  <- silent for our purposes
Control SysEx (query, mode switch, backlight) and the sensor stream are on hw:1,0,0.

Mode switching is REVERSIBLE and does NOT touch firmware -- it cannot brick the
device (only an interrupted firmware *flash* can; we never flash). See
kmss-research.org.
"""
import subprocess, time, sys

PORT = "hw:1,0,0"

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


def send(*msgs, gap_ms=150, retries=8):
    """Send one or more hex SysEx messages to the device, gap_ms apart.

    Retries on a busy port -- amidi -d (the sensor reader) holds the rawmidi node,
    and it can take a moment to free after we terminate it, so the next send may
    transiently get EBUSY. Returns True on success."""
    hexstr = " ".join(msgs)
    for attempt in range(retries):
        r = subprocess.run(["amidi", "-p", PORT, "-i", str(gap_ms), "-S", hexstr],
                           capture_output=True, text=True)
        if r.returncode == 0:
            return True
        if "busy" in (r.stderr + r.stdout).lower() and attempt < retries - 1:
            time.sleep(0.25)
            continue
        sys.stderr.write(f"send failed: {(r.stderr or r.stdout).strip()}\n")
        return False
    return False


def enter_tether():
    """Switch device into hosted/tether streaming mode (reversible, non-persistent)."""
    return send(STANDALONE_OFF, TETHER_ON)


def restore_standalone():
    """Return device to its normal standalone Program-Change mode."""
    return send(TETHER_OFF, STANDALONE_ON)


def query(timeout=2):
    """Send the firmware query, return the raw reply hex (or '' if no reply)."""
    try:
        r = subprocess.run(["amidi", "-p", PORT, "-S", FW_QUERY, "-d", "-t", str(int(timeout))],
                           capture_output=True, text=True, timeout=timeout + 4)
        return r.stdout.strip()
    except subprocess.TimeoutExpired:
        return ""


if __name__ == "__main__":
    # Quick self-test: prove the send path.
    print(f"port {PORT}; firmware query reply:")
    print(" ", query() or "(no reply -- check connection / port)")
