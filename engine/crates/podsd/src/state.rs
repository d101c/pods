// SPDX-License-Identifier: GPL-3.0-or-later
//! What the engine knows about one pair of AirPods, and how each message
//! changes it. Pure: the link task feeds messages in and publishes what
//! comes out (changed properties, signals).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use tokio::time::Instant;

use pods_proto::ble::{Advert, Level, Levels};
use pods_proto::control::{self, id};
use pods_proto::info::FIRMWARE_RECORD;
use pods_proto::model::{UNKNOWN_MODEL, arrange, learn, next_in_cycle};
use pods_proto::{
    Message, ModelSpec, NoiseMode, Part, Side, Wearing, model_for_number, model_for_product,
};

use crate::alerts::LowBattery;
use crate::case::Snapshot;
use crate::spatial::{HeadState, SpatialMode, SpatialStatus};

/// How long after our own order an echo of that mode counts as ours.
/// The Pro 1 echoes within 0.6 s.
pub const TILE_WINDOW: Duration = Duration::from_secs(5);

/// Who changed the listening mode (the `source` of `NoiseModeChanged`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Stem,
    Tile,
    Other,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Stem => "stem",
            Source::Tile => "tile",
            Source::Other => "other",
        }
    }
}

/// Device properties of the contract that the engine changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Prop {
    Name,
    Model,
    ModelName,
    Firmware,
    Connected,
    Capabilities,
    Battery,
    Wearing,
    NoiseMode,
    NoiseModes,
    PressHoldModes,
    EarDetection,
    PauseOnRemove,
    ResumeOnReturn,
    Color,
    MediaPaused,
    ConversationAwareness,
    SpatialMode,
    SpatialStatus,
    FollowScreen,
    HeadTracking,
}

/// What a message did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub props: Vec<Prop>,
    pub noise_changed: Option<(NoiseMode, Source)>,
    /// (left, right) before and after.
    pub wearing_changed: Option<((Wearing, Wearing), (Wearing, Wearing))>,
    pub low_battery: Vec<(Part, u8)>,
    pub session_end: bool,
    /// The press-and-hold cycle was read or learned from the buds: worth
    /// keeping for the next start.
    pub press_hold_learned: bool,
}

