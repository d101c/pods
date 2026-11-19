"""Test signals (48 kHz, float32 stereo), as in airpods-study/spatial-bench.

  gen_signals.py <out dir>
  sine500.wav  500 Hz sine, -12 dBFS, L = R (click detection)
  pink.wav     pink noise, -18 dBFS RMS, uncorrelated L/R (CPU, tone)
  pinkc.wav    pink noise, -18 dBFS RMS, the same on L and R (a centred source)
  impulse.wav  one click per second on L only (latency)
"""
import os
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wavio import write_wav  # noqa: E402

RATE, DUR = 48000, 20.0
out = sys.argv[1]
os.makedirs(out, exist_ok=True)
n = int(RATE * DUR)
t = np.arange(n) / RATE
fade = np.minimum(1.0, np.minimum(t, DUR - t) / 0.05)  # 50 ms fades, no edge click

s = 10 ** (-12 / 20) * np.sin(2 * np.pi * 500 * t) * fade
write_wav(os.path.join(out, "sine500.wav"), np.stack([s, s], 1))

rng = np.random.default_rng(1)


def pink(n):
    w = rng.standard_normal(n)
    f = np.fft.rfftfreq(n, 1 / RATE)
    spec = np.fft.rfft(w)
    spec[1:] /= np.sqrt(f[1:])
    spec[0] = 0
    p = np.fft.irfft(spec, n)
    return p / np.sqrt(np.mean(p ** 2)) * 10 ** (-18 / 20)


write_wav(os.path.join(out, "pink.wav"), np.stack([pink(n) * fade, pink(n) * fade], 1))
c = pink(n) * fade
write_wav(os.path.join(out, "pinkc.wav"), np.stack([c, c], 1))

imp = np.zeros((n, 2))
imp[RATE // 2::RATE, 0] = 0.5
write_wav(os.path.join(out, "impulse.wav"), imp)
