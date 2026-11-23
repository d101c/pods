// SPDX-License-Identifier: GPL-3.0-or-later
// The product id table is derived from podctl `src/caps.rs`
// (https://github.com/Rockykln/podctl), Copyright (c) 2026 pods contributors,
// MIT OR Apache-2.0. See engine/NOTICE.
//! Values shared with the D-Bus API, and what each model can do.
//!
//! The spellings returned by `as_str` are the vocabulary of the D-Bus
//! contract (`docs/io.github.d101c.Pods1.xml`); keep them in step.

use std::fmt;

/// Listening (noise control) mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoiseMode {
    Off,
    Transparency,
    Anc,
    Adaptive,
}

impl NoiseMode {
    /// Display order of the contract.
    pub const ALL: [NoiseMode; 4] = [
        NoiseMode::Off,
        NoiseMode::Transparency,
        NoiseMode::Anc,
        NoiseMode::Adaptive,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NoiseMode::Off => "off",
            NoiseMode::Transparency => "transparency",
            NoiseMode::Anc => "anc",
            NoiseMode::Adaptive => "adaptive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        NoiseMode::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// Value of the `LISTENING_MODE` (0x0D) control. Confirmed on the Pro 1.
    pub fn wire(self) -> u8 {
        match self {
            NoiseMode::Off => 1,
            NoiseMode::Anc => 2,
            NoiseMode::Transparency => 3,
            NoiseMode::Adaptive => 4,
        }
    }

    pub fn from_wire(b: u8) -> Option<Self> {
        Some(match b {
            1 => NoiseMode::Off,
            2 => NoiseMode::Anc,
            3 => NoiseMode::Transparency,
            4 => NoiseMode::Adaptive,
            _ => return None,
        })
    }

    /// Capability string announcing this mode.
    pub fn capability(self) -> Capability {
        match self {
            NoiseMode::Off => Capability::NoiseOff,
            NoiseMode::Transparency => Capability::NoiseTransparency,
            NoiseMode::Anc => Capability::NoiseAnc,
            NoiseMode::Adaptive => Capability::NoiseAdaptive,
        }
    }
}

impl fmt::Display for NoiseMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a bud is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Wearing {
    Ear,
    Out,
    Case,
    #[default]
    Unknown,
}

impl Wearing {
    pub fn as_str(self) -> &'static str {
        match self {
            Wearing::Ear => "ear",
            Wearing::Out => "out",
            Wearing::Case => "case",
            Wearing::Unknown => "unknown",
        }
    }

    /// Byte of the ear notification (0x06). 0, 1 and 2 are documented by
    /// LibrePods. 3 was seen on the Pro 1 for a bud lying in the closed
    /// case (its battery reported as disconnected at the same time).
    pub fn from_wire(b: u8) -> Self {
        match b {
            0x00 => Wearing::Ear,
            0x01 => Wearing::Out,
            0x02 | 0x03 => Wearing::Case,
            _ => Wearing::Unknown,
        }
    }
}

/// Battery part.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    Left,
    Right,
    Case,
}

impl Part {
    pub fn as_str(self) -> &'static str {
        match self {
            Part::Left => "left",
            Part::Right => "right",
            Part::Case => "case",
        }
    }
}

/// A bud.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
}

/// Capability strings of the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    NoiseOff,
    NoiseTransparency,
    NoiseAnc,
    NoiseAdaptive,
    Conversation,
    PressHold,
    Rename,
    EarDetection,
    SpatialHead,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::NoiseOff => "noise.off",
            Capability::NoiseTransparency => "noise.transparency",
            Capability::NoiseAnc => "noise.anc",
            Capability::NoiseAdaptive => "noise.adaptive",
            Capability::Conversation => "conversation",
            Capability::PressHold => "pressHold",
            Capability::Rename => "rename",
            Capability::EarDetection => "earDetection",
            Capability::SpatialHead => "spatial.head",
        }
    }
}

/// What one model can do, as far as this engine is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelSpec {
    /// Product id, `None` for a model this table does not know.
    pub product: Option<u16>,
    pub name: &'static str,
    /// Listening modes, in display order.
    pub noise_modes: &'static [NoiseMode],
    /// The press-and-hold cycle out of the box.
    pub default_press_hold: &'static [NoiseMode],
    pub press_hold: bool,
    pub ear_detection: bool,
    pub conversation: bool,
    pub rename: bool,
    /// Motion sensors the engine can read for head tracking
    /// (`spatial.head`): every model with spatial audio, not AirPods 1 and 2.
    pub head_tracking: bool,
}

