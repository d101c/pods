// SPDX-License-Identifier: GPL-3.0-or-later
//! The session bus API, `io.github.d101c.Pods1`, exactly as written in
//! `docs/io.github.d101c.Pods1.xml`. A test compares the introspection of
//! these objects with that file.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, warn};
use zbus::message::Header;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::wire::{ObjectPath, OwnedObjectPath, Variant};
use zbus::{DBusError, interface};

use pods_bt::Bluez;
use pods_proto::model::arrange;
use pods_proto::{Capability, NoiseMode, packet};

use crate::config::ConfigStore;
use crate::spatial::{SpatialMode, SpatialMsg};
use crate::state::{DeviceState, Prop, Source};

pub const BUS_NAME: &str = "io.github.d101c.Pods";
pub const MANAGER_PATH: &str = "/io/github/d101c/Pods";
pub const DEVICES_PATH: &str = "/io/github/d101c/Pods/devices";

/// Object path of the device with `address`.
pub fn device_path(address: &str) -> String {
    format!("{DEVICES_PATH}/{}", address.replace(':', "_"))
}

#[derive(Debug, DBusError)]
#[zbus(prefix = "io.github.d101c.Pods1.Error")]
pub enum PodsError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotConnected(String),
    NotSupported(String),
    InvalidArgument(String),
    Failed(String),
}

// ---------------------------------------------------------------- Manager

#[derive(Debug, Default)]
pub struct ManagerCtx {
    pub devices: Mutex<Vec<OwnedObjectPath>>,
    pub active: Mutex<Option<OwnedObjectPath>>,
    pub conflict: Mutex<String>,
}

pub struct ManagerIface {
    pub ctx: Arc<ManagerCtx>,
}

#[interface(name = "io.github.d101c.Pods1.Manager")]
impl ManagerIface {
    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        self.ctx.devices.lock().expect("lock").clone()
    }

    #[zbus(property)]
    fn active_device(&self) -> OwnedObjectPath {
        self.ctx
            .active
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_else(|| OwnedObjectPath::try_from("/").expect("root path"))
    }

    #[zbus(property)]
    fn engine_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    #[zbus(property)]
    fn conflict(&self) -> String {
        self.ctx.conflict.lock().expect("lock").clone()
    }

    /// M2.
    #[zbus(signal)]
    pub async fn case_opened(
        emitter: &SignalEmitter<'_>,
        device: ObjectPath<'_>,
        snapshot: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<()>;

    /// M2.
    #[zbus(signal)]
    pub async fn case_closed(
        emitter: &SignalEmitter<'_>,
        device: ObjectPath<'_>,
    ) -> zbus::Result<()>;
}

// ----------------------------------------------------------------- Device

/// An order for the link task.
#[derive(Debug)]
pub enum Command {
    /// Send this packet on the AAP channel.
    Send {
        packet: Vec<u8>,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// Everything a device object needs; shared with its link task.
pub struct DeviceCtx {
    pub path: OwnedObjectPath,
    pub state: Mutex<DeviceState>,
    pub commands: mpsc::UnboundedSender<Command>,
    pub config: Arc<ConfigStore>,
    pub bluez: Bluez,
    /// The spatial task of this device.
    pub spatial: mpsc::UnboundedSender<SpatialMsg>,
    /// Unique bus names of the clients watching `HeadPose`.
    pub pose_watchers: Mutex<BTreeSet<String>>,
}

impl DeviceCtx {
    pub fn state(&self) -> std::sync::MutexGuard<'_, DeviceState> {
        self.state.lock().expect("device state lock")
    }

    pub fn address(&self) -> String {
        self.state().address.clone()
    }

    /// Sends `packet` on the open AAP link.
    pub async fn send(&self, packet: Vec<u8>) -> Result<(), PodsError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Send { packet, reply })
            .map_err(|_| PodsError::NotConnected("the link is closed".into()))?;
        match answer.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(PodsError::NotConnected(e)),
            Err(_) => Err(PodsError::NotConnected("the link closed".into())),
        }
    }

    /// Queues `packet` for the open AAP link without waiting: for the
    /// spatial task, which must never wait on the link task.
    pub fn post(&self, packet: Vec<u8>) {
        let (reply, _) = oneshot::channel();
        let _ = self.commands.send(Command::Send { packet, reply });
    }

    /// Tells the spatial task.
    pub fn spatial(&self, msg: SpatialMsg) {
        let _ = self.spatial.send(msg);
    }

    /// `client` starts or stops watching `HeadPose`. The spatial task hears
    /// only when the first one comes or the last one goes.
    pub fn watch_pose(&self, client: &str, on: bool) {
        let edge = {
            let mut w = self.pose_watchers.lock().expect("watchers lock");
            let before = w.is_empty();
            if on {
                w.insert(client.to_string());
            } else {
                w.remove(client);
            }
            (before != w.is_empty()).then_some(!w.is_empty())
        };
        if let Some(watched) = edge {
            debug!(watched, "HeadPose watchers");
            self.spatial(SpatialMsg::Watch(watched));
        }
    }

    fn require_connected(&self) -> Result<(), PodsError> {
        if self.state().connected {
            Ok(())
        } else {
            Err(PodsError::NotConnected(
                "the AirPods are not connected".into(),
            ))
        }
    }

    fn require(&self, cap: Capability) -> Result<(), PodsError> {
        if self.state().model().capabilities().contains(&cap) {
            Ok(())
        } else {
            Err(PodsError::NotSupported(format!(
                "this model has no {}",
                cap.as_str()
            )))
        }
    }

    async fn set_mode(&self, mode: NoiseMode) -> Result<(), PodsError> {
        if !self.state().model().noise_modes.contains(&mode) {
            return Err(PodsError::NotSupported(format!(
                "this model has no {} mode",
                mode.as_str()
            )));
        }
        self.require_connected()?;
        self.state().expect_mode(mode, Instant::now());
        self.send(packet::set_noise_mode(mode)).await
    }
}

