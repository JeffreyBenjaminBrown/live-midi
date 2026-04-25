#!/usr/bin/env bash
# Dead-simple brightness check: paint one row with ascending
# brightness 0..15, left to right. If the monome shows a smooth
# gradient, varibright works; if it's effectively binary
# (cells x=0..7 dark, x=8..15 lit, all the same brightness), the
# device or its firmware doesn't support /grid/led/level/set.
#
# Usage:
#   bash 12-brightness-test.sh        # row 0 (top)
#   bash 12-brightness-test.sh 8      # row 8
#
# Best run with grid_synth NOT running, since live key presses
# while it's running would overwrite our LEDs with binary on/off.

set -e
ROW="${1:-0}"
PREFIX=/256-1-cable

python3 - <<EOF
import socket, struct, time

PREFIX = "$PREFIX"
ROW    = $ROW

def osc(addr, *args):
    a = addr.encode() + b'\0'; a += b'\0' * ((4 - len(a) % 4) % 4)
    tags, body = ',', b''
    for v in args:
        if isinstance(v, int):
            tags += 'i'; body += struct.pack('>i', v)
        elif isinstance(v, str):
            tags += 's'
            sb = v.encode() + b'\0'; sb += b'\0' * ((4 - len(sb) % 4) % 4)
            body += sb
    t = tags.encode() + b'\0'; t += b'\0' * ((4 - len(t) % 4) % 4)
    return a + t + body

def parse_osc(b):
    end = b.index(0); addr = b[:end].decode()
    pad = (4 - (end + 1) % 4) % 4; p = end + 1 + pad
    end = b.index(0, p); tags = b[p+1:end].decode()
    pad = (4 - (end + 1 - p) % 4) % 4; p = end + 1 + pad
    args = []
    for t in tags:
        if t == 'i':
            args.append(struct.unpack('>i', b[p:p+4])[0]); p += 4
        elif t == 's':
            end = b.index(0, p); args.append(b[p:end].decode())
            pad = (4 - (end + 1 - p) % 4) % 4; p = end + 1 + pad
    return addr, args

# Bind to an ephemeral port so we don't fight grid_synth on :9000.
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(('0.0.0.0', 0))
my_port = s.getsockname()[1]

# Discover.
s.sendto(osc('/serialosc/list', '127.0.0.1', my_port), ('127.0.0.1', 12002))
s.settimeout(2.0)
device_port = None
deadline = time.time() + 2.0
while time.time() < deadline:
    try:
        d, _ = s.recvfrom(2048)
        addr, args = parse_osc(d)
        if addr == '/serialosc/device' and len(args) >= 3:
            device_port = args[2]
            print(f"discovered device on port {device_port}")
            # Don't break — keep draining in case multiple are announced.
    except socket.timeout:
        break

if device_port is None:
    raise SystemExit("no monome found; is serialoscd running and a grid plugged in?")

dev = ('127.0.0.1', device_port)
# Set the prefix and clear the grid. We deliberately do NOT set
# /sys/host or /sys/port — we don't want events forwarded to us.
s.sendto(osc('/sys/prefix', PREFIX), dev)
s.sendto(osc(f'{PREFIX}/grid/led/all', 0), dev)
time.sleep(0.05)

print(f"painting row y={ROW} with brightness 0..15 across x=0..15:")
for x in range(16):
    s.sendto(osc(f'{PREFIX}/grid/led/level/set', x, ROW, x), dev)
    print(f"  ({x:>2}, {ROW:>2})  level={x:>2}")

print("\nLook at the grid. Expected: a smooth ramp, dark on the left,")
print("brightest on the right. If you see only two states (off vs on)")
print("with the boundary near the middle, varibright isn't working.")
EOF
