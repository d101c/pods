// SPDX-License-Identifier: GPL-3.0-or-later
// Derived from podtrack `crates/podtrack-core/src/filter.rs`, Copyright 2026
// René Onier, Apache-2.0. Changes: angle-wide defaults, no serde. See
// engine/NOTICE. The filter itself is Casiez, Roussel and Vogel, "1€ Filter"
// (CHI 2012).
//! One-Euro filter: smooth at rest, little lag while the head moves.

use std::f64::consts::PI;

fn alpha(cutoff_hz: f64, dt: f64) -> f64 {
    let tau = 1.0 / (2.0 * PI * cutoff_hz);
    1.0 / (1.0 + tau / dt)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OneEuroParams {
    /// Cut-off at rest, Hz.
    pub min_cutoff_hz: f64,
    /// How fast the cut-off rises with speed, Hz per (unit/s).
    pub beta: f64,
    /// Cut-off of the speed estimate, Hz.
    pub d_cutoff_hz: f64,
}

impl Default for OneEuroParams {
    /// Tuned for a yaw in degrees at 50 Hz: rest noise of 0.24° (σ, M0
    /// quater) falls under 0.1°, a 100°/s turn gets a 6 Hz cut-off.
    fn default() -> Self {
        Self {
            min_cutoff_hz: 1.0,
            beta: 0.05,
            d_cutoff_hz: 1.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OneEuro {
    pub params: OneEuroParams,
    x: Option<f64>,
    dx: f64,
}

impl OneEuro {
    pub fn new(params: OneEuroParams) -> Self {
        Self {
            params,
            x: None,
            dx: 0.0,
        }
    }

    /// Filters `x`, `dt` seconds after the previous value.
    pub fn apply(&mut self, x: f64, dt: f64) -> f64 {
        let Some(prev) = self.x.filter(|_| dt > 0.0) else {
            self.x = Some(x);
            self.dx = 0.0;
            return x;
        };
        let p = self.params;
        let dx = (x - prev) / dt;
        self.dx += alpha(p.d_cutoff_hz, dt) * (dx - self.dx);
        let out = prev + alpha(p.min_cutoff_hz + p.beta * self.dx.abs(), dt) * (x - prev);
        self.x = Some(out);
        out
    }

    pub fn reset(&mut self) {
        self.x = None;
        self.dx = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_value_passes_then_smooths() {
        let mut f = OneEuro::new(OneEuroParams::default());
        assert_eq!(f.apply(5.0, 0.02), 5.0);
        let y = f.apply(6.0, 0.02);
        assert!(y > 5.0 && y < 6.0);
    }

    #[test]
    fn a_fast_turn_is_followed_closely() {
        let mut f = OneEuro::new(OneEuroParams::default());
        let mut y = 0.0;
        // 100°/s for 0.9 s, sampled at 50 Hz.
        for i in 0..=45 {
            y = f.apply(i as f64 * 2.0, 0.02);
        }
        assert!(90.0 - y < 10.0, "lag {}", 90.0 - y);
    }
}
