#!/usr/bin/env python3
"""Live test/monitor for expression pedals coming in over MIDI -- built for a
DOREMiDi MPC-20 with two M-Audio EX-P pedals, but it will report ANY MIDI CC input.

WHAT IT SHOWS. For every (channel, CC) pair it sees, a live row:

    ch  cc   value  [########################----------]  min .. max   (range)

The min/max are the extremes seen since start (press `z` to re-zero them). This is the
whole point of the tool: a healthy EX-P sweeps its CC from 0 to 127; a mis-set one only
reaches, say, 35..127. Watch the `min` column -- if it never gets to 0, that pedal's
range is clipped and needs fixing (see pedals/README.org: the EX-P MIN knob + polarity
switch, and the MPC-20's own min/max calibration).

Two pedals set up right land on two different channels -- e.g. MIDI channel 1 and 2
(shown 1-based here), which is channel 0 and 1 zero-based. This tool prints BOTH numbers
so there's no ambiguity.

  python3 code/python/pedals/pedals.py            # auto-pick the pedal MIDI port
  python3 code/python/pedals/pedals.py --list     # list candidate MIDI-in ports and exit
  python3 code/python/pedals/pedals.py --port hw:2,0,0   # force a specific rawmidi port
  python3 code/python/pedals/pedals.py 8          # run ~8s then exit (for testing)

Keys while running:  z re-zero the min/max holds   q quit (Ctrl-C also works)

CSV: logs/session-<stamp>.csv -- one row per MIDI message (t_iso,t_ms,port,status,
channel_1based,channel_0based,kind,data1,data2). Load it for offline analysis.

NOTE: unlike the SoftStep, an expression-pedal-to-MIDI box is a plain MIDI controller --
no tether/SysEx mode switching, nothing to restore. We only listen.
"""
import os, sys, time, threading, subprocess, re, signal, select, termios, tty, csv, datetime

HERE = os.path.dirname(os.path.abspath(__file__))
LOGDIR = os.path.join(HERE, "logs")

# ALSA client-name substrings we should NOT treat as a pedal controller: the two
# SoftSteps and the host's onboard HDA audio codec. Anything else that presents a MIDI
# input is a candidate (the MPC-20 typically enumerates as a generic "USB MIDI" device).
NOT_PEDALS = ("SSCOM", "SoftStep", "Midi Through")

CSV_COLS = ["t_iso", "t_ms", "port", "status", "channel_1based", "channel_0based",
            "kind", "data1", "data2"]

# MIDI status high-nibble -> human name (channel voice messages).
KIND = {0x8: "note_off", 0x9: "note_on", 0xA: "aftertouch", 0xB: "control_change",
        0xC: "program_change", 0xD: "channel_pressure", 0xE: "pitch_bend"}

lock = threading.RLock()
seen = {}          # (ch0, cc) -> {"val", "min", "max", "count", "t"}
events = []        # recent non-CC messages, newest last (for note/PC pedals)
recent = "(move a pedal)"


def list_midi_inputs():
    """Parse `amidi -l`; return input-capable rawmidi ports as dicts {port,card,name}."""
    out = []
    try:
        txt = subprocess.run(["amidi", "-l"], capture_output=True, text=True, timeout=5).stdout
    except (FileNotFoundError, subprocess.TimeoutExpired):
        txt = ""
    for line in txt.splitlines():
        m = re.match(r"\s*([IO]+)\s+(hw:(\d+),\d+,\d+)\s+(.*\S)\s*$", line)
        if not m:
            continue
        dirs, port, card, name = m.group(1), m.group(2), int(m.group(3)), m.group(4)
        if "I" in dirs:
            out.append({"port": port, "card": card, "name": name})
    return out


def pedal_candidates():
    """MIDI inputs that look like an external controller (not a SoftStep / onboard / thru)."""
    return [d for d in list_midi_inputs()
            if not any(sub.lower() in d["name"].lower() for sub in NOT_PEDALS) and d["card"] != 0]


class Csv:
    def __init__(self):
        self.f = self.w = self.path = None

    def open(self):
        os.makedirs(LOGDIR, exist_ok=True)
        stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        self.path = os.path.join(LOGDIR, f"session-{stamp}.csv")
        self.f = open(self.path, "w", newline="")
        self.w = csv.writer(self.f)
        self.w.writerow(CSV_COLS)
        self.f.flush()

    def row(self, now, port, status, ch0, kind, d1, d2):
        if self.w is None:
            return
        self.w.writerow([datetime.datetime.now().isoformat(timespec="milliseconds"),
                         f"{now * 1000:.1f}", port, f"0x{status:02X}",
                         ch0 + 1, ch0, kind, d1, "" if d2 is None else d2])
        self.f.flush()

    def close(self):
        if self.f:
            self.f.flush(); self.f.close(); self.f = self.w = None


CSVLOG = Csv()


def note_event(now, port, status, ch0, kind, d1, d2):
    """Record a channel-voice message: update the per-(ch,cc) hold for CC, else log a line."""
    global recent
    CSVLOG.row(now, port, status, ch0, kind, d1, d2)
    if kind == "control_change":
        key = (ch0, d1)
        e = seen.get(key)
        if e is None:
            seen[key] = {"val": d2, "min": d2, "max": d2, "count": 1, "t": now}
        else:
            e["val"] = d2
            e["min"] = min(e["min"], d2)
            e["max"] = max(e["max"], d2)
            e["count"] += 1
            e["t"] = now
        recent = f"ch{ch0 + 1} (0-based {ch0})  CC{d1} = {d2}"
    else:
        msg = f"ch{ch0 + 1} {kind} d1={d1}" + (f" d2={d2}" if d2 is not None else "")
        events.append(msg)
        del events[:-6]
        recent = msg


