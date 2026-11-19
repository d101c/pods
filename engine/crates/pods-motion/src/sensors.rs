// SPDX-License-Identifier: GPL-3.0-or-later
//! When to start and stop the head sensors, and over which path.
//!
//! Facts from M0 on the AirPods Pro (1st generation), docs §9:
//! - the sensors are announced (RemoteHID DeviceConnect) **only while sound
//!   plays to the AirPods**, with both buds worn; HostConnect alone does
//!   nothing when the output is idle;
//! - RemoteHID works there, SensorDataWX stays silent (it is the H2 path);
//! - a re-announce (primary bud switch) cancels the stream: the interval
//!   is sent again after every DeviceConnect of `devmotion6`;
//! - stopping is an interval of 0.
//!
//! The caller says whether head tracking is wanted (mode chosen, a bud
//! worn) and whether sound flows to the AirPods; this module answers with
//! the frames to send and the samples received. No I/O, no clock: every
//! call takes `now` from the caller's monotonic clock.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::block::{BLOCK_MIN_LEN, MotionSample};
use crate::remotehid::{self, kind};
use crate::wx;

/// The two ways AirPods serve their motion sensors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SensorPath {
    /// HID devices on descriptor 0x800: H1 models (AirPods 3, Pro 1).
    RemoteHid,
    /// Protobuf on descriptor 0x100000: H2 models.
    SensorDataWx,
}

impl SensorPath {
    /// The path to try first for a product id; the other one is the
    /// fallback. H2 models go to SensorDataWX, everything else (H1, Max,
    /// unknown) to RemoteHID, which also costs nothing when it is unused.
    pub fn preferred(product: Option<u16>) -> SensorPath {
        match product {
            // Pro 2 (Lightning, USB-C), Pro 3, AirPods 4 and 4 ANC.
            Some(0x2014 | 0x2024 | 0x2027 | 0x2019 | 0x201B) => SensorPath::SensorDataWx,
            _ => SensorPath::RemoteHid,
        }
    }

    pub fn other(self) -> SensorPath {
        match self {
            SensorPath::RemoteHid => SensorPath::SensorDataWx,
            SensorPath::SensorDataWx => SensorPath::RemoteHid,
        }
    }
}

/// What the sensors are doing, for logs and the interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorState {
    /// Not wanted, or no sound for longer than the grace period.
    Off,
    /// Asked for, no sample yet (waiting for the announce or the stream).
    Waiting(SensorPath),
    Streaming(SensorPath),
}

/// Delays of the state machine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SensorTiming {
    /// Report interval asked for: 20 000 µs, measured at 50.3 Hz.
    pub interval_us: u32,
    /// HostConnect or start request repeated this often while waiting.
    pub retry: Duration,
    /// Waiting this long on one path moves to the other one.
    pub path_timeout: Duration,
    /// No sample for this long while streaming: ask again.
    pub stall: Duration,
    /// Sensors kept running this long after the sound stops (docs §2.5).
    pub silence_grace: Duration,
}

impl Default for SensorTiming {
    fn default() -> Self {
        Self {
            interval_us: 20_000,
            retry: Duration::from_secs(3),
            path_timeout: Duration::from_secs(9),
            stall: Duration::from_secs(2),
            silence_grace: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Off,
    Active {
        path: SensorPath,
        /// A start request went out that needs an interval of 0 to undo.
        armed: bool,
        streaming: bool,
        /// When this path was (re)started.
        since: Duration,
        next_retry: Duration,
        last_sample: Duration,
    },
}

/// What a received frame gave.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Received {
    /// Frames to send back (re-arming after an announce).
    pub send: Vec<Vec<u8>>,
    pub samples: Vec<MotionSample>,
    /// The stream (re)starts from a fresh announce: the attitude may come
    /// from the other bud now.
    pub restarted: bool,
}

#[derive(Clone, Debug)]
pub struct SensorLink {
    timing: SensorTiming,
    preferred: SensorPath,
    wanted: bool,
    audio: bool,
    silent_since: Option<Duration>,
    /// RemoteHID device id → AccessoryService, from the DeviceConnects.
    devices: BTreeMap<u8, String>,
    motion_device: Option<u8>,
    phase: Phase,
    generation: u8,
    wx_seq: u8,
}

impl SensorLink {
    pub fn new(product: Option<u16>, timing: SensorTiming) -> Self {
        Self {
            timing,
            preferred: SensorPath::preferred(product),
            wanted: false,
            audio: false,
            silent_since: None,
            devices: BTreeMap::new(),
            motion_device: None,
            phase: Phase::Off,
            generation: 0,
            wx_seq: 0x40,
        }
    }

