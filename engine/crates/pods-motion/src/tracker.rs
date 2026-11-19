// SPDX-License-Identifier: GPL-3.0-or-later
//! Motion samples to a head pose: yaw by twist about gravity, tilt,
//! One-Euro smoothing, recentring and Follow Screen.
//!
//! **Yaw.** The Euler yaw of the AirPods' quaternion is wrong because the
//! earbud sits tilted in the ear (gravity seen by the bud ≈ (0.33, 0.81,
//! 0.46) g in M0 quater). The yaw is the twist of the relative rotation
//! `q_ref⁻¹·q` about the gravity measured at the reference. M0 checked it
//! against the gyroscope projected on gravity (3° apart over 36 s).
//!
//! **Sign.** In that computation a turn to the left is negative; the pose
//! flips it: 0 straight ahead, positive to the left (the PipeWire azimuth
//! convention, 90° = left).
//!
//! **Follow Screen.** When the head stays turned more than 20° for more than
//! 6 s, the reference glides to the new direction in about 3 s, like Apple's
//! "follow the device". Below the threshold the reference creeps towards
//! the head with a long time constant, which absorbs the slow drift of the
//! AirPods' fusion (−2.6°/min measured at rest) before it can be heard.
//! Neither happens while the head is tilted far (looking up or down).

use std::time::Duration;

use crate::block::MotionSample;
use crate::filter::{OneEuro, OneEuroParams};
use crate::quat::{Quat, Vec3, unit, wrap180};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackerConfig {
    /// Let the reference follow the head (see the module docs).
    pub follow_screen: bool,
    /// Yaw beyond which the head counts as turned away, degrees.
    pub follow_threshold_deg: f64,
    /// How long the head must stay turned away before the reference moves.
    pub follow_hold: Duration,
    /// How long the reference takes to glide to the new direction.
    pub follow_glide: Duration,
    /// Time constant of the creep below the threshold (drift absorption).
    pub drift_time_constant: Duration,
    /// Beyond this tilt the head looks up or down, not at another screen:
    /// Follow Screen waits (looking up couples a few degrees into the yaw).
    pub follow_max_tilt_deg: f64,
    pub filter: OneEuroParams,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            follow_screen: true,
            follow_threshold_deg: 20.0,
            follow_hold: Duration::from_secs(6),
            follow_glide: Duration::from_secs(3),
            drift_time_constant: Duration::from_secs(40),
            follow_max_tilt_deg: 30.0,
            filter: OneEuroParams::default(),
        }
    }
}

/// Where the head points, relative to the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    /// Azimuth of the head, degrees in (−180, 180]: 0 ahead, positive to the
    /// left. A source fixed in front of the screen sits at `-yaw_deg`.
    pub yaw_deg: f64,
    /// How far the head leans away from its upright reference posture,
    /// degrees, any direction (nod or roll). Unsigned: telling a nod from a
    /// roll needs the bud's forward axis, which no gesture has given yet.
    pub tilt_deg: f64,
    /// Arrival time of the sample, on the caller's clock.
    pub time: Duration,
    /// The reference is gliding (Follow Screen).
    pub following: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    /// The next sample becomes "straight ahead".
    Recenter,
    /// The next sample becomes the reference, the yaw carries on from the
    /// last pose (sensor restarted, possibly on the other bud).
    KeepYaw,
}

#[derive(Clone, Copy, Debug)]
struct Reference {
    inverse: Quat,
    axis: Vec3,
}

#[derive(Clone, Copy, Debug)]
struct Glide {
    from: f64,
    elapsed: f64,
}

/// Nominal report period at 20 000 µs asked (19 892 µs measured).
const NOMINAL_DT: f64 = 0.0199;
/// Longest gap taken as one step of the filters, seconds.
const MAX_DT: f64 = 0.2;

#[derive(Clone, Debug)]
pub struct Tracker {
    config: TrackerConfig,
    reference: Option<Reference>,
    anchor: Option<Anchor>,
    gravity: Option<Vec3>,
    last_twist: f64,
    unwrapped: f64,
    offset: f64,
    filter: OneEuro,
    clock: f64,
    last_sensor_us: Option<u64>,
    last_received: Option<Duration>,
    away_since: Option<f64>,
    glide: Option<Glide>,
    last_pose: Option<Pose>,
}

impl Tracker {
    pub fn new(config: TrackerConfig) -> Self {
        Self {
            config,
            reference: None,
            anchor: None,
            gravity: None,
            last_twist: 0.0,
            unwrapped: 0.0,
            offset: 0.0,
            filter: OneEuro::new(config.filter),
            clock: 0.0,
            last_sensor_us: None,
            last_received: None,
            away_since: None,
            glide: None,
            last_pose: None,
        }
    }

