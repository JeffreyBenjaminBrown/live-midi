#!/usr/bin/env python3
"""Guided KMSS capture session -- the ONE batched manual step.

Fully SELF-PACED: each step prints an instruction and then waits for you to press
Enter -- it never times you. Do the named action, press Enter, on to the next. It
writes a single time-stamped log that Claude reads directly, puts the device into
tether mode for the sensor phases, and ALWAYS restores standalone mode at the end
(even on Ctrl-C / crash). No firmware is touched.

  python3 tools/softstep/capture.py --board softstep S   # scan-rate timing, newer board
  python3 tools/softstep/capture.py --board sscom   S     # scan-rate timing, original board
  python3 tools/softstep/capture.py --board softstep      # all phases A, B, C on the newer board
  python3 tools/softstep/capture.py A B                   # some phases (one board connected)

With two boards connected you MUST pass --board sscom|softstep so a log is unambiguously
one unit (a bare `sscom` / `softstep` token works too). With a single board connected it
is optional.

Phases:
  A  tether: per-pad map (press 1..9 then 0) + dynamics (soft/med/hard on pad 1).
     -> gives us which CCs belong to which printed pad, and the pressure range
        from which velocity is derived.
  B  tether: TWO FEET at once -- proves two simultaneous pads both stream.
  C  standalone: the same two-pad stomp in normal Program-Change mode -- settles
     whether standalone really drops one of two simultaneous hits.
  S  tether: TIMING -- slow pressure rolls + a hold-with-wiggle + taps on ONE pad, so
     consecutive sensor frames keep landing at the device scan rate. The delta between
     frames is the refresh interval (~Hz); the taps give onset->peak latency. Run it
     once per board to compare the two units.

Output: tools/softstep/captures/session-<timestamp>-<board>.log
Tell Claude when it's done; it reads the log from disk.
"""
import os, sys, time, signal, threading, subprocess, datetime
import ss

LOGDIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "captures")

_reader = None      # current amidi reader (so cleanup can stop it)
_logf = None
_path = None
_cleaned = False
_port = None        # the chosen board's hw:card,dev,sub port (see --board / resolve_board)
_board = None       # the chosen board's device dict (kind/label/port/name), for the header