pub struct DeviceIface {
    pub ctx: Arc<DeviceCtx>,
}

#[interface(name = "io.github.d101c.Pods1.Device")]
impl DeviceIface {
    #[zbus(property)]
    fn address(&self) -> String {
        self.ctx.state().address.clone()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.ctx.state().name()
    }

    #[zbus(property)]
    fn model(&self) -> String {
        self.ctx.state().model().product_string()
    }

    #[zbus(property)]
    fn model_name(&self) -> String {
        self.ctx.state().model().name.to_string()
    }

    #[zbus(property)]
    fn firmware(&self) -> String {
        self.ctx.state().firmware()
    }

    /// From the BLE advertisement.
    #[zbus(property)]
    fn color(&self) -> String {
        self.ctx.state().color.clone()
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        self.ctx.state().connected
    }

    #[zbus(property)]
    fn capabilities(&self) -> Vec<String> {
        self.ctx
            .state()
            .model()
            .capabilities()
            .iter()
            .map(|c| c.as_str().to_string())
            .collect()
    }

    #[zbus(property)]
    fn battery(&self) -> HashMap<String, (u8, bool)> {
        self.ctx.state().battery_map()
    }

    #[zbus(property)]
    fn wearing(&self) -> (String, String) {
        let (l, r) = self.ctx.state().wearing();
        (l.as_str().to_string(), r.as_str().to_string())
    }

    #[zbus(property)]
    fn noise_mode(&self) -> String {
        self.ctx.state().noise_mode_str().to_string()
    }

    #[zbus(property)]
    fn noise_modes(&self) -> Vec<String> {
        self.ctx
            .state()
            .model()
            .noise_modes
            .iter()
            .map(|m| m.as_str().to_string())
            .collect()
    }

    #[zbus(property)]
    fn press_hold_modes(&self) -> Vec<String> {
        self.ctx
            .state()
            .press_hold_modes()
            .iter()
            .map(|m| m.as_str().to_string())
            .collect()
    }

    #[zbus(property)]
    fn ear_detection(&self) -> bool {
        self.ctx.state().ear_detection()
    }

    #[zbus(property)]
    fn pause_on_remove(&self) -> bool {
        self.ctx.state().pause_on_remove
    }

    #[zbus(property)]
    fn resume_on_return(&self) -> bool {
        self.ctx.state().resume_on_return
    }

    #[zbus(property)]
    fn media_paused(&self) -> bool {
        self.ctx.state().media_paused
    }

    #[zbus(property)]
    fn conversation_awareness(&self) -> bool {
        self.ctx.state().conversation_awareness()
    }

    #[zbus(property)]
    fn spatial_mode(&self) -> String {
        self.ctx.state().spatial_mode.as_str().to_string()
    }

    #[zbus(property)]
    fn spatial_status(&self) -> String {
        self.ctx.state().spatial_status.as_str().to_string()
    }