    pub fn config(&self) -> TrackerConfig {
        self.config
    }

    pub fn set_follow_screen(&mut self, on: bool) {
        self.config.follow_screen = on;
        if !on {
            self.glide = None;
            self.away_since = None;
        }
    }

    /// The next sample becomes straight ahead (yaw 0).
    pub fn recenter(&mut self) {
        self.anchor = Some(Anchor::Recenter);
    }

    /// The next sample becomes the new reference while the yaw carries on
    /// from the last pose: for a stream that restarts, possibly from the
    /// other bud, whose attitude has nothing to do with the old reference.
    pub fn reanchor(&mut self) {
        if self.anchor.is_none() && self.reference.is_some() {
            self.anchor = Some(Anchor::KeepYaw);
        }
    }

    /// Forgets everything: the next sample is a fresh start at yaw 0.
    pub fn reset(&mut self) {
        *self = Self::new(self.config);
    }

    pub fn last_pose(&self) -> Option<Pose> {
        self.last_pose
    }

    fn step(&mut self, s: &MotionSample) -> Option<f64> {
        let sensor = match (self.last_sensor_us, s.sensor_time_us) {
            (Some(a), Some(b)) if b > a && b - a <= (MAX_DT * 1e6) as u64 => {
                Some((b - a) as f64 / 1e6)
            }
            _ => None,
        };
        let host = self
            .last_received
            .and_then(|a| s.received.checked_sub(a))
            .map(|d| d.as_secs_f64());
        let first = self.last_received.is_none();
        self.last_sensor_us = s.sensor_time_us;
        self.last_received = Some(s.received);
        if first {
            return None;
        }
        let dt = sensor.or(host).unwrap_or(NOMINAL_DT);
        Some(if dt <= 0.0 {
            NOMINAL_DT
        } else {
            dt.min(MAX_DT)
        })
    }

    /// Feeds one sample; `None` while the AirPods' fusion warms up.
    pub fn push(&mut self, s: &MotionSample) -> Option<Pose> {
        if !s.is_ready() {
            return None;
        }
        let dt = self.step(s);
        let dts = dt.unwrap_or(0.0);
        self.clock += dts;
        let g = unit(s.gravity)?;
        let gravity = match (self.gravity, dt) {
            (Some(prev), Some(dt)) => {
                let a = dt / (0.25 + dt);
                unit([0, 1, 2].map(|i| prev[i] + a * (g[i] - prev[i]))).unwrap_or(g)
            }
            _ => g,
        };
        self.gravity = Some(gravity);

        let anchor = match (self.reference, self.anchor.take()) {
            (None, _) => Some(Anchor::Recenter),
            (Some(_), a) => a,
        };
        if let Some(anchor) = anchor {
            let keep = match anchor {
                Anchor::KeepYaw => self.last_pose.map(|p| p.yaw_deg),
                Anchor::Recenter => None,
            };
            self.reference = Some(Reference {
                inverse: s.attitude.conj(),
                axis: gravity,
            });
            self.last_twist = 0.0;
            self.unwrapped = 0.0;
            self.offset = keep.map_or(0.0, |y| -y);
            self.filter.reset();
            self.glide = None;
            self.away_since = None;
        }
        let reference = self.reference?;

        let r = reference.inverse * s.attitude;
        let twist = r.twist_deg(reference.axis);
        self.unwrapped += wrap180(twist - self.last_twist);
        self.last_twist = twist;
        let head = self.filter.apply(-self.unwrapped, dts);
        let tilt = r.swing_deg(reference.axis);
        self.follow(head, tilt, dts);
        let pose = Pose {
            yaw_deg: wrap180(head - self.offset),
            tilt_deg: tilt,
            time: s.received,
            following: self.glide.is_some(),
        };
        self.last_pose = Some(pose);
        Some(pose)
    }

