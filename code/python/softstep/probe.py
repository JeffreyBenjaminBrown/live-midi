#!/usr/bin/env python3
"""Autonomous reachability probe -- NO foot needed.

Proves we can talk TO the SoftStep over the rawmidi path by firing two queries
and printing whatever the device replies. A reply = the send path works.

  python3 code/python/softstep/probe.py

Expected (on this device): the KMI firmware query gets a long F0 00 1B 48 ... F7
reply; the universal identity request gets nothing (this firmware ignores it).
"""
import subprocess
import ss

print(f"port: {ss.PORT}\n")

print("[1] KMI firmware query  ->")
reply = ss.query()
print("    " + (reply.replace("\n", "\n    ") if reply else "(no reply)"))

print("\n[2] Universal identity request (F0 7E 7F 06 01 F7)  ->")
r = subprocess.run(["amidi", "-p", ss.PORT, "-S", ss.IDENTITY_REQUEST, "-d", "-t", "2"],
                   capture_output=True, text=True, timeout=8)
print("    " + (r.stdout.strip() or "(no reply -- expected; not an error)"))

print("\nIf [1] shows a reply, sending to the device works. We are NOT in standalone-"
      "\nonly jail anymore.")
