//! When to claim, keep, pause or give back the default output.
//!
//! Pure state machine (no PipeWire): the supervisor feeds it what it sees in
//! the graph and executes the actions it returns. Rules, from the bench's
//! Listen Together scenario (airpods-study 03, §6A):
//!
//! 1. Our sink appears → make it the default output (unless sharing is on).
//! 2. The default output becomes Listen Together (or another combine sink)
//!    → pause, keep the sink so Listen Together can hand the output back,
//!    and never take the default back while sharing.
//! 3. Sharing ends and the default comes back to us → resume.
//! 4. The user picks any other output → stop, without touching the default.
//! 5. The AirPods node disappears, the backend dies, or stop is requested →
//!    tear down and restore the previous default if it is still ours.

use std::time::{Duration, Instant};

use crate::conf::SINK_NODE_NAME;

/// After sharing ends, Listen Together needs a moment to hand the default
/// output back; until then another default is not taken as a user choice.
pub const SHARING_GRACE: Duration = Duration::from_secs(3);

/// Is this sink one that duplicates the sound to several headsets?
/// `listen_together` is Listen Together's combine-stream sink (it only exists
/// while sharing is on); `combine_bluetooth*` is the permanent combine sink
/// some setups declare in pipewire.conf.d (active only when chosen).
pub fn is_sharing_sink(node_name: &str) -> bool {
    node_name == "listen_together" || node_name.starts_with("combine_bluetooth")
}

/// Extracts `name` from a `Spa:String:JSON` default-node value such as
/// `{ "name": "bluez_output.XX.1" }`.
pub fn parse_default_name(json: &str) -> Option<String> {
    let rest = &json[json.find("\"name\"")? + 6..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

/// The metadata value that designates a sink by name.
pub fn default_json(node_name: &str) -> String {
    format!("{{ \"name\": {} }}", crate::conf::spa_json_string(node_name))
}

/// Why the spatial output is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    /// Listen Together (or another combine sink) shares the sound.
    Sharing,
}

/// Why the spatial output stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// `stop()` was called.
    Requested,
    /// The user chose another output (its node name).
    UserSwitched(String),
    /// The AirPods sink went away.
    TargetGone,
    /// The client process exited or the PipeWire connection broke.
    BackendFailed(String),
}

/// What the tile shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Waiting for the virtual sink to appear.
    Starting,
    /// Our sink is the default output and renders to the AirPods.
    Active,
    /// Our sink exists but is not used ("Spatial Off · Sharing").
    Paused(PauseReason),
    Stopped(StopReason),
}

/// What the graph shows the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Our virtual sink exists.
    SinkReady,
    /// The effective default sink (`default.audio.sink`) changed.
    DefaultSink(Option<String>),
    /// Sharing started or ended.
    Sharing(bool),
    TargetGone,
    BackendFailed(String),
    StopRequested,
    /// Periodic check (every few hundred ms).
    Tick,
}

/// What the supervisor must do, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Remember the configured default and make our sink the default.
    ClaimDefault,
    /// Put the remembered default back, if the configured one is still ours.
    RestoreDefault,
    /// Stop the backend (client process or in-process nodes).
    Teardown,
}

