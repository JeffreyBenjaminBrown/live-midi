"""Wait for a new USB device and/or ALSA-seq client (the drum), describe it,
start a detached timestamped MIDI recorder on its ports, beep, and exit."""
import os, re, struct, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "out")
USB = "/sys/bus/usb/devices"
BEEP = "/home/sound/beep-harsh.wav"


def usb_devices():
    return {d for d in os.listdir(USB) if ":" not in d and not d.startswith("usb")}


def seq_clients():
    txt = subprocess.run(["aconnect", "-l"], capture_output=True, text=True).stdout
    clients, cur = {}, None
    for line in txt.splitlines():
        m = re.match(r"client (\d+): '(.*?)' \[(.*?)\]", line)
        if m:
            cur = int(m[1])
            clients[cur] = {"name": m[2], "info": m[3], "ports": []}
            continue
        m = re.match(r"\s+(\d+) '(.*?)\s*'$", line)
        if m and cur is not None:
            clients[cur]["ports"].append((int(m[1]), m[2]))
    return clients


def rd(path):
    try:
        with open(path, "rb") as f:
            return f.read()
    except OSError as e:
        return f"<{e}>".encode()


def describe_usb(dev):
    p = os.path.join(USB, dev)
    lines = [f"USB device {dev} ({os.path.realpath(p)})"]
    for f in ("idVendor", "idProduct", "manufacturer", "product", "serial",
              "bcdDevice", "speed", "bDeviceClass", "bMaxPower", "busnum", "devnum"):
        lines.append(f"  {f}: {rd(os.path.join(p, f)).decode(errors='replace').strip()}")
    for i in sorted(x for x in os.listdir(p) if x.startswith(dev + ":")):
        ip = os.path.join(p, i)
        drv = os.path.basename(os.readlink(os.path.join(ip, "driver"))) if os.path.exists(os.path.join(ip, "driver")) else "-"
        lines.append(f"  iface {i}: class={rd(ip+'/bInterfaceClass').decode().strip()} "
                     f"sub={rd(ip+'/bInterfaceSubClass').decode().strip()} "
                     f"proto={rd(ip+'/bInterfaceProtocol').decode().strip()} driver={drv} "
                     f"name={rd(ip+'/interface').decode(errors='replace').strip()}")
    desc = rd(os.path.join(p, "descriptors"))
    lines.append("  descriptors (len,type,hex):")
    i = 0
    while i + 2 <= len(desc):
        n = desc[i]
        if n == 0:
            break
        lines.append(f"    {n:3d} 0x{desc[i+1]:02x} {desc[i:i+n].hex(' ')}")
        i += n
    return "\n".join(lines)


def log(msg):
    print(msg, flush=True)
    with open(os.path.join(OUT, "detect.txt"), "a") as f:
        f.write(msg + "\n")


def main():
    os.makedirs(OUT, exist_ok=True)
    base_usb, base_seq = usb_devices(), set(seq_clients())
    log(f"{time.strftime('%F %T')} baseline usb={sorted(base_usb)} seq={sorted(base_seq)}; waiting...")
    new_usb, first_seen = set(), None
    while True:
        time.sleep(0.3)
        added = usb_devices() - base_usb - new_usb
        for d in sorted(added):
            log(f"{time.strftime('%F %T')} NEW " + describe_usb(d))
            new_usb.add(d)
            first_seen = first_seen or time.time()
        clients = seq_clients()
        new_seq = {c: v for c, v in clients.items() if c not in base_seq}
        if new_seq:
            time.sleep(0.5)  # let all ports register
            new_seq = {c: v for c, v in seq_clients().items() if c not in base_seq}
            for c, v in new_seq.items():
                log(f"{time.strftime('%F %T')} NEW seq client {c}: '{v['name']}' [{v['info']}] ports={v['ports']}")
            ports = ",".join(f"{c}:{p}" for c, v in new_seq.items() for p, _ in v["ports"])
            logf = os.path.join(OUT, "midi.log")
            subprocess.Popen([sys.executable, "-I", os.path.join(HERE, "record.py"), ports, logf],
                             start_new_session=True, stdin=subprocess.DEVNULL,
                             stdout=subprocess.DEVNULL, stderr=open(logf + ".err", "a"))
            log(f"recording {ports} -> {logf}")
            break
        if first_seen and time.time() - first_seen > 10:
            log("USB device appeared but no ALSA-seq client after 10s -- not class-compliant, "
                "or the host has no driver bound; MIDI must be read from the raw USB endpoint")
            break
    env = dict(os.environ, XDG_RUNTIME_DIR="/run/user/1000")
    subprocess.run(["pw-play", BEEP], env=env, capture_output=True)


main()
