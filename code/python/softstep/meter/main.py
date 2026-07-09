#!/usr/bin/env python3
"""Live per-pad pressure meter + velocity audition + per-group .org logger.

Puts the SoftStep in tether mode and, for each of the 10 pads (labeled as printed
on the board), draws a live pressure bar (sum of the pad's 4 sensors, 0..508) with a
peak-hold. When you strike a pad it DETECTS the hit, prints its velocity, optionally
PLAYS the configured audition sample (default snare) at that velocity, and LOGS the raw sum
trace of each "event group" to an .org file, alongside a re-derived interpretation
(how many hits it renders, at what velocities/times) so you can see exactly why a hit
did or didn't fire. Restores standalone mode on exit.

  python3 code/python/softstep/meter/main.py          # run until 'q' or Ctrl-C
  python3 code/python/softstep/meter/main.py 5         # run ~5s then exit (for testing)

While it runs:
  r   reload config.toml (thresholds, dB range, group settings) live
  q   quit (Ctrl-C also works)

CONFIG: code/python/softstep/meter/config.toml -- all tunables, hot-reloadable.

EVENT GROUPS: grouping is PER PAD -- each pad has its own independent groups, so several
can be ongoing at once. A pad's group starts when ITS sum-of-4 rises to/above
`event-group-separation-threshold`, and ends once it has stayed below that for
`group-end-hold-ms` (0 = close the instant it dips below). Each group is written to
logs/pad-<label>/<N>.org, N counting up from the highest already in that pad's folder
(never overwriting). The .org file has a `* sequence` headline (an org-table: the sum at
every sampled moment) and a `* interpretation` headline (`** constants` then `** result`
-- the hits that sequence renders under those constants). On start you'll see
"pad <label>: new group (ongoing): N"; on close, "pad <label>: wrote group: N".
  See TODO/calibrate-softstep/claude-asks.org for the design choices behind this.

HOW VELOCITY SCALES LOUDNESS: loudness is perceived ~logarithmically, so vel 127 is NOT
127x the amplitude of vel 1 (a ~42 dB spread). Instead the velocity range is spread over
`vel-db-range` dB; at 20 dB the softest hit is 1/10 the amplitude of the hardest.
"""
import os, sys, time, threading, subprocess, re, signal, tomllib, select, termios, tty
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))  # find sibling ss.py
import ss

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG_PATH = os.path.join(HERE, "config.toml")
LOGDIR = os.path.join(HERE, "logs")
REPO_ROOT = os.path.abspath(os.path.join(HERE, "..", "..", "..", ".."))  # meter/ -> repo root
SAMPLES_DIR = os.path.join(REPO_ROOT, "drum-samples")  # `audition-sample` resolves under here

# Defaults for every config key. config.toml overrides these; an unknown key there is
# ignored (with a warning). Keys are hyphenated to match requests.org's naming.
DEFAULTS = {
    "on-sum": 40,           # onset threshold on sum-of-4 (a hit starts above this)
    "off-sum": 20,          # release threshold (hysteresis: must drop below to re-arm)
    "attack-ms": 14,        # fire on onset; keep watching this long to raise velocity to a later peak
    "debounce-ms": 100,     # min gap between two hits on the SAME pad (contact-bounce guard); 0 = off
    "vel-full-scale": 460,  # pad sum-of-4 that maps to velocity 127
    "vel-db-range": 20.0,   # dB between the softest (vel 1) and hardest (vel 127) hit
    "master-volume": 0.9,   # ceiling passed to pw-play (0..1)
    "audition": True,       # play a sample on each detected hit
    "audition-sample": "snare.wav",  # which sample to audition (a filename under drum-samples/)
    "event-group-separation-threshold": 20,  # sum-of-4 below this = that pad is "quiet"
    "group-end-hold-ms": 150,                # board stays quiet this long -> close group
    "silence-to-zero-ms": 25,                # a sensor with no CC for this long reads 0 (de-stick); 0 = off
}
CFG = dict(DEFAULTS)