#[derive(Debug)]
pub struct Policy {
    status: Status,
    sink_ready: bool,
    sharing: bool,
    default_sink: Option<String>,
    /// We made our sink the default (and must give it back at the end).
    claimed: bool,
    /// The default was seen on our sink since the claim (until then, the old
    /// value may still be in flight and is not a user choice).
    seen_ours: bool,
    sharing_ended_at: Option<Instant>,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy {
    pub fn new() -> Self {
        Self {
            status: Status::Starting,
            sink_ready: false,
            sharing: false,
            default_sink: None,
            claimed: false,
            seen_ours: false,
            sharing_ended_at: None,
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.status, Status::Stopped(_))
    }

    fn default_is_ours(&self) -> bool {
        self.default_sink.as_deref() == Some(SINK_NODE_NAME)
    }

    fn stop(&mut self, reason: StopReason, restore: bool) -> Vec<Action> {
        let mut actions = vec![Action::Teardown];
        if restore && self.claimed {
            actions.push(Action::RestoreDefault);
        }
        self.status = Status::Stopped(reason);
        actions
    }

    fn claim(&mut self) -> Vec<Action> {
        self.claimed = true;
        self.seen_ours = self.default_is_ours();
        self.status = Status::Active;
        vec![Action::ClaimDefault]
    }

    /// Feeds one observation; returns the actions to run, in order.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Action> {
        if self.is_stopped() {
            return Vec::new();
        }
        match input {
            Input::StopRequested => self.stop(StopReason::Requested, true),
            Input::TargetGone => self.stop(StopReason::TargetGone, true),
            Input::BackendFailed(e) => self.stop(StopReason::BackendFailed(e), true),
            Input::SinkReady => {
                if self.sink_ready {
                    return Vec::new();
                }
                self.sink_ready = true;
                if self.sharing {
                    self.status = Status::Paused(PauseReason::Sharing);
                    Vec::new()
                } else {
                    self.claim()
                }
            }
            Input::Sharing(on) => {
                if on == self.sharing {
                    return Vec::new();
                }
                self.sharing = on;
                if !self.sink_ready {
                    return Vec::new();
                }
                if on {
                    self.sharing_ended_at = None;
                    self.status = Status::Paused(PauseReason::Sharing);
                    Vec::new()
                } else {
                    self.sharing_ended_at = Some(now);
                    if !self.claimed {
                        // Started while sharing: the output was never ours.
                        self.claim()
                    } else {
                        if self.default_is_ours() {
                            self.status = Status::Active;
                        }
                        Vec::new()
                    }
                }
            }
            Input::DefaultSink(name) => {
                self.default_sink = name;
                if !self.sink_ready {
                    return Vec::new();
                }
                let name = self.default_sink.clone().unwrap_or_default();
                if self.default_is_ours() {
                    self.seen_ours = true;
                    if !self.sharing {
                        self.status = Status::Active;
                    }
                    return Vec::new();
                }
                if self.sharing || is_sharing_sink(&name) {
                    // A combine sink as default is sharing too (rule 2).
                    self.status = Status::Paused(PauseReason::Sharing);
                    return Vec::new();
                }
                self.check_user_switch(now)
            }
            Input::Tick => {
                if self.sink_ready && !self.sharing && !self.default_is_ours() {
                    let name = self.default_sink.clone().unwrap_or_default();
                    if !is_sharing_sink(&name) {
                        return self.check_user_switch(now);
                    }
                }
                Vec::new()
            }
        }
    }

