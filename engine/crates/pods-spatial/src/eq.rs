//! Diffuse-field equalisation of the HRIR set (issue #26).
//!
//! The MIT KEMAR set carries what every one of its measurements shares: the
//! measurement loudspeaker (no bass: −20 dB at 40–80 Hz) and the dummy
//! head's ear canal (+12 dB at 1.25–5 kHz). Rendered as is, music sounds
//! like a phone call. What differs between directions (the cues that place a
//! sound) is kept; what is common to all of them is removed:
//!
//! 1. the power spectrum of every measurement of the file, both ears,
//!    weighted by the solid angle each one covers, gives the diffuse field;
//! 2. smoothed over 1/3 octave, inverted around its mean level, the
//!    correction capped to +[`MAX_BOOST_DB`] / −[`MAX_CUT_DB`] and held flat
//!    below the crossover and above 20 kHz;
//! 3. turned into a minimum-phase filter ([`EQ_LEN`] taps, no added
//!    latency) and convolved into every HRIR of the table;
//! 4. the table is scaled so that the whole chain plays as loud as the
//!    plain stereo output (ITU-R BS.1770 K-weighted pink noise, mean of a
//!    centred and of an uncorrelated signal);
//! 5. the bass stays out of it: below [`CROSSOVER_HZ`] the renderer passes
//!    the stereo input straight through (see [`BassPath`]), phase-aligned
//!    with the binaural band.

use std::sync::Arc;

use realfft::num_complex::Complex64;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::angle::{SPEAKER_SPREAD_DEG, degree_index};
use crate::hrir::{AZIMUTHS, BassPath, Ear, HrirSet, Measurements};
use crate::xover::{Biquad, Lr4};

/// Below this the stereo input is passed dry; above it, the binaural band.
pub const CROSSOVER_HZ: f64 = 150.0;
/// Largest boost of the correction, relative to the mean level.
pub const MAX_BOOST_DB: f64 = 12.0;
/// Largest cut of the correction, relative to the mean level.
pub const MAX_CUT_DB: f64 = 15.0;
/// Length of the minimum-phase correction filter.
pub const EQ_LEN: usize = 512;
/// Longest equalised HRIR: with the renderer's 1024-sample block, the FFT
/// stays at 2048 points.
pub const MAX_IR_LEN: usize = 1024;

/// Analysis FFT (5.9 Hz bins at 48 kHz).
const ANALYSIS_FFT: usize = 8192;
/// Band of the reference level of the correction.
const REF_BAND: (f64, f64) = (200.0, 10_000.0);
/// Band of the loudness match.
const LOUDNESS_BAND: (f64, f64) = (20.0, 20_000.0);
/// The correction is held flat above this (and above 0.45 × rate).
const HOLD_HIGH_HZ: f64 = 20_000.0;
/// Samples kept before the earliest onset when the common delay is trimmed.
const ONSET_MARGIN: usize = 8;
/// Longest delay of the dry bass path (10 ms at 48 kHz).
const MAX_DRY_DELAY: usize = 480;
/// Points of the sphere used to weigh the measurements by solid angle.
const SPHERE_POINTS: usize = 8192;
/// A point of the sphere farther than this from every measurement counts for
/// none (KEMAR has nothing below −40° of elevation).
const MAX_GAP_DEG: f64 = 15.0;
/// Fade at the end of a truncated filter or HRIR.
const FADE_EQ: usize = 64;
const FADE_IR: usize = 32;

/// The correction filter and what it was made from.
#[derive(Debug, Clone)]
pub struct Design {
    pub rate: u32,
    /// Width of an analysis bin, Hz.
    pub bin_hz: f64,
    /// Diffuse-field power per analysis bin (0..=N/2), 1/3-octave smoothed, dB.
    pub diffuse_db: Vec<f64>,
    /// Mean diffuse-field level over 200 Hz–10 kHz (log frequency), dB.
    pub ref_db: f64,
    /// Correction per analysis bin, dB (capped and held).
    pub correction_db: Vec<f64>,
    /// The minimum-phase filter, [`EQ_LEN`] taps.
    pub filter: Vec<f64>,
}