    /// The model became known (information frame 0x1D after the link opened).
    pub fn set_product(&mut self, product: Option<u16>) {
        self.preferred = SensorPath::preferred(product);
    }

    pub fn state(&self) -> SensorState {
        match self.phase {
            Phase::Off => SensorState::Off,
            Phase::Active {
                path,
                streaming: true,
                ..
            } => SensorState::Streaming(path),
            Phase::Active { path, .. } => SensorState::Waiting(path),
        }
    }

    /// RemoteHID devices announced on this link, by id.
    pub fn devices(&self) -> &BTreeMap<u8, String> {
        &self.devices
    }

    pub fn motion_device(&self) -> Option<u8> {
        self.motion_device
    }

    /// Head tracking wanted: Head Tracked mode chosen and a bud worn.
    pub fn set_wanted(&mut self, wanted: bool, now: Duration) -> Vec<Vec<u8>> {
        self.wanted = wanted;
        let mut out = Vec::new();
        self.update(now, &mut out);
        out
    }

    /// Sound flows to the AirPods (the `bluez_output` node is running).
    /// Every resume re-arms a stream that has gone quiet.
    pub fn set_audio(&mut self, flowing: bool, now: Duration) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let resumed = flowing && !self.audio;
        if flowing {
            self.silent_since = None;
        } else if self.audio {
            self.silent_since = Some(now);
        }
        self.audio = flowing;
        if resumed
            && let Phase::Active {
                path,
                streaming,
                last_sample,
                ..
            } = self.phase
            && !(streaming && now.saturating_sub(last_sample) < self.timing.stall)
        {
            self.start(path, now, &mut out);
        }
        self.update(now, &mut out);
        out
    }

