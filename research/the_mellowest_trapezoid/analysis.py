import numpy as np

N = 2**16  # samples per period (65536; power of 2 for fast FFT)
N_HARMONICS = 50  # how many harmonics to include in analysis

def trapezoidal_wave(s, n_samples):
    """Generate one period of the trapezoidal wave with side-hugging s."""
    phi = np.linspace(0, 1, n_samples, endpoint=False)
    h = s / 2
    r = (1 - s) / 2
    y = np.empty_like(phi)
    for i, p in enumerate(phi):
        if p < h:
            y[i] = -1.0
        elif p < h + r:
            y[i] = -1.0 + 2.0 * (p - h) / r if r > 0 else 1.0
        elif p < 2 * h + r:
            y[i] = 1.0
        else:
            y[i] = 1.0 - 2.0 * (p - 2 * h - r) / r if r > 0 else -1.0
    return y

def analyze_harmonics(y, n_harmonics):
    """Return arrays of harmonic numbers and their power."""
    fft = np.fft.rfft(y)
    power = np.abs(fft) ** 2
    # Harmonics are at bins 1, 2, 3, ...
    harmonics = np.arange(1, n_harmonics + 1)
    harm_power = power[harmonics]
    return harmonics.astype(float), harm_power

def spectral_slope(freqs, power):
    """Slope of linear fit to log-power vs log-frequency (dB/octave)."""
    mask = power > 1e-20
    if np.sum(mask) < 2:
        return 0.0
    log_f = np.log2(freqs[mask])
    log_p = 10 * np.log10(power[mask])
    slope, _ = np.polyfit(log_f, log_p, 1)
    return slope

def spectral_centroid(freqs, power):
    """Power-weighted mean frequency (in units of fundamental)."""
    total = np.sum(power)
    if total == 0:
        return 0.0
    return np.sum(freqs * power) / total

def spectral_centroid_log(freqs, power):
    """Power-weighted mean of log2-frequency."""
    mask = power > 1e-20
    if np.sum(mask) == 0:
        return 0.0
    f, p = freqs[mask], power[mask]
    return np.sum(np.log2(f) * p) / np.sum(p)

def spectral_flatness(power):
    """Geometric mean / arithmetic mean of power spectrum."""
    p = power[power > 1e-20]
    if len(p) == 0:
        return 0.0
    log_mean = np.mean(np.log(p))
    return np.exp(log_mean) / np.mean(p)

s_values = np.sort(np.unique(np.concatenate([
    np.linspace(0, 1, 11),
    np.linspace(0.2, 0.3, 11)[1:-1],
    np.linspace(0.3, 0.4, 11)[1:-1],
])))

import csv
rows = []
for s in s_values:
    y = trapezoidal_wave(s, N)
    freqs, power = analyze_harmonics(y, N_HARMONICS)
    rows.append({
        "side_hugging": f"{s:.2f}",
        "slope_dB_per_oct": f"{spectral_slope(freqs, power):.2f}",
        "centroid": f"{spectral_centroid(freqs, power):.4f}",
        "centroid_log": f"{spectral_centroid_log(freqs, power):.4f}",
        "flatness": f"{spectral_flatness(power):.6f}",
    })

out_path = "analysis.csv"
with open(out_path, "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=rows[0].keys())
    w.writeheader()
    w.writerows(rows)
print(f"Wrote {out_path}")

# Find the minimum of each measure using scipy.optimize
from scipy.optimize import minimize_scalar

measures = {
    "slope (most negative)": lambda s: -spectral_slope(  # negate: most negative = mellowist
        *analyze_harmonics(trapezoidal_wave(s, N), N_HARMONICS)),
    "centroid":     lambda s: spectral_centroid(
        *analyze_harmonics(trapezoidal_wave(s, N), N_HARMONICS)),
    "centroid_log": lambda s: spectral_centroid_log(
        *analyze_harmonics(trapezoidal_wave(s, N), N_HARMONICS)),
    "flatness":     lambda s: spectral_flatness(
        analyze_harmonics(trapezoidal_wave(s, N), N_HARMONICS)[1]),
}

print("\nMinima (to 3 decimal places):")
print(f"  {'measure':<25} {'s_min':>7}  {'value':>12}")
print(f"  {'-'*25} {'-'*7}  {'-'*12}")
for name, fn in measures.items():
    res = minimize_scalar(fn, bounds=(0.01, 0.99), method="bounded",
                          options={"xatol": 1e-4})
    val = -res.fun if "slope" in name else res.fun
    print(f"  {name:<25} {res.x:7.3f}  {val:12.6f}")