    #[zbus(property)]
    fn follow_screen(&self) -> bool {
        self.ctx.state().follow_screen()
    }

    #[zbus(property)]
    fn head_tracking(&self) -> String {
        self.ctx.state().head_tracking.as_str().to_string()
    }

    async fn set_noise_mode(&self, mode: &str) -> Result<(), PodsError> {
        let m = NoiseMode::parse(mode)
            .ok_or_else(|| PodsError::InvalidArgument(format!("unknown noise mode '{mode}'")))?;
        self.ctx.set_mode(m).await
    }

    async fn cycle_noise_mode(&self) -> Result<(), PodsError> {
        let next =
            self.ctx.state().next_mode().ok_or_else(|| {
                PodsError::NotSupported("this model has no listening modes".into())
            })?;
        self.ctx.set_mode(next).await
    }

    async fn set_press_hold_modes(
        &self,
        modes: Vec<String>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), PodsError> {
        self.ctx.require(Capability::PressHold)?;
        let supported = self.ctx.state().model().noise_modes;
        let mut chosen = Vec::new();
        for word in &modes {
            let m = NoiseMode::parse(word).ok_or_else(|| {
                PodsError::InvalidArgument(format!("unknown noise mode '{word}'"))
            })?;
            if !supported.contains(&m) {
                return Err(PodsError::InvalidArgument(format!(
                    "this model has no {word} mode"
                )));
            }
            if chosen.contains(&m) {
                return Err(PodsError::InvalidArgument(format!("{word} given twice")));
            }
            chosen.push(m);
        }
        if chosen.len() < 2 {
            return Err(PodsError::InvalidArgument(
                "the press-and-hold needs at least two modes".into(),
            ));
        }
        // The buds cycle in their own order; show that one.
        let ordered = arrange(&chosen, &self.ctx.state().press_hold_modes());
        let address = self.ctx.address();
        let words: Vec<String> = ordered.iter().map(|m| m.as_str().to_string()).collect();
        if let Err(e) = self.ctx.config.update_device(&address, |d| {
            d.press_hold_modes = Some(words.clone());
            d.press_hold_cycle = Some(words);
        }) {
            warn!(error = %e, "could not save the press-and-hold modes");
        }
        let (changed, connected) = {
            let mut s = self.ctx.state();
            let before = s.press_hold_modes();
            s.press_hold = Some(ordered.clone());
            (before != ordered, s.connected)
        };
        if changed {
            let _ = self.press_hold_modes_changed(&emitter).await;
        }
        // Stored either way; rewritten at every connection.
        if connected {
            self.ctx
                .send(packet::set_press_hold_modes(&ordered))
                .await
                .map_err(|e| PodsError::Failed(format!("saved, not applied: {e}")))?;
        }
        Ok(())
    }

    async fn rename(
        &self,
        name: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), PodsError> {
        let name = name.trim();
        let frame = packet::rename(name).ok_or_else(|| {
            PodsError::InvalidArgument(format!(
                "a name is 1 to {} bytes long",
                packet::MAX_NAME_BYTES
            ))
        })?;
        self.ctx.require(Capability::Rename)?;
        self.ctx.require_connected()?;
        self.ctx.send(frame).await?;
        let bluez_path = self.ctx.state().bluez_path.clone();
        let alias_result = self.ctx.bluez.set_alias(&bluez_path, name).await;
        let changed = {
            let mut s = self.ctx.state();
            let before = s.name();
            s.info_name = Some(name.to_string());
            if alias_result.is_ok() {
                s.alias = name.to_string();
            }
            s.name() != before
        };
        if changed {
            let _ = self.name_changed(&emitter).await;
        }
        alias_result
            .map_err(|e| PodsError::Failed(format!("renamed on the AirPods, not in BlueZ: {e}")))
    }

    async fn set_ear_detection(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), PodsError> {
        self.ctx.require(Capability::EarDetection)?;
        self.ctx.require_connected()?;
        self.ctx.send(packet::set_ear_detection(enabled)).await?;
        let address = self.ctx.address();
        if let Err(e) = self
            .ctx
            .config
            .update_device(&address, |d| d.ear_detection = Some(enabled))
        {
            warn!(error = %e, "could not save ear detection");
        }
        if self.ctx.state().set_ear_detection(enabled) {
            let _ = self.ear_detection_changed(&emitter).await;
        }
        Ok(())
    }

