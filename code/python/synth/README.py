# PURPOSE:
# Interactively test how different just and edo chords sound.
# Built for easy exploration of harmony
# -- not speed or efficiency.

from synth import *

# Use the Python REPL.
# These commands play some major chords
#     effects    waveform  notation  hz   chord
play( 2*       ( tri(      just(     220, [1,1.25,1.5]) )))
play( clip(3)  ( tri(      just(     220, [1,1.25,1.5]) )))
play( clip(3)  ( tri(      edo(12,   220, [0, 3.86, 7.02])))) # the truth (to 2-decimal places) represented as 12-edo
play( clip(3)  ( tri(      edo(12,   220, [0, 4, 7])))) # the familiar 12-edo approximation

# trap(s, freqs) renders through a trapezoidal waveform.
# s=0 is triangle, s~0.3 is mellowist, s=1 is square.
play( clip(3)  ( trap(0.3, just(     220, [1,1.25,1.5]) )))
play( clip(3)  ( trap(0.3, edo(12,   220, [0, 4, 7]))))