use NoiseMode::{Adaptive, Anc, Off, Transparency};

const NONE: &[NoiseMode] = &[];
const OTA: &[NoiseMode] = &[Off, Transparency, Anc];
const OTAA: &[NoiseMode] = &[Off, Transparency, Anc, Adaptive];

const fn basic(product: u16, name: &'static str, head_tracking: bool) -> ModelSpec {
    ModelSpec {
        product: Some(product),
        name,
        noise_modes: NONE,
        default_press_hold: NONE,
        press_hold: false,
        ear_detection: true,
        conversation: false,
        rename: true,
        head_tracking,
    }
}

/// Product ids → models. The ids are podctl's table (The Apple Wiki and
/// OpenPods); the Pro 1 entry (0x200E) is checked against real AirPods:
/// off, transparency and ANC only, no adaptive, no conversation awareness.
const MODELS: &[ModelSpec] = &[
    basic(0x2002, "AirPods (1st generation)", false),
    basic(0x200F, "AirPods (2nd generation)", false),
    basic(0x2013, "AirPods (3rd generation)", true),
    basic(0x2019, "AirPods 4", true),
    ModelSpec {
        product: Some(0x200E),
        name: "AirPods Pro (1st generation)",
        noise_modes: OTA,
        default_press_hold: &[Transparency, Anc],
        press_hold: true,
        ear_detection: true,
        conversation: false,
        rename: true,
        head_tracking: true,
    },
    ModelSpec {
        product: Some(0x2014),
        name: "AirPods Pro (2nd generation)",
        noise_modes: OTAA,
        default_press_hold: &[Transparency, Anc],
        press_hold: true,
        ear_detection: true,
        conversation: true,
        rename: true,
        head_tracking: true,
    },
    ModelSpec {
        product: Some(0x2024),
        name: "AirPods Pro (2nd generation, USB-C)",
        noise_modes: OTAA,
        default_press_hold: &[Transparency, Anc],
        press_hold: true,
        ear_detection: true,
        conversation: true,
        rename: true,
        head_tracking: true,
    },
    ModelSpec {
        product: Some(0x2027),
        name: "AirPods Pro (3rd generation)",
        noise_modes: OTAA,
        default_press_hold: &[Transparency, Anc],
        press_hold: true,
        ear_detection: true,
        conversation: true,
        rename: true,
        head_tracking: true,
    },
    ModelSpec {
        product: Some(0x201B),
        name: "AirPods 4 (ANC)",
        noise_modes: OTAA,
        default_press_hold: &[Transparency, Anc],
        press_hold: true,
        ear_detection: true,
        conversation: true,
        rename: true,
        head_tracking: true,
    },
    ModelSpec {
        product: Some(0x200A),
        name: "AirPods Max",
        noise_modes: OTA,
        default_press_hold: &[Transparency, Anc],
        press_hold: true,
        ear_detection: false,
        conversation: false,
        rename: true,
        head_tracking: true,
    },
    ModelSpec {
        product: Some(0x201F),
        name: "AirPods Max (USB-C)",
        noise_modes: OTA,
        default_press_hold: &[Transparency, Anc],
        press_hold: true,
        ear_detection: false,
        conversation: false,
        rename: true,
        head_tracking: true,
    },
    ModelSpec {
        product: Some(0x202D),
        name: "AirPods Max (2nd generation)",
        noise_modes: OTA,
        default_press_hold: &[Transparency, Anc],
        press_hold: true,
        ear_detection: false,
        conversation: false,
        rename: true,
        head_tracking: true,
    },
];

/// A model nobody has mapped: no listening modes are offered, only what
/// every AirPods does. Better a short menu than a mode the buds refuse.
pub const UNKNOWN_MODEL: ModelSpec = ModelSpec {
    product: None,
    name: "AirPods",
    noise_modes: NONE,
    default_press_hold: NONE,
    press_hold: false,
    ear_detection: false,
    conversation: false,
    rename: true,
    head_tracking: false,
};

/// The model for a product id.
pub fn model_for_product(product: u16) -> ModelSpec {
    MODELS
        .iter()
        .copied()
        .find(|m| m.product == Some(product))
        .unwrap_or(ModelSpec {
            product: Some(product),
            ..UNKNOWN_MODEL
        })
}

