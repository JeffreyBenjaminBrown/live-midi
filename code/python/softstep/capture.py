#!/usr/bin/env python3
"""Guided KMSS capture session -- the ONE batched manual step.

Fully SELF-PACED: each step prints an instruction and then waits for you to press
Enter -- it never times you. Do the named action, press Enter, on to the next. It
writes a single time-stamped log that Claude reads directly, puts the device into
tether mode for the sensor phases, and ALWAYS restores standalone mode at the end
(even on Ctrl-C / crash). No firmware is touched.

  python3 code/python/softstep/capture.py            # all phases A, B, C
  python3 code/python/softstep/capture.py A           # just one/some phases
  python3 code/python/softstep/capture.py A B

Phases:
  A  tether: per-pad map (press 1..9 then 0) + dynamics (soft/med/hard on pad 1).
     -> gives us which CCs belong to which printed pad, and the pressure range
        from which velocity is derived.
  B  tether: TWO FEET at once -- proves two simultaneous pads both stream.
  C  standalone: the same two-pad stomp in normal Program-Change mode -- settles
     whether standalone really drops one of two simultaneous hits.

Output: code/python/softstep/captures/session-<timestamp>.log
Tell Claude when it's done; it reads the log from disk.
"""
import os, sys, time, signal, threading, subprocess, datetime
import ss

LOGDIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "captures")

_reader = None      # current amidi reader (so cleanup can stop it)
_logf = None
_path = None
_cleaned = False


class Reader:
    """Runs `amidi -p PORT -d -T monotonic` and appends each line to the log as DATA."""
    def __init__(self, logf):
        self.logf, self.proc, self.thread = logf, None, None

    def start(self):
        # stdbuf -oL forces amidi to flush each line immediately; without it amidi
        # block-buffers stdout when piped and live readings wouldn't reach the log.
        self.proc = subprocess.Popen(
            ["stdbuf", "-oL", "amidi", "-p", ss.PORT, "-d", "-T", "monotonic"],
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
    ss.enter_tether()
    _reader = Reader(_logf); _reader.start()
    mark("PHASE_A begin tether")

    print("\n-- A1: press each named pad ONCE (a clear press, then release) --")
    for lab in [1, 2, 3, 4, 5, 6, 7, 8, 9, 0]:
        step(f"A1 pad {lab}", f"Press the pad LABELED {lab}  (press, then release).")

    print("\n-- A2: dynamics on PAD 1 -- vary the force; this calibrates velocity --")
    for force in ["SOFT", "MEDIUM", "HARD", "SOFT", "MEDIUM", "HARD"]:
        step(f"A2 pad1 {force}", f"Tap PAD 1 -- {force}.")

    mark("PHASE_A end")
    _reader.stop(); _reader = None
    ss.restore_standalone()
    print("\nPhase A done.")


def phase_B():
    global _reader
    print("\n========== PHASE B: TWO FEET at once (tether) ==========")
    input("Foot off the board, then press Enter to begin... ")
    ss.enter_tether()
    _reader = Reader(_logf); _reader.start()
    mark("PHASE_B begin tether")
    print("\nStomp BOTH named pads as SIMULTANEOUSLY as you can, hard, then release.")
    for a, b in [(1, 3), (1, 3), (4, 6), (4, 6), (2, 9), (2, 9)]:
        step(f"B stomp {a}+{b}", f"Stomp PADS {a} AND {b} TOGETHER (hard), then release.")
    mark("PHASE_B end")
    _reader.stop(); _reader = None
    ss.restore_standalone()
    print("\nPhase B done.")


def phase_C():
    global _reader
    print("\n========== PHASE C: standalone two-pad Program-Change test ==========")
    input("Foot off the board, then press Enter to begin... ")
    ss.restore_standalone()        # make sure we're in normal PC mode
    _reader = Reader(_logf); _reader.start()
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


def cleanup(*_a):
    global _cleaned
    if _cleaned:
        return
    _cleaned = True
    try:
        if _reader:
            _reader.stop()
        ss.restore_standalone()
    finally:
        if _logf:
            _logf.flush(); _logf.close()
        print(f"\nRestored standalone mode. Log saved:\n  {_path}")
        print("Tell Claude: the capture is done.")


def main():
    global _logf, _path
    os.makedirs(LOGDIR, exist_ok=True)
    phases = [p.upper() for p in sys.argv[1:]] or ["A", "B", "C"]
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    _path = os.path.join(LOGDIR, f"session-{stamp}.log")
    _logf = open(_path, "w")
    _logf.write(f"# KMSS capture {stamp} phases={phases} port={ss.PORT}\n"); _logf.flush()
    print(f"Logging to {_path}")

    signal.signal(signal.SIGINT, lambda *a: (cleanup(), sys.exit(0)))
    signal.signal(signal.SIGTERM, lambda *a: (cleanup(), sys.exit(0)))
    try:
        if "A" in phases: phase_A()
        if "B" in phases: phase_B()
        if "C" in phases: phase_C()
    finally:
        cleanup()


if __name__ == "__main__":
    main()