/// What [`equalise`] did, for the logs and the bench.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Report {
    pub ref_db: f64,
    /// Largest boost and cut actually applied between the crossover and 20 kHz, dB.
    pub max_boost_db: f64,
    pub max_cut_db: f64,
    /// Overall gain of the table for the loudness match, dB.
    pub gain_db: f64,
    /// Common leading delay removed from every HRIR, samples.
    pub trimmed: usize,
    pub ir_len: usize,
    pub bass: BassPath,
}

/// Solid-angle weight of every direction (summing to 1): the share of an
/// even spread of points on the sphere for which it is the nearest
/// measurement. Uniform when nothing can be told.
pub fn direction_weights(dirs: &[[f32; 3]]) -> Vec<f64> {
    let n = dirs.len();
    if n == 0 {
        return Vec::new();
    }
    let mut counts = vec![0usize; n];
    let min_dot = MAX_GAP_DEG.to_radians().cos();
    let golden = std::f64::consts::PI * (1.0 + 5f64.sqrt());
    for i in 0..SPHERE_POINTS {
        // Fibonacci sphere: evenly spread points.
        let z = 1.0 - 2.0 * (i as f64 + 0.5) / SPHERE_POINTS as f64;
        let r = (1.0 - z * z).sqrt();
        let phi = golden * (i as f64 + 0.5);
        let p = [r * phi.cos(), r * phi.sin(), z];
        let (best, dot) = dirs
            .iter()
            .enumerate()
            .map(|(j, d)| (j, p[0] * d[0] as f64 + p[1] * d[1] as f64 + p[2] * d[2] as f64))
            .fold((0, f64::MIN), |a, b| if b.1 > a.1 { b } else { a });
        if dot >= min_dot {
            counts[best] += 1;
        }
    }
    let total: usize = counts.iter().sum();
    if total == 0 {
        return vec![1.0 / n as f64; n];
    }
    counts.iter().map(|&c| c as f64 / total as f64).collect()
}

/// Weighted mean power spectrum of every measurement, both ears, on an
/// `n_fft`-point grid (`n_fft / 2 + 1` bins).
pub fn diffuse_field_power(m: &Measurements, n_fft: usize) -> Vec<f64> {
    let weights = direction_weights(&m.dirs);
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n_fft);
    let mut time = fft.make_input_vec();
    let mut spec = fft.make_output_vec();
    let mut acc = vec![0.0f64; n_fft / 2 + 1];
    for (i, &w) in weights.iter().enumerate() {
        if w == 0.0 {
            continue;
        }
        for ear in [Ear::Left, Ear::Right] {
            time.fill(0.0);
            for (t, &v) in time.iter_mut().zip(m.ir(i, ear)) {
                *t = v as f64;
            }
            fft.process(&mut time, &mut spec).expect("FFT sizes are consistent");
            for (a, s) in acc.iter_mut().zip(&spec) {
                *a += 0.5 * w * s.norm_sqr();
            }
        }
    }
    acc
}

/// Mean power over a band `fraction` of an octave wide around every bin.
pub fn smooth_octave(power: &[f64], fraction: f64) -> Vec<f64> {
    let n = power.len();
    let mut prefix = vec![0.0f64; n + 1];
    for (k, &p) in power.iter().enumerate() {
        prefix[k + 1] = prefix[k] + p;
    }
    let half = 2f64.powf(0.5 / fraction);
    (0..n)
        .map(|k| {
            if k == 0 {
                return power[0];
            }
            let lo = ((k as f64 / half).ceil() as usize).max(1);
            let hi = ((k as f64 * half).floor() as usize).min(n - 1).max(lo);
            (prefix[hi + 1] - prefix[lo]) / (hi + 1 - lo) as f64
        })
        .collect()
}

fn db(p: f64) -> f64 {
    10.0 * p.max(1e-30).log10()
}

/// Mean of `values_db` over a band, with log-frequency (pink) weighting.
fn band_mean(values_db: &[f64], bin_hz: f64, band: (f64, f64)) -> f64 {
    let (mut sum, mut wsum) = (0.0, 0.0);
    for (k, v) in values_db.iter().enumerate().skip(1) {
        let f = k as f64 * bin_hz;
        if f >= band.0 && f <= band.1 {
            sum += v / f;
            wsum += 1.0 / f;
        }
    }
    if wsum > 0.0 { sum / wsum } else { 0.0 }
}

