// SPDX-License-Identifier: GPL-3.0-or-later
//! The case card: when the lid of our AirPods opens and closes, and what
//! the card shows. Pure: the BLE task feeds readings and the time in,
//! and sends the `CaseOpened` / `CaseClosed` signals that come out.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;
use zbus::wire::Variant;

use pods_proto::ble::{Advert, Level, Levels};

/// A lid that reads "open" without its counter having moved must stay
/// open this long (or be read open a second time within it) before it
/// counts: a single stray reading does not open the card.
pub const DEBOUNCE: Duration = Duration::from_millis(300);
/// While the lid stays open, a changed card goes out at most this often.
pub const RESEND: Duration = Duration::from_secs(2);
/// No reading at all for this long while open: closed. BlueZ normally
/// says it first with `DeviceLost` after 8 s of silence (the monitor's
/// low RSSI timeout); this is the net under it. BlueZ only signals
/// advertisements that change, so a quiet open case may legitimately go
/// unheard on D-Bus for a while.
pub const SILENCE_FALLBACK: Duration = Duration::from_secs(60);

/// What the lid byte says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LidReading {
    pub closed: bool,
    pub counter: u8,
    /// The lid byte only means something with both buds in the case.
    pub both_in_case: bool,
}

impl From<&Advert> for LidReading {
    fn from(a: &Advert) -> Self {
        Self {
            closed: a.lid_closed,
            counter: a.lid_counter,
            both_in_case: a.both_in_case,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LidChange {
    Opened,
    Closed,
}

/// Closed → open → closed, with the anti-bounce of the architecture
/// (§2.4): the opening counts when the opening counter moved, when "open"
/// is read twice within [`DEBOUNCE`], or when it held that long.
#[derive(Debug, Default)]
pub struct LidMachine {
    open: bool,
    counter: Option<u8>,
    /// First "open" reading not confirmed yet.
    candidate: Option<Instant>,
    last_reading: Option<Instant>,
}

impl LidMachine {
    pub fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) -> Option<LidChange> {
        self.candidate = None;
        std::mem::replace(&mut self.open, false).then_some(LidChange::Closed)
    }

    fn open_now(&mut self) -> Option<LidChange> {
        self.candidate = None;
        (!std::mem::replace(&mut self.open, true)).then_some(LidChange::Opened)
    }

    pub fn reading(&mut self, now: Instant, r: LidReading) -> Option<LidChange> {
        self.last_reading = Some(now);
        if !r.both_in_case {
            // A bud out: the lid byte reads "open" whatever the lid does.
            self.counter = None;
            return self.close();
        }
        let moved = self.counter.is_some_and(|c| c != r.counter);
        self.counter = Some(r.counter);
        if r.closed {
            return self.close();
        }
        if self.open {
            return None;
        }
        if moved {
            return self.open_now();
        }
        match self.candidate {
            Some(t) if now.duration_since(t) <= DEBOUNCE => self.open_now(),
            _ => {
                self.candidate = Some(now);
                None
            }
        }
    }

    /// BlueZ lost the advertiser.
    pub fn lost(&mut self) -> Option<LidChange> {
        self.counter = None;
        self.close()
    }

    /// Time-driven changes: a candidate that held, a silence too long.
    pub fn poll(&mut self, now: Instant) -> Option<LidChange> {
        if let Some(t) = self.candidate
            && now >= t + DEBOUNCE
        {
            return self.open_now();
        }
        if self.open
            && let Some(t) = self.last_reading
            && now >= t + SILENCE_FALLBACK
        {
            self.counter = None;
            return self.close();
        }
        None
    }

    pub fn deadline(&self) -> Option<Instant> {
        if let Some(t) = self.candidate {
            return Some(t + DEBOUNCE);
        }
        if self.open {
            return self.last_reading.map(|t| t + SILENCE_FALLBACK);
        }
        None
    }
}

/// What the card shows: the `snapshot` of `CaseOpened`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub name: String,
    pub model: String,
    pub color: String,
    pub connected: bool,
    pub levels: Levels,
    pub buds_in_case: u8,
}

impl Snapshot {
    /// The `a{sv}` of the contract. Unknown values are left out.
    pub fn to_dict(&self) -> HashMap<&'static str, Variant<'static>> {
        let mut d: HashMap<&'static str, Variant<'static>> = HashMap::new();
        for (key, value) in [
            ("name", &self.name),
            ("model", &self.model),
            ("color", &self.color),
        ] {
            if !value.is_empty() {
                d.insert(key, Variant::from(value.clone()));
            }
        }
        d.insert("connected", Variant::from(self.connected));
        let parts: [(&'static str, &'static str, Option<Level>); 3] = [
            ("left", "leftCharging", self.levels.left),
            ("right", "rightCharging", self.levels.right),
            ("case", "caseCharging", self.levels.case),
        ];
        for (key, charging, level) in parts {
            if let Some(l) = level {
                d.insert(key, Variant::from(l.percent));
                d.insert(charging, Variant::from(l.charging));
            }
        }
        d.insert("budsInCase", Variant::from(self.buds_in_case));
        d
    }
}

/// Once per opening, then at most every [`RESEND`] while it changes.
#[derive(Debug, Default)]
pub struct CardThrottle {
    last: Option<(Instant, Snapshot)>,
    pending: Option<Snapshot>,
}

impl CardThrottle {
    /// The lid opened: this card goes out now.
    pub fn opened(&mut self, now: Instant, s: Snapshot) -> Snapshot {
        self.pending = None;
        self.last = Some((now, s.clone()));
        s
    }

