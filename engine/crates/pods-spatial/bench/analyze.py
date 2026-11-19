"""Analysis of the pods-spatial bench recordings.

  analyze.py <results dir>     -> <results dir>/summary.txt and summary.json

Clicks (method of airpods-study/spatial-bench/analyze.py): the input is a
500 Hz sine, and everything a linear filter legitimately produces stays at
500 Hz, even while it changes slowly. Filter-switch transients are broadband,
so we measure the energy ABOVE 2 kHz in 2 ms windows, in dB relative to the
signal level, and count the windows above -60 and -40 dB.

Also: output level per ear (fixed vs tracked must match), CPU per process
(cpu.jsonl), renderer cost per cycle (*.stats.json) and the input -> output
lag of an impulse (latency/in_out_*.wav).

Tone (tone/, azimuths/): 4-channel recordings of what enters the spatial
sink (FL, FR) and what reaches fake_airpods (RL, RR), pink noise. With
uncorrelated L/R the whole 2x2 transfer matrix H(f) comes out of one
recording (H = Syx Sxx^-1, Welch, 8192 points). From it, per octave and
1/3 octave, relative to the plain stereo output (0 dB):
  speaker  one input channel alone, power summed over both ears (plain
           stereo: that channel in one ear, 0 dB);
  centre   L = R (a centred source), each ear, measured directly with pinkc;
  azimuths the speaker response averaged over 12 head yaws (24 azimuths).
Loudness: ITU-R BS.1770 K-weighted level of the output (both ears) against
plain stereo, for uncorrelated (pink) and centred (pinkc) pink noise.
"""
import glob
import json
import os
import sys

import numpy as np
from scipy import signal

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wavio import read_wav  # noqa: E402

EDGE = 300  # 2 ms windows dropped at each end (0.6 s): start/stop edges
OCT = [31.5, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000]
THIRD = [round(1000 * 2 ** (k / 3), 0) for k in range(-15, 13)]  # 31.5 Hz .. 16 kHz
NPERSEG = 8192


def active(x):
    e = np.abs(x).max(1)
    idx = np.nonzero(e > 1e-3)[0]
    return x[idx[0]: idx[-1]] if len(idx) else x[:0]


def hf_profile(x, rate, win_ms=2.0):
    sos = signal.butter(8, 2000, "highpass", fs=rate, output="sos")
    h = signal.sosfiltfilt(sos, x, axis=0)
    w = int(rate * win_ms / 1000)
    n = len(h) // w
    hw = h[: n * w].reshape(n, w, -1)
    return 10 * np.log10(np.mean(hw ** 2, axis=(1, 2)) + 1e-20)


def dropouts(x, min_len=32):
    """Start sample and length of every run of >= min_len exact zeros (a
    missed graph cycle: the buffer was never written)."""
    z = (np.abs(x).max(1) < 1e-9).astype(np.int8)
    d = np.diff(np.concatenate([[0], z, [0]]))
    starts, ends = np.nonzero(d == 1)[0], np.nonzero(d == -1)[0]
    keep = (ends - starts) >= min_len
    return starts[keep], (ends - starts)[keep]


