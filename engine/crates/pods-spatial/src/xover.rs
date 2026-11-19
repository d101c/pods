//! The bass crossover: 4th-order Linkwitz-Riley filters (two cascaded
//! 2nd-order Butterworth sections). Low-pass and high-pass are in phase at
//! every frequency and sum to an all-pass, so the dry bass and the binaural
//! band join without a dip at the crossover.

use realfft::num_complex::Complex64;

/// One 2nd-order section, transposed direct form II, in f64 (the poles of a
/// 150 Hz filter at 48 kHz sit too close to 1 for f32 coefficients).
#[derive(Debug, Clone, Copy, Default)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    s: [f64; 2],
}

impl Biquad {
    /// Butterworth low-pass (Q = 1/√2), bilinear transform with prewarping.
    pub fn butterworth_lowpass(fc: f64, rate: f64) -> Self {
        let k = (std::f64::consts::PI * fc / rate).tan();
        let norm = 1.0 / (1.0 + std::f64::consts::SQRT_2 * k + k * k);
        let b0 = k * k * norm;
        Self::from_coeffs([b0, 2.0 * b0, b0], [2.0 * (k * k - 1.0) * norm, (1.0 - std::f64::consts::SQRT_2 * k + k * k) * norm])
    }

    /// Butterworth high-pass (Q = 1/√2), bilinear transform with prewarping.
    pub fn butterworth_highpass(fc: f64, rate: f64) -> Self {
        let k = (std::f64::consts::PI * fc / rate).tan();
        let norm = 1.0 / (1.0 + std::f64::consts::SQRT_2 * k + k * k);
        Self::from_coeffs([norm, -2.0 * norm, norm], [2.0 * (k * k - 1.0) * norm, (1.0 - std::f64::consts::SQRT_2 * k + k * k) * norm])
    }

    /// From normalised coefficients (`a0` = 1).
    pub fn from_coeffs(b: [f64; 3], a: [f64; 2]) -> Self {
        Self { b, a, s: [0.0; 2] }
    }

    #[inline]
    pub fn tick(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.s[0];
        self.s[0] = self.b[1] * x - self.a[0] * y + self.s[1];
        self.s[1] = self.b[2] * x - self.a[1] * y;
        y
    }

    pub fn reset(&mut self) {
        self.s = [0.0; 2];
    }

    /// Clears a state that decayed to nothing (no denormals in silence).
    pub fn flush_tiny(&mut self) {
        for s in &mut self.s {
            if s.abs() < 1e-25 {
                *s = 0.0;
            }
        }
    }

    /// Frequency response at `f` Hz.
    pub fn response(&self, f: f64, rate: f64) -> Complex64 {
        let w = 2.0 * std::f64::consts::PI * f / rate;
        let z1 = Complex64::from_polar(1.0, -w);
        let z2 = z1 * z1;
        (self.b[0] + self.b[1] * z1 + self.b[2] * z2) / (1.0 + self.a[0] * z1 + self.a[1] * z2)
    }
}

/// A 4th-order Linkwitz-Riley filter: the same Butterworth section twice.
#[derive(Debug, Clone, Copy)]
pub struct Lr4 {
    stages: [Biquad; 2],
}

impl Lr4 {
    pub fn lowpass(fc: f64, rate: f64) -> Self {
        let s = Biquad::butterworth_lowpass(fc, rate);
        Self { stages: [s, s] }
    }

    pub fn highpass(fc: f64, rate: f64) -> Self {
        let s = Biquad::butterworth_highpass(fc, rate);
        Self { stages: [s, s] }
    }

    #[inline]
    pub fn tick(&mut self, x: f64) -> f64 {
        let y = self.stages[0].tick(x);
        self.stages[1].tick(y)
    }

    pub fn reset(&mut self) {
        for s in &mut self.stages {
            s.reset();
        }
    }

    pub fn flush_tiny(&mut self) {
        for s in &mut self.stages {
            s.flush_tiny();
        }
    }

    pub fn response(&self, f: f64, rate: f64) -> Complex64 {
        self.stages[0].response(f, rate) * self.stages[1].response(f, rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lr4_branches_sum_to_an_allpass() {
        let (fc, rate) = (150.0, 48_000.0);
        let (lp, hp) = (Lr4::lowpass(fc, rate), Lr4::highpass(fc, rate));
        for f in [10.0, 50.0, 100.0, 150.0, 200.0, 400.0, 1000.0, 10_000.0] {
            let sum = (lp.response(f, rate) + hp.response(f, rate)).norm();
            assert!((sum - 1.0).abs() < 1e-9, "|LP+HP| at {f} Hz = {sum}");
        }
        // Each branch is −6 dB at the crossover, and in phase with the other.
        let (l, h) = (lp.response(fc, rate), hp.response(fc, rate));
        assert!((l.norm() - 0.5).abs() < 1e-9 && (h.norm() - 0.5).abs() < 1e-9);
        assert!((l.arg() - h.arg()).abs() < 1e-9);
    }

    #[test]
    fn time_domain_matches_the_response() {
        // A 1 kHz sine through the low-pass: about −66 dB (4th order, 2.7 octaves up).
        let rate = 48_000.0;
        let mut lp = Lr4::lowpass(150.0, rate);
        let mut peak = 0.0f64;
        for i in 0..48_000 {
            let y = lp.tick((2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate).sin());
            if i > 24_000 {
                peak = peak.max(y.abs());
            }
        }
        let expect = lp.response(1000.0, rate).norm();
        assert!((peak - expect).abs() < 1e-6, "{peak} vs {expect}");
        assert!(20.0 * expect.log10() < -60.0);
    }
}
