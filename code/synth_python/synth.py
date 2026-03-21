"""
Interactive synthesizer with selectable waveforms.
See local README for usage.
"""

import numpy as np
import sounddevice as sd

SAMPLE_RATE = 48_000
DURATION = 2.0
AMPLITUDE = 0.18
ATTACK = 0.02
RELEASE = 0.3

def _trapezoid(phase, s):
    """Trapezoidal wave from phase array (in radians) and side-hugging s.
The 'side-hugging' parameter
  dictates the fraction of the waveform spent at the extreme values 1 or -1.
  s=0 yields a triangle wave.
  s=1 yields a square wave.
  The mellowest value of s is around 0.25 to 0.3 --
    determined experimentally at research/the_mellowest_trapezoid/"""
    phi = (phase / (2 * np.pi)) % 1.0
    h = s / 2
    r = (1 - s) / 2
    y = np.empty_like(phi)
    if r > 0:
        m1 = phi < h
        m2 = (phi >= h) & (phi < h + r)
        m3 = (phi >= h + r) & (phi < 2 * h + r)
        m4 = phi >= 2 * h + r
        y[m1] = -1.0
        y[m2] = -1.0 + 2.0 * (phi[m2] - h) / r
        y[m3] = 1.0
        y[m4] = 1.0 - 2.0 * (phi[m4] - 2 * h - r) / r
    else:  # square wave
        y = np.where(phi < 0.5, -1.0, 1.0)
    return y

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

def _render_freqs(freqs, s=0.0):
    """Render a list of frequencies to a numpy audio array."""
    n = int(DURATION * SAMPLE_RATE)
    t = np.arange(n) / SAMPLE_RATE
    mix = sum(_trapezoid(2 * np.pi * f * t, s) for f in freqs) / len(freqs)
    return (AMPLITUDE * envelope(n) * mix).astype(np.float32)

def just(fund, ratios):
    """Return a list of frequencies from fundamental and ratios."""
    return [fund * r for r in ratios]

def edo(edo, fund, notes):
    """Return a list of frequencies in an equal division of the octave.
    hz = fund * 2**(note/edo) for each note in notes."""
    return [fund * 2 ** (n / edo) for n in notes]

def trap(s, freqs):
    """Render frequencies through a trapezoidal waveform with side-hugging s.
    s=0 is triangle, s=1 is square, ~0.3 is mellowist."""
    return _render_freqs(freqs, s)

def tri(freqs):
    """Render frequencies as triangle waves (trap with s=0)."""
    return _render_freqs(freqs, 0.0)

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