/// The product id behind an Apple model number (`A2084`), for when BlueZ
/// gave no modalias. Only numbers seen on real hardware are here: A2084
/// (with its other bud A2083) on our Pro 1, A3047 on podctl's Pro 2 USB-C.
pub fn model_for_number(number: &str) -> Option<u16> {
    match number {
        "A2083" | "A2084" => Some(0x200E),
        "A3047" => Some(0x2024),
        _ => None,
    }
}

/// The product id in a BlueZ `Modalias` such as `bluetooth:v004Cp200Ed0A4C`
/// (also `usb:v004Cp200E…`). Apple's vendor id is required.
pub fn product_from_modalias(modalias: &str) -> Option<u16> {
    let rest = modalias.split_once(':')?.1;
    let vendor = rest.strip_prefix('v')?.get(..4)?;
    if !vendor.eq_ignore_ascii_case("004C") {
        return None;
    }
    let product = rest.get(5..)?.strip_prefix('p')?.get(..4)?;
    u16::from_str_radix(product, 16).ok()
}

impl ModelSpec {
    /// Capabilities in the order of the contract's vocabulary.
    pub fn capabilities(&self) -> Vec<Capability> {
        let mut caps: Vec<Capability> = NoiseMode::ALL
            .into_iter()
            .filter(|m| self.noise_modes.contains(m))
            .map(NoiseMode::capability)
            .collect();
        if self.conversation {
            caps.push(Capability::Conversation);
        }
        if self.press_hold {
            caps.push(Capability::PressHold);
        }
        if self.rename {
            caps.push(Capability::Rename);
        }
        if self.ear_detection {
            caps.push(Capability::EarDetection);
        }
        if self.head_tracking {
            caps.push(Capability::SpatialHead);
        }
        caps
    }

    /// Product id as the contract writes it: `0x200E`, or "" when unknown.
    pub fn product_string(&self) -> String {
        self.product
            .map(|p| format!("0x{p:04X}"))
            .unwrap_or_default()
    }
}

/// The mode after `current` in a press-and-hold cycle. A mode outside the
/// cycle (or none yet) goes to the first one of it.
pub fn next_in_cycle(cycle: &[NoiseMode], current: Option<NoiseMode>) -> Option<NoiseMode> {
    let first = *cycle.first()?;
    let Some(cur) = current else {
        return Some(first);
    };
    match cycle.iter().position(|&m| m == cur) {
        Some(i) => Some(cycle[(i + 1) % cycle.len()]),
        None => Some(first),
    }
}

/// The order the buds walk their press-and-hold modes in, as a cycle.
/// Measured on AirPods Pro (1st generation), mask 0x07: transparency →
/// off → ANC → transparency (issue #14). Where adaptive sits is not
/// known yet; the engine learns the real order from stem presses.
pub const BUDS_ORDER: [NoiseMode; 4] = [Off, Anc, Transparency, Adaptive];

/// A cycle written from its mode that comes first in display order, so
/// the same cycle always reads the same.
pub fn canonical(cycle: &[NoiseMode]) -> Vec<NoiseMode> {
    let rank = |m: &NoiseMode| NoiseMode::ALL.iter().position(|x| x == m).unwrap_or(0);
    let Some(start) = (0..cycle.len()).min_by_key(|&i| rank(&cycle[i])) else {
        return Vec::new();
    };
    cycle[start..]
        .iter()
        .chain(&cycle[..start])
        .copied()
        .collect()
}

/// The cycle through `members`: in the order of `hint` (what was learned)
/// for the modes it knows, each other mode placed after its nearest
/// predecessor in [`BUDS_ORDER`].
pub fn arrange(members: &[NoiseMode], hint: &[NoiseMode]) -> Vec<NoiseMode> {
    let mut out: Vec<NoiseMode> = Vec::new();
    for m in hint {
        if members.contains(m) && !out.contains(m) {
            out.push(*m);
        }
    }
    for (i, m) in BUDS_ORDER.iter().enumerate() {
        if !members.contains(m) || out.contains(m) {
            continue;
        }
        let n = BUDS_ORDER.len();
        let after = (1..n)
            .map(|k| BUDS_ORDER[(i + n - k) % n])
            .find_map(|p| out.iter().position(|x| *x == p));
        match after {
            Some(j) => out.insert(j + 1, *m),
            None => out.push(*m),
        }
    }
    canonical(&out)
}

