# Writes a file to /tmp/beep.wav.
# Soothing hawaiian guitar-like tones:
# soft sinewaves with slight warmth,
# drifting independently between 300-800 Hz.

python3 << 'EOF'
import wave
import math
import struct

duration = 3.0  # seconds
sample_rate = 48000
amplitude = 0.12  # quiet

# Three voices drifting independently (not in parallel)
voices = [
  {"start_freq": 420, "end_freq": 360, "drift_rate": 0.23},
  {"start_freq": 530, "end_freq": 620, "drift_rate": 0.31},
  {"start_freq": 340, "end_freq": 295, "drift_rate": 0.17},
]

def soft_clip(x, threshold=0.6):
  """Soft saturation for warmth."""
  if abs(x) < threshold:
    return x
  sign = 1 if x > 0 else -1
  excess = abs(x) - threshold
  return sign * (threshold + excess / (1 + excess * 4))

def envelope(t, dur, attack=0.4, release=1.2):
  """Gentle fade in/out."""
  if t < attack:
    return t / attack
  elif t > dur - release:
    return (dur - t) / release
  return 1.0

with wave.open('/tmp/beep.wav', 'w') as wav_file:
  wav_file.setnchannels(1)
  wav_file.setsampwidth(2)
  wav_file.setframerate(sample_rate)

  num_samples = int(duration * sample_rate)
  phases = [0.0 for _ in voices]

  for i in range(num_samples):
    t = i / sample_rate
    progress = i / num_samples

    sample_value = 0

    for vi, voice in enumerate(voices):
      # Each voice drifts at its own rate with gentle wobble
      drift = voice["start_freq"] + (voice["end_freq"] - voice["start_freq"]) * progress
      wobble = math.sin(2 * math.pi * voice["drift_rate"] * t) * 12
      freq = drift + wobble

      # Accumulate phase for smooth waveform
      phases[vi] += 2 * math.pi * freq / sample_rate

      # Nearly pure sine with tiny 2nd harmonic for warmth
      wave_val = math.sin(phases[vi])
      wave_val += 0.06 * math.sin(2 * phases[vi])

      sample_value += wave_val

    # Average voices and apply envelope
    sample_value = sample_value / len(voices)
    sample_value *= envelope(t, duration)

    # Gentle soft clip
    sample_value = soft_clip(sample_value * 1.1)

    value = int(amplitude * 32767 * sample_value)
    value = max(-32767, min(32767, value))

    wav_file.writeframes(struct.pack('h', value))

print("Soothing beep created at /tmp/beep.wav")
EOF