# printed label (as on the board) -> base CC of that pad's 4 sensors (MEASURED)
LABEL_BASE = [("1", 44), ("2", 52), ("3", 60), ("4", 68), ("5", 76),
              ("6", 40), ("7", 48), ("8", 56), ("9", 64), ("0", 72)]
BASE_LABEL = {b: l for l, b in LABEL_BASE}
BASES = [b for _, b in LABEL_BASE]
SUM_MAX = 508.0
BARW = 44

sensors = [0] * 128                              # raw last-received value per CC
last_seen = [0.0] * 128                          # monotonic time each CC was last received (0 = never)
state = {b: "idle" for b in BASES}               # idle / watching / held
peak = {b: 0 for b in BASES}                      # attack peak (sum) while watching
onset = {b: 0.0 for b in BASES}                   # monotonic time of onset
hold_peak = {b: (0, 0.0) for b in BASES}          # bar peak-hold (value, time)
last_vel = {b: 0 for b in BASES}                  # last fired velocity, for display
last_fire = {b: None for b in BASES}              # onset time of this pad's last FIRED hit (debounce gate)
recent = "(strike a pad)"
MESSAGES = []                                     # recent group/reload lines, newest last
lock = threading.RLock()  # reentrant: a SIGTERM handler may flush while the main thread holds it

# Org-table columns for the sequence: the pad's 4 sensors and their sum at every sampled
# moment. `ms` is milliseconds since the group started. Derived state/velocity live in the
# interpretation, not here. Rows are written UN-aligned (single-space padded) -- hit TAB
# in Emacs to align the columns; no widths are computed here.
SEQ_COLS = ["ms", "s0", "s1", "s2", "s3", "sum"]

# Constants (config keys) restated in each group's interpretation, for reproducibility.
INTERP_CONSTANTS = ["on-sum", "off-sum", "attack-ms", "debounce-ms", "vel-full-scale",
                    "vel-db-range", "event-group-separation-threshold", "group-end-hold-ms",
                    "silence-to-zero-ms"]


def _org_row(cells):
    """One un-aligned org-table row: `| a | b | ... |`. TAB in Emacs aligns the columns."""
    return "| " + " | ".join(str(c) for c in cells) + " |"


def announce(msg):
    """Record a line for the on-screen message panel (call under `lock`)."""
    MESSAGES.append(msg)
    del MESSAGES[:-8]  # keep the last 8


# --------------------------------------------------------------------------- config
def load_config():
    """Re-read config.toml over the defaults. On a parse error keep the current config."""
    cfg = dict(DEFAULTS)
    try:
        with open(CONFIG_PATH, "rb") as f:
            data = tomllib.load(f)
    except FileNotFoundError:
        announce(f"config.toml not found at {CONFIG_PATH}; using defaults")
        data = {}
    except tomllib.TOMLDecodeError as e:
        announce(f"config parse error (kept previous): {e}")
        return
    for k, v in data.items():
        if k in DEFAULTS:
            cfg[k] = v
        else:
            announce(f"ignoring unknown config key: {k}")
    CFG.update(cfg)
    for g in GROUPS.values():
        g.set_params(CFG["event-group-separation-threshold"], CFG["group-end-hold-ms"])
    if CFG["audition"] and not os.path.exists(audition_path()):
        announce(f"audition sample not found: {audition_path()} (no sound will play)")


# --------------------------------------------------------------------------- grouping
def highest_existing_group(logdir):
    """The largest N among existing logs/<N>.org (0 if none) -- so we never overwrite."""
    best = 0
    if os.path.isdir(logdir):
        for name in os.listdir(logdir):
            m = re.fullmatch(r"(\d+)\.org", name)
            if m:
                best = max(best, int(m.group(1)))
    return best


def _vel(peak_sum, full_scale):
    """Sum-of-4 attack peak -> velocity 1..127 (the meter's / kit's mapping)."""
    return max(1, min(127, round(peak_sum / full_scale * 127)))


def _db_of(vel, db_range):
    """The dB below full-scale a velocity plays at (vel 127 -> 0 dB)."""
    return (vel - 1) / 126.0 * db_range - db_range