def reader(port):
    proc = subprocess.Popen(["stdbuf", "-oL", "amidi", "-p", port, "-d"],
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
                    if status >= 0xF0:          # system message: no channel; skip its data
                        status = None
                        continue
                if status is None:
                    i += 1; continue
                high, ch0 = status >> 4, status & 0x0F
                kind = KIND.get(high, f"status_{high:X}")
                two = high != 0xC and high != 0xD  # PC and channel-pressure take ONE data byte
                if i < len(toks):
                    d1 = toks[i]; i += 1
                    d2 = None
                    if two:
                        if i < len(toks):
                            d2 = toks[i]; i += 1
                        else:
                            break
                    note_event(now, port, status, ch0, kind, d1, d2)
                else:
                    break


BARW = 34


def bar(val):
    fill = int(val / 127 * BARW)
    return "#" * fill + "-" * (BARW - fill)


def draw(port_name):
    with lock:
        lines = ["",
                 f"  Expression-pedal MIDI monitor -- {port_name}",
                 "  A healthy pedal sweeps CC 0..127; a clipped one won't reach 0 (watch `min`).",
                 f"  keys: [z]ero holds  [q]uit   |   CSV: {os.path.relpath(CSVLOG.path, HERE) if CSVLOG.path else 'off'}",
                 "",
                 f"  {'ch':>2} {'(0b)':>4}  {'cc':>3}  {'value':>5}  [{'sweep 0..127':^{BARW}}]  {'min':>3}..{'max':<3}  {'#msgs':>6}"]
        if seen:
            for (ch0, cc) in sorted(seen):
                e = seen[(ch0, cc)]
                span = e["max"] - e["min"]
                flag = ""
                if e["max"] - e["min"] >= 8:                 # enough travel to judge
                    if e["min"] > 4:
                        flag = f"  <- min never reaches 0 (clipped low by {e['min']})"
                    elif e["max"] < 123:
                        flag = f"  <- max never reaches 127 (short by {127 - e['max']})"
                    else:
                        flag = "  full range OK"
                lines.append(f"  {ch0 + 1:>2} {ch0:>4}  {cc:>3}  {e['val']:>5}  [{bar(e['val'])}]  "
                             f"{e['min']:>3}..{e['max']:<3}  {e['count']:>6}{flag}")
        else:
            lines.append("  (no CC yet -- move a pedal; if nothing appears, try --list / --port)")
        lines += ["", f"  last: {recent}"]
        for m in events:
            lines.append(f"    other: {m}")
    sys.stdout.write("\033[2J\033[H" + "\n".join(lines) + "\n")
    sys.stdout.flush()


def handle_key(ch):
    if ch in ("q", "\x03", "\x04"):
        return False
    if ch == "z":
        with lock:
            for e in seen.values():
                e["min"] = e["max"] = e["val"]
    return True


def choose_port(argv):
    """Return (port, name) or exit with guidance. Honors --port / --list."""
    if "--list" in argv:
        cands = pedal_candidates()
        print("MIDI input ports (candidates for the pedal controller):")
        for d in list_midi_inputs():
            tag = "  <- candidate" if d in cands else ""
            print(f"  {d['port']:12} {d['name']!r}{tag}")
        sys.exit(0)
    if "--port" in argv:
        p = argv[argv.index("--port") + 1]
        return p, p
    cands = pedal_candidates()
    if not cands:
        print("No pedal-controller MIDI input found. Connected MIDI inputs:")
        for d in list_midi_inputs():
            print(f"  {d['port']:12} {d['name']!r}")
        print("\nPlug in the DOREMiDi MPC-20 (USB), then re-run. Or pass --port hw:X,0,0 "
              "\n(see --list). The onboard audio card and the two SoftSteps are excluded "
              "automatically.")
        sys.exit(1)
    if len(cands) > 1:
        print("Multiple candidate ports; using the first. Override with --port:")
        for d in cands:
            print(f"  {d['port']:12} {d['name']!r}")
    return cands[0]["port"], cands[0]["name"]


def main():
    argv = sys.argv[1:]
    port, name = choose_port(argv)
    limit = next((float(a) for a in argv if re.fullmatch(r"[0-9.]+", a)), None)

    CSVLOG.open()
    print(f"Reading {name} ({port}). CSV -> {CSVLOG.path}")
    print("Move each pedal heel-to-toe a few times; watch the min/max columns.\n")

    interactive = sys.stdin.isatty()
    old_term = termios.tcgetattr(sys.stdin.fileno()) if interactive else None
    if interactive:
        tty.setcbreak(sys.stdin.fileno())

    done = []

    def cleanup():
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
        CSVLOG.close()
        sys.stdout.write(f"\nDone. CSV log: {CSVLOG.path}\n")

    for sig in (signal.SIGTERM, signal.SIGHUP):
        try:
            signal.signal(sig, lambda *_a: (cleanup(), sys.exit(0)))
        except (ValueError, OSError):
            pass

    threading.Thread(target=reader, args=(port,), daemon=True).start()
    start = time.monotonic()
    try:
        while True:
            draw(name)
            if interactive and select.select([sys.stdin], [], [], 0.03)[0]:
                if not handle_key(sys.stdin.read(1)):
                    break
            else:
                time.sleep(0.03)
            if limit and time.monotonic() - start > limit:
                break
    except KeyboardInterrupt:
        pass
    finally:
        cleanup()


if __name__ == "__main__":
    main()
