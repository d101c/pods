// SPDX-License-Identifier: GPL-3.0-or-later
//! Just enough quaternion algebra for the yaw: product, conjugate, vector
//! rotation and the swing–twist split about an axis.

pub type Vec3 = [f64; 3];

pub fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn norm(a: Vec3) -> f64 {
    dot(a, a).sqrt()
}

/// `a` scaled to length 1; `None` for a (near) zero vector.
pub fn unit(a: Vec3) -> Option<Vec3> {
    let n = norm(a);
    (n > 1e-9).then(|| a.map(|c| c / n))
}

/// Wraps an angle in degrees into (−180, 180].
pub fn wrap180(deg: f64) -> f64 {
    let r = (deg + 180.0).rem_euclid(360.0) - 180.0;
    if r == -180.0 { 180.0 } else { r }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat {
    pub w: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Quat {
    pub const IDENTITY: Quat = Quat {
        w: 1.0,
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// Rotation of `angle_rad` about the unit vector `axis`.
    pub fn from_axis_angle(axis: Vec3, angle_rad: f64) -> Quat {
        let (s, c) = (angle_rad / 2.0).sin_cos();
        Quat {
            w: c,
            x: axis[0] * s,
            y: axis[1] * s,
            z: axis[2] * s,
        }
    }

    pub fn norm(self) -> f64 {
        (self.w * self.w + self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    pub fn normalized(self) -> Quat {
        let n = self.norm();
        if n < 1e-12 {
            return Quat::IDENTITY;
        }
        Quat {
            w: self.w / n,
            x: self.x / n,
            y: self.y / n,
            z: self.z / n,
        }
    }

    pub fn conj(self) -> Quat {
        Quat {
            w: self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }

    /// `v` rotated by this (unit) quaternion.
    pub fn rotate(self, v: Vec3) -> Vec3 {
        let p = Quat {
            w: 0.0,
            x: v[0],
            y: v[1],
            z: v[2],
        };
        let r = self * p * self.conj();
        [r.x, r.y, r.z]
    }

    /// Angle in degrees of the twist of this rotation about the unit vector
    /// `axis` (swing–twist split), in (−180, 180].
    pub fn twist_deg(self, axis: Vec3) -> f64 {
        let d = dot([self.x, self.y, self.z], axis);
        wrap180(2.0 * d.atan2(self.w).to_degrees())
    }

    /// Angle in degrees of the swing: how far this rotation tips `axis`.
    pub fn swing_deg(self, axis: Vec3) -> f64 {
        let moved = self.rotate(axis);
        dot(moved, axis).clamp(-1.0, 1.0).acos().to_degrees()
    }
}

impl std::ops::Mul for Quat {
    type Output = Quat;

    /// Hamilton product: `a * b` applies `b` first, then `a`.
    fn mul(self, o: Quat) -> Quat {
        Quat {
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twist_and_swing_split_a_rotation() {
        let up = [0.0, 0.0, 1.0];
        let side = [1.0, 0.0, 0.0];
        let turn = Quat::from_axis_angle(up, 40f64.to_radians());
        let nod = Quat::from_axis_angle(side, 25f64.to_radians());
        let q = nod * turn;
        assert!((q.twist_deg(up) - 40.0).abs() < 2.0);
        assert!((q.swing_deg(up) - 25.0).abs() < 1e-9);
        assert!((turn.twist_deg(up) - 40.0).abs() < 1e-9);
        assert!(turn.swing_deg(up).abs() < 1e-6);
    }

    #[test]
    fn twist_is_wrapped() {
        let up = [0.0, 0.0, 1.0];
        let q = Quat::from_axis_angle(up, 200f64.to_radians());
        assert!((q.twist_deg(up) + 160.0).abs() < 1e-9);
        assert_eq!(wrap180(540.0), 180.0);
        assert_eq!(wrap180(-190.0), 170.0);
    }
}
