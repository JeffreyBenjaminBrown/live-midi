"""record.py PORTS LOGFILE -- aseqdump PORTS, each line prefixed with wall-clock
time and seconds since start, until killed."""
import subprocess, sys, time

ports, logf = sys.argv[1], sys.argv[2]
t0 = time.monotonic()
p = subprocess.Popen(["stdbuf", "-oL", "aseqdump", "-p", ports],
                     stdout=subprocess.PIPE, text=True, bufsize=1)
with open(logf, "a", buffering=1) as f:
    f.write(f"# {time.strftime('%F %T')} aseqdump -p {ports}\n")
    for line in p.stdout:
        f.write(f"{time.strftime('%T')} {time.monotonic() - t0:10.4f} {line}")
