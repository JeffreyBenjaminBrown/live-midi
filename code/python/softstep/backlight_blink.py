#!/usr/bin/env python3
"""Visible 'can we talk TO the SoftStep?' test.

Sends the SoftStep's backlight-off / backlight-on SysEx (from KMI's own editor)
with a loud real-time countdown so you know exactly when to look at the device.
If the backlight blinks in time with the countdown, our SysEx is reaching the
hardware. If it never blinks, the problem is the send path, not the protocol.

Run it yourself so the countdown is live in your terminal:
    python3 code/softstep/backlight_blink.py            # port 20:0, 5 blinks
    python3 code/softstep/backlight_blink.py 20:1       # try the other port
    python3 code/softstep/backlight_blink.py 20:0 8     # 8 blinks

Needs `aseqsend` (alsa-utils). The SoftStep is ALSA client 20 (SSCOM):
20:0 = "SSCOM MIDI 1", 20:1 = "SSCOM MIDI 2".
"""
import subprocess, sys, time

# 35-byte backlight messages, verbatim from softstep_editor/shared/sysexmessages.h
# (KMI id 00 01 5F, magic 7A). Toggling the display/key backlight is a hardware
# effect, so it works regardless of standalone/tether mode.
BACKLIGHT_ON  = bytes.fromhex("f000015f7a01000000000000000000000001000400050825012000007b2c0000000cf7")
BACKLIGHT_OFF = bytes.fromhex("f000015f7a01000000000000000000000001000400050825002000004c1c0000000cf7")

def send(msg, port):
    open("/tmp/_ss_blink.syx", "wb").write(msg)
    subprocess.run(["aseqsend", "-p", port, "-s", "/tmp/_ss_blink.syx"],
                   capture_output=True)

def main():
    port   = sys.argv[1] if len(sys.argv) > 1 else "20:0"
    blinks = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    print(f"Backlight blink test on port {port}. WATCH THE SOFTSTEP.\n")
    for n in range(1, blinks + 1):
        print(f"--- blink {n}/{blinks} ---")
        for c in (3, 2, 1):
            print(f"   {c}...", flush=True); time.sleep(1)
        send(BACKLIGHT_OFF, port); print("   >>> OFF  (look now)", flush=True)
        time.sleep(1.5)
        send(BACKLIGHT_ON, port);  print("   >>> ON\n", flush=True)
        time.sleep(1.5)
    print("Done. Did the backlight follow the OFF/ON cues?")

if __name__ == "__main__":
    main()
