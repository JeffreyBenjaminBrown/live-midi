#!/usr/bin/env python3
"""Live DUAL-SoftStep sensor meter + per-hit pressure + CSV / .org logging.

We now have TWO SoftSteps (see ss.list_softsteps): they enumerate differently, so the
code tells them apart by ALSA port name -- the original "SSCOM" unit and the newer
"SoftStep" unit. This meter puts EVERY connected SoftStep into tether mode and reads
them all at once.

To keep the screen small with two boards' worth of pads, the live display shows only
the LAST TWO PADS PRESSED (across both boards). Each is one row of all five values --
the pad's 4 individual sensors s0 s1 s2 s3 and their sum -- tagged with WHICH board and
WHICH pad it is. (The older single-board meter drew a bar per pad; that got to 20 bars
with two boards, hence this compact view.)

  python3 code/python/softstep/meter/main.py          # run until 'q' or Ctrl-C
  python3 code/python/softstep/meter/main.py 5         # run ~5s then exit (for testing)

While it runs:
  r   reload config.toml (thresholds, dB range, group settings) live
  f   flush open .org event groups to disk now
  q   quit (Ctrl-C also works)

LOGGING, two forms, both under meter/logs/:
  * CSV  (csv/session-<stamp>.csv) -- one row per sensor frame and per fired hit, across
    BOTH boards: t_iso,t_ms,device,kind,pad,s0,s1,s2,s3,sum,event,pressure. This is the
    "deep dive" log -- load it in a spreadsheet / pandas. `event` is blank for a plain
    sample and "fire" on the frame a hit's onset fires (pressure 0..1 filled in then).
  * .org event groups (<device>/pad-<label>/<N>.org) -- the older per-pad grouped trace
    + re-derived hit interpretation, now namespaced per board. See below.

CONFIG: code/python/softstep/meter/config.toml -- all tunables, hot-reloadable.

EVENT GROUPS: grouping is PER PAD PER BOARD -- each pad has its own independent groups.
A pad's group starts when ITS sum-of-4 rises above `event-group-separation-threshold`
and ends once it has stayed at/below that for `group-end-hold-ms`. Each group is written
to logs/<device>/pad-<label>/<N>.org (raw sequence + re-derived interpretation).

HOW PRESSURE SCALES LOUDNESS: loudness is perceived ~logarithmically, so full pressure is
NOT 10x the amplitude of a light press -- the pressure range is spread over `gain-db-range`
dB of gain (pressure 1.0 -> 0 dB, 0.0 -> -`gain-db-range` dB).
"""
import os, sys, time, threading, subprocess, re, signal, tomllib, select, termios, tty, csv, datetime
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))  # find sibling ss.py
import ss

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG_PATH = os.path.join(HERE, "config.toml")
LOGDIR = os.path.join(HERE, "logs")
CSVDIR = os.path.join(LOGDIR, "csv")
REPO_ROOT = os.path.abspath(os.path.join(HERE, "..", "..", "..", ".."))  # meter/ -> repo root
SAMPLES_DIR = os.path.join(REPO_ROOT, "drum-samples")  # `audition-sample` resolves under here

# Defaults for every config key. config.toml overrides these; an unknown key there is
# ignored (with a warning). Keys are hyphenated to match requests.org's naming.
DEFAULTS = {
    "on-sum": 40,           # onset threshold on sum-of-4 (a hit starts above this)
    "off-sum": 20,          # release threshold (hysteresis: must drop below to re-arm)
    "attack-ms": 14,        # fire on onset; keep watching this long to raise pressure to a later peak
    "debounce-ms": 100,     # min gap between two hits on the SAME pad (contact-bounce guard); 0 = off
    "pressure-full-scale": 460,  # pad sum-of-4 that maps to full pressure (1.0)
    "gain-db-range": 20.0,   # dB between the softest (pressure 0) and hardest (pressure 1) hit
    "master-volume": 0.9,   # ceiling passed to pw-play (0..1)
    "audition": True,       # play a sample on each detected hit
    "audition-sample": "snare.wav",  # which sample to audition (a filename under drum-samples/)
    "event-group-separation-threshold": 20,  # sum-of-4 below this = that pad is "quiet"
    "group-end-hold-ms": 150,                # board stays quiet this long -> close group
    "silence-to-zero-ms": 25,                # a sensor with no CC for this long reads 0 (de-stick); 0 = off
    "csv": True,                             # write the deep-dive CSV log (csv/session-*.csv)
}
CFG = dict(DEFAULTS)