    /// The values may have changed while open: the card to send now, if any.
    pub fn offer(&mut self, now: Instant, s: Snapshot) -> Option<Snapshot> {
        let (at, sent) = self.last.as_ref()?;
        if *sent == s {
            self.pending = None;
            return None;
        }
        if now.duration_since(*at) >= RESEND {
            self.pending = None;
            self.last = Some((now, s.clone()));
            Some(s)
        } else {
            self.pending = Some(s);
            None
        }
    }

    pub fn poll(&mut self, now: Instant) -> Option<Snapshot> {
        let at = self.last.as_ref()?.0;
        if self.pending.is_some() && now >= at + RESEND {
            let s = self.pending.take()?;
            self.last = Some((now, s.clone()));
            return Some(s);
        }
        None
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref()?;
        self.last.as_ref().map(|(at, _)| *at + RESEND)
    }

    pub fn reset(&mut self) {
        self.last = None;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(counter: u8) -> LidReading {
        LidReading {
            closed: false,
            counter,
            both_in_case: true,
        }
    }

    fn closed(counter: u8) -> LidReading {
        LidReading {
            closed: true,
            counter,
            both_in_case: true,
        }
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn a_counter_that_moves_opens_at_once() {
        let t = Instant::now();
        let mut m = LidMachine::default();
        // The first advertisement after the case wakes still says closed
        // with the old counter (podctl's capture), then open with a new one.
        assert_eq!(m.reading(t, closed(2)), None);
        assert_eq!(m.reading(t + MS(40), open(3)), Some(LidChange::Opened));
        assert!(m.is_open());
        assert_eq!(m.reading(t + MS(80), open(3)), None);
        assert_eq!(m.reading(t + MS(500), closed(3)), Some(LidChange::Closed));
        assert_eq!(m.reading(t + MS(600), closed(3)), None);
    }

    #[test]
    fn without_a_counter_two_readings_within_300_ms() {
        let t = Instant::now();
        let mut m = LidMachine::default();
        assert_eq!(m.reading(t, open(5)), None);
        assert_eq!(m.deadline(), Some(t + DEBOUNCE));
        assert_eq!(m.reading(t + MS(120), open(5)), Some(LidChange::Opened));
    }

    #[test]
    fn or_one_reading_that_holds_300_ms() {
        let t = Instant::now();
        let mut m = LidMachine::default();
        assert_eq!(m.reading(t, open(5)), None);
        assert_eq!(m.poll(t + MS(299)), None);
        assert_eq!(m.poll(t + DEBOUNCE), Some(LidChange::Opened));
    }

    #[test]
    fn a_stray_open_reading_is_not_an_opening() {
        let t = Instant::now();
        let mut m = LidMachine::default();
        assert_eq!(m.reading(t, open(5)), None);
        assert_eq!(m.reading(t + MS(100), closed(5)), None);
        assert_eq!(m.poll(t + MS(400)), None);
        assert!(!m.is_open());
        assert_eq!(m.deadline(), None);
    }

    #[test]
    fn a_bud_out_of_the_case_tells_nothing_about_the_lid() {
        let t = Instant::now();
        let mut m = LidMachine::default();
        let worn = LidReading {
            closed: false,
            counter: 1,
            both_in_case: false,
        };
        assert_eq!(m.reading(t, worn), None);
        assert_eq!(m.poll(t + Duration::from_secs(1)), None);
        // Open, then a bud is taken out: the card goes.
        m.reading(t, closed(1));
        assert_eq!(m.reading(t + MS(10), open(2)), Some(LidChange::Opened));
        assert_eq!(m.reading(t + MS(20), worn), Some(LidChange::Closed));
        // Back in the open case: no counter to compare, the debounce applies.
        assert_eq!(m.reading(t + MS(30), open(2)), None);
        assert_eq!(m.poll(t + MS(330)), Some(LidChange::Opened));
    }

    #[test]
    fn lost_or_silent_closes() {
        let t = Instant::now();
        let mut m = LidMachine::default();
        m.reading(t, closed(1));
        m.reading(t, open(2));
        assert_eq!(m.lost(), Some(LidChange::Closed));
        assert_eq!(m.lost(), None);
        m.reading(t, closed(1));
        m.reading(t, open(2));
        assert_eq!(m.deadline(), Some(t + SILENCE_FALLBACK));
        assert_eq!(m.poll(t + SILENCE_FALLBACK), Some(LidChange::Closed));
    }

    fn snap(case: u8) -> Snapshot {
        Snapshot {
            name: "Pods".into(),
            levels: Levels {
                case: Some(Level {
                    percent: case,
                    charging: false,
                }),
                ..Default::default()
            },
            buds_in_case: 2,
            ..Default::default()
        }
    }

    #[test]
    fn the_card_goes_out_once_then_at_most_every_two_seconds() {
        let t = Instant::now();
        let mut th = CardThrottle::default();
        // Nothing to update before an opening.
        assert_eq!(th.offer(t, snap(50)), None);
        assert_eq!(th.opened(t, snap(50)), snap(50));
        // Same values: nothing.
        assert_eq!(th.offer(t + MS(100), snap(50)), None);
        // Changed within 2 s: held back, then sent when due.
        assert_eq!(th.offer(t + MS(500), snap(51)), None);
        assert_eq!(th.offer(t + MS(900), snap(52)), None);
        assert_eq!(th.deadline(), Some(t + RESEND));
        assert_eq!(th.poll(t + MS(1999)), None);
        assert_eq!(th.poll(t + RESEND), Some(snap(52)));
        assert_eq!(th.deadline(), None);
        // Changed after 2 s: at once.
        assert_eq!(
            th.offer(t + Duration::from_secs(5), snap(53)),
            Some(snap(53))
        );
        // A change that went back is dropped.
        assert_eq!(th.offer(t + MS(5100), snap(54)), None);
        assert_eq!(th.offer(t + MS(5200), snap(53)), None);
        assert_eq!(th.poll(t + Duration::from_secs(8)), None);
        th.reset();
        assert_eq!(th.offer(t + Duration::from_secs(9), snap(10)), None);
    }

    #[test]
    fn the_snapshot_speaks_the_contract() {
        let s = Snapshot {
            name: "Pods".into(),
            model: "0x200E".into(),
            color: "white".into(),
            connected: true,
            levels: Levels {
                left: Some(Level {
                    percent: 84,
                    charging: true,
                }),
                right: None,
                case: Some(Level {
                    percent: 62,
                    charging: false,
                }),
            },
            buds_in_case: 1,
        };
        let d = s.to_dict();
        let mut keys: Vec<_> = d.keys().copied().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "budsInCase",
                "case",
                "caseCharging",
                "color",
                "connected",
                "left",
                "leftCharging",
                "model",
                "name"
            ]
        );
        assert_eq!(d["left"], Variant::from(84u8));
        assert_eq!(d["leftCharging"], Variant::from(true));
        assert_eq!(d["budsInCase"], Variant::from(1u8));
        assert_eq!(d["connected"], Variant::from(true));
        let empty = Snapshot::default().to_dict();
        assert!(!empty.contains_key("name") && !empty.contains_key("left"));
    }
}
