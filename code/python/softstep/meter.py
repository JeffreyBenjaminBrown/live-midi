#!/usr/bin/env python3
"""Live per-pad pressure meter + velocity audition -- calibrate your touch.

Puts the SoftStep in tether mode and, for each of the 10 pads (labeled as printed
on the board), draws a live pressure bar (sum of the pad's 4 sensors, 0..508) with
a peak-hold. When you strike a pad it also DETECTS the hit, prints its velocity, and
PLAYS the low-tom sample at that velocity so you can hear what soft/medium/hard map
to. Restores standalone mode on exit.

  python3 code/python/softstep/meter.py          # run until Ctrl-C
  python3 code/python/softstep/meter.py 5         # run ~5s then exit (for testing)

------------------------------------------------------------------------------
HOW VELOCITY SCALES LOUDNESS (the part you asked about):
Loudness is perceived ~logarithmically, so we do NOT make vel 127 literally 127x
the amplitude of vel 1 (that would be a ~42 dB spread -- way too much). Instead we
spread the whole velocity range over a fixed VEL_DB_RANGE in decibels. With the
default 20 dB, the softest hit is 1/10 the amplitude of the hardest (10^(-20/20)).
Tweak VEL_DB_RANGE below to taste: smaller = flatter dynamics, larger = more spread.
------------------------------------------------------------------------------
"""
import os, sys, time, threading, subprocess, re, math
import ss

# ---- TUNABLES (easily noticed and modified) --------------------------------
VEL_DB_RANGE   = 20.0   # dB between the softest (vel 1) and hardest (vel 127) hit
MASTER_VOLUME  = 0.9    # overall ceiling passed to pw-play (0..1); the vel-127 level
VEL_FULL_SCALE = 460    # pad sum-of-4 that maps to velocity 127 (hard hits ~ 430-460)
ON_SUM         = 40     # onset threshold on sum-of-4 (a hit starts above this)
OFF_SUM        = 20     # release threshold (hysteresis: must drop below to re-arm)
ATTACK_MS      = 14     # capture the attack peak this long after onset, then fire
                        # (hard-floor data: strikes peak on the 2nd sensor refresh ~10-11ms)
# ----------------------------------------------------------------------------

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
LOW_TOM = os.path.join(REPO_ROOT, "drum-samples", "low_tom.wav")

# printed label (as on the board) -> base CC of that pad's 4 sensors (MEASURED)
LABEL_BASE = [("1", 44), ("2", 52), ("3", 60), ("4", 68), ("5", 76),
              ("6", 40), ("7", 48), ("8", 56), ("9", 64), ("0", 72)]
SUM_MAX = 508.0
BARW = 44

sensors = [0] * 128
state = {b: "idle" for _, b in LABEL_BASE}       # idle / rising / held
peak = {b: 0 for _, b in LABEL_BASE}             # attack peak (sum) while rising
onset = {b: 0.0 for _, b in LABEL_BASE}          # monotonic time of onset
hold_peak = {b: (0, 0.0) for _, b in LABEL_BASE} # bar peak-hold (value, time)
last_vel = {b: 0 for _, b in LABEL_BASE}         # last fired velocity, for display
recent = "(strike a pad)"
lock = threading.Lock()


def velocity_from_sum(peak_sum):
    return max(1, min(127, round(peak_sum / VEL_FULL_SCALE * 127)))


def gain_from_velocity(vel):
    """Map velocity 1..127 to a linear gain over VEL_DB_RANGE dB (vel 127 -> 0 dB)."""
    t = (vel - 1) / 126.0
    gain_db = (t - 1.0) * VEL_DB_RANGE          # -VEL_DB_RANGE .. 0
    return 10.0 ** (gain_db / 20.0)             # 0.1 .. 1.0 at 20 dB