class Reader:
    """Runs `amidi -p PORT -d -T monotonic` and appends each line to the log as DATA."""
    def __init__(self, logf, port):
        self.logf, self.port, self.proc, self.thread = logf, port, None, None

    def start(self):
        # stdbuf -oL forces amidi to flush each line immediately; without it amidi
        # block-buffers stdout when piped and live readings wouldn't reach the log.
        self.proc = subprocess.Popen(
            ["stdbuf", "-oL", "amidi", "-p", self.port, "-d", "-T", "monotonic"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
        self.thread = threading.Thread(target=self._pump, daemon=True)
        self.thread.start()
        time.sleep(0.3)  # let it open the port before we prompt

    def _pump(self):
        for line in self.proc.stdout:
            line = line.rstrip("\n")
            if line.strip():
                self.logf.write(f"DATA {line}\n"); self.logf.flush()

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try: self.proc.wait(timeout=2)
            except subprocess.TimeoutExpired: self.proc.kill()
        if self.thread:
            self.thread.join(timeout=2)
        time.sleep(0.2)  # let the port free before any send


def mark(text):
    """Write a timestamped boundary into the log. Data captured AFTER this mark and
    before the next mark belongs to this step."""
    t = time.monotonic()
    _logf.write(f"MARK {t:.6f} {text}\n"); _logf.flush()


def step(label, instruction):
    """Self-paced: mark the boundary, show the instruction, wait for Enter. No timer."""
    mark(label)
    print(f"\n  {instruction}")
    try:
        input("    (do it, then press Enter) ")
    except EOFError:
        pass


def phase_A():
    global _reader
    print("\n========== PHASE A: per-pad map + dynamics (tether) ==========")
    print("For each prompt: do the action, then press Enter. No rush -- it waits for you.")
    input("Take any foot OFF the board, then press Enter to begin... ")
    ss.enter_tether(port=_port)
    _reader = Reader(_logf, _port); _reader.start()
    mark("PHASE_A begin tether")

    print("\n-- A1: press each named pad ONCE (a clear press, then release) --")
    for lab in [1, 2, 3, 4, 5, 6, 7, 8, 9, 0]:
        step(f"A1 pad {lab}", f"Press the pad LABELED {lab}  (press, then release).")

    print("\n-- A2: dynamics on PAD 1 -- vary the force; this calibrates velocity --")
    for force in ["SOFT", "MEDIUM", "HARD", "SOFT", "MEDIUM", "HARD"]:
        step(f"A2 pad1 {force}", f"Tap PAD 1 -- {force}.")

    mark("PHASE_A end")
    _reader.stop(); _reader = None
    ss.restore_standalone(port=_port)
    print("\nPhase A done.")


def phase_B():
    global _reader
    print("\n========== PHASE B: TWO FEET at once (tether) ==========")
    input("Foot off the board, then press Enter to begin... ")
    ss.enter_tether(port=_port)
    _reader = Reader(_logf, _port); _reader.start()
    mark("PHASE_B begin tether")
    print("\nStomp BOTH named pads as SIMULTANEOUSLY as you can, hard, then release.")
    for a, b in [(1, 3), (1, 3), (4, 6), (4, 6), (2, 9), (2, 9)]:
        step(f"B stomp {a}+{b}", f"Stomp PADS {a} AND {b} TOGETHER (hard), then release.")
    mark("PHASE_B end")
    _reader.stop(); _reader = None
    ss.restore_standalone(port=_port)
    print("\nPhase B done.")


def phase_C():
    global _reader
    print("\n========== PHASE C: standalone two-pad Program-Change test ==========")
    input("Foot off the board, then press Enter to begin... ")
    ss.restore_standalone(port=_port)        # make sure we're in normal PC mode
    _reader = Reader(_logf, _port); _reader.start()
    mark("PHASE_C begin standalone")
    print("\nTwo pads at once in NORMAL mode -- do we get two Program Changes or one?")
    for a, b in [(1, 3), (1, 3), (4, 6), (4, 6)]:
        step(f"C stomp {a}+{b}", f"Stomp PADS {a} AND {b} TOGETHER, then release.")
    print("\nControl: single taps (should be one Program Change each).")
    for lab in [1, 2, 3]:
        step(f"C single {lab}", f"Tap pad {lab} once.")
    mark("PHASE_C end")
    _reader.stop(); _reader = None
    print("\nPhase C done.")


def phase_S():
    """TIMING: a slow pressure roll and a hold-with-wiggle keep the reading CHANGING, so
    the on-change tether stream emits a frame every scan -- consecutive-frame deltas are
    the device refresh interval. The taps add onset->peak latency. Run per board."""
    global _reader
    print("\n========== PHASE S: scan-rate + latency timing (tether) ==========")
    print("One pad only (PAD 1). Keep the pressure CHANGING during the rolls/hold so every")
    print("scan sends a frame -- a perfectly still hold streams nothing (the stream is on-change).")
    input("Foot off the board, then press Enter to begin... ")
    ss.enter_tether(port=_port)
    _reader = Reader(_logf, _port); _reader.start()
    mark("PHASE_S begin tether")
    step("S roll 1", "PAD 1: press SLOWLY from light to HARD and back, ~3 seconds, one smooth roll.")
    step("S roll 2", "PAD 1 again: another slow light -> hard -> light roll, ~3 seconds.")
    step("S hold+wiggle", "Press PAD 1 and HOLD ~4 s, wiggling the pressure a little the WHOLE time.")
    step("S taps", "Tap PAD 1 five times -- crisp and HARD -- with a clear gap between each.")
    mark("PHASE_S end")
    _reader.stop(); _reader = None
    ss.restore_standalone(port=_port)
    print("\nPhase S done.")


def cleanup(*_a):
    global _cleaned
    if _cleaned:
        return
    _cleaned = True
    try:
        if _reader:
            _reader.stop()
        ss.restore_standalone(port=_port)
    finally:
        if _logf:
            _logf.flush(); _logf.close()
        print(f"\nRestored standalone mode. Log saved:\n  {_path}")
        print("Tell Claude: the capture is done.")


def parse_args(argv):
    """Split argv into (board_kind, phases). Board: `--board KIND`, `--board=KIND`, `-b
    KIND`, or a bare `sscom` / `softstep` token. Phases: any of A/B/C/S (default A B C)."""
    board, phases, it = None, [], iter(argv)
    for tok in it:
        low = tok.lower()
        if low in ("sscom", "softstep"):
            board = low
        elif low in ("--board", "-b"):
            board = (next(it, "") or "").lower()
        elif low.startswith("--board="):
            board = low.split("=", 1)[1]
        elif tok.upper() in ("A", "B", "C", "S"):
            phases.append(tok.upper())
        else:
            sys.exit(f"unrecognized argument {tok!r}\n"
                     "  usage: capture.py [--board sscom|softstep] [A B C S]")
    if board not in (None, "sscom", "softstep"):
        sys.exit(f"--board must be 'sscom' or 'softstep', got {board!r}")
    return board, (phases or ["A", "B", "C"])


def resolve_board(board_kind):
    """Pick which SoftStep to capture, returning its device dict. Requires an explicit
    --board when two units are connected, so a log is never silently the wrong board."""
    devs = ss.list_softsteps()
    if not devs:
        sys.exit("no SoftStep found (amidi -l shows neither 'SSCOM MIDI 1' nor "
                 "'SoftStep Control Surface'). Plug one in.")
    if board_kind:
        for d in devs:
            if d["kind"] == board_kind:
                return d
        have = ", ".join(f"{d['kind']} ({d['port']})" for d in devs)
        sys.exit(f"no {board_kind!r} board connected; connected: {have}")
    if len(devs) > 1:
        have = ", ".join(d["kind"] for d in devs)
        sys.exit(f"two boards connected ({have}); pass --board sscom|softstep so the log "
                 "is unambiguously one unit.")
    return devs[0]


def main():
    global _logf, _path, _port, _board
    os.makedirs(LOGDIR, exist_ok=True)
    board_kind, phases = parse_args(sys.argv[1:])
    _board = resolve_board(board_kind)
    _port = _board["port"]
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    _path = os.path.join(LOGDIR, f"session-{stamp}-{_board['kind']}.log")
    _logf = open(_path, "w")
    _logf.write(f"# KMSS capture {stamp} phases={phases} "
                f"board={_board['kind']} label={_board['label']} port={_port} "
                f"name={_board['name']!r}\n"); _logf.flush()
    print(f"Capturing the {_board['label']} board ({_port}). Logging to {_path}")

    signal.signal(signal.SIGINT, lambda *a: (cleanup(), sys.exit(0)))
    signal.signal(signal.SIGTERM, lambda *a: (cleanup(), sys.exit(0)))
    try:
        if "A" in phases: phase_A()
        if "B" in phases: phase_B()
        if "C" in phases: phase_C()
        if "S" in phases: phase_S()
    finally:
        cleanup()


if __name__ == "__main__":
    main()
