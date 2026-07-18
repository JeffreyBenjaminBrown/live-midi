#!/usr/bin/env python3
"""Bridge the DOREMiDi MPC-20 pedals into a VIRTUAL ALSA-seq MIDI port, so the container
synth can read them.

WHY: the MPC-20's WCH CH345 chip makes no ALSA MIDI device on Linux (composite-device
quirk gap -- see README.org). This reads the chip's raw USB endpoint
directly (the same path pedals_usb.py proved works) and re-emits every MIDI message on a
virtual ALSA *sequencer* port named "MPC-20 pedals". The container shares the host ALSA
sequencer (/dev/snd/seq), so that port appears in the container's `aconnect -l` and the
synth (midir) -- or `aseqdump` -- can read the pedals like any MIDI input.

It forwards RAW MIDI (no rescaling). Normalizing each pedal's raw ~1..120 to a continuous
0.0..1.0 float happens in the synth reader (midi_pulse::expression_pedals, PEDAL_RAW_BOTTOM/TOP).

  *** RUN ON THE HOST *** (the container has no /dev/bus/usb), as root (to take the USB
  interface from snd-usb-audio). The sibling wrapper does the whole dance -- the nix
  shell (bridge.nix), the quoting, and handing sudo the shell's own python:

  tools/pedals/bridge.sh

Verify from the CONTAINER, without the synth:  aseqdump -p 'MPC-20 pedals'  (move a pedal).
Ctrl-C to stop. It reconnects if the pedals are replugged.
"""
import sys, time

try:
    import usb.core
    import usb.util
except ImportError:
    sys.exit("pyusb not found. Run tools/pedals/bridge.sh (wraps this in the right nix shell).")
try:
    import rtmidi
except ImportError:
    sys.exit("python-rtmidi not found. Run tools/pedals/bridge.sh (wraps this in the right nix shell).")

import pedals_usb  # sibling: open_device(), decode_usb_midi(), EP_IN, MIDI_IFACE

PORT_NAME = "MPC-20 pedals"


def make_output():
    """A MidiOut on the ALSA-seq backend (what the container shares), if available. The
    ALSA *client* is named PORT_NAME too (not the rtmidi default "RtMidiOut Client"), so the
    port is addressable by name -- `aseqdump -p 'MPC-20 pedals'` and midir's name match both
    resolve it."""
    apis = rtmidi.get_compiled_api()
    if hasattr(rtmidi, "API_LINUX_ALSA") and rtmidi.API_LINUX_ALSA in apis:
        return rtmidi.MidiOut(rtmidi.API_LINUX_ALSA, name=PORT_NAME)
    return rtmidi.MidiOut(name=PORT_NAME)


def pump(dev, reattach, midiout):
    """Read the CH345 bulk endpoint and forward each decoded MIDI message. Returns on a
    USB error (e.g. unplug) so the caller can reconnect; always releases the interface."""
    seen = set()
    forwarded = 0
    try:
        while True:
            try:
                data = dev.read(pedals_usb.EP_IN, 64, timeout=200)
            except usb.core.USBError as e:
                if e.errno == 110 or "timeout" in str(e).lower():
                    continue  # no data this interval
                raise         # real error (unplug) -> let the caller reconnect
            for status, d1, d2 in pedals_usb.decode_usb_midi(data):
                msg = [status]
                if d1 is not None:
                    msg.append(d1)
                if d2 is not None:
                    msg.append(d2)
                midiout.send_message(msg)
                forwarded += 1
                key = (status & 0x0F, status & 0xF0, d1)
                if key not in seen:  # announce each new (channel, kind, controller) once
                    seen.add(key)
                    ch = (status & 0x0F) + 1
                    print(f"  forwarding ch{ch} status 0x{status:02X} d1={d1}  (total {forwarded})")
    finally:
        try:
            usb.util.release_interface(dev, pedals_usb.MIDI_IFACE)
            if reattach:
                dev.attach_kernel_driver(pedals_usb.MIDI_IFACE)
        except Exception:
            pass


def main():
    midiout = make_output()
    midiout.open_virtual_port(PORT_NAME)
    print(f"opened virtual ALSA-seq output port {PORT_NAME!r} -- visible in `aconnect -l`.")
    print("waiting for pedal activity; Ctrl-C to stop.\n")
    try:
        while True:
            try:
                dev, reattach = pedals_usb.open_device()  # exits if not found/claimable
            except SystemExit as e:
                print(f"  {e}  retrying in 2s...")
                time.sleep(2)
                continue
            print("connected to the MPC-20.")
            try:
                pump(dev, reattach, midiout)
            except usb.core.USBError as e:
                print(f"  USB error ({e}); reconnecting in 1s...")
                time.sleep(1)
    except KeyboardInterrupt:
        print("\nstopped.")
    finally:
        midiout.close_port()


if __name__ == "__main__":
    main()