/// Net evidence a transition needs before it counts as known.
const CONFIRMED: u8 = 2;
/// Cap on that evidence, so that a cycle that really changed (another host
/// rewrote the mask) is learned again after a few presses.
const MAX_WEIGHT: u8 = 4;

/// Learns the press-and-hold cycle from stem presses.
///
/// AAP gives no way to read the order the buds walk their modes in: the
/// `0x1A` control is a bit mask, and the Pro 1 does not even report it on
/// connect (issue #1). So the order is inferred, carefully, because the buds
/// also change mode on their own when a bud goes in or out:
///
/// - only successive stem presses count: a press teaches `from → to` only
///   when `from` was itself reached by the stem ([`CycleLearner::stem`]);
///   anything else ([`CycleLearner::interrupt`]) starts a new run;
/// - an order is retained after a full tour (the run comes back to a mode
///   it already visited), or once every transition of a closed loop has
///   been seen at least twice (net of contrary sightings);
/// - a retained order is only replaced by such a loop seen twice; a single
///   tour may only extend it (a new member, the known ones in the same
///   order).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CycleLearner {
    retained: Option<Vec<NoiseMode>>,
    /// Successor of each mode as seen so far, and its net count.
    next: [Option<(NoiseMode, u8)>; 4],
    /// Modes reached by successive stem presses, oldest first.
    run: Vec<NoiseMode>,
}

fn slot(m: NoiseMode) -> usize {
    NoiseMode::ALL.iter().position(|&x| x == m).unwrap_or(0)
}

/// `a` and `b` walk the modes they share in the same cyclic order.
fn same_order(a: &[NoiseMode], b: &[NoiseMode]) -> bool {
    let ab: Vec<NoiseMode> = a.iter().filter(|m| b.contains(m)).copied().collect();
    let ba: Vec<NoiseMode> = b.iter().filter(|m| a.contains(m)).copied().collect();
    canonical(&ab) == canonical(&ba)
}

impl CycleLearner {
    /// `retained`: an order kept from an earlier run (`press_hold_cycle`).
    pub fn new(retained: Option<&[NoiseMode]>) -> Self {
        Self {
            retained: retained.filter(|c| c.len() >= 2).map(canonical),
            ..Self::default()
        }
    }

    /// The order retained, in canonical form.
    pub fn retained(&self) -> Option<&[NoiseMode]> {
        self.retained.as_deref()
    }

    /// The mode changed some other way (the tile, the buds themselves, a
    /// new session): the next stem press starts a new run.
    pub fn interrupt(&mut self) {
        self.run.clear();
    }

    /// The stem went from `from` to `to`. Returns the new retained order
    /// when this press changed it.
    pub fn stem(&mut self, from: NoiseMode, to: NoiseMode) -> Option<Vec<NoiseMode>> {
        if from == to {
            return None;
        }
        if self.run.last() != Some(&from) {
            // `from` was not reached by the stem: nothing to learn yet.
            self.run = vec![to];
            return None;
        }
        self.observe(from, to);
        let tour = self.run.iter().position(|&m| m == to).map(|i| {
            let t = canonical(&self.run[i..]);
            self.run.clear();
            t
        });
        self.run.push(to);
        if let Some(c) = self.confirmed_loop(to)
            && self.retained.as_ref() != Some(&c)
        {
            self.retained = Some(c.clone());
            return Some(c);
        }
        let t = tour?;
        let accept = match &self.retained {
            None => true,
            Some(r) => *r != t && r.iter().all(|m| t.contains(m)) && same_order(r, &t),
        };
        accept.then(|| {
            self.retained = Some(t.clone());
            t
        })
    }

    fn observe(&mut self, from: NoiseMode, to: NoiseMode) {
        let e = &mut self.next[slot(from)];
        *e = match *e {
            Some((n, w)) if n == to => Some((n, (w + 1).min(MAX_WEIGHT))),
            Some((n, w)) if w > 1 => Some((n, w - 1)),
            _ => Some((to, 1)),
        };
    }

