# PURPOSE:
# Interactively test how different just and edo chords sound.
# Built for easy exploration of harmony
# -- not speed or efficiency.

from synth import *

# Use the Python REPL.
# These commands play some major chords
#     effects    notation  hz   chord
play( 2*       ( just(     220, [1,1.25,1.5]) ))
play( clip(3)  ( just(     220, [1,1.25,1.5]) ))
play( clip(3)  ( edo(12,   220, [0, 4, 7])))
play( clip(3)  ( edo(12,   220, [0, 4.14, 7])))
