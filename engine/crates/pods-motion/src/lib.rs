// SPDX-License-Identifier: GPL-3.0-or-later
//! Head tracking with AirPods: when to start the motion sensors, how to read
//! them, and how to turn them into a yaw that keeps a sound scene in front
//! of the screen.
//!
//! Pure code, like `pods-proto`: AAP frames in, AAP frames and poses out.
//! Nothing here opens a socket, reads a clock or touches PipeWire; the
//! caller passes `now` from its monotonic clock. The tests replay the
//! frames of real AirPods Pro (1st generation) captured in M0
//! (`tests/m0_quater.rs`).
//!
//! [`HeadTracking`] is the whole thing for `podsd`:
//!
//! ```
//! use std::time::Duration;
//! use pods_motion::{Event, HeadTracking, MotionConfig};
//!
//! let mut ht = HeadTracking::new(Some(0x200E), MotionConfig::default());
//! let now = Duration::from_secs(1);
//! let mut events = ht.set_enabled(true, now); // Head Tracked mode, buds worn
//! events.extend(ht.set_audio(true, now)); // bluez_output node running
//! for e in events {
//!     match e {
//!         Event::Send(frame) => { /* write to the AAP socket */ }
//!         Event::State(state) => { /* log, D-Bus property */ }
//!         Event::RenderPose(pose) => { /* azimuths of pods-spatial */ }
//!         Event::UiPose(pose) => { /* HeadPose signal, 30 Hz at most */ }
//!     }
//! }
//! // Then: every received frame to `handle_frame`, `tick` about once a second.
//! ```
//!
//! Origins (details in `engine/NOTICE`): the RemoteHID and SensorDataWX
//! wire formats and the One-Euro filter come from MousePods and podtrack
//! (Apache-2.0); the Pro 1 behaviour (announce only while sound plays,
//! offsets and scales, twist about gravity, drift) was measured on
//! 19 November 2026.

pub mod block;
pub mod filter;
pub mod quat;
pub mod remotehid;
pub mod sensors;
pub mod throttle;
pub mod tracker;
pub mod wx;

use std::time::Duration;

pub use block::MotionSample;
pub use filter::OneEuroParams;
pub use sensors::{SensorLink, SensorPath, SensorState, SensorTiming};
pub use throttle::Throttle;
pub use tracker::{Pose, Tracker, TrackerConfig};

/// Everything that can be tuned, with the values chosen in docs §2.5.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionConfig {
    pub sensors: SensorTiming,
    pub tracker: TrackerConfig,
    /// Pose rate for the renderer (pods-spatial), at most.
    pub render_hz: f64,
    /// Pose rate for the interface (D-Bus `HeadPose`), at most.
    pub ui_hz: f64,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            sensors: SensorTiming::default(),
            tracker: TrackerConfig::default(),
            render_hz: 50.0,
            ui_hz: 30.0,
        }
    }
}

/// What the caller has to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Write this frame to the AAP link.
    Send(Vec<u8>),
    /// The sensors changed state.
    State(SensorState),
    /// A pose for the renderer (≤ `render_hz`).
    RenderPose(Pose),
    /// A pose for the interface (≤ `ui_hz`), only while watched.
    UiPose(Pose),
}

/// Sensors, tracker and rate limits for one pair of AirPods.
#[derive(Clone, Debug)]
pub struct HeadTracking {
    link: SensorLink,
    tracker: Tracker,
    render: Throttle,
    ui: Throttle,
    ui_watched: bool,
    state: SensorState,
}

impl HeadTracking {
    /// `product`: the AirPods' product id (0x200E for the Pro 1), when known.
    pub fn new(product: Option<u16>, config: MotionConfig) -> Self {
        Self {
            link: SensorLink::new(product, config.sensors),
            tracker: Tracker::new(config.tracker),
            render: Throttle::new(config.render_hz),
            ui: Throttle::new(config.ui_hz),
            ui_watched: false,
            state: SensorState::Off,
        }
    }