# printed label (as on the board) -> base CC of that pad's 4 sensors (MEASURED on the
# SSCOM unit; CONFIRMED identical on the newer "SoftStep" unit 2026-07-15 by a guided
# per-pad capture -- both boards use this exact map. See learnings/keith-mcmillen-softstep.org).
LABEL_BASE = [("1", 44), ("2", 52), ("3", 60), ("4", 68), ("5", 76),
              ("6", 40), ("7", 48), ("8", 56), ("9", 64), ("0", 72)]
BASE_LABEL = {b: l for l, b in LABEL_BASE}
BASES = [b for _, b in LABEL_BASE]
SUM_MAX = 508.0

recent = "(strike a pad)"
MESSAGES = []                                     # recent group/reload lines, newest last
LAST_PRESSED = []                                 # [(dev_index, base)] most-recent-first, len<=2
lock = threading.RLock()  # reentrant: a SIGTERM handler may flush while the main thread holds it

# Org-table columns for the .org sequence: the pad's 4 sensors and their sum at every
# sampled moment. `ms` is milliseconds since the group started.
SEQ_COLS = ["ms", "s0", "s1", "s2", "s3", "sum"]

# Constants (config keys) restated in each group's interpretation, for reproducibility.
INTERP_CONSTANTS = ["on-sum", "off-sum", "attack-ms", "debounce-ms", "pressure-full-scale",
                    "gain-db-range", "event-group-separation-threshold", "group-end-hold-ms",
                    "silence-to-zero-ms"]

# CSV columns for the deep-dive log.
CSV_COLS = ["t_iso", "t_ms", "device", "kind", "pad", "s0", "s1", "s2", "s3", "sum", "event", "pressure"]


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
    for dev in DEVICES:
        for g in dev.groups.values():
            g.set_params(CFG["event-group-separation-threshold"], CFG["group-end-hold-ms"])
    if CFG["audition"] and not os.path.exists(audition_path()):
        announce(f"audition sample not found: {audition_path()} (no sound will play)")


# --------------------------------------------------------------------------- pressure/gain math
def _pressure(peak_sum, full_scale):
    """Sum-of-4 attack peak -> continuous pressure 0.0..1.0 (the meter's / kit's mapping).
    No quantization -- matches the Rust decoder's `pressure_from_peak`."""
    return max(0.0, min(1.0, peak_sum / max(1, full_scale)))


def _db_of(pressure, db_range):
    """The dB below full-scale a pressure plays at (pressure 1.0 -> 0 dB, 0.0 -> -db_range)."""
    return (max(0.0, min(1.0, pressure)) - 1.0) * db_range


def pressure_from_sum(peak_sum):
    return _pressure(peak_sum, CFG["pressure-full-scale"])


def gain_from_pressure(pressure):
    """Map pressure 0.0..1.0 to a linear gain over `gain-db-range` dB (pressure 1.0 -> 0 dB).
    Matches the Rust `gain_from_pressure`."""
    return 10.0 ** (_db_of(pressure, CFG["gain-db-range"]) / 20.0)


