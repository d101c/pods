// SPDX-License-Identifier: GPL-3.0-or-later
//! Low battery alerts: once per threshold, re-armed by charging.

use std::collections::HashMap;

use pods_proto::Part;

/// Thresholds, highest first.
pub const THRESHOLDS: [u8; 2] = [10, 5];

/// Above this (without charging) every threshold is armed again: the
/// bud was charged while we did not see it. The margin keeps a level
/// that wobbles around 10 % from alerting twice.
pub const REARM_ABOVE: u8 = 15;

#[derive(Debug, Default, Clone)]
pub struct LowBattery {
    /// Per part, the lowest threshold already announced.
    fired: HashMap<Part, u8>,
}

impl LowBattery {
    /// A new reading. Returns the level to announce, if any. Only the
    /// buds are watched; a low case is not worth an interruption.
    pub fn update(&mut self, part: Part, level: u8, charging: bool) -> Option<u8> {
        if part == Part::Case {
            return None;
        }
        if charging || level > REARM_ABOVE {
            self.fired.remove(&part);
            return None;
        }
        // The most severe threshold reached; one alert even if the level
        // jumped past both.
        let reached = THRESHOLDS.iter().copied().filter(|&t| level <= t).min()?;
        match self.fired.get(&part) {
            Some(&done) if done <= reached => None,
            _ => {
                self.fired.insert(part, reached);
                Some(level)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_per_threshold() {
        let mut lb = LowBattery::default();
        assert_eq!(lb.update(Part::Left, 50, false), None);
        assert_eq!(lb.update(Part::Left, 11, false), None);
        assert_eq!(lb.update(Part::Left, 10, false), Some(10));
        assert_eq!(lb.update(Part::Left, 9, false), None);
        assert_eq!(lb.update(Part::Left, 10, false), None);
        assert_eq!(lb.update(Part::Left, 6, false), None);
        assert_eq!(lb.update(Part::Left, 5, false), Some(5));
        assert_eq!(lb.update(Part::Left, 4, false), None);
        assert_eq!(lb.update(Part::Left, 1, false), None);
    }

    #[test]
    fn a_jump_past_both_alerts_once() {
        let mut lb = LowBattery::default();
        assert_eq!(lb.update(Part::Right, 4, false), Some(4));
        assert_eq!(lb.update(Part::Right, 3, false), None);
    }

    #[test]
    fn charging_rearms() {
        let mut lb = LowBattery::default();
        assert_eq!(lb.update(Part::Left, 9, false), Some(9));
        assert_eq!(lb.update(Part::Left, 9, true), None);
        assert_eq!(lb.update(Part::Left, 9, false), Some(9));
    }

    #[test]
    fn charged_out_of_sight_rearms() {
        let mut lb = LowBattery::default();
        assert_eq!(lb.update(Part::Left, 5, false), Some(5));
        assert_eq!(lb.update(Part::Left, 12, false), None);
        assert_eq!(
            lb.update(Part::Left, 10, false),
            None,
            "12 is within the margin"
        );
        assert_eq!(lb.update(Part::Left, 80, false), None);
        assert_eq!(lb.update(Part::Left, 10, false), Some(10));
    }

    #[test]
    fn buds_are_independent_and_the_case_is_ignored() {
        let mut lb = LowBattery::default();
        assert_eq!(lb.update(Part::Left, 8, false), Some(8));
        assert_eq!(lb.update(Part::Right, 8, false), Some(8));
        assert_eq!(lb.update(Part::Case, 3, false), None);
    }
}