def clicks(path):
    rate, x = read_wav(path)
    x = active(x)
    if len(x) < 2 * rate:
        return None
    sig_db = 10 * np.log10(np.mean(x ** 2) + 1e-20)
    hf_all = hf_profile(x, rate) - sig_db
    # Windows within 20 ms of a dropout show the dropout, not the filter.
    mask = np.ones(len(hf_all), bool)
    w = int(rate * 0.002)
    starts, lens = dropouts(x)
    for s, n in zip(starts, lens):
        mask[max(0, s // w - 10): (s + n) // w + 11] = False
    hf = hf_all[EDGE:-EDGE][mask[EDGE:-EDGE]]
    lvl = 10 * np.log10(np.mean(x[len(x) // 4: 3 * len(x) // 4] ** 2, axis=0) + 1e-20)
    return dict(
        seconds=round(len(x) / rate, 1),
        dropouts=int(len(starts)),
        dropout_samples=[int(n) for n in lens[:8]],
        hf_median_db=round(float(np.median(hf)), 1),
        hf_p99_db=round(float(np.percentile(hf, 99)), 1),
        hf_max_db=round(float(hf.max()), 1),
        pct_over_60=round(100.0 * float(np.mean(hf > -60)), 2),
        pct_over_40=round(100.0 * float(np.mean(hf > -40)), 2),
        level_l_db=round(float(lvl[0]), 2),
        level_r_db=round(float(lvl[1]), 2),
    )


def latency(path):
    rate, x = read_wav(path)
    inp, out = x[:, 0], x[:, 2:4]
    ii = np.nonzero(np.abs(inp) > 0.25)[0]
    if len(ii) == 0:
        return None
    # one index per impulse
    starts = [ii[0]] + [b for a, b in zip(ii, ii[1:]) if b - a > rate // 2]
    lags = []
    for i in starts[:5]:
        seg = np.abs(out[i: i + 4096]).max(1)
        if seg.max() <= 0:
            continue
        on = int(np.nonzero(seg > seg.max() * 0.05)[0][0])
        lags.append((on, int(np.argmax(seg))))
    return dict(impulses=len(starts), onset_peak_samples=lags)


def io_segment(path):
    """(rate, inputs (n, 2), outputs (n, 2)) of a record_io recording, the
    first and last second of activity dropped."""
    rate, x = read_wav(path)
    e = np.abs(x[:, :2]).max(1)
    idx = np.nonzero(e > 1e-4)[0]
    if len(idx) < 4 * rate:
        return None
    x = x[idx[0] + rate: idx[-1] - rate]
    return rate, x[:, :2], x[:, 2:4]


def transfer_matrix(path):
    """f, H (nf, 2 outputs, 2 inputs)."""
    seg = io_segment(path)
    if seg is None:
        return None
    rate, xi, yo = seg
    kw = dict(fs=rate, nperseg=NPERSEG, noverlap=NPERSEG // 2, window="hann")
    f, _ = signal.csd(xi[:, 0], xi[:, 0], **kw)
    sxx = np.empty((len(f), 2, 2), complex)
    syx = np.empty((len(f), 2, 2), complex)
    for k in range(2):
        for j in range(2):
            sxx[:, k, j] = signal.csd(xi[:, j], xi[:, k], **kw)[1]
            syx[:, k, j] = signal.csd(xi[:, j], yo[:, k], **kw)[1]
    ok = np.abs(np.linalg.det(sxx)) > 0
    h = np.zeros_like(syx)
    h[ok] = syx[ok] @ np.linalg.inv(sxx[ok])
    return f, h


def centre_response(path):
    """f, |H|^2 per ear for a centred input (pinkc): output / input L."""
    seg = io_segment(path)
    if seg is None:
        return None
    rate, xi, yo = seg
    kw = dict(fs=rate, nperseg=NPERSEG, noverlap=NPERSEG // 2, window="hann")
    f, pxx = signal.welch(xi[:, 0], **kw)
    ears = [np.abs(signal.csd(xi[:, 0], yo[:, e], **kw)[1] / np.maximum(pxx, 1e-30)) ** 2 for e in range(2)]
    return f, np.array(ears)


def band_db(f, p, centres, width):
    """Mean power in bands `width` octaves wide, dB."""
    out = []
    for c in centres:
        b = (f >= c * 2 ** (-width / 2)) & (f < c * 2 ** (width / 2))
        out.append(10 * np.log10(np.mean(p[b]) + 1e-30) if b.any() else float("nan"))
    return np.array(out)


def summary_bands(f, p):
    octv = band_db(f, p, OCT, 1.0)
    third = band_db(f, p, THIRD, 1 / 3)
    t = np.array(THIRD)
    o = np.array(OCT)
    in_oct = (o >= 63) & (o <= 8000)
    in_third = (t >= 63) & (t <= 12500)
    return dict(octave=[round(float(v), 1) for v in octv],
                octave_63_8k=[round(float(octv[in_oct].min()), 1), round(float(octv[in_oct].max()), 1)],
                third_63_12k5=[round(float(third[in_third].min()), 1), round(float(third[in_third].max()), 1)],
                third=[round(float(v), 1) for v in third])


K_SHELF = ([1.53512485958697, -2.69169618940638, 1.19839281085285], [1.0, -1.69065929318241, 0.73248077421585])
K_HP = ([1.0, -2.0, 1.0], [1.0, -1.99004745483398, 0.99007225036621])


def loudness(path):
    """BS.1770 K-weighted level of the output channels (48 kHz), LKFS."""
    seg = io_segment(path)
    if seg is None:
        return None
    _, _, yo = seg
    y = signal.lfilter(*K_SHELF, yo, axis=0)
    y = signal.lfilter(*K_HP, y, axis=0)
    y = y[len(y) // 10:]
    return -0.691 + 10 * np.log10(np.sum(np.mean(y ** 2, axis=0)) + 1e-30)


def tone(res, out, lines):
    d = os.path.join(res, "tone")
    if not os.path.isdir(d):
        return
    cases = ["off", "fixed-raw", "fixed-eq", "tracked-eq"]
    out["tone"] = {}
    lines.append("== tone: response relative to plain stereo, dB (octave bands %s)" % " ".join("%g" % c for c in OCT))
    ref = {sig: loudness(os.path.join(d, "off-%s.wav" % sig)) for sig in ("pink", "pinkc")}
    for c in cases:
        r = {}
        m = transfer_matrix(os.path.join(d, "%s-pink.wav" % c))
        if m is not None:
            f, h = m
            spk = 0.5 * (np.abs(h[:, 0, 0]) ** 2 + np.abs(h[:, 1, 0]) ** 2 + np.abs(h[:, 0, 1]) ** 2 + np.abs(h[:, 1, 1]) ** 2)
            r["speaker"] = summary_bands(f, spk)
        cr = centre_response(os.path.join(d, "%s-pinkc.wav" % c))
        if cr is not None:
            f, ears = cr
            r["centre"] = summary_bands(f, ears.mean(0))
        r["loudness_db"] = {sig: round(loudness(os.path.join(d, "%s-%s.wav" % (c, sig))) - ref[sig], 2)
                            for sig in ("pink", "pinkc")}
        out["tone"][c] = r
        for k in ("speaker", "centre"):
            if k in r:
                b = r[k]
                lines.append("  %-11s %-7s %s | oct 63-8k %+.1f..%+.1f, 1/3 63-12.5k %+.1f..%+.1f" % (
                    c, k, " ".join("%6.1f" % v for v in b["octave"]), *b["octave_63_8k"], *b["third_63_12k5"]))
        lines.append("  %-11s loudness vs plain stereo: uncorrelated %+.2f dB, centred %+.2f dB" % (
            c, r["loudness_db"]["pink"], r["loudness_db"]["pinkc"]))


def azimuths(res, out, lines):
    d = os.path.join(res, "azimuths")
    if not os.path.isdir(d):
        return
    out["azimuths"] = {}
    lines.append("== azimuths: speaker response averaged over 12 yaws (24 azimuths), dB re plain stereo")
    for tone_ in ("raw", "eq"):
        acc, n, f = None, 0, None
        for path in sorted(glob.glob(os.path.join(d, "%s-yaw*.wav" % tone_))):
            m = transfer_matrix(path)
            if m is None:
                continue
            f, h = m
            spk = 0.5 * (np.abs(h[:, 0, 0]) ** 2 + np.abs(h[:, 1, 0]) ** 2 + np.abs(h[:, 0, 1]) ** 2 + np.abs(h[:, 1, 1]) ** 2)
            acc = spk if acc is None else acc + spk
            n += 1
        if not n:
            continue
        b = summary_bands(f, acc / n)
        b["yaws"] = n
        out["azimuths"][tone_] = b
        lines.append("  %-11s %-7s %s | oct 63-8k %+.1f..%+.1f, 1/3 63-12.5k %+.1f..%+.1f" % (
            tone_, "avg", " ".join("%6.1f" % v for v in b["octave"]), *b["octave_63_8k"], *b["third_63_12k5"]))


def main(res):
    out = {"clicks": {}, "cpu": [], "stats": {}, "latency": {}}
    lines = []
    for d in sorted(glob.glob(os.path.join(res, "clicks-q*"))):
        q = os.path.basename(d)
        lines.append("== %s: clicks (500 Hz sine, HF > 2 kHz in 2 ms windows, dB re signal)" % q)
        lines.append("   (windows within 20 ms of a dropout excluded; dropouts counted apart)")
        lines.append("  %-26s %8s %8s %8s %8s %8s %9s %9s %8s" % (
            "case", "median", "p99", "max", ">-60dB%", ">-40dB%", "L dB", "R dB", "dropouts"))
        for f in sorted(glob.glob(os.path.join(d, "*.wav"))):
            k = os.path.basename(f)[:-4]
            if not k.startswith("sine_"):
                rate, x = read_wav(f)
                n = len(dropouts(active(x))[0])
                out["clicks"]["%s/%s" % (q, k)] = dict(dropouts=n)
                lines.append("  %-26s %62s %8d" % (k, "(pink noise: dropouts only)", n))
                continue
            r = clicks(f)
            if r is None:
                continue
            out["clicks"]["%s/%s" % (q, k)] = r
            lines.append("  %-26s %8.1f %8.1f %8.1f %8.2f %8.2f %9.2f %9.2f %8d" % (
                k, r["hf_median_db"], r["hf_p99_db"], r["hf_max_db"], r["pct_over_60"], r["pct_over_40"],
                r["level_l_db"], r["level_r_db"], r["dropouts"]))
        cpu_file = os.path.join(d, "cpu.jsonl")
        if os.path.exists(cpu_file):
            lines.append("== %s: CPU, %% of one core over 15 s" % q)
            lines.append("  %-26s %8s %8s %8s %8s" % ("case", "daemon", "wp", "engine", "total"))
            for line in open(cpu_file):
                c = json.loads(line)
                tot = sum(c[k] for k in ("daemon_cpu_pct", "wireplumber_cpu_pct", "engine_cpu_pct"))
                c["total_cpu_pct"] = round(tot, 3)
                out["cpu"].append(c)
                lines.append("  %-26s %8.2f %8.2f %8.2f %8.2f" % (
                    c["case"], c["daemon_cpu_pct"], c["wireplumber_cpu_pct"], c["engine_cpu_pct"], tot))
        st = sorted(glob.glob(os.path.join(d, "*.stats.json")))
        if st:
            lines.append("== %s: renderer (process callback)" % q)
            for f in st:
                s = json.load(open(f))
                if not s.get("cycles"):
                    continue
                k = os.path.basename(f)[: -len(".stats.json")]
                s["mean_us_per_cycle"] = round(s["busy_ns"] / s["cycles"] / 1000, 1)
                s["max_us"] = round(s["max_busy_ns"] / 1000, 1)
                s["dsp_cpu_pct"] = round(s["busy_ns"] / 1e9 / (s["samples"] / 48000) * 100, 3)
                out["stats"]["%s/%s" % (q, k)] = s
                lines.append("  %-26s %6d cycles, %5d transitions, mean %6.1f us/cycle, max %7.1f us, DSP %.2f %% of a core" % (
                    k, s["cycles"], s["transitions"], s["mean_us_per_cycle"], s["max_us"], s["dsp_cpu_pct"]))
    for f in sorted(glob.glob(os.path.join(res, "latency", "in_out_*.wav"))):
        r = latency(f)
        k = os.path.basename(f)[len("in_out_"):-4]
        out["latency"][k] = r
        lines.append("== latency %s: %s" % (k, json.dumps(r)))
    tone(res, out, lines)
    azimuths(res, out, lines)
    json.dump(out, open(os.path.join(res, "summary.json"), "w"), indent=1)
    open(os.path.join(res, "summary.txt"), "w").write("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main(sys.argv[1])
