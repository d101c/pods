// SPDX-License-Identifier: GPL-3.0-or-later
//! Spatial audio and head tracking for one pair of AirPods (docs §2.5,
//! §2.6, issue #23).
//!
//! One task per device owns the renderer (`pods-spatial`) and the head
//! sensors (`pods-motion`):
//!
//! - every AAP frame of the link goes through `HeadTracking::handle_frame`
//!   (the Pro 1 announces its sensors once, right after the link opens, so
//!   the frames are read whatever the mode); the frames it answers go back
//!   on the link;
//! - whether sound flows to the AirPods comes from the state of their
//!   PipeWire node `bluez_output.<address>.1` ([`AudioMonitor`]);
//! - render poses go to the renderer's yaw, interface poses to the
//!   `HeadPose` signal (only while a client watches);
//! - `SpatialMode` is applied while the AirPods are connected and the node
//!   exists, `SpatialStatus` follows what the renderer reports.
//!
//! The renderer and the node monitor are traits so that the tests run the
//! engine without PipeWire.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};
use zbus::object_server::InterfaceRef;

use pods_motion::{Event, HeadTracking, MotionConfig, SensorState, TrackerConfig};
use pods_proto::{Capability, Wearing};
use pods_spatial::{NodeRun, Options, SinkWatch, Spatial, Target};

pub use pods_spatial::{Mode as RenderMode, PauseReason, Status as RenderStatus, StopReason};

use crate::api::{self, DeviceCtx, DeviceIface};
use crate::state::Prop;

/// Sensor timeouts and held-back poses are checked this often.
const TICK: Duration = Duration::from_millis(500);
/// Longest wait for the stop frames of the sensors at shutdown.
const STOP_WRITE: Duration = Duration::from_secs(1);

// ------------------------------------------------------------ vocabulary

/// `SpatialMode` of the contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialMode {
    #[default]
    Off,
    Fixed,
    Head,
}

impl SpatialMode {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "off" => Some(Self::Off),
            "fixed" => Some(Self::Fixed),
            "head" => Some(Self::Head),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Fixed => "fixed",
            Self::Head => "head",
        }
    }
}

/// `SpatialStatus` of the contract: what the renderer does for the mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialStatus {
    #[default]
    Off,
    Starting,
    Active,
    /// Another app shares the sound (Listen Together).
    Paused,
    Failed,
}

impl SpatialStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Failed => "failed",
        }
    }
}

/// `HeadTracking` of the contract: what the sensors do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeadState {
    /// No sensors on this model, or the AirPods are not connected.
    #[default]
    Unavailable,
    /// Sensors stopped: another mode, no sound, buds not worn.
    Idle,
    /// Asked for, no sample yet.
    Starting,
    /// Samples flow.
    Active,
}

impl HeadState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Active => "active",
        }
    }
}

// ---------------------------------------------------------- abstractions

/// Where render status changes go; called from the renderer's thread.
pub type StatusSink = Box<dyn Fn(RenderStatus) + Send + 'static>;

/// Starts the spatial output. Production: `pods-spatial` on the session's
/// PipeWire. Tests: a fake that records what it is asked.
pub trait Renderer: Send + Sync {
    /// Starts rendering to the AirPods with `address` (`name` for the sink
    /// description). Blocking (about 0.7 s): called on a blocking thread.
    fn start(
        &self,
        mode: RenderMode,
        address: &str,
        name: &str,
        on_status: StatusSink,
    ) -> Result<Box<dyn Rendering>, String>;
}

/// A running spatial output.
pub trait Rendering: Send + Sync {
    /// Head yaw, degrees, positive to the left. Cheap (one atomic store).
    fn set_yaw(&self, yaw_deg: f32);
    /// Tears the output down and gives the default sink back. Blocking.
    fn stop(self: Box<Self>);
}

/// The AirPods' PipeWire output node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkState {
    /// The node is in the graph.
    pub present: bool,
    /// The node is running: sound flows to the AirPods.
    pub running: bool,
    /// Counts the node's appearances: a node that went away and came back
    /// between two looks still shows as a new one.
    pub appearances: u64,
}

/// Follows the output node of each pair of AirPods.
pub trait AudioMonitor: Send + Sync {
    /// The state of `bluez_output.<address>.1`, from now on.
    fn watch(&self, address: &str) -> watch::Receiver<SinkState>;
}

// ------------------------------------------------------ the real ones

/// `pods-spatial` on the session's PipeWire.
#[derive(Debug, Default)]
pub struct PipeWireRenderer;