    pub fn set_product(&mut self, product: Option<u16>) {
        self.link.set_product(product);
    }

    pub fn state(&self) -> SensorState {
        self.state
    }

    /// The last pose computed, whatever the rate limits let through.
    pub fn pose(&self) -> Option<Pose> {
        self.tracker.last_pose()
    }

    pub fn sensors(&self) -> &SensorLink {
        &self.link
    }

    /// Head tracking wanted: Head Tracked mode chosen and a bud worn.
    pub fn set_enabled(&mut self, on: bool, now: Duration) -> Vec<Event> {
        let frames = self.link.set_wanted(on, now);
        self.finish(frames)
    }

    /// Sound flows to the AirPods or not. The sensors stop after
    /// `silence_grace` without sound and are re-armed when it resumes.
    pub fn set_audio(&mut self, flowing: bool, now: Duration) -> Vec<Event> {
        let frames = self.link.set_audio(flowing, now);
        self.finish(frames)
    }

    /// One frame received on the AAP link (any opcode; others are ignored).
    pub fn handle_frame(&mut self, frame: &[u8], now: Duration) -> Vec<Event> {
        let rx = self.link.handle_frame(frame, now);
        if rx.restarted {
            self.tracker.reanchor();
        }
        let mut events: Vec<Event> = rx.send.into_iter().map(Event::Send).collect();
        for s in &rx.samples {
            if let Some(pose) = self.tracker.push(s) {
                if let Some(p) = self.render.offer(pose, now) {
                    events.push(Event::RenderPose(p));
                }
                if self.ui_watched
                    && let Some(p) = self.ui.offer(pose, now)
                {
                    events.push(Event::UiPose(p));
                }
            }
        }
        self.push_state(&mut events);
        events
    }

    /// Timeouts of the sensors, and poses held back by the rate limits.
    pub fn tick(&mut self, now: Duration) -> Vec<Event> {
        let frames = self.link.tick(now);
        let mut events = self.finish(frames);
        if let Some(p) = self.render.poll(now) {
            events.push(Event::RenderPose(p));
        }
        if self.ui_watched
            && let Some(p) = self.ui.poll(now)
        {
            events.push(Event::UiPose(p));
        }
        events
    }

    /// The current direction of the head becomes straight ahead.
    pub fn recenter(&mut self) {
        self.tracker.recenter();
    }

    pub fn set_follow_screen(&mut self, on: bool) {
        self.tracker.set_follow_screen(on);
    }

    /// The interface wants live poses (`WatchHeadPose`).
    pub fn watch_ui(&mut self, on: bool) {
        self.ui_watched = on;
        if !on {
            self.ui.clear();
        }
    }

    /// Stops the sensors (interval 0) before the link is closed on purpose.
    pub fn stop(&mut self) -> Vec<Event> {
        let frames = self.link.stop();
        self.finish(frames)
    }

    /// The AAP link dropped: nothing can be sent, everything starts over.
    pub fn link_lost(&mut self) -> Vec<Event> {
        self.link.link_lost();
        self.render.clear();
        self.ui.clear();
        let mut events = Vec::new();
        self.push_state(&mut events);
        events
    }

    fn finish(&mut self, frames: Vec<Vec<u8>>) -> Vec<Event> {
        let mut events: Vec<Event> = frames.into_iter().map(Event::Send).collect();
        self.push_state(&mut events);
        events
    }

    fn push_state(&mut self, events: &mut Vec<Event>) {
        let state = self.link.state();
        if state == self.state {
            return;
        }
        // A fresh start (sound resumed, buds back in): the user most likely
        // faces the screen, and the old reference may belong to the other bud.
        if self.state == SensorState::Off {
            self.tracker.recenter();
            self.render.clear();
            self.ui.clear();
        }
        self.state = state;
        events.push(Event::State(state));
    }
}