    async fn set_pause_on_remove(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), PodsError> {
        let address = self.ctx.address();
        self.ctx
            .config
            .update_device(&address, |d| d.pause_on_remove = Some(enabled))
            .map_err(|e| PodsError::Failed(format!("could not save: {e}")))?;
        let changed = {
            let mut s = self.ctx.state();
            std::mem::replace(&mut s.pause_on_remove, enabled) != enabled
        };
        if changed {
            let _ = self.pause_on_remove_changed(&emitter).await;
        }
        Ok(())
    }

    async fn set_resume_on_return(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), PodsError> {
        let address = self.ctx.address();
        self.ctx
            .config
            .update_device(&address, |d| d.resume_on_return = Some(enabled))
            .map_err(|e| PodsError::Failed(format!("could not save: {e}")))?;
        let changed = {
            let mut s = self.ctx.state();
            std::mem::replace(&mut s.resume_on_return, enabled) != enabled
        };
        if changed {
            let _ = self.resume_on_return_changed(&emitter).await;
        }
        Ok(())
    }

    async fn set_conversation_awareness(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), PodsError> {
        self.ctx.require(Capability::Conversation)?;
        self.ctx.require_connected()?;
        self.ctx
            .send(packet::set_conversation_awareness(enabled))
            .await?;
        let changed = {
            let mut s = self.ctx.state();
            std::mem::replace(&mut s.conversation, enabled) != enabled
        };
        if changed {
            let _ = self.conversation_awareness_changed(&emitter).await;
        }
        Ok(())
    }

    /// The Connect button of the case card: BlueZ connects the buds, then
    /// the engine opens its AAP link as for any connection.
    async fn connect(&self) -> Result<(), PodsError> {
        if self.ctx.state().connected {
            return Ok(());
        }
        let path = self.ctx.state().bluez_path.clone();
        debug!(path, "connect requested");
        self.ctx
            .bluez
            .connect(&path)
            .await
            .map_err(|e| PodsError::Failed(e.to_string()))
    }

    async fn disconnect(&self) -> Result<(), PodsError> {
        let path = self.ctx.state().bluez_path.clone();
        debug!(path, "disconnect requested");
        self.ctx
            .bluez
            .disconnect(&path)
            .await
            .map_err(|e| PodsError::Failed(e.to_string()))
    }

    /// Starts, switches or stops the spatial output; kept per device and
    /// applied again at every connection. Returns at once: `SpatialStatus`
    /// tells how it goes.
    async fn set_spatial_mode(
        &self,
        mode: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), PodsError> {
        let m = SpatialMode::parse(mode)
            .ok_or_else(|| PodsError::InvalidArgument(format!("unknown spatial mode '{mode}'")))?;
        if m == SpatialMode::Head {
            self.ctx.require(Capability::SpatialHead)?;
        }
        self.ctx.require_connected()?;
        let address = self.ctx.address();
        if let Err(e) = self
            .ctx
            .config
            .update_device(&address, |d| d.spatial_mode = Some(m.as_str().to_string()))
        {
            warn!(error = %e, "could not save the spatial mode");
        }
        let changed = {
            let mut s = self.ctx.state();
            std::mem::replace(&mut s.spatial_mode, m) != m
        };
        if changed {
            let _ = self.spatial_mode_changed(&emitter).await;
        }
        self.ctx.spatial(SpatialMsg::Mode);
        Ok(())
    }

    async fn set_follow_screen(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), PodsError> {
        self.ctx.require(Capability::SpatialHead)?;
        let address = self.ctx.address();
        self.ctx
            .config
            .update_device(&address, |d| d.follow_screen = Some(enabled))
            .map_err(|e| PodsError::Failed(format!("could not save: {e}")))?;
        let changed = {
            let mut s = self.ctx.state();
            std::mem::replace(&mut s.follow_screen_on, enabled) != enabled
        };
        if changed {
            let _ = self.follow_screen_changed(&emitter).await;
        }
        self.ctx.spatial(SpatialMsg::FollowScreen(enabled));
        Ok(())
    }

    /// The current direction of the head becomes straight ahead.
    async fn recenter(&self) -> Result<(), PodsError> {
        self.ctx.require(Capability::SpatialHead)?;
        self.ctx.require_connected()?;
        self.ctx.spatial(SpatialMsg::Recenter);
        Ok(())
    }

