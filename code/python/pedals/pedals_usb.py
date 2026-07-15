#!/usr/bin/env python3
"""Read the DOREMiDi MPC-20 (WCH CH345, USB 1a86:752d) DIRECTLY over USB via libusb --
bypassing ALSA, because the kernel binds this chip's MIDI interface but (on this
composite MIDI+serial device) creates no ALSA MIDI port, so `amidi -l` shows nothing.

The MIDI interface has ordinary bulk endpoints (IN 0x81 / OUT 0x01), so we just read the
raw stream ourselves and decode it. Works even with the broken ALSA path.

  *** RUN THIS ON THE HOST, NOT IN THE CONTAINER ***  (the container has no /dev/bus/usb)

Needs pyusb + libusb, and root (to detach the kernel driver holding the interface). On
NixOS, one line:

  nix-shell -p 'python3.withPackages(p: [p.pyusb])' libusb1 \
    --run 'sudo python3 code/python/pedals/pedals_usb.py'

What it shows: for every (channel, CC) it decodes, a live value + 0..127 sweep bar +
min/max seen -- so a pedal that only reaches ~35..127 is obvious (its `min` never gets to
0). Channel is shown BOTH 1-based and 0-based. It ALSO prints the raw USB bytes, because
the CH345 is quirky and this is the ground truth of what it emits.

  python3 pedals_usb.py           # run until Ctrl-C
  python3 pedals_usb.py --raw     # raw hex only (no decode) -- use if decode looks wrong
  python3 pedals_usb.py 8         # run ~8s then exit

CSV: logs/usb-session-<stamp>.csv. On exit it re-attaches the kernel driver.
"""
import sys, os, time, datetime, csv

try:
    import usb.core, usb.util
except ImportError:
    sys.exit("pyusb not found. Run under: nix-shell -p \"python3.withPackages(p: [p.pyusb])\" libusb1 "
             "--run 'sudo python3 code/python/pedals/pedals_usb.py'")

VID, PID = 0x1A86, 0x752D
MIDI_IFACE = 1            # interface :1.1 (MIDIStreaming); bulk IN 0x81, bulk OUT 0x01
EP_IN = 0x81
HERE = os.path.dirname(os.path.abspath(__file__))
LOGDIR = os.path.join(HERE, "logs")

# This is a RAW measurement tool: it reports the true CC values the pedals send, and
# never rescales them. The EX-P + MPC-20 sweep ~1 (heel) .. ~120 (toe), not a full
# 0..127 -- that is the pedal's real range (confirmed in the DOREMiDi tool too), so it is
# NOT a bug to fix here. Normalization to a continuous 0.0..1.0 belongs in the BRIDGE that
# feeds the synth, not in this meter. See README.org ("Observed range").

# USB-MIDI 1.0 Code Index Number -> total MIDI bytes in the packet's payload (b1..b3).
CIN_LEN = {0x8: 3, 0x9: 3, 0xA: 3, 0xB: 3, 0xC: 2, 0xD: 2, 0xE: 3, 0xF: 1,
           0x2: 2, 0x3: 3, 0x4: 3, 0x5: 1, 0x6: 2, 0x7: 3}
KIND = {0x8: "note_off", 0x9: "note_on", 0xA: "aftertouch", 0xB: "control_change",
        0xC: "program_change", 0xD: "channel_pressure", 0xE: "pitch_bend"}

seen = {}   # (ch0, cc) -> {"val","min","max","count"}


def decode_usb_midi(buf):
    """Yield (status, d1, d2) MIDI messages from a CH345 bulk-IN buffer.

    Handles both encodings the chip might use: standard USB-MIDI 4-byte event packets
    (byte0 = cable|CIN, then up to 3 MIDI bytes), and -- as a fallback -- a raw MIDI byte
    stream (with running status). We pick per-4-byte-group: if byte0's low nibble is a
    known CIN and byte1 looks like a status byte, it's a USB-MIDI packet; otherwise we
    treat the whole buffer as raw MIDI."""
    b = list(buf)
    # Heuristic: USB-MIDI packets come in 4s and byte1 (the MIDI status) has bit7 set.
    looks_packetised = len(b) >= 4 and len(b) % 4 == 0 and all(
        (b[i] == 0 and b[i + 1] == 0) or ((b[i] & 0x0F) in CIN_LEN and b[i + 1] >= 0x80)
        for i in range(0, len(b), 4))
    if looks_packetised:
        for i in range(0, len(b), 4):
            cin = b[i] & 0x0F
            if cin not in CIN_LEN or b[i] == 0:
                continue
            n = CIN_LEN[cin]
            payload = b[i + 1:i + 1 + n]
            if payload and payload[0] >= 0x80:
                yield (payload[0], payload[1] if n > 1 else None, payload[2] if n > 2 else None)
        return
    # Raw MIDI fallback (running status).
    status = None
    i = 0
    while i < len(b):
        if b[i] >= 0x80:
            status = b[i]; i += 1
            if status >= 0xF8:      # real-time, no data
                yield (status, None, None); status = None; continue
        if status is None:
            i += 1; continue
        high = status >> 4
        n = 1 if high in (0xC, 0xD) else 2
        if i + n > len(b):
            break
        d1 = b[i]; d2 = b[i + 1] if n == 2 else None
        i += n
        yield (status, d1, d2)