impl Outcome {
    fn prop(&mut self, p: Prop) {
        if !self.props.contains(&p) {
            self.props.push(p);
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceState {
    pub address: String,
    pub bluez_path: String,
    /// BlueZ alias.
    pub alias: String,
    /// Name the buds gave in their information record.
    pub info_name: Option<String>,
    pub modalias_product: Option<u16>,
    pub model_number: Option<String>,
    firmware: Option<(u8, String)>,
    pub connected: bool,
    pub battery: BTreeMap<Part, (u8, bool)>,
    primary: Side,
    ear: Option<(Wearing, Wearing)>,
    pub noise_mode: Option<NoiseMode>,
    /// First listening mode of this session not seen yet: it is a
    /// reading, not a change.
    mode_seen: bool,
    /// The press-and-hold cycle, in the buds' order (canonical form, see
    /// [`pods_proto::model::canonical`]): what the buds reported, what the
    /// stem showed, or what the user set. `None` is the model's default.
    pub press_hold: Option<Vec<NoiseMode>>,
    ear_detection: bool,
    pub pause_on_remove: bool,
    pub resume_on_return: bool,
    pub low_battery_alerts: bool,
    pending: Option<(NoiseMode, Instant)>,
    alerts: LowBattery,
    /// From the BLE advertisement ("" until seen).
    pub color: String,
    /// The engine holds players it paused for these buds.
    pub media_paused: bool,
    /// Conversation awareness as last reported or set.
    pub conversation: bool,
    /// What the user chose (persisted, applied while connected).
    pub spatial_mode: SpatialMode,
    /// What the renderer does for it (set by the spatial task).
    pub spatial_status: SpatialStatus,
    /// The user's choice; see [`DeviceState::follow_screen`].
    pub follow_screen_on: bool,
    /// What the head sensors do (set by the spatial task).
    pub head_tracking: HeadState,
}

impl DeviceState {
    pub fn new(address: &str, bluez_path: &str) -> Self {
        Self {
            address: address.to_string(),
            bluez_path: bluez_path.to_string(),
            alias: String::new(),
            info_name: None,
            modalias_product: None,
            model_number: None,
            firmware: None,
            connected: false,
            battery: BTreeMap::new(),
            primary: Side::Right,
            ear: None,
            noise_mode: None,
            mode_seen: false,
            press_hold: None,
            ear_detection: true,
            pause_on_remove: true,
            resume_on_return: true,
            low_battery_alerts: true,
            pending: None,
            alerts: LowBattery::default(),
            color: String::new(),
            media_paused: false,
            conversation: false,
            spatial_mode: SpatialMode::Off,
            spatial_status: SpatialStatus::Off,
            follow_screen_on: true,
            head_tracking: HeadState::Unavailable,
        }
    }

    pub fn model(&self) -> ModelSpec {
        let product = self
            .modalias_product
            .or_else(|| self.model_number.as_deref().and_then(model_for_number));
        product.map_or(UNKNOWN_MODEL, model_for_product)
    }

    pub fn name(&self) -> String {
        match &self.info_name {
            Some(n) if !n.is_empty() => n.clone(),
            _ => self.alias.clone(),
        }
    }

    pub fn firmware(&self) -> String {
        self.firmware
            .as_ref()
            .map(|(_, f)| f.clone())
            .unwrap_or_default()
    }

    /// (left, right).
    pub fn wearing(&self) -> (Wearing, Wearing) {
        let Some((primary, secondary)) = self.ear else {
            return (Wearing::Unknown, Wearing::Unknown);
        };
        match self.primary {
            Side::Right => (secondary, primary),
            Side::Left => (primary, secondary),
        }
    }

    pub fn noise_mode_str(&self) -> &'static str {
        self.noise_mode.unwrap_or(NoiseMode::Off).as_str()
    }

    /// The press-and-hold cycle, in the order the buds walk it.
    pub fn press_hold_modes(&self) -> Vec<NoiseMode> {
        let model = self.model();
        match &self.press_hold {
            Some(m) if model.press_hold => m.clone(),
            _ => arrange(model.default_press_hold, &[]),
        }
    }

    /// Follow Screen, for models with head tracking.
    pub fn follow_screen(&self) -> bool {
        self.model().head_tracking && self.follow_screen_on
    }

    pub fn conversation_awareness(&self) -> bool {
        self.model().conversation && self.conversation
    }

    /// What the case card shows for this advertisement. Parts the
    /// advertisement does not give come from the AAP link when it is open.
    pub fn snapshot(&self, adv: &Advert, levels: Levels) -> Snapshot {
        let aap = |part: Part| {
            self.connected
                .then(|| self.battery.get(&part))
                .flatten()
                .map(|&(percent, charging)| Level { percent, charging })
        };
        Snapshot {
            name: self.name(),
            model: self.model().product_string(),
            color: self.color.clone(),
            connected: self.connected,
            levels: Levels {
                left: levels.left.or_else(|| aap(Part::Left)),
                right: levels.right.or_else(|| aap(Part::Right)),
                case: levels.case.or_else(|| aap(Part::Case)),
            },
            buds_in_case: adv.buds_in_case(),
        }
    }

    pub fn set_color(&mut self, color: &str) -> bool {
        if self.color == color {
            return false;
        }
        self.color = color.to_string();
        true
    }

    pub fn ear_detection(&self) -> bool {
        self.model().ear_detection && self.ear_detection
    }

    pub fn set_ear_detection(&mut self, on: bool) -> bool {
        let changed = self.ear_detection != on;
        self.ear_detection = on;
        changed
    }

    pub fn battery_map(&self) -> HashMap<String, (u8, bool)> {
        self.battery
            .iter()
            .map(|(p, v)| (p.as_str().to_string(), *v))
            .collect()
    }

    /// What GNOME's Power panel gets: the lowest bud.
    pub fn lowest_bud(&self) -> Option<u8> {
        self.battery
            .iter()
            .filter(|(p, _)| **p != Part::Case)
            .map(|(_, (lvl, _))| *lvl)
            .min()
    }

    /// The mode a click on the tile goes to.
    pub fn next_mode(&self) -> Option<NoiseMode> {
        next_in_cycle(&self.press_hold_modes(), self.noise_mode)
    }

    /// We are about to ask for `mode`: its echo is ours.
    pub fn expect_mode(&mut self, mode: NoiseMode, now: Instant) {
        self.pending = Some((mode, now));
    }

    /// A new AAP session starts (also a silent reopen): the first mode
    /// it reports is a reading.
    pub fn begin_session(&mut self) {
        self.mode_seen = false;
        self.pending = None;
    }

    /// The link opened or closed. Readings of a closed link are dropped.
    pub fn set_connected(&mut self, connected: bool) -> Vec<Prop> {
        if self.connected == connected {
            return Vec::new();
        }
        self.connected = connected;
        self.mode_seen = false;
        self.pending = None;
        let mut props = vec![Prop::Connected];
        if !connected {
            if !self.battery.is_empty() {
                self.battery.clear();
                props.push(Prop::Battery);
            }
            if self.ear.take().is_some() {
                props.push(Prop::Wearing);
            }
        }
        props
    }

    /// Model-dependent properties, for when the model became known.
    fn model_props(out: &mut Outcome) {
        for p in [
            Prop::Model,
            Prop::ModelName,
            Prop::Capabilities,
            Prop::NoiseModes,
            Prop::PressHoldModes,
            Prop::EarDetection,
            Prop::FollowScreen,
        ] {
            out.prop(p);
        }
    }

    pub fn set_modalias_product(&mut self, product: Option<u16>) -> Vec<Prop> {
        if self.modalias_product == product {
            return Vec::new();
        }
        let before = self.model();
        self.modalias_product = product;
        let mut out = Outcome::default();
        if self.model() != before {
            Self::model_props(&mut out);
        }
        out.props
    }

    pub fn set_alias(&mut self, alias: &str) -> Vec<Prop> {
        let before = self.name();
        self.alias = alias.to_string();
        if self.name() != before {
            vec![Prop::Name]
        } else {
            Vec::new()
        }
    }

    pub fn apply(&mut self, msg: &Message, now: Instant) -> Outcome {
        let mut out = Outcome::default();
        match msg {
            Message::SessionEnd => out.session_end = true,
            Message::Battery(parts) => {
                let before = self.battery.clone();
                for c in parts {
                    match c.level {
                        Some(level) => {
                            self.battery.insert(c.part, (level, c.charging));
                            if self.low_battery_alerts
                                && let Some(pct) = self.alerts.update(c.part, level, c.charging)
                            {
                                out.low_battery.push((c.part, pct));
                            }
                        }
                        None => {
                            self.battery.remove(&c.part);
                        }
                    }
                }
                if self.battery != before {
                    out.prop(Prop::Battery);
                }
            }
            Message::Ear { primary, secondary } => {
                let before = self.wearing();
                self.ear = Some((*primary, *secondary));
                self.wearing_outcome(before, &mut out);
            }
            Message::PrimaryBud(side) => {
                let before = self.wearing();
                self.primary = *side;
                self.wearing_outcome(before, &mut out);
            }
            Message::Control { id, value } => self.apply_control(*id, value, now, &mut out),
            Message::Info(info) => {
                let model_before = self.model();
                let name_before = self.name();
                if !info.name.is_empty() {
                    self.info_name = Some(info.name.clone());
                }
                if !info.model_number.is_empty() {
                    self.model_number = Some(info.model_number.clone());
                }
                if let Some(fw) = info.firmware() {
                    // Once the firmware record has spoken, only it may
                    // change the value.
                    let take = match &self.firmware {
                        None => true,
                        Some((record, _)) => {
                            *record != FIRMWARE_RECORD || info.record == FIRMWARE_RECORD
                        }
                    };
                    if take {
                        let changed = self.firmware.as_ref().map(|(_, f)| f.as_str()) != Some(fw);
                        self.firmware = Some((info.record, fw.to_string()));
                        if changed {
                            out.prop(Prop::Firmware);
                        }
                    }
                }
                if self.name() != name_before {
                    out.prop(Prop::Name);
                }
                if self.model() != model_before {
                    Self::model_props(&mut out);
                }
            }
            Message::HandshakeAck
            | Message::Capabilities(_)
            | Message::Keys(_)
            | Message::Other { .. } => {}
        }
        out
    }

    /// AirPods Pro (1st generation) never report their press-and-hold
    /// cycle (control 0x1A) on connect, and AAP has no way to ask for it:
    /// the stem shows it, one press at a time (issue #14).
    fn learn_from_stem(&mut self, from: NoiseMode, to: NoiseMode, out: &mut Outcome) {
        let model = self.model();
        if !model.press_hold || !model.noise_modes.contains(&to) {
            return;
        }
        if let Some(cycle) = learn(&self.press_hold_modes(), from, to) {
            self.press_hold = Some(cycle);
            out.prop(Prop::PressHoldModes);
            out.press_hold_learned = true;
        }
    }

    fn wearing_outcome(&self, before: (Wearing, Wearing), out: &mut Outcome) {
        let after = self.wearing();
        if after != before {
            out.prop(Prop::Wearing);
            out.wearing_changed = Some((before, after));
        }
    }

    fn apply_control(&mut self, cid: u8, value: &[u8; 4], now: Instant, out: &mut Outcome) {
        match cid {
            id::LISTENING_MODE => {
                let Some(mode) = NoiseMode::from_wire(value[0]) else {
                    return;
                };
                let ours = matches!(self.pending, Some((m, at)) if m == mode && now.duration_since(at) <= TILE_WINDOW);
                if ours {
                    self.pending = None;
                }
                let first = !self.mode_seen;
                self.mode_seen = true;
                if self.noise_mode == Some(mode) {
                    return;
                }
                let previous = self.noise_mode.replace(mode);
                out.prop(Prop::NoiseMode);
                if !first {
                    let source = if ours { Source::Tile } else { Source::Stem };
                    out.noise_changed = Some((mode, source));
                    if !ours && let Some(from) = previous {
                        self.learn_from_stem(from, mode, out);
                    }
                }
            }
            id::LISTENING_MODE_CONFIGS => {
                // The mask says which modes, not in which order: keep the
                // order already known.
                let modes = arrange(&control::modes_of(value[0]), &self.press_hold_modes());
                if modes.len() >= 2 && self.press_hold_modes() != modes {
                    self.press_hold = Some(modes);
                    out.prop(Prop::PressHoldModes);
                    out.press_hold_learned = true;
                }
            }
            id::CONVERSATION_DETECT => {
                if let Some(on) = control::switch_from_byte(value[0])
                    && self.conversation != on
                {
                    self.conversation = on;
                    out.prop(Prop::ConversationAwareness);
                }
            }
            id::EAR_DETECTION => {
                if let Some(on) = control::switch_from_byte(value[0])
                    && self.set_ear_detection(on)
                {
                    out.prop(Prop::EarDetection);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pods_proto::{BatteryComponent, DeviceInfo};

    fn pro1() -> DeviceState {
        let mut s = DeviceState::new("AA:BB:CC:DD:EE:FF", "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        s.alias = "Pods".into();
        s.set_modalias_product(Some(0x200E));
        s.set_connected(true);
        s
    }

    fn mode(m: NoiseMode) -> Message {
        Message::Control {
            id: id::LISTENING_MODE,
            value: [m.wire(), 0, 0, 0],
        }
    }

    fn battery(parts: &[(Part, Option<u8>, bool)]) -> Message {
        Message::Battery(
            parts
                .iter()
                .map(|&(part, level, charging)| BatteryComponent {
                    part,
                    level,
                    charging,
                })
                .collect(),
        )
    }

    #[test]
    fn the_first_mode_of_a_session_is_not_a_change() {
        let mut s = pro1();
        let t = Instant::now();
        let out = s.apply(&mode(NoiseMode::Anc), t);
        assert_eq!(out.props, [Prop::NoiseMode]);
        assert_eq!(out.noise_changed, None);
        // Then the stem.
        let out = s.apply(&mode(NoiseMode::Transparency), t);
        assert_eq!(
            out.noise_changed,
            Some((NoiseMode::Transparency, Source::Stem))
        );
        // A repeat is nothing.
        assert_eq!(
            s.apply(&mode(NoiseMode::Transparency), t),
            Outcome::default()
        );
    }

    #[test]
    fn our_own_order_comes_back_as_tile() {
        let mut s = pro1();
        let t = Instant::now();
        s.apply(&mode(NoiseMode::Anc), t);
        s.expect_mode(NoiseMode::Transparency, t);
        let out = s.apply(
            &mode(NoiseMode::Transparency),
            t + Duration::from_millis(600),
        );
        assert_eq!(
            out.noise_changed,
            Some((NoiseMode::Transparency, Source::Tile))
        );
        // The pending order is used up: the next change is the stem's.
        let out = s.apply(&mode(NoiseMode::Anc), t + Duration::from_secs(1));
        assert_eq!(out.noise_changed, Some((NoiseMode::Anc, Source::Stem)));
    }

    #[test]
    fn a_late_or_different_echo_is_the_stem() {
        let mut s = pro1();
        let t = Instant::now();
        s.apply(&mode(NoiseMode::Anc), t);
        s.expect_mode(NoiseMode::Transparency, t);
        let out = s.apply(&mode(NoiseMode::Off), t + Duration::from_millis(100));
        assert_eq!(out.noise_changed, Some((NoiseMode::Off, Source::Stem)));
        s.expect_mode(NoiseMode::Anc, t);
        let out = s.apply(
            &mode(NoiseMode::Anc),
            t + TILE_WINDOW + Duration::from_secs(1),
        );
        assert_eq!(out.noise_changed, Some((NoiseMode::Anc, Source::Stem)));
    }

    #[test]
    fn reconnecting_reads_the_mode_again_silently() {
        let mut s = pro1();
        let t = Instant::now();
        s.apply(&mode(NoiseMode::Anc), t);
        s.set_connected(false);
        assert_eq!(s.noise_mode, Some(NoiseMode::Anc), "kept across the gap");
        s.set_connected(true);
        let out = s.apply(&mode(NoiseMode::Transparency), t);
        assert_eq!(out.props, [Prop::NoiseMode]);
        assert_eq!(out.noise_changed, None);
    }

    #[test]
    fn unknown_mode_bytes_are_ignored() {
        let mut s = pro1();
        let out = s.apply(
            &Message::Control {
                id: id::LISTENING_MODE,
                value: [9, 0, 0, 0],
            },
            Instant::now(),
        );
        assert_eq!(out, Outcome::default());
    }

    #[test]
    fn battery_and_its_lowest_bud() {
        let mut s = pro1();
        let t = Instant::now();
        let out = s.apply(
            &battery(&[
                (Part::Right, Some(86), false),
                (Part::Left, None, false),
                (Part::Case, None, false),
            ]),
            t,
        );
        assert_eq!(out.props, [Prop::Battery]);
        assert_eq!(
            s.battery_map(),
            HashMap::from([("right".to_string(), (86, false))])
        );
        assert_eq!(s.lowest_bud(), Some(86));
        let out = s.apply(
            &battery(&[(Part::Left, Some(40), true), (Part::Case, Some(5), false)]),
            t,
        );
        assert_eq!(out.props, [Prop::Battery]);
        assert_eq!(s.lowest_bud(), Some(40), "the case does not count");
        // Same values: no change.
        assert!(
            s.apply(&battery(&[(Part::Left, Some(40), true)]), t)
                .props
                .is_empty()
        );
        // A part going away is a change.
        let out = s.apply(&battery(&[(Part::Left, None, false)]), t);
        assert_eq!(out.props, [Prop::Battery]);
        assert_eq!(s.lowest_bud(), Some(86));
    }

    #[test]
    fn low_battery_comes_out_of_battery_messages() {
        let mut s = pro1();
        let t = Instant::now();
        let out = s.apply(
            &battery(&[(Part::Left, Some(9), false), (Part::Right, Some(30), false)]),
            t,
        );
        assert_eq!(out.low_battery, [(Part::Left, 9)]);
        let out = s.apply(&battery(&[(Part::Left, Some(8), false)]), t);
        assert!(out.low_battery.is_empty());
        s.low_battery_alerts = false;
        let out = s.apply(&battery(&[(Part::Right, Some(3), false)]), t);
        assert!(out.low_battery.is_empty());
    }

    #[test]
    fn wearing_is_mapped_through_the_primary_bud() {
        let mut s = pro1();
        let t = Instant::now();
        assert_eq!(s.wearing(), (Wearing::Unknown, Wearing::Unknown));
        // M0: right bud worn and primary, left in the case.
        let out = s.apply(
            &Message::Ear {
                primary: Wearing::Ear,
                secondary: Wearing::Case,
            },
            t,
        );
        assert_eq!(s.wearing(), (Wearing::Case, Wearing::Ear));
        assert_eq!(
            out.wearing_changed,
            Some((
                (Wearing::Unknown, Wearing::Unknown),
                (Wearing::Case, Wearing::Ear)
            ))
        );
        let out = s.apply(&Message::PrimaryBud(Side::Right), t);
        assert_eq!(out, Outcome::default());
        let out = s.apply(&Message::PrimaryBud(Side::Left), t);
        assert_eq!(s.wearing(), (Wearing::Ear, Wearing::Case));
        assert_eq!(out.props, [Prop::Wearing]);
    }

    #[test]
    fn disconnecting_drops_live_readings() {
        let mut s = pro1();
        let t = Instant::now();
        s.apply(&battery(&[(Part::Right, Some(50), false)]), t);
        s.apply(
            &Message::Ear {
                primary: Wearing::Ear,
                secondary: Wearing::Ear,
            },
            t,
        );
        let props = s.set_connected(false);
        assert_eq!(props, [Prop::Connected, Prop::Battery, Prop::Wearing]);
        assert!(s.battery.is_empty());
        assert_eq!(s.wearing(), (Wearing::Unknown, Wearing::Unknown));
        assert!(s.set_connected(false).is_empty());
    }

    #[test]
    fn press_hold_defaults_then_follows_the_buds() {
        let mut s = pro1();
        assert_eq!(
            s.press_hold_modes(),
            [NoiseMode::Transparency, NoiseMode::Anc]
        );
        assert_eq!(s.next_mode(), Some(NoiseMode::Transparency));
        let out = s.apply(
            &Message::Control {
                id: id::LISTENING_MODE_CONFIGS,
                value: [0x07, 0, 0, 0],
            },
            Instant::now(),
        );
        assert_eq!(out.props, [Prop::PressHoldModes]);
        assert!(out.press_hold_learned);
        // In the order the buds walk them: off → ANC → transparency.
        assert_eq!(
            s.press_hold_modes(),
            [NoiseMode::Off, NoiseMode::Anc, NoiseMode::Transparency]
        );
        // A one-mode mask is not a cycle.
        let out = s.apply(
            &Message::Control {
                id: id::LISTENING_MODE_CONFIGS,
                value: [0x02, 0, 0, 0],
            },
            Instant::now(),
        );
        assert!(out.props.is_empty());
    }

    #[test]
    fn cycling_follows_the_press_hold_modes() {
        let mut s = pro1();
        let t = Instant::now();
        s.apply(&mode(NoiseMode::Anc), t);
        assert_eq!(s.next_mode(), Some(NoiseMode::Transparency));
        s.press_hold = Some(vec![
            NoiseMode::Off,
            NoiseMode::Transparency,
            NoiseMode::Anc,
        ]);
        assert_eq!(s.next_mode(), Some(NoiseMode::Off));
    }

    /// Issue #14: the real Pro 1 went transparency → off → ANC on the
    /// stem while the engine believed transparency/ANC.
    #[test]
    fn the_stem_teaches_the_real_cycle() {
        let mut s = pro1();
        let t = Instant::now();
        s.apply(&mode(NoiseMode::Anc), t);
        s.apply(&mode(NoiseMode::Transparency), t);
        assert_eq!(
            s.press_hold_modes(),
            [NoiseMode::Transparency, NoiseMode::Anc]
        );
        let out = s.apply(&mode(NoiseMode::Off), t);
        assert_eq!(out.noise_changed, Some((NoiseMode::Off, Source::Stem)));
        assert!(out.props.contains(&Prop::PressHoldModes));
        assert!(out.press_hold_learned);
        assert_eq!(
            s.press_hold_modes(),
            [NoiseMode::Off, NoiseMode::Anc, NoiseMode::Transparency]
        );
        // A click on the tile now follows the stem: off → ANC.
        assert_eq!(s.next_mode(), Some(NoiseMode::Anc));
        // The next press confirms the order: nothing more to learn.
        let out = s.apply(&mode(NoiseMode::Anc), t);
        assert!(!out.press_hold_learned);
        // Our own changes teach nothing.
        s.expect_mode(NoiseMode::Off, t);
        let out = s.apply(&mode(NoiseMode::Off), t);
        assert_eq!(out.noise_changed, Some((NoiseMode::Off, Source::Tile)));
        assert!(!out.press_hold_learned);
    }

    #[test]
    fn the_first_reading_of_a_session_teaches_nothing() {
        let mut s = pro1();
        let t = Instant::now();
        s.apply(&mode(NoiseMode::Transparency), t);
        s.set_connected(false);
        s.set_connected(true);
        let out = s.apply(&mode(NoiseMode::Off), t);
        assert!(!out.press_hold_learned);
        assert_eq!(
            s.press_hold_modes(),
            [NoiseMode::Transparency, NoiseMode::Anc]
        );
    }

    #[test]
    fn conversation_awareness_reports() {
        let mut s = DeviceState::new("AA:BB:CC:DD:EE:FF", "/x");
        s.set_modalias_product(Some(0x2024));
        let msg = |v| Message::Control {
            id: id::CONVERSATION_DETECT,
            value: [v, 0, 0, 0],
        };
        let out = s.apply(&msg(1), Instant::now());
        assert_eq!(out.props, [Prop::ConversationAwareness]);
        assert!(s.conversation_awareness());
        assert!(s.apply(&msg(1), Instant::now()).props.is_empty());
        s.apply(&msg(2), Instant::now());
        assert!(!s.conversation_awareness());
        // The Pro 1 has none, whatever is reported.
        let mut p = pro1();
        p.apply(&msg(1), Instant::now());
        assert!(!p.conversation_awareness());
    }

    #[test]
    fn the_card_takes_what_the_advertisement_lacks_from_the_link() {
        use pods_proto::ble::parse_advert;
        let mut s = pro1();
        s.color = "white".into();
        s.apply(
            &battery(&[(Part::Left, Some(77), false), (Part::Case, Some(40), true)]),
            Instant::now(),
        );
        // Both in the case, primary right at 90 %, left and case unknown.
        let mut data = vec![
            0x07, 0x19, 0x01, 0x0E, 0x20, 0x54, 0xF9, 0x0F, 0x01, 0x00, 0x00,
        ];
        data.resize(27, 0);
        let adv = parse_advert(&data).unwrap();
        let card = s.snapshot(&adv, adv.coarse_levels());
        assert_eq!(card.name, "Pods");
        assert_eq!(card.model, "0x200E");
        assert_eq!(card.color, "white");
        assert!(card.connected);
        assert_eq!(card.buds_in_case, 2);
        assert_eq!(card.levels.right.unwrap().percent, 90);
        assert_eq!(card.levels.left.unwrap().percent, 77);
        assert!(card.levels.case.unwrap().charging);
        s.set_connected(false);
        let card = s.snapshot(&adv, adv.coarse_levels());
        assert!(!card.connected);
        assert_eq!(card.levels.left, None);
    }

    #[test]
    fn ear_detection_report() {
        let mut s = pro1();
        assert!(s.ear_detection());
        let out = s.apply(
            &Message::Control {
                id: id::EAR_DETECTION,
                value: [2, 0, 0, 0],
            },
            Instant::now(),
        );
        assert_eq!(out.props, [Prop::EarDetection]);
        assert!(!s.ear_detection());
    }

    fn info(record: u8, name: &str, number: &str, fw: &str) -> Message {
        Message::Info(Box::new(DeviceInfo {
            record,
            name: name.into(),
            model_number: number.into(),
            version3: fw.into(),
            ..Default::default()
        }))
    }

    #[test]
    fn information_gives_name_model_and_firmware() {
        let mut s = DeviceState::new("AA:BB:CC:DD:EE:FF", "/x");
        s.alias = "Alias".into();
        assert_eq!(s.model().product, None);
        assert_eq!(s.name(), "Alias");
        let t = Instant::now();
        // The Pro 1 order: record 0x0D first, then 0x04.
        let out = s.apply(&info(0x0D, "Buds", "A2084", "3344646"), t);
        assert!(out.props.contains(&Prop::Name));
        assert!(out.props.contains(&Prop::Model));
        assert!(out.props.contains(&Prop::Firmware));
        assert_eq!(s.model().product, Some(0x200E));
        assert_eq!(s.name(), "Buds");
        let out = s.apply(&info(0x04, "Buds", "A2084", "6619472"), t);
        assert_eq!(out.props, [Prop::Firmware]);
        assert_eq!(s.firmware(), "6619472");
        // Record 0x0D again does not take it back.
        let out = s.apply(&info(0x0D, "Buds", "A2084", "3344646"), t);
        assert!(out.props.is_empty());
        assert_eq!(s.firmware(), "6619472");
    }

    #[test]
    fn modalias_wins_over_the_model_number() {
        let mut s = DeviceState::new("AA:BB:CC:DD:EE:FF", "/x");
        let props = s.set_modalias_product(Some(0x2024));
        assert!(props.contains(&Prop::NoiseModes));
        s.apply(&info(0x04, "Buds", "A2084", "1"), Instant::now());
        assert_eq!(s.model().product, Some(0x2024));
        assert!(s.set_modalias_product(Some(0x2024)).is_empty());
    }

    #[test]
    fn session_end() {
        let mut s = pro1();
        assert!(s.apply(&Message::SessionEnd, Instant::now()).session_end);
    }

    #[test]
    fn source_words() {
        assert_eq!(Source::Stem.as_str(), "stem");
        assert_eq!(Source::Tile.as_str(), "tile");
        assert_eq!(Source::Other.as_str(), "other");
    }
}