def interpret_hits(seq, on_sum, off_sum, attack_s, full_scale, debounce_s=0.0):
    """Re-derive the hits a (t, sum) sequence renders under the given constants -- the same
    onset/watch/fire logic as the live meter and the kit. A hit FIRES at onset (the sum
    first exceeds on-sum), so even a one-sample tap counts; its velocity is the PEAK sum
    over the attack window (until release below off-sum, or attack_s elapses), which the kit
    ramps its voice up to. A hit is suppressed if its onset is within `debounce_s` of the
    last fired hit's onset (onset-to-onset, like the kit). Returns (onset_t, velocity)."""
    hits = []
    st, pk, ons = "idle", 0, 0.0
    last_fire = None
    for t, s in seq:
        if st == "idle":
            if s > on_sum:
                st, pk, ons = "watching", s, t
        elif st == "watching":
            pk = max(pk, s)
            if s < off_sum or (t - ons) >= attack_s:   # released, or window elapsed -> finalize
                if last_fire is None or (ons - last_fire) >= debounce_s:
                    hits.append((ons, _vel(pk, full_scale)))
                    last_fire = ons
                st = "held" if s >= off_sum else "idle"
        elif st == "held":
            if s < off_sum:
                st = "idle"
    if st == "watching" and (last_fire is None or (ons - last_fire) >= debounce_s):
        hits.append((ons, _vel(pk, full_scale)))  # trace ended mid-window -> the hit still fired
    return hits


class PadGroup:
    """One pad's event grouping. A group opens when THIS pad's sum-of-4 rises to/above
    `sep` and closes after it has stayed below that for `hold`. Grouping is per pad, so
    several pads can have a group open at once. While open it buffers the pad's sum trace;
    on close it writes logs/pad-<label>/<N>.org (raw sequence + re-derived interpretation)."""

    def __init__(self, label, logdir):
        self.label = label
        self.logdir = logdir  # created lazily on the first group, so idle pads leave no dir
        self.sep = DEFAULTS["event-group-separation-threshold"]
        self.hold = DEFAULTS["group-end-hold-ms"] / 1000.0
        self.num = highest_existing_group(logdir)  # highest N already recorded for this pad
        self.open = False
        self.start = 0.0
        self.samples = []  # (t_rel, s0, s1, s2, s3, sum) while open
        self.last_active = 0.0

    def set_params(self, sep, hold_ms):
        self.sep = sep
        self.hold = hold_ms / 1000.0

    def update(self, sum4, now):
        """Open/refresh/close this pad's group from its current sum-of-4. Under `lock`.
        A pad is "active" while its sum EXCEEDS `sep` and "quiet" at/below it (strict, like
        on-sum/off-sum) -- so sep=0 means "log any nonzero activity".
        With hold == 0 the group closes the instant the sum drops to/below `sep`."""
        if sum4 > self.sep:
            if not self.open:
                self._start(now)
            self.last_active = now
        elif self.open and (now - self.last_active) >= self.hold:
            self._end()

    def sample(self, now, s0, s1, s2, s3, summ):
        """Record one sampled moment (the sum, plus its 4 sensors) while the group is open."""
        if self.open:
            self.samples.append((now - self.start, s0, s1, s2, s3, summ))

    def _start(self, now):
        self.num = max(1, self.num + 1)  # file is written on close; num reserved now
        self.start = now
        self.samples = []
        self.open = True
        announce(f"pad {self.label}: new group (ongoing): {self.num}")

    def _end(self):
        self.open = False
        os.makedirs(self.logdir, exist_ok=True)
        seq = [(t, summ) for (t, _s0, _s1, _s2, _s3, summ) in self.samples]
        hits = interpret_hits(seq, CFG["on-sum"], CFG["off-sum"], CFG["attack-ms"] / 1000.0,
                              CFG["vel-full-scale"], CFG["debounce-ms"] / 1000.0)
        with open(os.path.join(self.logdir, f"{self.num}.org"), "w") as f:
            f.write(f"#+title: pad {self.label} -- event group {self.num}\n\n")
            f.write("* sequence\n")
            f.write(_org_row(SEQ_COLS) + "\n|---|\n")
            for (t, s0, s1, s2, s3, summ) in self.samples:
                f.write(_org_row([f"{t * 1000:.3f}", s0, s1, s2, s3, summ]) + "\n")
            f.write("\n* interpretation\n** constants\n")
            for k in INTERP_CONSTANTS:
                f.write(f"{k} = {CFG[k]}\n")
            f.write("\n** result\n")
            if hits:
                f.write(f"{len(hits)} hit{'s' if len(hits) != 1 else ''}.\n")
                for i, (t, vel) in enumerate(hits, 1):
                    f.write(f"  hit {i}  t={t * 1000:.1f}ms  velocity {vel}  ({_db_of(vel, CFG['vel-db-range']):+.1f} dB)\n")
            else:  # with fire-on-onset, 0 hits means the sum never crossed on-sum at all
                pk = max((summ for (_t, _s0, _s1, _s2, _s3, summ) in self.samples), default=0)
                f.write(f"0 hits -- the sum peaked at {pk}, never crossing on-sum={CFG['on-sum']}.\n")
        announce(f"pad {self.label}: wrote group: {self.num}")

    def flush(self):
        """Close the group if open, writing it to disk. Used by the 'f' command and the
        exit/kill path, so an in-progress group is never lost."""
        if self.open:
            self._end()