def note(w, now, status, d1, d2):
    high, ch0 = status >> 4, status & 0x0F
    kind = KIND.get(high, f"status_{high:X}")
    if w:
        w.writerow([datetime.datetime.now().isoformat(timespec="milliseconds"),
                    f"{now*1000:.1f}", f"0x{status:02X}", ch0 + 1, ch0, kind,
                    d1 if d1 is not None else "", d2 if d2 is not None else ""])
    if high == 0xB and d1 is not None and d2 is not None:
        e = seen.get((ch0, d1))
        if e is None:
            seen[(ch0, d1)] = {"val": d2, "min": d2, "max": d2, "count": 1}
        else:
            e["val"] = d2; e["min"] = min(e["min"], d2); e["max"] = max(e["max"], d2); e["count"] += 1


BARW = 34


def draw():
    os.write(1, b"\033[2J\033[H")
    out = ["", "  DOREMiDi MPC-20 -- USB-direct pedal monitor (bypasses ALSA)",
           "  Shows the TRUE CC values (no rescaling). min..max = the range each pedal spans.", "",
           f"  {'ch':>2} {'(0b)':>4} {'cc':>3} {'value':>5}  [{'0 .. 127':^{BARW}}]  {'min':>3}..{'max':<3}  {'#msgs':>6}"]
    if seen:
        for (ch0, cc) in sorted(seen):
            e = seen[(ch0, cc)]
            fill = int(e["val"] / 127 * BARW)
            bar = "#" * fill + "-" * (BARW - fill)
            out.append(f"  {ch0+1:>2} {ch0:>4} {cc:>3} {e['val']:>5}  [{bar}]  "
                       f"{e['min']:>3}..{e['max']:<3}  {e['count']:>6}")
    else:
        out.append("  (no CC decoded yet -- move a pedal)")
    os.write(1, ("\n".join(out) + "\n").encode())


def open_device():
    dev = usb.core.find(idVendor=VID, idProduct=PID)
    if dev is None:
        sys.exit(f"DOREMiDi MPC-20 ({VID:04x}:{PID:04x}) not found. Plugged in? Running on the HOST?")
    reattach = False
    try:
        if dev.is_kernel_driver_active(MIDI_IFACE):
            dev.detach_kernel_driver(MIDI_IFACE)   # snd-usb-audio holds it; needs root
            reattach = True
    except usb.core.USBError as e:
        sys.exit(f"could not detach kernel driver from interface {MIDI_IFACE}: {e}\n"
                 "Run with sudo (root is needed to take the interface from snd-usb-audio).")
    try:
        usb.util.claim_interface(dev, MIDI_IFACE)
    except usb.core.USBError as e:
        sys.exit(f"could not claim MIDI interface {MIDI_IFACE}: {e}")
    return dev, reattach


def main():
    argv = sys.argv[1:]
    raw_only = "--raw" in argv
    limit = next((float(a) for a in argv if a.replace(".", "", 1).isdigit()), None)
    os.makedirs(LOGDIR, exist_ok=True)
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    path = os.path.join(LOGDIR, f"usb-session-{stamp}.csv")
    f = open(path, "w", newline=""); w = csv.writer(f)
    w.writerow(["t_iso", "t_ms", "status", "channel_1based", "channel_0based", "kind", "data1", "data2"])
    f.flush()

    dev, reattach = open_device()
    print(f"Reading MPC-20 over USB (interface {MIDI_IFACE}, EP {EP_IN:#x}). CSV -> {path}")
    print("Move each pedal heel<->toe several times. Ctrl-C to stop.\n")
    if raw_only:
        print("(--raw: showing raw USB bytes only)\n")
    start = time.monotonic()
    last_draw = 0.0
    try:
        while True:
            try:
                data = dev.read(EP_IN, 64, timeout=200)
            except usb.core.USBError as e:
                if e.errno == 110 or "timeout" in str(e).lower():   # no data this interval
                    data = None
                else:
                    raise
            now = time.monotonic()
            if data:
                if raw_only:
                    os.write(1, (" ".join(f"{x:02X}" for x in data) + "\n").encode())
                else:
                    for status, d1, d2 in decode_usb_midi(data):
                        note(w, now, status, d1, d2)
                    f.flush()
            if not raw_only and now - last_draw > 0.05:
                draw(); last_draw = now
            if limit and now - start > limit:
                break
    except KeyboardInterrupt:
        pass
    finally:
        f.flush(); f.close()
        try:
            usb.util.release_interface(dev, MIDI_IFACE)
            if reattach:
                dev.attach_kernel_driver(MIDI_IFACE)
        except Exception:
            pass
        print(f"\nDone. CSV: {path}\n(kernel driver re-attached)")


if __name__ == "__main__":
    main()
