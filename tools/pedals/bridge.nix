# The environment bridge.py needs: python with pyusb (raw USB access to the
# CH345) + python-rtmidi (the virtual ALSA-seq output port), and libusb1 for
# pyusb's backend. bridge.sh wraps this in the full invocation (including the
# sudo dance); README.org "Run the bridge" is the usage doc.
#
# Deliberately un-pinned: it uses the host's <nixpkgs> channel, since all three
# dependencies are old and stable. Runnable by hand too:
#   nix-shell tools/pedals/bridge.nix
{ pkgs ? import <nixpkgs> { } }:
pkgs.mkShell {
  packages = [
    (pkgs.python3.withPackages (p: [ p.pyusb p.python-rtmidi ]))
    pkgs.libusb1
  ];
}