    /// The default is neither ours nor a sharing sink: a user choice, unless
    /// we are still waiting for our claim to land or for Listen Together to
    /// hand the output back.
    fn check_user_switch(&mut self, now: Instant) -> Vec<Action> {
        if !self.claimed || !self.seen_ours {
            return Vec::new();
        }
        if let Some(t) = self.sharing_ended_at
            && now.duration_since(t) < SHARING_GRACE
        {
            return Vec::new();
        }
        let name = self.default_sink.clone().unwrap_or_default();
        self.stop(StopReason::UserSwitched(name), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(n: &str) -> Input {
        Input::DefaultSink(Some(n.to_owned()))
    }

    #[test]
    fn parses_default_values() {
        assert_eq!(parse_default_name(r#"{ "name": "bluez_output.A.1" }"#).as_deref(), Some("bluez_output.A.1"));
        assert_eq!(parse_default_name(r#"{"name":"x\"y"}"#).as_deref(), Some("x\"y"));
        assert_eq!(parse_default_name("{}"), None);
        assert_eq!(parse_default_name(&default_json("spatial_airpods")).as_deref(), Some("spatial_airpods"));
    }

    #[test]
    fn claims_then_stops_and_restores() {
        let t = Instant::now();
        let mut p = Policy::new();
        assert!(p.handle(ds("speakers"), t).is_empty());
        assert_eq!(p.handle(Input::SinkReady, t), vec![Action::ClaimDefault]);
        assert_eq!(p.status(), &Status::Active);
        // The old default may still be reported before our claim lands.
        assert!(p.handle(ds("speakers"), t).is_empty());
        assert!(p.handle(ds(SINK_NODE_NAME), t).is_empty());
        assert_eq!(p.handle(Input::StopRequested, t), vec![Action::Teardown, Action::RestoreDefault]);
        assert_eq!(p.status(), &Status::Stopped(StopReason::Requested));
        assert!(p.handle(Input::SinkReady, t).is_empty());
    }

    #[test]
    fn user_choice_stops_without_restoring() {
        let t = Instant::now();
        let mut p = Policy::new();
        p.handle(Input::SinkReady, t);
        p.handle(ds(SINK_NODE_NAME), t);
        assert_eq!(p.handle(ds("speakers"), t), vec![Action::Teardown]);
        assert_eq!(p.status(), &Status::Stopped(StopReason::UserSwitched("speakers".into())));
    }

    #[test]
    fn listen_together_pauses_and_hands_back() {
        let t = Instant::now();
        let mut p = Policy::new();
        p.handle(Input::SinkReady, t);
        p.handle(ds(SINK_NODE_NAME), t);
        // Listen Together starts: its node appears, then it becomes default.
        assert!(p.handle(Input::Sharing(true), t).is_empty());
        assert_eq!(p.status(), &Status::Paused(PauseReason::Sharing));
        assert!(p.handle(ds("listen_together"), t).is_empty());
        // It stops: its node goes, WirePlumber falls back to some sink for a
        // moment, then Listen Together restores ours.
        assert!(p.handle(Input::Sharing(false), t).is_empty());
        assert!(p.handle(ds("speakers"), t + Duration::from_millis(100)).is_empty());
        assert!(p.handle(Input::Tick, t + Duration::from_millis(500)).is_empty());
        assert!(p.handle(ds(SINK_NODE_NAME), t + Duration::from_millis(600)).is_empty());
        assert_eq!(p.status(), &Status::Active);
    }

    #[test]
    fn sharing_that_does_not_hand_back_ends_as_user_choice() {
        let t = Instant::now();
        let mut p = Policy::new();
        p.handle(Input::SinkReady, t);
        p.handle(ds(SINK_NODE_NAME), t);
        p.handle(Input::Sharing(true), t);
        p.handle(ds("listen_together"), t);
        p.handle(Input::Sharing(false), t);
        p.handle(ds("speakers"), t);
        assert!(p.handle(Input::Tick, t + Duration::from_secs(1)).is_empty());
        assert_eq!(p.handle(Input::Tick, t + SHARING_GRACE), vec![Action::Teardown]);
    }

    #[test]
    fn permanent_combine_sink_as_default_is_sharing() {
        let t = Instant::now();
        let mut p = Policy::new();
        p.handle(Input::SinkReady, t);
        p.handle(ds(SINK_NODE_NAME), t);
        assert!(p.handle(ds("combine_bluetooth"), t).is_empty());
        assert_eq!(p.status(), &Status::Paused(PauseReason::Sharing));
        assert!(p.handle(Input::Tick, t + Duration::from_secs(10)).is_empty());
        p.handle(ds(SINK_NODE_NAME), t);
        assert_eq!(p.status(), &Status::Active);
    }

    #[test]
    fn started_during_sharing_claims_afterwards() {
        let t = Instant::now();
        let mut p = Policy::new();
        p.handle(Input::Sharing(true), t);
        p.handle(ds("listen_together"), t);
        assert!(p.handle(Input::SinkReady, t).is_empty());
        assert_eq!(p.status(), &Status::Paused(PauseReason::Sharing));
        assert_eq!(p.handle(Input::Sharing(false), t), vec![Action::ClaimDefault]);
        assert_eq!(p.status(), &Status::Active);
    }

    #[test]
    fn target_gone_restores() {
        let t = Instant::now();
        let mut p = Policy::new();
        p.handle(Input::SinkReady, t);
        assert_eq!(p.handle(Input::TargetGone, t), vec![Action::Teardown, Action::RestoreDefault]);
        let mut q = Policy::new();
        // Never claimed: nothing to restore.
        assert_eq!(q.handle(Input::BackendFailed("x".into()), t), vec![Action::Teardown]);
    }

    #[test]
    fn sharing_sinks() {
        assert!(is_sharing_sink("listen_together"));
        assert!(is_sharing_sink("combine_bluetooth"));
        assert!(is_sharing_sink("combine_bluetooth.2"));
        assert!(!is_sharing_sink("bluez_output.AA.1"));
        assert!(!is_sharing_sink(SINK_NODE_NAME));
    }
}