impl Renderer for PipeWireRenderer {
    fn start(
        &self,
        mode: RenderMode,
        address: &str,
        name: &str,
        on_status: StatusSink,
    ) -> Result<Box<dyn Rendering>, String> {
        let target = Target::new(Target::bluez_node_name(address), name);
        let spatial = Spatial::start(mode, target, Options::new(), on_status)
            .map_err(|e| e.to_string())?;
        Ok(Box::new(spatial))
    }
}

impl Rendering for Spatial {
    fn set_yaw(&self, yaw_deg: f32) {
        Spatial::set_yaw(self, yaw_deg);
    }

    fn stop(self: Box<Self>) {
        let last = Spatial::stop(*self);
        debug!(status = ?last, "spatial output stopped");
    }
}

type SinkSenders = Arc<Mutex<HashMap<String, watch::Sender<SinkState>>>>;

/// The `bluez_output.*` nodes of the session's PipeWire, through the
/// `pipewire` crate (node `info` events), no subprocess.
pub struct PipeWireSinks {
    senders: SinkSenders,
    _watch: Option<SinkWatch>,
}

impl PipeWireSinks {
    pub fn start() -> Self {
        let senders = SinkSenders::default();
        let shared = senders.clone();
        let watch = SinkWatch::start("bluez_output.", None, move |name, run| {
            let Some(address) = address_of_node(name) else {
                return;
            };
            let mut map = shared.lock().expect("sinks lock");
            let tx = sender(&mut map, &address);
            tx.send_modify(|s| {
                let present = run != NodeRun::Gone;
                if present && !s.present {
                    s.appearances += 1;
                }
                s.present = present;
                s.running = run == NodeRun::Running;
            });
        });
        let watch = match watch {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(error = %e, "cannot follow the AirPods' audio node");
                None
            }
        };
        Self {
            senders,
            _watch: watch,
        }
    }
}

fn sender<'a>(
    map: &'a mut HashMap<String, watch::Sender<SinkState>>,
    address: &str,
) -> &'a watch::Sender<SinkState> {
    map.entry(address.to_uppercase())
        .or_insert_with(|| watch::channel(SinkState::default()).0)
}

impl AudioMonitor for PipeWireSinks {
    fn watch(&self, address: &str) -> watch::Receiver<SinkState> {
        let mut map = self.senders.lock().expect("sinks lock");
        sender(&mut map, address).subscribe()
    }
}

/// `bluez_output.AA_BB_CC_DD_EE_FF.1` → `AA:BB:CC:DD:EE:FF`; `None` for
/// any other node (another profile, another naming).
pub fn address_of_node(name: &str) -> Option<String> {
    let rest = name.strip_prefix("bluez_output.")?;
    let (addr, _) = rest.split_once('.')?;
    let address = addr.replace('_', ":").to_uppercase();
    (address.len() == 17 && Target::bluez_node_name(&address) == name).then_some(address)
}

// --------------------------------------------------------------- the task

/// What the device task, the API and the renderer tell the spatial task.
#[derive(Debug)]
pub enum SpatialMsg {
    /// An AAP session opened (true) or ended (false; also before a silent
    /// reopen, while `Connected` stays true).
    Session(bool),
    /// `Connected` went false: the AirPods are gone.
    Disconnected,
    /// A frame received on the AAP link.
    Frame(Vec<u8>),
    /// `Wearing` changed.
    Wearing,
    /// The model may have changed.
    Model,
    /// `SpatialMode` was set (also: try again after a failure).
    Mode,
    Recenter,
    FollowScreen(bool),
    /// Someone watches `HeadPose` (or nobody any more).
    Watch(bool),
    /// The renderer of this generation reports a status.
    Render(u64, RenderStatus),
    /// Stop everything and end; answer when done.
    Shutdown(Option<oneshot::Sender<()>>),
}

struct Running {
    generation: u64,
    mode: RenderMode,
    handle: Box<dyn Rendering>,
    status: RenderStatus,
}

/// Why no renderer runs although one is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    /// Nothing holds: start as soon as the node is there.
    Clear,
    /// The renderer failed; again when the node appears anew or the mode is set.
    Failed { appearances: u64 },
    /// The node went away under the renderer: wait for it to come back.
    NodeGone { appearances: u64 },
    /// The user picked another output: wait for the mode to be set again
    /// or for the next connection.
    UserSwitched,
}

pub struct SpatialTask {
    ctx: Arc<DeviceCtx>,
    iface: InterfaceRef<DeviceIface>,
    renderer: Arc<dyn Renderer>,
    rx: mpsc::UnboundedReceiver<SpatialMsg>,
    /// For the renderer's status callbacks.
    tx: mpsc::UnboundedSender<SpatialMsg>,
    sink: watch::Receiver<SinkState>,
    sink_open: bool,
    epoch: Instant,
    session: bool,
    ht: Option<HeadTracking>,
    watched: bool,
    render: Option<Running>,
    generation: u64,
    hold: Hold,
}