/// Minimum-phase filter of `len` taps whose magnitude is `mag` (one value
/// per bin of an `n_fft`-point grid), by the real cepstrum.
pub fn minimum_phase(mag: &[f64], n_fft: usize, len: usize) -> Vec<f64> {
    assert_eq!(mag.len(), n_fft / 2 + 1);
    let mut planner = RealFftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n_fft);
    let inv = planner.plan_fft_inverse(n_fft);
    let mut spec: Vec<Complex64> = mag.iter().map(|&m| Complex64::new(m.max(1e-12).ln(), 0.0)).collect();
    let mut cep = inv.make_output_vec();
    inv.process(&mut spec, &mut cep).expect("FFT sizes are consistent");
    let scale = 1.0 / n_fft as f64;
    // Fold the (even) cepstrum onto positive quefrencies: the causal,
    // minimum-phase counterpart of the same magnitude.
    for (q, c) in cep.iter_mut().enumerate() {
        *c *= scale
            * match q {
                0 => 1.0,
                q if q < n_fft / 2 => 2.0,
                q if q == n_fft / 2 => 1.0,
                _ => 0.0,
            };
    }
    let mut out = fwd.make_output_vec();
    fwd.process(&mut cep, &mut out).expect("FFT sizes are consistent");
    for c in out.iter_mut() {
        *c = c.exp();
    }
    if let Some(first) = out.first_mut() {
        first.im = 0.0;
    }
    if let Some(last) = out.last_mut() {
        last.im = 0.0;
    }
    let mut h = inv.make_output_vec();
    inv.process(&mut out, &mut h).expect("FFT sizes are consistent");
    let mut filter: Vec<f64> = h.iter().take(len).map(|v| v * scale).collect();
    fade_out(&mut filter, FADE_EQ);
    filter
}

/// Half-Hann fade over the last `n` samples.
fn fade_out(x: &mut [f64], n: usize) {
    let n = n.min(x.len());
    let start = x.len() - n;
    for (i, v) in x[start..].iter_mut().enumerate() {
        *v *= 0.5 + 0.5 * (std::f64::consts::PI * (i + 1) as f64 / n as f64).cos();
    }
}

/// The correction for a set of measurements.
pub fn design(m: &Measurements) -> Design {
    let n_fft = ANALYSIS_FFT;
    let bin_hz = m.rate as f64 / n_fft as f64;
    let diffuse_db: Vec<f64> = smooth_octave(&diffuse_field_power(m, n_fft), 3.0).into_iter().map(db).collect();
    let ref_db = band_mean(&diffuse_db, bin_hz, REF_BAND);
    let hold_high = HOLD_HIGH_HZ.min(0.45 * m.rate as f64);
    let bin = |f: f64| ((f / bin_hz).round() as usize).min(diffuse_db.len() - 1);
    let (k_lo, k_hi) = (bin(CROSSOVER_HZ), bin(hold_high));
    let capped = |k: usize| (ref_db - diffuse_db[k]).clamp(-MAX_CUT_DB, MAX_BOOST_DB);
    let correction_db: Vec<f64> = (0..diffuse_db.len()).map(|k| capped(k.clamp(k_lo, k_hi))).collect();
    let mag: Vec<f64> = correction_db.iter().map(|g| 10f64.powf(g / 20.0)).collect();
    let filter = minimum_phase(&mag, n_fft, EQ_LEN);
    Design { rate: m.rate, bin_hz, diffuse_db, ref_db, correction_db, filter }
}