def interpret_hits(seq, on_sum, off_sum, attack_s, full_scale, debounce_s=0.0):
    """Re-derive the hits a (t, sum) sequence renders under the given constants -- the same
    onset/watch/fire logic as the live meter and the kit. A hit FIRES at onset (the sum
    first exceeds on-sum), so even a one-sample tap counts; its pressure is the PEAK sum
    over the attack window (until release below off-sum, or attack_s elapses) / full scale,
    which the kit ramps its voice up to. A hit is suppressed if its onset is within
    `debounce_s` of the last fired hit's onset (onset-to-onset, like the kit).
    Returns (onset_t, pressure)."""
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
                    hits.append((ons, _pressure(pk, full_scale)))
                    last_fire = ons
                st = "held" if s >= off_sum else "idle"
        elif st == "held":
            if s < off_sum:
                st = "idle"
    if st == "watching" and (last_fire is None or (ons - last_fire) >= debounce_s):
        hits.append((ons, _pressure(pk, full_scale)))  # trace ended mid-window -> the hit still fired
    return hits


# --------------------------------------------------------------------------- .org grouping
def highest_existing_group(logdir):
    """The largest N among existing logs/<N>.org (0 if none) -- so we never overwrite."""
    best = 0
    if os.path.isdir(logdir):
        for name in os.listdir(logdir):
            m = re.fullmatch(r"(\d+)\.org", name)
            if m:
                best = max(best, int(m.group(1)))
    return best


class PadGroup:
    """One pad's event grouping (on one board). A group opens when THIS pad's sum-of-4
    rises above `sep` and closes after it has stayed at/below that for `hold`. While open
    it buffers the pad's sum trace; on close it writes logs/<device>/pad-<label>/<N>.org."""

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
        """Open/refresh/close this pad's group from its current sum-of-4. Under `lock`."""
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
        announce(f"{os.path.basename(self.logdir)}: new group (ongoing): {self.num}")

    def _end(self):
        self.open = False
        os.makedirs(self.logdir, exist_ok=True)
        seq = [(t, summ) for (t, _s0, _s1, _s2, _s3, summ) in self.samples]
        hits = interpret_hits(seq, CFG["on-sum"], CFG["off-sum"], CFG["attack-ms"] / 1000.0,
                              CFG["pressure-full-scale"], CFG["debounce-ms"] / 1000.0)
        with open(os.path.join(self.logdir, f"{self.num}.org"), "w") as f:
            f.write(f"#+title: {os.path.basename(self.logdir)} -- event group {self.num}\n\n")
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
                for i, (t, pressure) in enumerate(hits, 1):
                    f.write(f"  hit {i}  t={t * 1000:.1f}ms  pressure {pressure:.3f}  ({_db_of(pressure, CFG['gain-db-range']):+.1f} dB)\n")
            else:  # with fire-on-onset, 0 hits means the sum never crossed on-sum at all
                pk = max((summ for (_t, _s0, _s1, _s2, _s3, summ) in self.samples), default=0)
                f.write(f"0 hits -- the sum peaked at {pk}, never crossing on-sum={CFG['on-sum']}.\n")
        announce(f"{os.path.basename(self.logdir)}: wrote group: {self.num}")

    def flush(self):
        """Close the group if open, writing it to disk (the 'f' command / exit path)."""
        if self.open:
            self._end()


# --------------------------------------------------------------------------- CSV log
class CsvLog:
    """The append-only deep-dive CSV, shared by every board's reader (guarded by `lock`)."""

    def __init__(self):
        self.f = None
        self.w = None
        self.path = None

    def open(self):
        if not CFG["csv"]:
            return
        os.makedirs(CSVDIR, exist_ok=True)
        stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        self.path = os.path.join(CSVDIR, f"session-{stamp}.csv")
        self.f = open(self.path, "w", newline="")
        self.w = csv.writer(self.f)
        self.w.writerow(CSV_COLS)
        self.f.flush()

    def row(self, now, dev, base, s4, summ, event="", pressure=""):
        """One CSV line: a sensor frame (event="") or a fired hit (event="fire", pressure set)."""
        if self.w is None:
            return
        self.w.writerow([datetime.datetime.now().isoformat(timespec="milliseconds"),
                         f"{now * 1000:.1f}", dev.label, dev.kind, BASE_LABEL[base],
                         s4[0], s4[1], s4[2], s4[3], summ, event,
                         f"{pressure:.4f}" if pressure != "" else ""])
        self.f.flush()

    def close(self):
        if self.f:
            self.f.flush()
            self.f.close()
            self.f = self.w = None