def fire(label, base, peak_sum):
    global recent
    vel = velocity_from_sum(peak_sum)
    gain = gain_from_velocity(vel)
    vol = max(0.0, min(1.0, MASTER_VOLUME * gain))
    last_vel[base] = vel
    recent = f"pad {label}: vel {vel:3d}  ({(vel-1)/126*VEL_DB_RANGE - VEL_DB_RANGE:+.1f} dB, vol {vol:.2f})"
    try:
        subprocess.Popen(["pw-play", f"--volume={vol:.3f}", LOW_TOM],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except FileNotFoundError:
        pass  # no pw-play: still shows velocity


def update_pad(base, now):
    """Run one pad's onset/peak state machine after a sensor update."""
    s = sum(sensors[base + k] for k in range(4))
    st = state[base]
    if st == "idle":
        if s > ON_SUM:
            state[base] = "rising"; peak[base] = s; onset[base] = now
    elif st == "rising":
        if s > peak[base]:
            peak[base] = s
        if s < OFF_SUM:                          # released before window: drop it
            state[base] = "idle"
    elif st == "held":
        if s < OFF_SUM:
            state[base] = "idle"


def check_fire(now):
    """Fire any pad whose attack window has elapsed (called often)."""
    for _, base in LABEL_BASE:
        if state[base] == "rising" and (now - onset[base]) * 1000.0 >= ATTACK_MS:
            fire_label = next(l for l, b in LABEL_BASE if b == base)
            fire(fire_label, base, peak[base])
            state[base] = "held"


def reader():
    proc = subprocess.Popen(["stdbuf", "-oL", "amidi", "-p", ss.PORT, "-d"],
                            stdout=subprocess.PIPE, text=True, bufsize=1)
    reader.proc = proc
    status = None
    for line in proc.stdout:
        toks = [int(x, 16) for x in line.split() if re.fullmatch(r"[0-9A-Fa-f]{2}", x)]
        i = 0
        with lock:
            now = time.monotonic()
            while i < len(toks):
                b = toks[i]
                if b >= 0x80:
                    status = b; i += 1
                if status is None:
                    i += 1; continue
                if (status & 0xF0) == 0xB0 and i + 1 < len(toks):
                    cc, val = toks[i], toks[i + 1]; i += 2
                    if cc < 128:
                        sensors[cc] = val
                        base = 40 + 4 * ((cc - 40) // 4) if 40 <= cc <= 79 else None
                        if base in state:
                            update_pad(base, now)
                else:
                    break
            check_fire(now)


def draw():
    now = time.monotonic()
    with lock:
        check_fire(now)
        lines = ["", "  KMSS velocity meter -- strike a pad: bar=pressure, plays low tom at that velocity.",
                 "  Ctrl-C to stop.", ""]
        for label, base in LABEL_BASE:
            s = sum(sensors[base + k] for k in range(4))
            pv, pt = hold_peak[base]
            if s >= pv or now - pt > 0.6:
                pv, pt = (s, now) if s >= pv else (max(0, pv - 24), pt)
                hold_peak[base] = (pv, pt)
            fill = int(s / SUM_MAX * BARW); pk = int(pv / SUM_MAX * BARW)
            bar = ["-"] * BARW
            for j in range(min(fill, BARW)):
                bar[j] = "#"
            if 0 <= pk < BARW:
                bar[pk] = "|"
            lv = last_vel[base]
            lvs = f"vel{lv:3d}" if lv else "      "
            lines.append(f"  pad {label}  [{''.join(bar)}] {s:3d}  {lvs}")
        lines += ["", f"  last hit: {recent}",
                  f"  (VEL_DB_RANGE={VEL_DB_RANGE:g} dB, full-scale sum={VEL_FULL_SCALE})"]
    sys.stdout.write("\033[2J\033[H" + "\n".join(lines) + "\n")
    sys.stdout.flush()


def main():
    if not os.path.exists(LOW_TOM):
        print(f"warning: low tom sample not found at {LOW_TOM}", file=sys.stderr)
    limit = float(sys.argv[1]) if len(sys.argv) > 1 else None
    ss.enter_tether()
    threading.Thread(target=reader, daemon=True).start()
    start = time.monotonic()
    try:
        while True:
            draw()
            time.sleep(0.02)
            if limit and time.monotonic() - start > limit:
                break
    except KeyboardInterrupt:
        pass
    finally:
        try:
            if getattr(reader, "proc", None):
                reader.proc.terminate()
        except Exception:
            pass
        time.sleep(0.2)
        ss.restore_standalone()
        sys.stdout.write("\nRestored standalone mode.\n")


if __name__ == "__main__":
    main()
