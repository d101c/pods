// SPDX-License-Identifier: GPL-3.0-or-later
//! The 58-byte motion block, the same on both sensor paths.
//!
//! Offsets and scales as measured on the AirPods Pro (1st generation) in
//! M0 (docs/ARCHITECTURE.md, Head tracking), the layout first described by MousePods:
//!
//! | offset | content | scale |
//! |---|---|---|
//! | 12 | sensor clock, u64 LE | µs (19 892 µs between reports at 20 000 µs asked) |
//! | 20 | attitude quaternion, vector part x, y, z (i16) | Q15, w = √(1 − ‖v‖²) |
//! | 26 | angular rate, earbud frame (i16 × 3) | 1/512 rad/s |
//! | 44 | gravity, earbud frame (i16 × 3) | 1/1024 g |

use std::time::Duration;

use crate::quat::{Quat, Vec3};

/// Smallest block a sample can be read from (gravity ends at byte 50).
pub const BLOCK_MIN_LEN: usize = 50;

/// One motion report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionSample {
    /// Attitude computed by the AirPods (earbud frame to their world frame).
    pub attitude: Quat,
    /// Angular rate in the earbud frame, rad/s.
    pub rate: Vec3,
    /// Gravity in the earbud frame, in g.
    pub gravity: Vec3,
    /// The AirPods' own clock, µs, when the block has one.
    pub sensor_time_us: Option<u64>,
    /// When the frame was received, on the caller's monotonic clock.
    pub received: Duration,
}

fn i16_at(b: &[u8], o: usize) -> f64 {
    i16::from_le_bytes([b[o], b[o + 1]]) as f64
}

impl MotionSample {
    /// Reads a block (without the report-type byte of RemoteHID).
    pub fn parse(block: &[u8], received: Duration) -> Option<Self> {
        if block.len() < BLOCK_MIN_LEN {
            return None;
        }
        let q15 = 32768.0;
        let (x, y, z) = (
            i16_at(block, 20) / q15,
            i16_at(block, 22) / q15,
            i16_at(block, 24) / q15,
        );
        let w = (1.0 - x * x - y * y - z * z).max(0.0).sqrt();
        let rate = [26, 28, 30].map(|o| i16_at(block, o) / 512.0);
        let gravity = [44, 46, 48].map(|o| i16_at(block, o) / 1024.0);
        let mut clock = [0u8; 8];
        clock.copy_from_slice(&block[12..20]);
        let sensor_time_us = Some(u64::from_le_bytes(clock)).filter(|&t| t != 0);
        Some(Self {
            attitude: Quat { w, x, y, z }.normalized(),
            rate,
            gravity,
            sensor_time_us,
            received,
        })
    }

    /// False for the first reports of a stream, sent before the AirPods'
    /// fusion has an attitude (vector part all zero) or a gravity.
    pub fn is_ready(&self) -> bool {
        let q = self.attitude;
        let g = crate::quat::norm(self.gravity);
        (q.x != 0.0 || q.y != 0.0 || q.z != 0.0) && g > 0.5 && g < 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn a_real_block_decodes() {
        // Report at 22.6554 s of the M0 quater capture (block only).
        let b = hex(concat!(
            "01803a190d90110000030443762fff5900000000209887fd153c06000e00010080",
            "01d683e00303000b000200fd0260024b013e0f9138673d0000"
        ));
        assert_eq!(b.len(), 58);
        let s = MotionSample::parse(&b, Duration::ZERO).unwrap();
        assert_eq!(s.sensor_time_us, Some(0x59ff2f76));
        assert!((s.attitude.x - (0x9820u16 as i16) as f64 / 32768.0).abs() < 1e-12);
        assert!((s.attitude.norm() - 1.0).abs() < 1e-12);
        let g = crate::quat::norm(s.gravity);
        assert!((g - 1.0).abs() < 0.05, "gravity {g}");
        assert!(s.is_ready());
    }

    #[test]
    fn the_warm_up_report_is_not_ready() {
        // First report of the stream: no attitude yet.
        let b = hex(concat!(
            "010000000000000000030082c0a8cf58000000000000000000000000000000006210",
            "62106210c40051032b06c20052032a029764953ac72d0000"
        ));
        assert_eq!(b.len(), 58);
        let s = MotionSample::parse(&b, Duration::ZERO).unwrap();
        assert!(!s.is_ready());
        assert!(MotionSample::parse(&b[..49], Duration::ZERO).is_none());
    }
}