CSVLOG = CsvLog()


# --------------------------------------------------------------------------- a board
class Device:
    """One SoftStep board: its own raw sensor shadow, per-pad detection state machine,
    and per-pad .org event groups. `kind` is "sscom"/"softstep" (how the two units
    enumerate); `label` is the short display tag; `port` is the amidi rawmidi port."""

    def __init__(self, index, info):
        self.index = index
        self.label = info["label"]
        self.kind = info["kind"]
        self.port = info["port"]
        self.name = info["name"]
        self.sensors = [0] * 128                    # raw last-received value per CC
        self.last_seen = [0.0] * 128                # monotonic time each CC was last received
        self.state = {b: "idle" for b in BASES}     # idle / watching / held
        self.peak = {b: 0 for b in BASES}            # attack peak (sum) while watching
        self.onset = {b: 0.0 for b in BASES}         # monotonic time of onset
        self.last_pressure = {b: 0.0 for b in BASES}  # last fired pressure, for display
        self.last_fire = {b: None for b in BASES}    # onset time of last FIRED hit (debounce gate)
        self.proc = None                             # amidi reader subprocess
        base_dir = os.path.join(LOGDIR, self.label.lower())
        self.groups = {b: PadGroup(l, os.path.join(base_dir, f"pad-{l}")) for l, b in LABEL_BASE}

    # --- interpreted sensor reads -------------------------------------------------
    def interp(self, cc, now):
        """The sensor's INTERPRETED value: its raw reading, or 0 once it has been silent
        (no CC) for longer than `silence-to-zero-ms` -- so a stuck sensor reads released."""
        silence = CFG["silence-to-zero-ms"] / 1000.0
        if silence > 0 and now - self.last_seen[cc] > silence:
            return 0
        return self.sensors[cc]

    def sensor4(self, base, now):
        return [self.interp(base + k, now) for k in range(4)]

    def pad_sum(self, base, now):
        return sum(self.interp(base + k, now) for k in range(4))

    # --- detection ----------------------------------------------------------------
    def check_fire(self, now):
        """Advance each pad's onset/watch state machine off its interpreted sum and return
        the hits that just completed as (base, pressure). Mirrors the kit's decoder."""
        fired = []
        for base in BASES:
            s = self.pad_sum(base, now)
            st = self.state[base]
            if st == "idle":
                if s > CFG["on-sum"]:
                    self.state[base] = "watching"; self.peak[base] = s; self.onset[base] = now
            elif st == "watching":
                self.peak[base] = max(self.peak[base], s)
                if s < CFG["off-sum"] or (now - self.onset[base]) * 1000.0 >= CFG["attack-ms"]:
                    debounce_s = CFG["debounce-ms"] / 1000.0
                    if self.last_fire[base] is None or (self.onset[base] - self.last_fire[base]) >= debounce_s:
                        pressure = pressure_from_sum(self.peak[base])
                        self.last_pressure[base] = pressure
                        self.last_fire[base] = self.onset[base]
                        fired.append((base, pressure))
                    self.state[base] = "held" if s >= CFG["off-sum"] else "idle"
            elif st == "held":
                if s < CFG["off-sum"]:
                    self.state[base] = "idle"
        return fired

    def process(self, now, pad_updates):
        """Advance groups, log CSV samples for changed pads, fire matured hits. Under `lock`.
        Returns the hits fired this pass as (base, pressure)."""
        for base in BASES:
            self.groups[base].update(self.pad_sum(base, now), now)     # opens on rise; closes after hold
        for base in dict.fromkeys(pad_updates):                        # one sample per changed pad
            s4 = self.sensor4(base, now); summ = sum(s4)
            self.groups[base].sample(now, *s4, summ)
            CSVLOG.row(now, self, base, s4, summ)
        fired = self.check_fire(now)
        for base, pressure in fired:
            s4 = self.sensor4(base, now)
            CSVLOG.row(now, self, base, s4, sum(s4), event="fire", pressure=pressure)
            note_press(self.index, base)
            play_audition(self.label, BASE_LABEL[base], pressure)
        return fired

    # --- MIDI in ------------------------------------------------------------------
    def reader(self):
        """Read this board's tether CC stream via `amidi -p PORT -d` and drive detection."""
        self.proc = subprocess.Popen(["stdbuf", "-oL", "amidi", "-p", self.port, "-d"],
                                     stdout=subprocess.PIPE, text=True, bufsize=1)
        status = None
        for line in self.proc.stdout:
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
                            self.sensors[cc] = val
                            self.last_seen[cc] = now
                            base = 40 + 4 * ((cc - 40) // 4) if 40 <= cc <= 79 else None
                            if base in self.state:
                                pad_updates.append(base)
                    else:
                        break
                self.process(now, pad_updates)


DEVICES = []  # populated in main() from ss.list_softsteps()


# --------------------------------------------------------------------------- shared events
def note_press(dev_index, base):
    """Record a pad press for the compact display: keep the last two DISTINCT (board, pad)
    pairs, most-recent first. Under `lock`."""
    key = (dev_index, base)
    if key in LAST_PRESSED:
        LAST_PRESSED.remove(key)
    LAST_PRESSED.insert(0, key)
    del LAST_PRESSED[2:]


def audition_path():
    """Absolute path of the configured audition sample (a filename under drum-samples/)."""
    return os.path.join(SAMPLES_DIR, CFG["audition-sample"])


def play_audition(dev_label, pad_label, pressure):
    global recent
    gain = gain_from_pressure(pressure)
    vol = max(0.0, min(1.0, CFG["master-volume"] * gain))
    db = _db_of(pressure, CFG["gain-db-range"])
    recent = f"{dev_label} pad {pad_label}: p {pressure:.2f}  ({db:+.1f} dB, vol {vol:.2f})"
    if CFG["audition"]:
        try:
            subprocess.Popen(["pw-play", f"--volume={vol:.3f}", audition_path()],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except FileNotFoundError:
            pass  # no pw-play: still shows pressure


# --------------------------------------------------------------------------- display
def render_row(dev_index, base, now):
    """One compact display row for a pressed pad: board, pad, its 4 live sensors + sum."""
    dev = DEVICES[dev_index]
    s4 = dev.sensor4(base, now)
    summ = sum(s4)
    lp = dev.last_pressure[base]
    lps = f"p {lp:.2f}" if lp else "  --  "
    g = dev.groups[base]
    gs = f"g{g.num}*" if g.open else (f"g{g.num}" if g.num else "-")
    return (f"  {dev.label:<10} [{BASE_LABEL[base]}]  "
            f"{s4[0]:4d} {s4[1]:4d} {s4[2]:4d} {s4[3]:4d}   {summ:4d}   {lps}  {gs:>5}")


def draw():
    now = time.monotonic()
    with lock:
        # Fire matured hits and close quiet groups even with no new MIDI (the tether stream
        # is on-change only, so it falls silent after a release).
        for dev in DEVICES:
            dev.process(now, [])
        ongoing = sum(1 for dev in DEVICES for g in dev.groups.values() if g.open)
        csv_note = f"CSV {os.path.relpath(CSVLOG.path, REPO_ROOT)}" if CSVLOG.path else "CSV off"
        lines = ["",
                 "  KMSS DUAL meter -- reads every SoftStep; rows = the LAST TWO pads pressed",
                 f"  (each row: board  [pad]  s0 s1 s2 s3  sum  last-pressure).  Plays {CFG['audition-sample']} on a hit.",
                 f"  keys: [r]eload  [f]lush  [q]uit   |   {ongoing} group(s) ongoing   |   {csv_note}",
                 "",
                 f"  {'board':<10} {'pad':>4}  {'s0':>4} {'s1':>4} {'s2':>4} {'s3':>4}   {'sum':>4}   {'press':>6}  {'grp':>5}"]
        if LAST_PRESSED:
            for dev_index, base in LAST_PRESSED:
                lines.append(render_row(dev_index, base, now))
            for _ in range(2 - len(LAST_PRESSED)):
                lines.append("  (waiting for another pad...)")
        else:
            lines.append("  (press a pad on either board...)")
            lines.append("")
        lines += ["",
                  "  boards: " + " | ".join(f"{d.label} {d.port} ({d.kind})" for d in DEVICES),
                  f"  last hit: {recent}",
                  f"  (on-sum={CFG['on-sum']} off-sum={CFG['off-sum']} attack={CFG['attack-ms']}ms "
                  f"debounce={CFG['debounce-ms']}ms full-scale={CFG['pressure-full-scale']} db-range={CFG['gain-db-range']:g} "
                  f"sep={CFG['event-group-separation-threshold']} hold={CFG['group-end-hold-ms']}ms "
                  f"silence={CFG['silence-to-zero-ms']}ms audition={'on' if CFG['audition'] else 'off'})", ""]
        for m in MESSAGES:
            lines.append(f"    {m}")
    sys.stdout.write("\033[2J\033[H" + "\n".join(lines) + "\n")
    sys.stdout.flush()


def flush_groups(reason):
    """Close every open group on every board, writing each to disk."""
    with lock:
        n = sum(1 for dev in DEVICES for g in dev.groups.values() if g.open)
        for dev in DEVICES:
            for g in dev.groups.values():
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


def main():
    infos = ss.list_softsteps()
    if not infos:
        print("No SoftStep found. Check `amidi -l` shows an 'SSCOM MIDI 1' or "
              "'SoftStep Control Surface' port.")
        sys.exit(1)
    for i, info in enumerate(infos):
        DEVICES.append(Device(i, info))

    print("=" * 78)
    print(f"  KMSS dual meter -- {len(DEVICES)} SoftStep(s):")
    for d in DEVICES:
        print(f"    [{d.label}] {d.port}  kind={d.kind}  ({d.name})")
    print("  Rust reference meter (single board, drives decode.rs): cargo run --bin softstep_meter")
    print("=" * 78)

    load_config()  # also warns (in the message panel) if the audition sample is missing
    CSVLOG.open()
    limit = float(sys.argv[1]) if len(sys.argv) > 1 else None

    interactive = sys.stdin.isatty()
    old_term = termios.tcgetattr(sys.stdin.fileno()) if interactive else None
    if interactive:
        tty.setcbreak(sys.stdin.fileno())

    done = []

    def cleanup():
        """Flush groups, close CSV, restore each board + the terminal. Idempotent; runs on
        q, Ctrl-C, the time limit, an error, OR a SIGTERM/SIGHUP kill."""
        if done:
            return
        done.append(True)
        if interactive and old_term is not None:
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, old_term)
        for dev in DEVICES:
            try:
                if dev.proc:
                    dev.proc.terminate()
            except Exception:
                pass
        flush_groups("exit")            # close + write every open group before we go
        CSVLOG.close()
        time.sleep(0.2)
        for dev in DEVICES:
            ss.restore_standalone(port=dev.port)
        sys.stdout.write("\nRestored standalone mode on all boards.\n")
        if CSVLOG.path:
            sys.stdout.write(f"CSV log: {CSVLOG.path}\n")

    # Flush + restore even when killed (SIGTERM/SIGHUP), not just on q / Ctrl-C.
    for sig in (signal.SIGTERM, signal.SIGHUP):
        try:
            signal.signal(sig, lambda *_a: (cleanup(), sys.exit(0)))
        except (ValueError, OSError):
            pass  # signals are only settable from the main thread / on supported platforms

    for dev in DEVICES:
        ss.enter_tether(port=dev.port)
    for dev in DEVICES:
        threading.Thread(target=dev.reader, daemon=True).start()
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