# One independent grouping per pad, each logging into logs/pad-<label>/.
GROUPS = {base: PadGroup(label, os.path.join(LOGDIR, f"pad-{label}")) for label, base in LABEL_BASE}


# --------------------------------------------------------------------------- detection
def interp(cc, now):
    """The sensor's INTERPRETED value: its raw reading, or 0 once it has been silent (no
    CC) for longer than `silence-to-zero-ms` -- so a stuck sensor is treated as released to
    0 (the tether stream is on-change, so a stuck sensor simply stops sending and its last
    value would otherwise linger). `silence-to-zero-ms` of 0 disables this (always raw)."""
    silence = CFG["silence-to-zero-ms"] / 1000.0
    if silence > 0 and now - last_seen[cc] > silence:
        return 0
    return sensors[cc]


def pad_sum(base, now):
    return sum(interp(base + k, now) for k in range(4))


def sensor4(base, now):
    return [interp(base + k, now) for k in range(4)]


def velocity_from_sum(peak_sum):
    return _vel(peak_sum, CFG["vel-full-scale"])


def gain_from_velocity(vel):
    """Map velocity 1..127 to a linear gain over `vel-db-range` dB (vel 127 -> 0 dB)."""
    t = (vel - 1) / 126.0
    gain_db = (t - 1.0) * CFG["vel-db-range"]
    return 10.0 ** (gain_db / 20.0)


def check_fire(now):
    """Advance each pad's onset/watch state machine off its interpreted sum and return the
    hits that just completed. A pad starts WATCHING when its sum crosses on-sum, and fires
    ONCE -- at the peak seen -- when it releases below off-sum OR the attack window elapses,
    so even a one-sample tap counts. Mirrors the kit's decoder; the kit fires at onset and
    ramps its voice up to this same peak (pw-play can't ramp, so the meter auditions the
    final peak loudness at window close). A stuck sensor going silent reads 0 via `interp`,
    which trips the release path and re-arms the pad."""
    fired = []
    for base in BASES:
        s = pad_sum(base, now)
        st = state[base]
        if st == "idle":
            if s > CFG["on-sum"]:
                state[base] = "watching"; peak[base] = s; onset[base] = now
        elif st == "watching":
            peak[base] = max(peak[base], s)
            if s < CFG["off-sum"] or (now - onset[base]) * 1000.0 >= CFG["attack-ms"]:
                debounce_s = CFG["debounce-ms"] / 1000.0
                # Debounce is onset-to-onset (like the kit): suppress a hit whose onset is
                # within `debounce-ms` of the last FIRED hit's onset on this pad.
                if last_fire[base] is None or (onset[base] - last_fire[base]) >= debounce_s:
                    vel = velocity_from_sum(peak[base])
                    last_vel[base] = vel
                    last_fire[base] = onset[base]
                    fired.append((base, vel))
                state[base] = "held" if s >= CFG["off-sum"] else "idle"
        elif st == "held":
            if s < CFG["off-sum"]:
                state[base] = "idle"
    return fired


