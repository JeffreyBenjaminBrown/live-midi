#!/usr/bin/env bash
# Two-row brightness diagnostic. Earlier observation: the device
# renders ~4 distinct levels across the 0..15 API. This narrows
# down the bucket boundaries.
#
# Row 0:  cells 0-3 at level 0;   4-7 at level 4;   8-11 at level 8;
#         12-15 at level 12.
# Row 8:  cells 0-3 at level 1;   4-7 at level 5;   8-11 at level 9;
#         12-15 at level 13.
#
# How to read it:
#   * If row 0 and row 8 look IDENTICAL across all four quartets,
#     the boundaries are 4-wide and aligned to multiples of 4 —
#     i.e. levels {0,1,2,3} all = off, {4..7} all = dim, {8..11}
#     all = mid, {12..15} all = bright. Then low_res_brightness(i)
#     can be just 4*i and we're done.
#   * If row 0's first quartet (level 0) is dark but row 8's first
#     quartet (level 1) is dim, the boundary between off and the
#     first lit bucket is between 0 and 1 (probably 0 alone is the
#     only off level). Tell me and we shift the mapping.
#   * If you see more than four shades total across the two rows,
#     buckets aren't 4-wide. Tell me what you see.
#
# Usage: bash 12-brightness-test.sh
# Best with grid_synth NOT running, since live key presses would
# overwrite our LEDs with binary on/off.

set -e
PREFIX=/256-1-cable

python3 - <<EOF
import socket, struct, time

PREFIX = "$PREFIX"

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

# Row 0: levels 0, 4, 8, 12 (one per quartet).
# Row 8: levels 1, 5, 9, 13 (one per quartet, shifted by 1).
ROWS = [
    (0, [0, 4, 8, 12]),
    (8, [1, 5, 9, 13]),
]
for (row, quartet_levels) in ROWS:
    print(f"row {row}:")
    for q, level in enumerate(quartet_levels):
        for x in range(q * 4, q * 4 + 4):
            s.sendto(osc(f'{PREFIX}/grid/led/level/set', x, row, level), dev)
        print(f"  cells {q*4}..{q*4+3} at level {level}")

print()
print("Compare row 0 (top) with row 8 (middle):")
print("  - identical across all quartets -> boundaries at 0,4,8,12;")
print("    low_res_brightness(i) = 4 * i")
print("  - row 0's first quartet dark but row 8's first quartet lit ->")
print("    only 0 is off; first lit level is 1; mapping shifts.")
print("  - more than four total shades -> buckets aren't 4-wide.")
EOF
