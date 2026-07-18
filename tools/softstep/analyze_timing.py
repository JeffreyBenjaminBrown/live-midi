#!/usr/bin/env python3
"""Measure a SoftStep's read timing from a capture.py phase-S log.

The tether stream is on-change, and `amidi -d -T monotonic` stamps every CC message with
the kernel monotonic clock (same clock capture.py's MARKs use). During a pressure roll /
hold-with-wiggle the device emits a burst of CC messages once per sensor scan, so:

  * SCAN INTERVAL = the gap between consecutive scan "frames" (a frame = messages sharing
    ~one timestamp). Its reciprocal is the scan rate in Hz. Measured over the roll+hold
    window (between the "S roll 1" and "S taps" marks), dropping gaps > CAP ms (a lifted
    foot / pause between gestures) and intra-frame gaps < EPS ms.
  * ONSET->PEAK LATENCY = per tap in the taps window, the time from the pad's sum-of-4
    crossing ON_SUM to the sample where that sum peaks. Depends on clean taps.

  python3 analyze_timing.py captures/session-*-softstep.log captures/session-*-sscom.log
"""
import sys, re, statistics

EPS_MS = 3.0     # gaps below this are within one scan frame, not between scans
CAP_MS = 40.0    # gaps above this are a pause/lift, not a scan interval (~100 Hz => ~10 ms)
ON_SUM = 20      # matches rigs/softstep.toml: a hit fires when sum-of-4 exceeds this
PAD_BASES = list(range(40, 80, 4))  # 40,44,...,76 -- each pad owns base..base+3

DATA_RE = re.compile(r"^DATA\s+([0-9]+\.[0-9]+)\)\s+(.+)$")
MARK_RE = re.compile(r"^MARK\s+([0-9]+\.[0-9]+)\s+(.*)$")


def parse(path):
    """-> (header, marks[(t,label)], data[(t,cc,val)] for pad CCs 40..79)."""
    header, marks, data = "", [], []
    with open(path) as f:
        for line in f:
            if line.startswith("#"):
                header = line[1:].strip()
                continue
            m = MARK_RE.match(line)
            if m:
                marks.append((float(m.group(1)), m.group(2).strip()))
                continue
            m = DATA_RE.match(line)
            if not m:
                continue
            t, hexs = float(m.group(1)), m.group(2).split()
            # amidi prints one CC message per line: B0 cc val. Keep pad sensors only.
            if len(hexs) >= 3 and hexs[0].upper() == "B0":
                cc, val = int(hexs[1], 16), int(hexs[2], 16)
                if 40 <= cc <= 79:
                    data.append((t, cc, val))
    return header, marks, data


def mark_time(marks, substr):
    for t, label in marks:
        if substr in label:
            return t
    return None


def window(data, t0, t1):
    return [d for d in data if (t0 is None or d[0] >= t0) and (t1 is None or d[0] < t1)]


def scan_intervals(data):
    """Cluster messages into scan frames (new frame when the gap exceeds EPS), then the
    frame-to-frame deltas in (EPS, CAP] ms are scan intervals."""
    if not data:
        return []
    frame_starts, last = [data[0][0]], data[0][0]
    for t, _, _ in data[1:]:
        if (t - last) * 1000.0 > EPS_MS:
            frame_starts.append(t)
        last = t
    out = []
    for a, b in zip(frame_starts, frame_starts[1:]):
        dms = (b - a) * 1000.0
        if EPS_MS < dms <= CAP_MS:
            out.append(dms)
    return out


def busiest_pad(data):
    """The pad base with the most sensor events in this window (the one being pressed)."""
    counts = {base: 0 for base in PAD_BASES}
    for _, cc, _ in data:
        counts[(cc - 40) // 4 * 4 + 40] += 1
    return max(counts, key=counts.get)


def onset_to_peak(data):
    """Per tap: time from the active pad's sum-of-4 crossing ON_SUM to its peak sample."""
    if not data:
        return []
    base = busiest_pad(data)
    cur = {base + k: 0 for k in range(4)}
    laten, in_tap, onset_t, peak_t, peak_v = [], False, None, None, 0
    for t, cc, val in data:
        if cc not in cur:
            continue
        cur[cc] = val
        s = sum(cur.values())
        if not in_tap and s > ON_SUM:
            in_tap, onset_t, peak_t, peak_v = True, t, t, s
        elif in_tap:
            if s > peak_v:
                peak_v, peak_t = s, t
            if s <= ON_SUM:  # tap released -> record it
                laten.append((peak_t - onset_t) * 1000.0)
                in_tap = False
    return laten


def stats(xs):
    if not xs:
        return None
    s = sorted(xs)
    pick = lambda p: s[min(len(s) - 1, int(p * len(s)))]
    return dict(n=len(s), min=s[0], med=statistics.median(s), p90=pick(0.90),
                p95=pick(0.95), max=s[-1], mean=statistics.fmean(s))


def report(path):
    header, marks, data = parse(path)
    board = re.search(r"board=(\S+)", header)
    board = board.group(1) if board else path
    roll0, taps, end = (mark_time(marks, m) for m in ("S roll 1", "S taps", "PHASE_S end"))

    scan = scan_intervals(window(data, roll0, taps))
    laten = onset_to_peak(window(data, taps, end))
    ss, ls = stats(scan), stats(laten)

    print(f"\n=== {board}  ({path}) ===")
    print(f"  pad CC events: {len(data)}   scan-window frames used: {ss['n'] if ss else 0}")
    if ss:
        print(f"  SCAN INTERVAL (ms): median {ss['med']:.2f}  min {ss['min']:.2f}  "
              f"p90 {ss['p90']:.2f}  p95 {ss['p95']:.2f}  max {ss['max']:.2f}")
        print(f"  SCAN RATE      ~ {1000.0/ss['med']:.1f} Hz (from the median interval)")
    else:
        print("  SCAN INTERVAL: no usable roll/hold data (did the pressure keep changing?)")
    if ls:
        print(f"  ONSET->PEAK (ms): median {ls['med']:.2f}  over {ls['n']} tap(s)  "
              f"[min {ls['min']:.2f} max {ls['max']:.2f}]")
    else:
        print("  ONSET->PEAK: no clean taps found in the taps window.")
    return board, ss, ls


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    results = [report(p) for p in sys.argv[1:]]
    if len(results) == 2:
        (b0, s0, _), (b1, s1, _) = results
        if s0 and s1:
            print(f"\n--- comparison ---")
            print(f"  {b0} median scan {s0['med']:.2f} ms ({1000/s0['med']:.1f} Hz)  vs  "
                  f"{b1} median scan {s1['med']:.2f} ms ({1000/s1['med']:.1f} Hz)")
            faster = b0 if s0['med'] < s1['med'] else b1
            diff = abs(s0['med'] - s1['med'])
            print(f"  -> {faster} scans {diff:.2f} ms faster at the median "
                  f"({'negligible' if diff < 1.0 else 'notable'} vs the ~10 ms scan).")


if __name__ == "__main__":
    main()
