"""
Interactive triangle-wave synthesizer.

    from synth import *
    play(just(220, [1, 1.25, 1.5]))        # just major triad
    play(edo(12, 220, [0, 4, 7]))           # 12-edo major chord
    play(clip(3)(edo(12, 220, [0, 4, 7])))  # hard-clipped
    play(fold(3)(just(220, [1, 1.5])))      # wave-folded
    stop()
"""

import numpy as np
import sounddevice as sd

SAMPLE_RATE = 48_000
DURATION = 2.0
AMPLITUDE = 0.18
ATTACK = 0.02
RELEASE = 0.3

def triangle(phase):
    """Triangle wave from phase array (in radians)."""
    t = (phase / (2 * np.pi)) % 1.0
    return np.where(t < 0.5, 4 * t - 1, 3 - 4 * t)

def envelope(n_samples):
    t = np.arange(n_samples) / SAMPLE_RATE
    env = np.ones(n_samples)
    # attack
    mask = t < ATTACK
    env[mask] = t[mask] / ATTACK
    # release
    mask = t > DURATION - RELEASE
    env[mask] = (DURATION - t[mask]) / RELEASE
    return env

def render_freqs(freqs):
    """Render a list of frequencies to a numpy audio array."""
    n = int(DURATION * SAMPLE_RATE)
    t = np.arange(n) / SAMPLE_RATE
    mix = sum(triangle(2 * np.pi * f * t) for f in freqs) / len(freqs)
    return (AMPLITUDE * envelope(n) * mix).astype(np.float32)

def just(fund, ratios):
    """Render triangle waves at fund*ratio for each ratio."""
    return render_freqs([fund * r for r in ratios])

def edo(edo, fund, notes):
    """Render triangle waves in an equal division of the octave.
    hz = fund * 2**(note/edo) for each note in notes."""
    return render_freqs([fund * 2 ** (n / edo) for n in notes])

# --- distortions (audio -> audio) ---

def clip(drive):
    """Hard clip: boost by drive, clamp to [-1, 1]."""
    def f(audio):
        return np.clip(audio * drive, -AMPLITUDE, AMPLITUDE)
    return f

def fold(drive):
    """Wave folder: boost then fold overflows back into range."""
    def f(audio):
        x = audio * drive / AMPLITUDE
        # Triangle-fold into [-1, 1]
        x = 4 * (np.abs((x - 1) / 4 - np.floor((x - 1) / 4 + 0.5)) - 0.25)
        return (x * AMPLITUDE).astype(np.float32)
    return f

def saturate(drive):
    """Soft saturation (tanh)."""
    def f(audio):
        return (AMPLITUDE * np.tanh(audio * drive / AMPLITUDE)).astype(np.float32)
    return f

def identity():
    return lambda audio: audio

# --- playback ---

def play(audio):
    """Play a rendered audio array."""
    sd.stop()
    sd.play(audio, SAMPLE_RATE)

def stop():
    """Silence immediately."""
    sd.stop()