    /// The loop through `start` made of transitions seen at least twice.
    fn confirmed_loop(&self, start: NoiseMode) -> Option<Vec<NoiseMode>> {
        let mut path = vec![start];
        loop {
            let (n, w) = self.next[slot(*path.last()?)]?;
            if w < CONFIRMED {
                return None;
            }
            if n == start {
                return (path.len() >= 2).then(|| canonical(&path));
            }
            if path.contains(&n) {
                return None;
            }
            path.push(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_spell_like_the_contract() {
        let words: Vec<_> = NoiseMode::ALL.iter().map(|m| m.as_str()).collect();
        assert_eq!(words, ["off", "transparency", "anc", "adaptive"]);
        for m in NoiseMode::ALL {
            assert_eq!(NoiseMode::parse(m.as_str()), Some(m));
            assert_eq!(NoiseMode::from_wire(m.wire()), Some(m));
        }
        assert_eq!(NoiseMode::parse("ANC"), None);
        assert_eq!(NoiseMode::from_wire(0), None);
        assert_eq!(NoiseMode::from_wire(5), None);
    }

    /// Bytes measured on the Pro 1: 1 off, 2 ANC, 3 transparency.
    #[test]
    fn listening_mode_bytes_are_the_measured_ones() {
        assert_eq!(NoiseMode::Off.wire(), 1);
        assert_eq!(NoiseMode::Anc.wire(), 2);
        assert_eq!(NoiseMode::Transparency.wire(), 3);
        assert_eq!(NoiseMode::Adaptive.wire(), 4);
    }

    #[test]
    fn wearing_bytes() {
        assert_eq!(Wearing::from_wire(0), Wearing::Ear);
        assert_eq!(Wearing::from_wire(1), Wearing::Out);
        assert_eq!(Wearing::from_wire(2), Wearing::Case);
        assert_eq!(Wearing::from_wire(3), Wearing::Case);
        assert_eq!(Wearing::from_wire(9), Wearing::Unknown);
        assert_eq!(Wearing::default().as_str(), "unknown");
    }

    #[test]
    fn the_pro_1_has_no_adaptive_mode() {
        let pro = model_for_product(0x200E);
        assert_eq!(pro.name, "AirPods Pro (1st generation)");
        assert_eq!(pro.noise_modes, &[Off, Transparency, Anc]);
        assert_eq!(pro.product_string(), "0x200E");
        let caps: Vec<_> = pro.capabilities().iter().map(|c| c.as_str()).collect();
        assert_eq!(
            caps,
            [
                "noise.off",
                "noise.transparency",
                "noise.anc",
                "pressHold",
                "rename",
                "earDetection",
                "spatial.head"
            ]
        );
    }

    #[test]
    fn head_tracking_follows_the_motion_sensors() {
        let head = |p: u16| {
            model_for_product(p)
                .capabilities()
                .contains(&Capability::SpatialHead)
        };
        // Pro 1, 2 (both), 3, AirPods 3, 4, 4 ANC, Max (all three).
        for p in [
            0x200E, 0x2014, 0x2024, 0x2027, 0x2013, 0x2019, 0x201B, 0x200A, 0x201F, 0x202D,
        ] {
            assert!(head(p), "{p:#06X}");
        }
        // AirPods 1 and 2 have no motion sensors; an unknown model is not promised any.
        for p in [0x2002, 0x200F, 0x2025] {
            assert!(!head(p), "{p:#06X}");
        }
        assert!(
            !UNKNOWN_MODEL
                .capabilities()
                .contains(&Capability::SpatialHead)
        );
    }

    #[test]
    fn the_pro_2_has_adaptive_and_conversation() {
        let caps: Vec<_> = model_for_product(0x2024)
            .capabilities()
            .iter()
            .map(|c| c.as_str())
            .collect();
        assert!(caps.contains(&"noise.adaptive"));
        assert!(caps.contains(&"conversation"));
    }

    #[test]
    fn an_unknown_product_offers_no_mode() {
        let m = model_for_product(0x2025);
        assert_eq!(m.product, Some(0x2025));
        assert!(m.noise_modes.is_empty());
        assert_eq!(m.product_string(), "0x2025");
        assert_eq!(UNKNOWN_MODEL.product_string(), "");
        let caps: Vec<_> = m.capabilities().iter().map(|c| c.as_str()).collect();
        assert_eq!(caps, ["rename"]);
    }

    #[test]
    fn every_known_default_cycle_is_within_the_modes() {
        for m in MODELS {
            for mode in m.default_press_hold {
                assert!(m.noise_modes.contains(mode), "{}", m.name);
            }
            assert!(m.default_press_hold.is_empty() || m.default_press_hold.len() >= 2);
            assert_eq!(m.press_hold, !m.default_press_hold.is_empty(), "{}", m.name);
        }
    }

    #[test]
    fn modalias() {
        assert_eq!(
            product_from_modalias("bluetooth:v004Cp200Ed0A4C"),
            Some(0x200E)
        );
        assert_eq!(product_from_modalias("usb:v004Cp2024d0100"), Some(0x2024));
        assert_eq!(
            product_from_modalias("bluetooth:v004cp200ed0a4c"),
            Some(0x200E)
        );
        assert_eq!(product_from_modalias("bluetooth:v05ACp200Ed0A4C"), None);
        assert_eq!(product_from_modalias("garbage"), None);
        assert_eq!(product_from_modalias("bluetooth:v004C"), None);
    }

    #[test]
    fn model_numbers() {
        assert_eq!(model_for_number("A2084"), Some(0x200E));
        assert_eq!(model_for_number("A9999"), None);
    }

    #[test]
    fn the_cycle_moves_on_and_wraps() {
        let c = [Transparency, Anc];
        assert_eq!(next_in_cycle(&c, Some(Transparency)), Some(Anc));
        assert_eq!(next_in_cycle(&c, Some(Anc)), Some(Transparency));
        // Off is not in the cycle: go to its start.
        assert_eq!(next_in_cycle(&c, Some(Off)), Some(Transparency));
        assert_eq!(next_in_cycle(&c, None), Some(Transparency));
        assert_eq!(next_in_cycle(&[], Some(Anc)), None);
        let c3 = [Off, Transparency, Anc];
        assert_eq!(next_in_cycle(&c3, Some(Anc)), Some(Off));
    }

    #[test]
    fn cycles_read_from_their_first_display_mode() {
        assert_eq!(
            canonical(&[Anc, Transparency, Off]),
            [Off, Anc, Transparency]
        );
        assert_eq!(canonical(&[Anc, Transparency]), [Transparency, Anc]);
        assert_eq!(canonical(&[]), Vec::<NoiseMode>::new());
    }

    /// The real Pro 1 with off, transparency and ANC enabled went
    /// transparency → off → ANC on the stem (issue #14).
    #[test]
    fn the_default_order_is_the_measured_pro_1_cycle() {
        let c = arrange(&[Off, Transparency, Anc], &[]);
        assert_eq!(c, [Off, Anc, Transparency]);
        assert_eq!(next_in_cycle(&c, Some(Transparency)), Some(Off));
        assert_eq!(next_in_cycle(&c, Some(Off)), Some(Anc));
        assert_eq!(next_in_cycle(&c, Some(Anc)), Some(Transparency));
        assert_eq!(arrange(&[Anc, Transparency], &[]), [Transparency, Anc]);
        assert_eq!(arrange(&[Off, Anc], &[]), [Off, Anc]);
        assert_eq!(
            arrange(&[Off, Transparency, Anc, Adaptive], &[]),
            [Off, Anc, Transparency, Adaptive]
        );
    }

    #[test]
    fn a_hint_keeps_its_order() {
        let learned = [Off, Transparency, Anc];
        assert_eq!(arrange(&[Off, Transparency, Anc], &learned), learned);
        // A new member goes after its predecessor in the buds' order.
        assert_eq!(
            arrange(&[Off, Transparency, Anc, Adaptive], &learned),
            [Off, Transparency, Adaptive, Anc]
        );
        assert_eq!(arrange(&[Transparency, Anc], &learned), [Transparency, Anc]);
    }

    /// Feeds stem presses walking `modes`; returns the last order retained.
    fn walk(l: &mut CycleLearner, modes: &[NoiseMode]) -> Option<Vec<NoiseMode>> {
        let mut last = None;
        for w in modes.windows(2) {
            if let Some(c) = l.stem(w[0], w[1]) {
                last = Some(c);
            }
        }
        last
    }

    /// The real Pro 1 cycle (issue #14): transparency → off → ANC.
    const REAL: [NoiseMode; 3] = [Off, Anc, Transparency];

    #[test]
    fn a_full_tour_is_retained() {
        let mut l = CycleLearner::new(None);
        // From transparency (set by the tile): the first press only starts
        // the run, the order is known when the run comes back to off.
        assert_eq!(l.stem(Transparency, Off), None);
        assert_eq!(l.stem(Off, Anc), None);
        assert_eq!(l.stem(Anc, Transparency), None);
        assert_eq!(l.stem(Transparency, Off).unwrap(), REAL);
        assert_eq!(l.retained().unwrap(), REAL);
        // Going on round agrees: nothing new.
        assert_eq!(walk(&mut l, &[Off, Anc, Transparency, Off, Anc]), None);
    }

    #[test]
    fn a_partial_run_retains_nothing() {
        let mut l = CycleLearner::new(None);
        assert_eq!(walk(&mut l, &[Transparency, Off, Anc, Transparency]), None);
        assert_eq!(l.retained(), None);
    }

    #[test]
    fn interrupted_runs_add_up_twice_each_transition() {
        let mut l = CycleLearner::new(None);
        // Never a full tour: the tile or the buds in between each time.
        assert_eq!(walk(&mut l, &[Transparency, Off, Anc, Transparency]), None);
        l.interrupt();
        assert_eq!(walk(&mut l, &[Anc, Transparency, Off]), None);
        l.interrupt();
        assert_eq!(walk(&mut l, &[Transparency, Off, Anc, Transparency]), None);
        l.interrupt();
        // Transparency → off is now known twice, like the other two: the
        // loop is closed.
        assert_eq!(walk(&mut l, &[Anc, Transparency, Off]).unwrap(), REAL);
    }

    #[test]
    fn interruptions_break_the_run() {
        let mut l = CycleLearner::new(None);
        // What the 0.1 engine took for presses: transparency → off,
        // the tile, then off → ANC → transparency → off would close a
        // tour only if every step came from the stem.
        assert_eq!(l.stem(Transparency, Off), None);
        assert_eq!(l.stem(Off, Anc), None);
        l.interrupt();
        assert_eq!(l.stem(Anc, Transparency), None);
        assert_eq!(l.stem(Transparency, Off), None);
        assert_eq!(l.retained(), None);
    }

    #[test]
    fn a_retained_order_resists_one_contrary_tour() {
        // What 0.1 saved after a bud went in and out: the wrong order.
        let wrong = [Off, Transparency, Anc];
        let mut l = CycleLearner::new(Some(&wrong));
        assert_eq!(l.retained().unwrap(), wrong);
        // One real tour: not enough to overturn it.
        assert_eq!(
            walk(&mut l, &[Transparency, Off, Anc, Transparency, Off]),
            None
        );
        assert_eq!(l.retained().unwrap(), wrong);
        // The second one is.
        assert_eq!(walk(&mut l, &[Off, Anc, Transparency, Off]).unwrap(), REAL);
    }

    #[test]
    fn one_stray_press_does_not_reorder() {
        let mut l = CycleLearner::new(Some(&REAL));
        walk(
            &mut l,
            &[Transparency, Off, Anc, Transparency, Off, Anc, Transparency],
        );
        // A change wrongly taken for a press: ANC → off, against the order.
        assert_eq!(walk(&mut l, &[Transparency, Anc, Off]), None);
        assert_eq!(l.retained().unwrap(), REAL);
        assert_eq!(walk(&mut l, &[Off, Anc, Transparency, Off]), None);
        assert_eq!(l.retained().unwrap(), REAL);
    }

    #[test]
    fn a_two_mode_tour_does_not_drop_a_member_at_once() {
        let mut l = CycleLearner::new(Some(&REAL));
        // ANC ↔ transparency: a tour, but it would lose off.
        assert_eq!(walk(&mut l, &[Off, Anc, Transparency, Anc]), None);
        assert_eq!(l.retained().unwrap(), REAL);
        // Twice more: the mask really lost off (another host changed it).
        assert_eq!(
            walk(&mut l, &[Anc, Transparency, Anc, Transparency, Anc]).unwrap(),
            [Transparency, Anc]
        );
    }

    #[test]
    fn a_tour_may_add_a_member_in_the_known_order() {
        let mut l = CycleLearner::new(Some(&[Transparency, Anc]));
        assert_eq!(
            walk(&mut l, &[Anc, Transparency, Off, Anc, Transparency]).unwrap(),
            REAL
        );
    }

    #[test]
    fn a_kept_order_needs_two_modes() {
        assert_eq!(CycleLearner::new(Some(&[Anc])).retained(), None);
        assert_eq!(
            CycleLearner::new(Some(&[Transparency, Off, Anc]))
                .retained()
                .unwrap(),
            [Off, Anc, Transparency]
        );
        let mut l = CycleLearner::new(None);
        assert_eq!(l.stem(Anc, Anc), None);
    }
}
