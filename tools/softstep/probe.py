#!/usr/bin/env python3
"""Autonomous reachability probe -- NO foot needed. Probes EVERY connected SoftStep.

Proves we can talk TO each SoftStep over the rawmidi path by firing two queries per
unit and printing whatever it replies. A reply = the send path works for that unit.

  python3 tools/softstep/probe.py

The two units answer DIFFERENTLY -- this is one of the ways code tells them apart:
  original unit ("SSCOM"):    the KMI firmware query gets a long  F0 00 1B 48 ... F7
                              reply; the universal identity request gets nothing.
  newer unit ("SoftStep"):    the universal identity request gets a
                              F0 7E 00 06 02 00 01 5F 0D 00 ... F7 reply (KMI mfr id
                              00 01 5F, device family 0D 00).
"""
import subprocess
import ss

devs = ss.list_softsteps()
if not devs:
    print("no SoftStep found -- check the USB connection.")
    raise SystemExit(1)

print(f"discovered {len(devs)} SoftStep(s):")
for d in devs:
    print(f"  [{d['label']:8}] kind={d['kind']:8} port={d['port']:10} name={d['name']!r}")

for d in devs:
    print(f"\n=== {d['label']} ({d['port']}) ===")
    print("[1] KMI firmware query          ->")
    reply = ss.query(port=d["port"])
    print("    " + (reply.replace("\n", "\n    ") if reply else "(no reply)"))

    print("[2] Universal identity request  ->")
    r = subprocess.run(["amidi", "-p", d["port"], "-S", ss.IDENTITY_REQUEST, "-d", "-t", "2"],
                       capture_output=True, text=True, timeout=8)
    print("    " + (r.stdout.strip() or "(no reply)"))

print("\nAny reply above means sending to that unit works. Each unit answering a "
      "\nDIFFERENT one of the two queries is itself a reliable way to identify it.")
