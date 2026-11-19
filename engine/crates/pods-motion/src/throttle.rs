// SPDX-License-Identifier: GPL-3.0-or-later
//! Caps the pose rate of one consumer (renderer, interface).
//!
//! The AirPods report at 50.3 Hz and Bluetooth delivers them in bursts. A
//! pose that comes too early is held, not lost: the next sample replaces
//! it, or [`Throttle::poll`] hands it over once its turn comes.

use std::time::Duration;

use crate::tracker::Pose;

#[derive(Clone, Debug)]
pub struct Throttle {
    rate_hz: f64,
    /// Token bucket: one token per pose, refilled at `rate_hz`, at most
    /// [`BURST`]. A report that Bluetooth delivered late is made up for by
    /// the next one, so a 50.3 Hz stream under a 50 Hz cap stays at 50 Hz.
    credit: f64,
    last: Option<Duration>,
    pending: Option<Pose>,
}

/// Poses that may go out back to back after a late delivery.
pub const BURST: f64 = 2.0;

impl Throttle {
    /// At most `max_hz` poses per second over any window, plus [`BURST`].
    pub fn new(max_hz: f64) -> Self {
        Self {
            rate_hz: max_hz.max(0.1),
            credit: 1.0,
            last: None,
            pending: None,
        }
    }

    /// Offers a pose at `now`; returns it when it may go out.
    pub fn offer(&mut self, pose: Pose, now: Duration) -> Option<Pose> {
        self.pending = Some(pose);
        self.poll(now)
    }

    /// The held pose, once its turn has come.
    pub fn poll(&mut self, now: Duration) -> Option<Pose> {
        if let Some(last) = self.last {
            let gained = now.saturating_sub(last).as_secs_f64() * self.rate_hz;
            self.credit = (self.credit + gained).min(BURST);
        }
        self.last = Some(now.max(self.last.unwrap_or(now)));
        if self.credit < 1.0 {
            return None;
        }
        let pose = self.pending.take()?;
        self.credit -= 1.0;
        Some(pose)
    }

    /// Drops the held pose and starts afresh.
    pub fn clear(&mut self) {
        *self = Self::new(self.rate_hz);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pose(t: f64) -> Pose {
        Pose {
            yaw_deg: t,
            tilt_deg: 0.0,
            time: Duration::from_secs_f64(t),
            following: false,
        }
    }

    #[test]
    fn a_faster_stream_is_capped() {
        let mut th = Throttle::new(30.0);
        let mut out = 0;
        // 50.3 Hz for 10 s.
        for i in 0..503 {
            let t = i as f64 / 50.3;
            out += th.offer(pose(t), Duration::from_secs_f64(t)).is_some() as u32;
        }
        assert!((295..=302).contains(&out), "{out}");
    }

    #[test]
    fn a_stream_just_above_the_cap_is_not_halved() {
        let mut th = Throttle::new(50.0);
        let mut out = 0;
        for i in 0..503 {
            let t = i as f64 / 50.3;
            out += th.offer(pose(t), Duration::from_secs_f64(t)).is_some() as u32;
        }
        assert!((495..=502).contains(&out), "{out}");
    }

    #[test]
    fn a_held_pose_comes_out_later() {
        let mut th = Throttle::new(30.0);
        assert!(th.offer(pose(0.0), Duration::ZERO).is_some());
        assert!(th.offer(pose(0.01), Duration::from_millis(10)).is_none());
        assert!(th.poll(Duration::from_millis(20)).is_none());
        let p = th.poll(Duration::from_millis(40)).unwrap();
        assert_eq!(p.yaw_deg, 0.01);
        assert!(th.poll(Duration::from_millis(80)).is_none());
    }
}