    /// Timeouts: retries, path fallback, stall, end of the grace period.
    /// Call it about once a second (and whenever convenient).
    pub fn tick(&mut self, now: Duration) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.update(now, &mut out);
        let Phase::Active {
            path,
            streaming,
            since,
            next_retry,
            last_sample,
            ..
        } = self.phase
        else {
            return out;
        };
        if !self.audio {
            // Grace period: the buds may stop on their own; the next resume
            // re-arms, and asking without sound is useless (M0 quater).
            return out;
        }
        if streaming {
            if now.saturating_sub(last_sample) >= self.timing.stall {
                self.start(path, now, &mut out);
            }
        } else if now.saturating_sub(since) >= self.timing.path_timeout {
            self.halt(&mut out);
            self.start(path.other(), now, &mut out);
        } else if now >= next_retry {
            self.retry(path, now, &mut out);
        }
        out
    }

    /// Stops the sensors for good (interval 0), e.g. before closing the link.
    pub fn stop(&mut self) -> Vec<Vec<u8>> {
        self.wanted = false;
        let mut out = Vec::new();
        self.halt(&mut out);
        out
    }

    /// The AAP link is gone: forget the devices, send nothing.
    pub fn link_lost(&mut self) {
        self.devices.clear();
        self.motion_device = None;
        self.phase = Phase::Off;
    }

    /// Reads one received AAP frame. Announces are recorded even while
    /// off: on the Pro 1 they arrive once, right after the link opens.
    pub fn handle_frame(&mut self, frame: &[u8], now: Duration) -> Received {
        let mut rx = Received::default();
        if let Some(subs) = remotehid::parse(frame) {
            for sub in subs {
                self.remotehid(sub, now, &mut rx);
            }
        } else if let Some(blocks) = wx::motion_blocks(frame)
            && self.is_active()
        {
            let samples: Vec<_> = blocks
                .iter()
                .filter_map(|b| MotionSample::parse(b, now))
                .collect();
            if !samples.is_empty() {
                self.got_sample(SensorPath::SensorDataWx, now);
                rx.samples.extend(samples);
            }
        }
        rx
    }

    fn remotehid(&mut self, sub: remotehid::SubPacket<'_>, now: Duration, rx: &mut Received) {
        match sub.kind {
            kind::CONNECT if !sub.response => {
                let Some(name) = remotehid::accessory_service(sub.payload) else {
                    return;
                };
                self.devices.insert(sub.device, name.to_string());
                if name == remotehid::MOTION_SERVICE {
                    self.motion_device = Some(sub.device);
                    if self.is_active() {
                        // RemoteHID answers: use it, whatever was tried.
                        self.halt_wx(&mut rx.send);
                        self.start(SensorPath::RemoteHid, now, &mut rx.send);
                        rx.restarted = true;
                    }
                }
            }
            kind::DISCONNECT if !sub.response => {
                self.devices.remove(&sub.device);
                if self.motion_device == Some(sub.device) {
                    self.motion_device = None;
                    if let Phase::Active {
                        path: SensorPath::RemoteHid,
                        ..
                    } = self.phase
                    {
                        // Ask for a new announce at the next tick.
                        self.phase = Phase::Active {
                            path: SensorPath::RemoteHid,
                            armed: false,
                            streaming: false,
                            since: now,
                            next_retry: now,
                            last_sample: now,
                        };
                    }
                }
            }
            kind::CONFIRM_CONNECT if self.is_active() => {
                let frame = remotehid::host_connect(self.next_generation());
                rx.send.push(frame);
            }
            kind::INPUT_REPORT
                if Some(sub.device) == self.motion_device
                    && self.is_active()
                    && sub.payload.len() > BLOCK_MIN_LEN =>
            {
                // payload[0] is the HID report type (0 = input).
                if let Some(s) = MotionSample::parse(&sub.payload[1..], now) {
                    self.got_sample(SensorPath::RemoteHid, now);
                    rx.samples.push(s);
                }
            }
            _ => {}
        }
    }

    fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Active { .. })
    }

    fn should_run(&self, now: Duration) -> bool {
        self.wanted
            && (self.audio
                || self
                    .silent_since
                    .is_some_and(|s| now.saturating_sub(s) < self.timing.silence_grace))
    }

    fn update(&mut self, now: Duration, out: &mut Vec<Vec<u8>>) {
        match (self.is_active(), self.should_run(now)) {
            (false, true) => self.start(self.preferred, now, out),
            (true, false) => self.halt(out),
            _ => {}
        }
    }

    fn next_generation(&mut self) -> u8 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    fn wx_stream(&mut self, service: u8, interval_us: u32) -> Vec<u8> {
        self.wx_seq = (self.wx_seq + 1) & 0x7F;
        wx::stream(self.wx_seq, service, interval_us)
    }

    /// Sends the start request of `path` and waits for samples.
    fn start(&mut self, path: SensorPath, now: Duration, out: &mut Vec<Vec<u8>>) {
        let interval = self.timing.interval_us;
        let armed = match path {
            SensorPath::RemoteHid => match self.motion_device {
                Some(dev) => {
                    let g = self.next_generation();
                    out.push(remotehid::set_interval(g, dev, interval));
                    true
                }
                None => {
                    let g = self.next_generation();
                    out.push(remotehid::host_connect(g));
                    false
                }
            },
            SensorPath::SensorDataWx => {
                // MousePods' sequence for the H2 models.
                out.push(self.wx_stream(wx::SERVICE_ACTIVITY, 40_000));
                out.push(self.wx_stream(wx::SERVICE_DEVMOTION6, interval));
                out.push(self.wx_stream(wx::SERVICE_ACTIVITY, 0));
                true
            }
        };
        self.phase = Phase::Active {
            path,
            armed,
            streaming: false,
            since: now,
            next_retry: now + self.timing.retry,
            last_sample: now,
        };
    }

    fn retry(&mut self, path: SensorPath, now: Duration, out: &mut Vec<Vec<u8>>) {
        let interval = self.timing.interval_us;
        let armed = match (path, self.motion_device) {
            (SensorPath::RemoteHid, Some(dev)) => {
                let g = self.next_generation();
                out.push(remotehid::set_interval(g, dev, interval));
                true
            }
            (SensorPath::RemoteHid, None) => {
                let g = self.next_generation();
                out.push(remotehid::host_connect(g));
                false
            }
            (SensorPath::SensorDataWx, _) => {
                out.push(self.wx_stream(wx::SERVICE_DEVMOTION6, interval));
                true
            }
        };
        if let Phase::Active {
            armed: a,
            next_retry,
            ..
        } = &mut self.phase
        {
            *a |= armed;
            *next_retry = now + self.timing.retry;
        }
    }

    fn got_sample(&mut self, from: SensorPath, now: Duration) {
        if let Phase::Active {
            path,
            armed,
            streaming,
            last_sample,
            ..
        } = &mut self.phase
        {
            *path = from;
            *armed = true;
            *streaming = true;
            *last_sample = now;
        }
    }

    /// Interval 0 for whatever was started, then off.
    fn halt(&mut self, out: &mut Vec<Vec<u8>>) {
        if let Phase::Active {
            path, armed: true, ..
        } = self.phase
        {
            match path {
                SensorPath::RemoteHid => {
                    if let Some(dev) = self.motion_device {
                        let g = self.next_generation();
                        out.push(remotehid::set_interval(g, dev, 0));
                    }
                }
                SensorPath::SensorDataWx => self.halt_wx(out),
            }
        }
        self.phase = Phase::Off;
    }

    fn halt_wx(&mut self, out: &mut Vec<Vec<u8>>) {
        if let Phase::Active {
            path: SensorPath::SensorDataWx,
            armed: true,
            ..
        } = self.phase
        {
            out.push(self.wx_stream(wx::SERVICE_DEVMOTION6, 0));
            out.push(self.wx_stream(wx::SERVICE_ACTIVITY, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn nothing_starts_without_sound() {
        let mut l = SensorLink::new(Some(0x200E), SensorTiming::default());
        assert!(l.set_wanted(true, secs(0.0)).is_empty());
        assert_eq!(l.state(), SensorState::Off);
        let out = l.set_audio(true, secs(1.0));
        assert_eq!(out, vec![remotehid::host_connect(1)]);
        assert_eq!(l.state(), SensorState::Waiting(SensorPath::RemoteHid));
    }

    #[test]
    fn h2_models_start_with_sensordatawx() {
        let mut l = SensorLink::new(Some(0x2014), SensorTiming::default());
        l.set_audio(true, secs(0.0));
        let out = l.set_wanted(true, secs(0.0));
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[1],
            wx::stream(0x42, wx::SERVICE_DEVMOTION6, 20_000),
            "ACTIVITY, DEVMOTION6, ACTIVITY 0"
        );
        // Silent for the whole path timeout: the other path is tried.
        let mut sent = Vec::new();
        for i in 1..=10 {
            sent.extend(l.tick(secs(i as f64)));
        }
        assert_eq!(l.state(), SensorState::Waiting(SensorPath::RemoteHid));
        assert!(sent.contains(&wx::stream(0x46, wx::SERVICE_DEVMOTION6, 0)));
        assert!(sent.iter().any(|f| f[..10] == remotehid::PREFIX));
    }

    #[test]
    fn host_connect_is_repeated_while_waiting() {
        let mut l = SensorLink::new(Some(0x200E), SensorTiming::default());
        l.set_wanted(true, secs(0.0));
        l.set_audio(true, secs(0.0));
        assert!(l.tick(secs(1.0)).is_empty());
        assert_eq!(l.tick(secs(3.0)), vec![remotehid::host_connect(2)]);
        assert!(l.tick(secs(4.0)).is_empty());
    }
}