    /// Per client (unique bus name): a client that leaves the bus stops
    /// watching.
    async fn watch_head_pose(
        &self,
        watch: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> Result<(), PodsError> {
        let client = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| PodsError::Failed("no sender".into()))?;
        if !watch {
            self.ctx.watch_pose(&client, false);
            return Ok(());
        }
        self.ctx.require(Capability::SpatialHead)?;
        self.ctx.watch_pose(&client, true);
        // It may have left before we counted it: its NameOwnerChanged is gone.
        let present = match zbus::fdo::DBusProxy::new(connection).await {
            Ok(dbus) => match zbus::names::BusName::try_from(client.as_str()) {
                Ok(name) => dbus.name_has_owner(name).await.unwrap_or(true),
                Err(_) => true,
            },
            Err(_) => true,
        };
        if !present {
            self.ctx.watch_pose(&client, false);
        }
        Ok(())
    }

    /// Named apart in Rust: `noise_mode_changed` is the property helper.
    #[zbus(signal, name = "NoiseModeChanged")]
    pub async fn noise_mode_switched(
        emitter: &SignalEmitter<'_>,
        mode: &str,
        source: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn low_battery(
        emitter: &SignalEmitter<'_>,
        part: &str,
        percent: u8,
    ) -> zbus::Result<()>;

    /// At most 30 Hz, only while watched. `pitch` is the tilt of the head
    /// away from upright, unsigned (a nod and a roll look the same for now).
    #[zbus(signal)]
    pub async fn head_pose(emitter: &SignalEmitter<'_>, yaw: f64, pitch: f64) -> zbus::Result<()>;
}

/// Sends `PropertiesChanged` for each of `props`.
pub async fn emit_props(iface: &InterfaceRef<DeviceIface>, props: &[Prop]) {
    if props.is_empty() {
        return;
    }
    let dev = iface.get().await;
    let e = iface.signal_emitter();
    for p in props {
        let r = match p {
            Prop::Name => dev.name_changed(e).await,
            Prop::Model => dev.model_changed(e).await,
            Prop::ModelName => dev.model_name_changed(e).await,
            Prop::Firmware => dev.firmware_changed(e).await,
            Prop::Connected => dev.connected_changed(e).await,
            Prop::Capabilities => dev.capabilities_changed(e).await,
            Prop::Battery => dev.battery_changed(e).await,
            Prop::Wearing => dev.wearing_changed(e).await,
            Prop::NoiseMode => dev.noise_mode_changed(e).await,
            Prop::NoiseModes => dev.noise_modes_changed(e).await,
            Prop::PressHoldModes => dev.press_hold_modes_changed(e).await,
            Prop::EarDetection => dev.ear_detection_changed(e).await,
            Prop::PauseOnRemove => dev.pause_on_remove_changed(e).await,
            Prop::ResumeOnReturn => dev.resume_on_return_changed(e).await,
            Prop::Color => dev.color_changed(e).await,
            Prop::MediaPaused => dev.media_paused_changed(e).await,
            Prop::ConversationAwareness => dev.conversation_awareness_changed(e).await,
            Prop::SpatialMode => dev.spatial_mode_changed(e).await,
            Prop::SpatialStatus => dev.spatial_status_changed(e).await,
            Prop::FollowScreen => dev.follow_screen_changed(e).await,
            Prop::HeadTracking => dev.head_tracking_changed(e).await,
        };
        if let Err(err) = r {
            debug!(?p, error = %err, "PropertiesChanged not sent");
        }
    }
}

pub async fn emit_noise_changed(
    iface: &InterfaceRef<DeviceIface>,
    mode: NoiseMode,
    source: Source,
) {
    if let Err(e) =
        DeviceIface::noise_mode_switched(iface.signal_emitter(), mode.as_str(), source.as_str())
            .await
    {
        debug!(error = %e, "NoiseModeChanged not sent");
    }
}

pub async fn emit_low_battery(iface: &InterfaceRef<DeviceIface>, part: &str, percent: u8) {
    if let Err(e) = DeviceIface::low_battery(iface.signal_emitter(), part, percent).await {
        debug!(error = %e, "LowBattery not sent");
    }
}

/// Sends `PropertiesChanged` for the manager's `name` property.
pub async fn emit_manager(iface: &InterfaceRef<ManagerIface>, prop: &str) {
    let m = iface.get().await;
    let e = iface.signal_emitter();
    let r = match prop {
        "Devices" => m.devices_changed(e).await,
        "ActiveDevice" => m.active_device_changed(e).await,
        "Conflict" => m.conflict_changed(e).await,
        _ => Ok(()),
    };
    if let Err(err) = r {
        debug!(prop, error = %err, "PropertiesChanged not sent");
    }
}