def audition_path():
    """Absolute path of the configured audition sample (a filename under drum-samples/)."""
    return os.path.join(SAMPLES_DIR, CFG["audition-sample"])


def play_audition(label, vel):
    global recent
    gain = gain_from_velocity(vel)
    vol = max(0.0, min(1.0, CFG["master-volume"] * gain))
    db = (vel - 1) / 126 * CFG["vel-db-range"] - CFG["vel-db-range"]
    recent = f"pad {label}: vel {vel:3d}  ({db:+.1f} dB, vol {vol:.2f})"
    if CFG["audition"]:
        try:
            subprocess.Popen(["pw-play", f"--volume={vol:.3f}", audition_path()],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except FileNotFoundError:
            pass  # no pw-play: still shows velocity


def log_events(now, pad_updates, fires):
    """Advance every pad's group, sample the pads that just changed, and audition any
    fired hits. Under `lock`. Groups are per pad and run independently/concurrently. The
    per-group .org (raw sequence + re-derived interpretation) is written when it closes."""
    for base in BASES:
        GROUPS[base].update(pad_sum(base, now), now)  # opens on rise; closes after the hold
    for base in dict.fromkeys(pad_updates):  # one sample per changed pad
        GROUPS[base].sample(now, *sensor4(base, now), pad_sum(base, now))
    for base, vel in fires:
        play_audition(BASE_LABEL[base], vel)


# --------------------------------------------------------------------------- MIDI in
def reader():
    proc = subprocess.Popen(["stdbuf", "-oL", "amidi", "-p", ss.PORT, "-d"],
                            stdout=subprocess.PIPE, text=True, bufsize=1)
    reader.proc = proc
    status = None
    for line in proc.stdout:
        toks = [int(x, 16) for x in line.split() if re.fullmatch(r"[0-9A-Fa-f]{2}", x)]
        i = 0
        pad_updates = []
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
                        last_seen[cc] = now
                        base = 40 + 4 * ((cc - 40) // 4) if 40 <= cc <= 79 else None
                        if base in state:
                            pad_updates.append(base)
                else:
                    break
            log_events(now, pad_updates, check_fire(now))


# --------------------------------------------------------------------------- display
def draw():
    now = time.monotonic()
    with lock:
        # Fire matured hits and close a group that has gone quiet, even with no new MIDI
        # (the tether stream is on-change only, so it falls silent after a release).
        log_events(now, [], check_fire(now))
        ongoing = sum(1 for g in GROUPS.values() if g.open)
        lines = ["", f"  KMSS velocity meter -- strike a pad: bar=pressure, plays {CFG['audition-sample']} at that velocity.",
                 f"  keys: [r]eload  [f]lush  [q]uit  (Ctrl-C / kill also flush+exit)   |   {ongoing} group(s) ongoing", ""]
        for label, base in LABEL_BASE:
            s = pad_sum(base, now)
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
            g = GROUPS[base]
            gs = f"g{g.num}*" if g.open else (f"g{g.num}" if g.num else "-")  # * = ongoing
            lines.append(f"  pad {label}  [{''.join(bar)}] {s:3d}  {lvs}  {gs:>5}")
        lines += ["", f"  last hit: {recent}",
                  f"  (on-sum={CFG['on-sum']} off-sum={CFG['off-sum']} attack={CFG['attack-ms']}ms "
                  f"debounce={CFG['debounce-ms']}ms full-scale={CFG['vel-full-scale']} db-range={CFG['vel-db-range']:g} "
                  f"sep={CFG['event-group-separation-threshold']} hold={CFG['group-end-hold-ms']}ms "
                  f"silence={CFG['silence-to-zero-ms']}ms audition={'on' if CFG['audition'] else 'off'})", ""]
        for m in MESSAGES:
            lines.append(f"    {m}")
    sys.stdout.write("\033[2J\033[H" + "\n".join(lines) + "\n")
    sys.stdout.flush()


def flush_groups(reason):
    """Close every open group, writing each to disk. The meter keeps running afterward (a
    new group opens on the pad's next activity); this is also the exit/kill path."""
    with lock:
        n = sum(1 for g in GROUPS.values() if g.open)
        for g in GROUPS.values():
            g.flush()
        announce(f"{reason}: flushed {n} open group(s)")


def handle_key(ch):
    """Return False to quit, True to keep running."""
    if ch in ("q", "\x03", "\x04"):
        return False
    if ch == "r":
        with lock:
            load_config()
            announce("reloaded config.toml")
    elif ch == "f":
        flush_groups("flush (f)")
    return True


def announce_rust_reference():
    """Startup banner: the Rust meter (code/rust/midi_pulse/softstep_meter.rs) now
    drives the SAME decode.rs the drumkit runtime uses -- exactly its onset/release
    thresholds, attack window, debounce, de-stick, and velocity mapping (including
    the ditto pad) -- so it is the more accurate reference for calibration. This
    Python meter is kept for comparison and its .org event-group logger (which the
    Rust meter does not have)."""
    print("=" * 78)
    print("  NOTE: code/rust/midi_pulse/softstep_meter.rs is now the reference")
    print("  implementation -- it drives the exact same decoder (decode.rs) the")
    print("  drumkit runtime plays through, so its detection/velocity/ditto reading")
    print("  is more likely to match what you'll actually hear. Run it (from the repo")
    print("  root, where Cargo.toml lives) with:")
    print("      cargo run --bin softstep_meter")
    print("  (add `pw-jack` in front if you turn on its optional audio audition,")
    print("  SOFTSTEP_METER_AUDIO=1 -- see its --help/source for details.)")
    print("  This Python meter still works and remains here for comparison (and for")
    print("  its per-pad .org event-group logger, which the Rust meter lacks).")
    print("=" * 78)


def main():
    announce_rust_reference()
    load_config()  # also warns (in the message panel) if the audition sample is missing
    limit = float(sys.argv[1]) if len(sys.argv) > 1 else None

    interactive = sys.stdin.isatty()
    old_term = termios.tcgetattr(sys.stdin.fileno()) if interactive else None
    if interactive:
        tty.setcbreak(sys.stdin.fileno())

    done = []

    def cleanup():
        """Flush open groups + restore the terminal/device. Idempotent; runs on q, Ctrl-C,
        the time limit, an error, OR a SIGTERM/SIGHUP kill -- so groups are never lost."""
        if done:
            return
        done.append(True)
        if interactive and old_term is not None:
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, old_term)
        try:
            if getattr(reader, "proc", None):
                reader.proc.terminate()
        except Exception:
            pass
        flush_groups("exit")            # close + write every open group before we go
        time.sleep(0.2)
        ss.restore_standalone()
        sys.stdout.write("\nRestored standalone mode.\n")

    # Flush + restore even when killed (SIGTERM/SIGHUP), not just on q / Ctrl-C.
    for sig in (signal.SIGTERM, signal.SIGHUP):
        try:
            signal.signal(sig, lambda *_a: (cleanup(), sys.exit(0)))
        except (ValueError, OSError):
            pass  # signals are only settable from the main thread / on supported platforms

    ss.enter_tether()
    threading.Thread(target=reader, daemon=True).start()
    start = time.monotonic()
    try:
        while True:
            draw()
            if interactive and select.select([sys.stdin], [], [], 0.02)[0]:
                if not handle_key(sys.stdin.read(1)):
                    break
            else:
                time.sleep(0.02)
            if limit and time.monotonic() - start > limit:
                break
    except KeyboardInterrupt:
        pass
    finally:
        cleanup()


if __name__ == "__main__":
    main()