/// The ITU-R BS.1770 K-weighting (pre-filter shelf and RLB high-pass), for
/// any sample rate (the analog prototypes of libebur128).
fn k_weighting(rate: f64) -> [Biquad; 2] {
    let (f0, gain_db, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let vh = 10f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::from_coeffs(
        [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let rlb = Biquad::from_coeffs([1.0, -2.0, 1.0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0]);
    [shelf, rlb]
}

/// FFT convolution of many signals with one filter.
struct Convolver {
    n_fft: usize,
    fwd: Arc<dyn RealToComplex<f64>>,
    inv: Arc<dyn ComplexToReal<f64>>,
    filter: Vec<Complex64>,
    time: Vec<f64>,
    spec: Vec<Complex64>,
}

impl Convolver {
    fn new(filter: &[f64], max_input: usize) -> Self {
        let n_fft = (filter.len() + max_input - 1).next_power_of_two();
        let mut planner = RealFftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(n_fft);
        let inv = planner.plan_fft_inverse(n_fft);
        let mut time = fwd.make_input_vec();
        time[..filter.len()].copy_from_slice(filter);
        let mut spec = fwd.make_output_vec();
        fwd.process(&mut time, &mut spec).expect("FFT sizes are consistent");
        Self { n_fft, fwd, inv, filter: spec.clone(), time, spec }
    }

    /// The full convolution, `x.len() + filter.len() − 1` samples.
    fn apply(&mut self, x: &[f32], out_len: usize) -> Vec<f64> {
        self.time.fill(0.0);
        for (t, &v) in self.time.iter_mut().zip(x) {
            *t = v as f64;
        }
        self.fwd.process(&mut self.time, &mut self.spec).expect("FFT sizes are consistent");
        let scale = 1.0 / self.n_fft as f64;
        for (s, h) in self.spec.iter_mut().zip(&self.filter) {
            *s *= h * scale;
        }
        if let Some(first) = self.spec.first_mut() {
            first.im = 0.0;
        }
        if let Some(last) = self.spec.last_mut() {
            last.im = 0.0;
        }
        self.inv.process(&mut self.spec, &mut self.time).expect("FFT sizes are consistent");
        self.time[..out_len].to_vec()
    }
}

/// Spectrum of an impulse response on an `n_fft`-point grid.
fn spectrum(fft: &Arc<dyn RealToComplex<f64>>, ir: &[f32], n_fft: usize) -> Vec<Complex64> {
    let mut time = vec![0.0f64; n_fft];
    for (t, &v) in time.iter_mut().zip(ir) {
        *t = v as f64;
    }
    let mut spec = fft.make_output_vec();
    fft.process(&mut time, &mut spec).expect("FFT sizes are consistent");
    spec
}

/// Applies the diffuse-field correction of `m` to `set` (the horizontal
/// table of the same file): equalised, trimmed, loudness-matched HRIRs and
/// the dry bass path that goes with them.
pub fn equalise(set: &HrirSet, m: &Measurements) -> (HrirSet, Report) {
    let d = design(m);
    let rate = set.rate();
    let full = set.len() + d.filter.len() - 1;
    let mut conv = Convolver::new(&d.filter, set.len());
    let mut irs: Vec<Vec<f64>> = Vec::with_capacity(AZIMUTHS * 2);
    for az in 0..AZIMUTHS {
        for ear in [Ear::Left, Ear::Right] {
            irs.push(conv.apply(set.ir(az, ear), full));
        }
    }

    // Common leading delay (the source distance): trimmed, which also
    // lowers the latency. The ITD (onset differences) is kept.
    let onset = |x: &Vec<f64>| {
        let peak = x.iter().fold(0.0f64, |a, v| a.max(v.abs()));
        x.iter().position(|v| v.abs() > 0.05 * peak).unwrap_or(0)
    };
    let trimmed = irs.iter().map(onset).min().unwrap_or(0).saturating_sub(ONSET_MARGIN);
    let len = (full - trimmed).min(MAX_IR_LEN);
    for ir in &mut irs {
        ir.drain(..trimmed);
        if ir.len() > len {
            ir.truncate(len);
            fade_out(ir, FADE_IR);
        }
    }

    // The ±30° pair (yaw 0), both ears.
    let n_fft = ANALYSIS_FFT;
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n_fft);
    let (az_l, az_r) = (degree_index(SPEAKER_SPREAD_DEG), degree_index(-SPEAKER_SPREAD_DEG));
    let as_f32 = |v: &Vec<f64>| v.iter().map(|&x| x as f32).collect::<Vec<f32>>();
    let spec_of = |az: usize, ear: Ear| spectrum(&fft, &as_f32(&irs[az * 2 + ear as usize]), n_fft);
    // pair[ear] = (from the left speaker, from the right speaker)
    let pair = [Ear::Left, Ear::Right].map(|ear| (spec_of(az_l, ear), spec_of(az_r, ear)));
    let rate_f = rate as f64;
    let (lp, hp) = (Lr4::lowpass(CROSSOVER_HZ, rate_f), Lr4::highpass(CROSSOVER_HZ, rate_f));

    // Dry bass delay: the one that puts the low-pass branch most in phase
    // with the binaural band of a centred source around the crossover.
    let band: Vec<(f64, Complex64, Complex64)> = (1..=n_fft / 2)
        .map(|k| (k as f64 * d.bin_hz, k))
        .filter(|(f, _)| *f >= 0.5 * CROSSOVER_HZ && *f <= 2.0 * CROSSOVER_HZ)
        .map(|(f, k)| (f, lp.response(f, rate_f), hp.response(f, rate_f) * (pair[0].0[k] + pair[0].1[k])))
        .collect();
    let score = |delay: usize| {
        band.iter()
            .map(|(f, l, b)| {
                let shift = Complex64::from_polar(1.0, -2.0 * std::f64::consts::PI * f * delay as f64 / rate_f);
                (l * shift * b.conj()).re
            })
            .sum::<f64>()
    };
    let dry_delay = (0..=MAX_DRY_DELAY)
        .map(|dl| (dl, score(dl)))
        .fold((0, f64::MIN), |a, b| if b.1 > a.1 { b } else { a })
        .0;

    // Loudness: the whole chain (dry bass + binaural) against the plain
    // stereo output, ITU-R BS.1770 K-weighted pink noise, mean of a centred
    // (L = R) and an uncorrelated signal. Solved for the binaural gain.
    struct Bin {
        /// Dry path (low-pass, delay).
        dry: Complex64,
        /// K-weighting × pink density.
        weight: f64,
        /// Per ear: high-passed binaural from the left and the right speaker.
        ears: [(Complex64, Complex64); 2],
    }
    let k_weight = k_weighting(rate_f);
    let bins: Vec<Bin> = (1..=n_fft / 2)
        .map(|k| (k, k as f64 * d.bin_hz))
        .filter(|(_, f)| *f >= LOUDNESS_BAND.0 && *f <= LOUDNESS_BAND.1.min(0.45 * rate_f))
        .map(|(k, f)| {
            let shift = Complex64::from_polar(1.0, -2.0 * std::f64::consts::PI * f * dry_delay as f64 / rate_f);
            let h = hp.response(f, rate_f);
            Bin {
                dry: lp.response(f, rate_f) * shift,
                weight: k_weight.iter().map(|b| b.response(f, rate_f).norm_sqr()).product::<f64>() / f,
                ears: [(pair[0].0[k] * h, pair[0].1[k] * h), (pair[1].0[k] * h, pair[1].1[k] * h)],
            }
        })
        .collect();
    let loudness_db = |gain: f64| {
        let (mut centred, mut uncorrelated, mut plain) = (0.0, 0.0, 0.0);
        for Bin { dry, weight: w, ears } in &bins {
            for (ear, (from_l, from_r)) in ears.iter().enumerate() {
                // The dry path feeds each ear its own channel.
                let (own, other) = if ear == 0 { (from_l, from_r) } else { (from_r, from_l) };
                centred += w * (dry + (own + other) * gain).norm_sqr();
                uncorrelated += w * ((dry + own * gain).norm_sqr() + (other * gain).norm_sqr());
                plain += w;
            }
        }
        0.5 * (db(centred / plain) + db(uncorrelated / plain))
    };
    // Monotonic in the gain above the crossover: bisection on dB.
    let (mut lo, mut hi) = (-40.0f64, 40.0f64);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if loudness_db(10f64.powf(mid / 20.0)) > 0.0 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let gain_db = 0.5 * (lo + hi);
    let gain = 10f64.powf(gain_db / 20.0);

    let mut table = vec![0.0f32; AZIMUTHS * 2 * len];
    for (i, ir) in irs.iter().enumerate() {
        for (t, &v) in table[i * len..i * len + ir.len()].iter_mut().zip(ir) {
            *t = (v * gain) as f32;
        }
    }
    let bass = BassPath { crossover_hz: CROSSOVER_HZ as f32, dry_delay };
    let applied = |k: usize| d.correction_db[k];
    let ks = (CROSSOVER_HZ / d.bin_hz) as usize..=((HOLD_HIGH_HZ.min(0.45 * rate as f64)) / d.bin_hz) as usize;
    let report = Report {
        ref_db: d.ref_db,
        max_boost_db: ks.clone().map(applied).fold(f64::MIN, f64::max),
        max_cut_db: -ks.map(applied).fold(f64::MAX, f64::min),
        gain_db,
        trimmed,
        ir_len: len,
        bass,
    };
    (HrirSet::from_parts(rate, len, table, Some(bass)), report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random numbers in [−1, 1).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f64 / (1u64 << 24) as f64) * 2.0 - 1.0
        }
    }

    /// A biquad peak (RBJ), for coloured synthetic measurements.
    fn peak(f0: f64, gain_db: f64, q: f64, rate: f64) -> ([f64; 3], [f64; 2]) {
        let a = 10f64.powf(gain_db / 40.0);
        let w = 2.0 * std::f64::consts::PI * f0 / rate;
        let alpha = w.sin() / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        (
            [(1.0 + alpha * a) / a0, -2.0 * w.cos() / a0, (1.0 - alpha * a) / a0],
            [-2.0 * w.cos() / a0, (1.0 - alpha / a) / a0],
        )
    }

    fn filt(x: &[f64], (b, a): ([f64; 3], [f64; 2])) -> Vec<f64> {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        x.iter()
            .map(|&x0| {
                let y0 = b[0] * x0 + b[1] * x1 + b[2] * x2 - a[0] * y1 - a[1] * y2;
                (x2, x1, y2, y1) = (x1, x0, y1, y0);
                y0
            })
            .collect()
    }

    /// Measurements on a sphere sharing a strong colouring (a +12 dB ear
    /// canal at 2.7 kHz, a −10 dB dip at 9 kHz, a falling top) plus a
    /// random, direction-dependent part, like a real set.
    fn coloured_set(len: usize) -> Measurements {
        let rate = 48_000u32;
        let mut rng = Lcg(11);
        let mut dirs = Vec::new();
        let mut irs = Vec::new();
        for el in (-40..=90).step_by(10) {
            let n_az = ((360.0 * (el as f64).to_radians().cos() / 10.0).round() as usize).max(1);
            for i in 0..n_az {
                let az = (i as f64 * 360.0 / n_az as f64).to_radians();
                let el = (el as f64).to_radians();
                dirs.push([(el.cos() * az.cos()) as f32, (el.cos() * az.sin()) as f32, el.sin() as f32]);
                for _ in 0..2 {
                    let mut x = vec![0.0f64; len];
                    let onset = 20 + (rng.next().abs() * 20.0) as usize;
                    for (t, v) in x.iter_mut().enumerate().skip(onset).take(48) {
                        *v = rng.next() * (-((t - onset) as f64) / 6.0).exp();
                    }
                    x[onset] += 1.0;
                    let x = filt(&x, peak(2700.0, 12.0, 1.5, rate as f64));
                    let x = filt(&x, peak(9000.0, -10.0, 2.0, rate as f64));
                    let x = filt(&x, peak(16_000.0, -6.0, 0.7, rate as f64));
                    irs.extend(x.iter().map(|&v| v as f32));
                }
            }
        }
        Measurements { rate, len, dirs, irs }
    }

    /// The 1/3-octave smoothed diffuse field of `m` once `filter` is applied, dB.
    fn equalised_diffuse_db(m: &Measurements, filter: &[f64]) -> Vec<f64> {
        let mut conv = Convolver::new(filter, m.len);
        let out_len = m.len + filter.len() - 1;
        let mut irs = Vec::new();
        for i in 0..m.dirs.len() {
            for ear in [Ear::Left, Ear::Right] {
                irs.extend(conv.apply(m.ir(i, ear), out_len).into_iter().map(|v| v as f32));
            }
        }
        let eqd = Measurements { rate: m.rate, len: out_len, dirs: m.dirs.clone(), irs };
        smooth_octave(&diffuse_field_power(&eqd, ANALYSIS_FFT), 3.0).into_iter().map(db).collect()
    }

    fn flat_within(diffuse_db: &[f64], bin_hz: f64, band: (f64, f64)) -> (f64, f64) {
        let mean = band_mean(diffuse_db, bin_hz, REF_BAND);
        let vals = diffuse_db
            .iter()
            .enumerate()
            .filter(|(k, _)| *k as f64 * bin_hz >= band.0 && *k as f64 * bin_hz <= band.1)
            .map(|(_, v)| v - mean);
        vals.fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)))
    }

    #[test]
    fn weights_follow_solid_angle() {
        // A dense equator (36 points, 10° apart) and the north pole: with
        // the 15° reach, the equator covers a ±15° band (4π·sin 15°) and
        // the pole a 15° cap (2π·(1 − cos 15°)).
        let mut dirs: Vec<[f32; 3]> = (0..36)
            .map(|i| {
                let a = (i as f32 * 10.0).to_radians();
                [a.cos(), a.sin(), 0.0]
            })
            .collect();
        dirs.push([0.0, 0.0, 1.0]);
        let w = direction_weights(&dirs);
        assert!((w.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        let eq = w[0];
        assert!(w[..36].iter().all(|&x| (x - eq).abs() / eq < 0.1), "{w:?}");
        let (band, cap) = (15f64.to_radians().sin() * 2.0, 1.0 - 15f64.to_radians().cos());
        let pole = cap / (band + cap);
        assert!((w[36] - pole).abs() / pole < 0.1, "pole {} vs {pole}", w[36]);
    }

    #[test]
    fn equalised_diffuse_field_is_flat() {
        let m = coloured_set(256);
        let d = design(&m);
        let raw = flat_within(&d.diffuse_db, d.bin_hz, (CROSSOVER_HZ, 16_000.0));
        assert!(raw.1 - raw.0 > 15.0, "test set not coloured enough: {raw:?}");
        let (lo, hi) = flat_within(&equalised_diffuse_db(&m, &d.filter), d.bin_hz, (CROSSOVER_HZ, 16_000.0));
        assert!(lo > -3.0 && hi < 3.0, "equalised diffuse field {lo:.2} .. {hi:.2} dB");
    }

    #[test]
    fn correction_is_capped() {
        // A 40 dB deep notch and a 30 dB peak shared by every direction: the
        // correction may not follow them all the way.
        let mut m = coloured_set(256);
        for ir in m.irs.chunks_mut(m.len) {
            let x: Vec<f64> = ir.iter().map(|&v| v as f64).collect();
            let x = filt(&x, peak(1000.0, -40.0, 4.0, 48_000.0));
            let x = filt(&x, peak(5000.0, 30.0, 4.0, 48_000.0));
            for (o, v) in ir.iter_mut().zip(x) {
                *o = v as f32;
            }
        }
        let d = design(&m);
        let max = d.correction_db.iter().cloned().fold(f64::MIN, f64::max);
        let min = d.correction_db.iter().cloned().fold(f64::MAX, f64::min);
        assert!((max - MAX_BOOST_DB).abs() < 1e-9, "max boost {max}");
        assert!((min + MAX_CUT_DB).abs() < 1e-9, "max cut {min}");
        // The realised filter respects the caps too (within the ripple of
        // the 512-tap truncation).
        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(ANALYSIS_FFT);
        let mut t = vec![0.0; ANALYSIS_FFT];
        t[..d.filter.len()].copy_from_slice(&d.filter);
        let mut s = fft.make_output_vec();
        fft.process(&mut t, &mut s).unwrap();
        for (k, c) in s.iter().enumerate().skip(1) {
            let g = 20.0 * c.norm().log10();
            assert!(g < MAX_BOOST_DB + 1.0 && g > -MAX_CUT_DB - 1.5, "bin {k}: {g:.2} dB");
        }
    }

    #[test]
    fn minimum_phase_is_causal_and_exact() {
        let m = coloured_set(256);
        let d = design(&m);
        // Magnitude: the filter realises the correction.
        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(ANALYSIS_FFT);
        let mut t = vec![0.0; ANALYSIS_FFT];
        t[..d.filter.len()].copy_from_slice(&d.filter);
        let mut s = fft.make_output_vec();
        fft.process(&mut t, &mut s).unwrap();
        for (k, c) in s.iter().enumerate() {
            let f = k as f64 * d.bin_hz;
            if (100.0..=16_000.0).contains(&f) {
                let err = 20.0 * c.norm().log10() - d.correction_db[k];
                assert!(err.abs() < 0.5, "{f:.0} Hz: {err:.2} dB off");
            }
        }
        // The untruncated design: almost no energy left after EQ_LEN (no
        // time aliasing, nothing lost by the truncation)...
        let mag: Vec<f64> = d.correction_db.iter().map(|g| 10f64.powf(g / 20.0)).collect();
        let long = minimum_phase(&mag, ANALYSIS_FFT, ANALYSIS_FFT);
        let e = |x: &[f64]| x.iter().map(|v| v * v).sum::<f64>();
        let tail_db = 10.0 * (e(&long[EQ_LEN..]) / e(&long)).log10();
        assert!(tail_db < -60.0, "tail {tail_db:.1} dB");
        // ...and the energy comes first: at every instant the minimum-phase
        // filter has delivered at least as much as the zero-phase one
        // with the same magnitude, centred (the energy-delay property).
        let zero_phase = {
            let inv = RealFftPlanner::<f64>::new().plan_fft_inverse(ANALYSIS_FFT);
            let mut spec: Vec<Complex64> = mag.iter().map(|&v| Complex64::new(v, 0.0)).collect();
            let mut h = inv.make_output_vec();
            inv.process(&mut spec, &mut h).unwrap();
            let n = ANALYSIS_FFT;
            (0..n).map(|i| h[(i + n / 2) % n] / n as f64).collect::<Vec<f64>>()
        };
        let (mut a, mut b) = (0.0, 0.0);
        for i in 0..ANALYSIS_FFT {
            a += long[i] * long[i];
            b += zero_phase[i] * zero_phase[i];
            assert!(a >= b - 1e-9 * e(&long), "sample {i}");
        }
        // Stable and deterministic: designing twice gives the same filter,
        // and every tap is finite.
        assert!(d.filter.iter().all(|v| v.is_finite()));
        assert_eq!(design(&m).filter, d.filter);
    }

    #[test]
    fn smoothing_keeps_a_flat_spectrum_flat() {
        let p = vec![2.0f64; 4097];
        assert!(smooth_octave(&p, 3.0).iter().all(|&v| (v - 2.0).abs() < 1e-12));
    }

    #[test]
    fn kemar_diffuse_field_is_flat_after_equalisation() {
        let path = std::path::Path::new(crate::conf::DEFAULT_SOFA);
        if !path.exists() {
            eprintln!("skipped: {} not installed", path.display());
            return;
        }
        let (set, m) = HrirSet::load_sofa_with_measurements(path, 48_000).unwrap();
        assert_eq!(m.dirs.len(), 710);
        let d = design(&m);
        // Before: the ear canal resonance and the loudspeaker's bass.
        let raw = flat_within(&d.diffuse_db, d.bin_hz, (60.0, 12_000.0));
        assert!(raw.0 < -10.0 && raw.1 > 10.0, "raw KEMAR {raw:?}");
        let (lo, hi) = flat_within(&equalised_diffuse_db(&m, &d.filter), d.bin_hz, (CROSSOVER_HZ, 16_000.0));
        assert!(lo > -3.0 && hi < 3.0, "equalised KEMAR diffuse field {lo:.2} .. {hi:.2} dB");

        let (eq, report) = equalise(&set, &m);
        assert!(eq.len() <= MAX_IR_LEN);
        assert!(report.max_boost_db <= MAX_BOOST_DB && report.max_cut_db <= MAX_CUT_DB);
        let bass = eq.bass().unwrap();
        assert!(bass.dry_delay > 0 && bass.dry_delay < MAX_DRY_DELAY, "{report:?}");
        // The interaural cues survive: 90° is still much louder on the left.
        let e = |x: &[f32]| x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>();
        assert!(10.0 * (e(eq.ir(90, Ear::Left)) / e(eq.ir(90, Ear::Right))).log10() > 8.0);
    }
}