    fn follow(&mut self, head: f64, tilt: f64, dt: f64) {
        let c = self.config;
        if !c.follow_screen {
            return;
        }
        if let Some(g) = &mut self.glide {
            g.elapsed += dt;
            let p = (g.elapsed / c.follow_glide.as_secs_f64().max(1e-3)).min(1.0);
            let eased = p * p * (3.0 - 2.0 * p);
            self.offset = g.from + wrap180(head - g.from) * eased;
            if p >= 1.0 {
                self.glide = None;
                self.away_since = None;
            }
            return;
        }
        if tilt > c.follow_max_tilt_deg {
            self.away_since = None;
            return;
        }
        let rel = wrap180(head - self.offset);
        if rel.abs() > c.follow_threshold_deg {
            let since = *self.away_since.get_or_insert(self.clock);
            if self.clock - since >= c.follow_hold.as_secs_f64() {
                self.glide = Some(Glide {
                    from: self.offset,
                    elapsed: 0.0,
                });
            }
        } else {
            self.away_since = None;
            let tau = c.drift_time_constant.as_secs_f64();
            if tau > 0.0 {
                self.offset += rel * (dt / tau).min(1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quat::Quat;

    /// An earbud tilted in the ear: its gravity is far from any of its axes.
    fn mount() -> Quat {
        Quat::from_axis_angle([0.0, 1.0, 0.0], 50f64.to_radians())
    }

    /// Sample for a head turned `yaw` degrees about the vertical (world Z),
    /// with the AirPods' convention: their attitude maps the bud to the world.
    fn sample(yaw: f64, t: f64) -> MotionSample {
        let head = Quat::from_axis_angle([0.0, 0.0, 1.0], yaw.to_radians());
        let attitude = head * mount();
        let gravity = attitude.conj().rotate([0.0, 0.0, 1.0]);
        MotionSample {
            attitude,
            rate: [0.0; 3],
            gravity,
            sensor_time_us: Some((t * 1e6) as u64 + 1),
            received: Duration::from_secs_f64(t),
        }
    }

    fn run(tr: &mut Tracker, from: f64, to: f64, yaw: impl Fn(f64) -> f64) -> Pose {
        let mut last = None;
        let mut t = from;
        while t < to {
            last = tr.push(&sample(yaw(t), t)).or(last);
            t += 0.02;
        }
        last.unwrap()
    }

    #[test]
    fn the_tilted_bud_gives_the_true_yaw() {
        let mut tr = Tracker::new(TrackerConfig {
            follow_screen: false,
            ..Default::default()
        });
        run(&mut tr, 0.0, 1.0, |_| 0.0);
        let p = run(&mut tr, 1.0, 3.0, |_| 60.0);
        // A turn about world Z is a negative twist here, so a positive pose.
        assert!((p.yaw_deg.abs() - 60.0).abs() < 0.5, "{p:?}");
        assert!(p.tilt_deg < 0.5);
        tr.recenter();
        let p = run(&mut tr, 3.0, 4.0, |_| 60.0);
        assert!(p.yaw_deg.abs() < 0.5);
    }

    #[test]
    fn follow_screen_glides_after_six_seconds() {
        let mut tr = Tracker::new(TrackerConfig::default());
        run(&mut tr, 0.0, 1.0, |_| 0.0);
        let p = run(&mut tr, 1.0, 6.5, |_| 45.0);
        assert!(p.yaw_deg.abs() > 40.0 && !p.following, "{p:?}");
        let p = run(&mut tr, 6.5, 8.5, |_| 45.0);
        assert!(p.following, "{p:?}");
        let p = run(&mut tr, 8.5, 10.5, |_| 45.0);
        assert!(p.yaw_deg.abs() < 1.0 && !p.following, "{p:?}");
    }

    #[test]
    fn a_short_glance_does_not_move_the_scene() {
        let mut tr = Tracker::new(TrackerConfig::default());
        run(&mut tr, 0.0, 1.0, |_| 0.0);
        let p = run(&mut tr, 1.0, 6.0, |_| 70.0);
        assert!(p.yaw_deg.abs() > 65.0);
        let p = run(&mut tr, 6.0, 8.0, |_| 0.0);
        assert!(p.yaw_deg.abs() < 0.5, "{p:?}");
    }

    #[test]
    fn slow_drift_is_absorbed() {
        let mut tr = Tracker::new(TrackerConfig::default());
        // The fusion drifts by 3°/min for ten minutes: 30° in all.
        let p = run(&mut tr, 0.0, 600.0, |t| t * 0.05);
        assert!(p.yaw_deg.abs() < 3.0, "{p:?}");
        let mut off = Tracker::new(TrackerConfig {
            follow_screen: false,
            ..Default::default()
        });
        let p = run(&mut off, 0.0, 600.0, |t| t * 0.05);
        assert!((p.yaw_deg.abs() - 30.0).abs() < 1.0, "{p:?}");
    }

    #[test]
    fn reanchor_keeps_the_yaw() {
        let mut tr = Tracker::new(TrackerConfig {
            follow_screen: false,
            ..Default::default()
        });
        run(&mut tr, 0.0, 1.0, |_| 0.0);
        let before = run(&mut tr, 1.0, 3.0, |_| 30.0);
        tr.reanchor();
        // The other bud: a different attitude for the same head.
        let after = run(&mut tr, 3.0, 3.5, |_| 170.0);
        assert!((after.yaw_deg - before.yaw_deg).abs() < 0.5);
    }
}