impl SpatialTask {
    pub fn new(
        ctx: Arc<DeviceCtx>,
        iface: InterfaceRef<DeviceIface>,
        renderer: Arc<dyn Renderer>,
        channel: (
            mpsc::UnboundedSender<SpatialMsg>,
            mpsc::UnboundedReceiver<SpatialMsg>,
        ),
        sink: watch::Receiver<SinkState>,
    ) -> Self {
        let (tx, rx) = channel;
        Self {
            ctx,
            iface,
            renderer,
            rx,
            tx,
            sink,
            sink_open: true,
            epoch: Instant::now(),
            session: false,
            ht: None,
            watched: false,
            render: None,
            generation: 0,
            hold: Hold::Clear,
        }
    }

    pub async fn run(mut self) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = self.rx.recv() => match msg {
                    Some(SpatialMsg::Shutdown(reply)) => {
                        self.shutdown().await;
                        if let Some(r) = reply {
                            let _ = r.send(());
                        }
                        break;
                    }
                    Some(m) => self.handle(m).await,
                    None => {
                        self.shutdown().await;
                        break;
                    }
                },
                r = async {
                    if self.sink_open { self.sink.changed().await } else { std::future::pending().await }
                } => {
                    if r.is_err() {
                        self.sink_open = false;
                    }
                    self.on_sink().await;
                }
                _ = tick.tick() => self.on_tick().await,
            }
        }
        debug!("spatial task ends");
    }

    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn capable(&self) -> (bool, bool) {
        let model = self.ctx.state().model();
        (
            model.capabilities().contains(&Capability::SpatialHead),
            model.product.is_some(),
        )
    }

    async fn handle(&mut self, msg: SpatialMsg) {
        match msg {
            SpatialMsg::Session(true) => {
                self.session = true;
                self.ensure_sensors().await;
            }
            SpatialMsg::Session(false) => {
                self.session = false;
                self.drop_sensors().await;
            }
            SpatialMsg::Disconnected => {
                self.session = false;
                self.drop_sensors().await;
                self.hold = Hold::Clear;
            }
            SpatialMsg::Frame(frame) => {
                let now = self.now();
                if let Some(ht) = &mut self.ht {
                    let events = ht.handle_frame(&frame, now);
                    self.process(events).await;
                }
                return;
            }
            SpatialMsg::Wearing => {}
            SpatialMsg::Model => self.ensure_sensors().await,
            SpatialMsg::Mode => self.hold = Hold::Clear,
            SpatialMsg::Recenter => {
                if let Some(ht) = &mut self.ht {
                    ht.recenter();
                    info!("head tracking recentred");
                }
            }
            SpatialMsg::FollowScreen(on) => {
                if let Some(ht) = &mut self.ht {
                    ht.set_follow_screen(on);
                }
            }
            SpatialMsg::Watch(on) => {
                self.watched = on;
                if let Some(ht) = &mut self.ht {
                    ht.watch_ui(on);
                }
            }
            SpatialMsg::Render(generation, status) => {
                self.on_render(generation, status).await;
            }
            SpatialMsg::Shutdown(_) => unreachable!("handled by run"),
        }
        self.reconcile().await;
    }

    /// Head tracking for this session, when the model has sensors.
    async fn ensure_sensors(&mut self) {
        if !self.session {
            return;
        }
        let (capable, _) = self.capable();
        let product = self.ctx.state().model().product;
        match &mut self.ht {
            Some(ht) => ht.set_product(product),
            None if capable => {
                let follow = self.ctx.state().follow_screen_on;
                let config = MotionConfig {
                    tracker: TrackerConfig {
                        follow_screen: follow,
                        ..TrackerConfig::default()
                    },
                    ..MotionConfig::default()
                };
                let mut ht = HeadTracking::new(product, config);
                ht.watch_ui(self.watched);
                let running = self.sink.borrow().running;
                let now = self.now();
                let events = ht.set_audio(running, now);
                self.ht = Some(ht);
                self.process(events).await;
            }
            None => {}
        }
        self.publish_head().await;
    }

    /// The link is gone: nothing can be sent, the sensors start over.
    async fn drop_sensors(&mut self) {
        if let Some(mut ht) = self.ht.take() {
            let events = ht.link_lost();
            self.process(events).await;
        }
        self.publish_head().await;
    }

    async fn on_sink(&mut self) {
        let s = *self.sink.borrow_and_update();
        if let Hold::Failed { appearances } | Hold::NodeGone { appearances } = self.hold
            && s.present
            && s.appearances != appearances
        {
            self.hold = Hold::Clear;
        }
        let now = self.now();
        if let Some(ht) = &mut self.ht {
            let events = ht.set_audio(s.running, now);
            self.process(events).await;
        }
        self.reconcile().await;
    }

    async fn on_tick(&mut self) {
        let now = self.now();
        if let Some(ht) = &mut self.ht {
            let events = ht.tick(now);
            self.process(events).await;
        }
    }

    async fn on_render(&mut self, generation: u64, status: RenderStatus) {
        let Some(r) = self.render.as_mut().filter(|r| r.generation == generation) else {
            return;
        };
        r.status = status.clone();
        let RenderStatus::Stopped(reason) = status else {
            return;
        };
        let appearances = self.sink.borrow().appearances;
        self.hold = match &reason {
            StopReason::Requested => Hold::Clear,
            StopReason::UserSwitched(to) => {
                info!(output = %to, "another output was chosen; spatial audio off");
                Hold::UserSwitched
            }
            StopReason::TargetGone => {
                info!("the AirPods' audio node went away; spatial audio waits for it");
                Hold::NodeGone { appearances }
            }
            StopReason::BackendFailed(e) => {
                warn!(error = %e, "spatial audio failed");
                Hold::Failed { appearances }
            }
        };
        self.stop_render().await;
    }

    /// Brings the renderer and the sensors in line with the mode, the
    /// connection and the node, then publishes the properties.
    async fn reconcile(&mut self) {
        let (connected, mode) = {
            let s = self.ctx.state();
            (s.connected, s.spatial_mode)
        };
        let (capable, _) = self.capable();
        let want = match mode {
            _ if !connected => None,
            SpatialMode::Off => None,
            SpatialMode::Fixed => Some(RenderMode::Fixed),
            SpatialMode::Head if capable => Some(RenderMode::HeadTracked),
            SpatialMode::Head => None,
        };
        if self.render.as_ref().is_some_and(|r| Some(r.mode) != want) {
            self.stop_render().await;
        }
        if want.is_none() {
            self.hold = Hold::Clear;
        }
        if let Some(mode) = want
            && self.render.is_none()
            && self.hold == Hold::Clear
            && self.sink.borrow().present
        {
            self.start_render(mode).await;
        }
        self.publish_status().await;
        self.update_sensors().await;
        self.publish_head().await;
    }

    async fn start_render(&mut self, mode: RenderMode) {
        self.generation += 1;
        let generation = self.generation;
        self.set_status(SpatialStatus::Starting).await;
        let (address, name) = {
            let s = self.ctx.state();
            (s.address.clone(), s.name())
        };
        let tx = self.tx.clone();
        let on_status: StatusSink = Box::new(move |s| {
            let _ = tx.send(SpatialMsg::Render(generation, s));
        });
        let renderer = self.renderer.clone();
        info!(mode = ?mode, "starting spatial audio");
        let started =
            tokio::task::spawn_blocking(move || renderer.start(mode, &address, &name, on_status))
                .await
                .unwrap_or_else(|e| Err(format!("renderer thread: {e}")));
        match started {
            Ok(handle) => {
                self.render = Some(Running {
                    generation,
                    mode,
                    handle,
                    status: RenderStatus::Starting,
                });
            }
            Err(e) => {
                warn!(error = %e, "spatial audio did not start");
                self.hold = Hold::Failed {
                    appearances: self.sink.borrow().appearances,
                };
            }
        }
    }

    async fn stop_render(&mut self) {
        let Some(r) = self.render.take() else {
            return;
        };
        info!(mode = ?r.mode, "stopping spatial audio");
        let handle = r.handle;
        let _ = tokio::task::spawn_blocking(move || handle.stop()).await;
    }

    fn status(&self) -> SpatialStatus {
        let (connected, mode) = {
            let s = self.ctx.state();
            (s.connected, s.spatial_mode)
        };
        let (capable, known) = self.capable();
        if !connected || mode == SpatialMode::Off {
            return SpatialStatus::Off;
        }
        if mode == SpatialMode::Head && !capable {
            // The model may still be unknown (no modalias, no 0x1D yet).
            return if known {
                SpatialStatus::Failed
            } else {
                SpatialStatus::Starting
            };
        }
        match (&self.render, self.hold) {
            (Some(r), _) => match r.status {
                RenderStatus::Starting | RenderStatus::Stopped(_) => SpatialStatus::Starting,
                RenderStatus::Active => SpatialStatus::Active,
                RenderStatus::Paused(_) => SpatialStatus::Paused,
            },
            (None, Hold::Failed { .. }) => SpatialStatus::Failed,
            (None, Hold::UserSwitched) => SpatialStatus::Off,
            (None, Hold::Clear | Hold::NodeGone { .. }) => SpatialStatus::Starting,
        }
    }

    async fn publish_status(&mut self) {
        let status = self.status();
        self.set_status(status).await;
    }

    async fn set_status(&self, status: SpatialStatus) {
        let changed = {
            let mut s = self.ctx.state();
            std::mem::replace(&mut s.spatial_status, status) != status
        };
        if changed {
            debug!(status = status.as_str(), "spatial status");
            api::emit_props(&self.iface, &[Prop::SpatialStatus]).await;
        }
    }

    /// Sensors on only when they serve: Head mode rendering, a bud worn.
    async fn update_sensors(&mut self) {
        let wanted = {
            let s = self.ctx.state();
            let (l, r) = s.wearing();
            // Unknown on both sides (ear detection off, nothing reported
            // yet): do not hold the sensors back.
            let worn = l == Wearing::Ear
                || r == Wearing::Ear
                || (l == Wearing::Unknown && r == Wearing::Unknown);
            s.connected && s.spatial_mode == SpatialMode::Head && worn
        } && self
            .render
            .as_ref()
            .is_some_and(|r| r.mode == RenderMode::HeadTracked && r.status == RenderStatus::Active);
        let now = self.now();
        if let Some(ht) = &mut self.ht {
            let events = ht.set_enabled(wanted, now);
            self.process(events).await;
        }
    }

    async fn process(&mut self, events: Vec<Event>) {
        for e in events {
            match e {
                Event::Send(frame) => self.ctx.post(frame),
                Event::State(state) => {
                    debug!(?state, "head sensors");
                    self.publish_head().await;
                }
                Event::RenderPose(pose) => {
                    if let Some(r) = &self.render {
                        r.handle.set_yaw(pose.yaw_deg as f32);
                    }
                }
                Event::UiPose(pose) => {
                    // The contract's pitch: the tilt of the head, unsigned
                    // (pods-motion cannot tell a nod from a roll yet).
                    if let Err(err) = DeviceIface::head_pose(
                        self.iface.signal_emitter(),
                        pose.yaw_deg,
                        pose.tilt_deg,
                    )
                    .await
                    {
                        debug!(error = %err, "HeadPose not sent");
                    }
                }
            }
        }
    }

    fn head_state(&self) -> HeadState {
        match &self.ht {
            Some(ht) if self.session => match ht.state() {
                SensorState::Off => HeadState::Idle,
                SensorState::Waiting(_) => HeadState::Starting,
                SensorState::Streaming(_) => HeadState::Active,
            },
            _ => HeadState::Unavailable,
        }
    }

    async fn publish_head(&self) {
        let head = self.head_state();
        let changed = {
            let mut s = self.ctx.state();
            std::mem::replace(&mut s.head_tracking, head) != head
        };
        if changed {
            info!(head_tracking = head.as_str(), "head tracking");
            api::emit_props(&self.iface, &[Prop::HeadTracking]).await;
        }
    }

    /// Daemon stopping or AirPods forgotten: sensors stopped on the buds
    /// (interval 0, written before the answer), renderer torn down.
    async fn shutdown(&mut self) {
        if let Some(mut ht) = self.ht.take() {
            for e in ht.stop() {
                if let Event::Send(frame) = e {
                    let _ = tokio::time::timeout(STOP_WRITE, self.ctx.send(frame)).await;
                }
            }
        }
        self.stop_render().await;
        let mut s = self.ctx.state();
        s.spatial_status = SpatialStatus::Off;
        s.head_tracking = HeadState::Unavailable;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        for m in [SpatialMode::Off, SpatialMode::Fixed, SpatialMode::Head] {
            assert_eq!(SpatialMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(SpatialMode::parse("Head"), None);
        assert_eq!(SpatialStatus::default().as_str(), "off");
        assert_eq!(HeadState::default().as_str(), "unavailable");
    }

    #[test]
    fn node_names() {
        assert_eq!(
            address_of_node("bluez_output.AA_BB_CC_DD_EE_FF.1").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(address_of_node("bluez_output.AA_BB_CC_DD_EE_FF.0"), None);
        assert_eq!(address_of_node("bluez_input.AA_BB_CC_DD_EE_FF.0"), None);
        assert_eq!(address_of_node("bluez_output.AA_BB.1"), None);
    }
}
