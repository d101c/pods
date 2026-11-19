//! Angle conventions shared by both spatial modes.
//!
//! Azimuths follow SOFA / `mysofa_s2c`: 0° is straight ahead, 90° is to the
//! left, 270° to the right. Yaw is the head rotation from the reference
//! direction, positive when the head turns left. The scene is fixed in the
//! room, so a virtual speaker placed at `θ` is heard at `θ − yaw`.

/// Virtual speakers sit at ±30° (a classic stereo triangle).
pub const SPEAKER_SPREAD_DEG: f32 = 30.0;

/// Head-tracked renderer: the scene only moves when the yaw has drifted by at
/// least this much from the angle being rendered.
pub const DEAD_ZONE_DEG: f32 = 1.0;

/// Brings any angle into [0, 360). The sofa plugin clamps instead of
/// wrapping (−5° would become 0°), so the engine always wraps itself.
pub fn wrap360(deg: f32) -> f32 {
    let r = deg.rem_euclid(360.0);
    // rem_euclid can round up to exactly 360.0 for tiny negative inputs.
    if r >= 360.0 { 0.0 } else { r }
}

/// Brings any angle into (−180, 180].
pub fn wrap180(deg: f32) -> f32 {
    let r = wrap360(deg);
    if r > 180.0 { r - 360.0 } else { r }
}

/// Azimuths of the left and right virtual speakers for a head yaw.
pub fn speaker_azimuths(yaw_deg: f32) -> (f32, f32) {
    (
        wrap360(SPEAKER_SPREAD_DEG - yaw_deg),
        wrap360(-SPEAKER_SPREAD_DEG - yaw_deg),
    )
}

/// Whole-degree index in 0..360 of an azimuth.
pub fn degree_index(deg: f32) -> usize {
    (wrap360(deg).round() as usize) % 360
}

/// Quantises the yaw to whole degrees with a dead zone: the rendered angle
/// only changes once the requested yaw is at least [`DEAD_ZONE_DEG`] away
/// from it, so sensor jitter never reaches the convolvers.
#[derive(Debug, Clone)]
pub struct YawGate {
    applied: i32,
    dead_zone: f32,
}

impl Default for YawGate {
    fn default() -> Self {
        Self::new(DEAD_ZONE_DEG)
    }
}

impl YawGate {
    pub fn new(dead_zone: f32) -> Self {
        Self { applied: 0, dead_zone }
    }

    /// The yaw currently rendered, in whole degrees within [0, 360).
    pub fn applied(&self) -> i32 {
        self.applied
    }

    /// Feeds a requested yaw. Returns the new whole-degree yaw when the
    /// rendered angle has to change, `None` otherwise (inside the dead zone,
    /// or a non-finite value).
    pub fn update(&mut self, yaw_deg: f32) -> Option<i32> {
        if !yaw_deg.is_finite() {
            return None;
        }
        if wrap180(yaw_deg - self.applied as f32).abs() < self.dead_zone {
            return None;
        }
        let next = degree_index(yaw_deg) as i32;
        if next == self.applied {
            return None;
        }
        self.applied = next;
        Some(next)
    }

    /// Jumps to a yaw without a transition (used when the renderer resets).
    pub fn reset(&mut self, yaw_deg: f32) {
        self.applied = if yaw_deg.is_finite() { degree_index(yaw_deg) as i32 } else { 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps() {
        assert_eq!(wrap360(-5.0), 355.0);
        assert_eq!(wrap360(360.0), 0.0);
        assert_eq!(wrap360(725.0), 5.0);
        assert!(wrap360(-1e-9) < 360.0);
        assert_eq!(wrap180(190.0), -170.0);
        assert_eq!(wrap180(-180.0), 180.0);
        assert_eq!(wrap180(10.0), 10.0);
    }

    #[test]
    fn speakers_follow_the_room() {
        assert_eq!(speaker_azimuths(0.0), (30.0, 330.0));
        // Head turned 40° to the left: the scene moves to the right.
        assert_eq!(speaker_azimuths(40.0), (350.0, 290.0));
        // Head turned 40° to the right.
        assert_eq!(speaker_azimuths(-40.0), (70.0, 10.0));
    }

    #[test]
    fn dead_zone_holds_small_moves() {
        let mut g = YawGate::default();
        assert_eq!(g.update(0.4), None);
        assert_eq!(g.update(-0.9), None);
        assert_eq!(g.update(1.0), Some(1));
        assert_eq!(g.update(1.6), None);
        assert_eq!(g.update(2.2), Some(2));
        assert_eq!(g.update(-3.0), Some(357));
        assert_eq!(g.update(-3.4), None);
        assert_eq!(g.update(f32::NAN), None);
        assert_eq!(g.applied(), 357);
    }
}
