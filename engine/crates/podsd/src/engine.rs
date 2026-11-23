// SPDX-License-Identifier: GPL-3.0-or-later
//! Wiring: BlueZ events in, device objects and their tasks out.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::object_server::InterfaceRef;
use zbus::wire::OwnedObjectPath;

use pods_bt::{AapConnector, BatteryProvider, Bluez, BluezDevice, BluezEvent};
use pods_proto::model::{CycleLearner, arrange, canonical};
use pods_proto::{NoiseMode, product_from_modalias};

use crate::api::{self, DeviceCtx, DeviceIface, ManagerCtx, ManagerIface};
use crate::config::ConfigStore;
use crate::keys::{KeyRing, KeyStore, SecretService};
use crate::link::{DeviceTask, Hooks, Timing};
use crate::media::{Media, MediaHost, RESUME_WINDOW, SystemHost};
use crate::spatial::{AudioMonitor, Renderer, SpatialMode, SpatialMsg, SpatialTask};
use crate::state::{DeviceState, Prop};

/// Wait before asking BlueZ again when it is not there.
const BLUEZ_RETRY: Duration = Duration::from_secs(5);

/// What the engine runs on. Production: the session and system buses and
/// the real L2CAP socket. Tests: a private bus, a fake BlueZ, fake buds.
pub struct EngineDeps {
    /// Where the API is served and MPRIS players live.
    pub session: Connection,
    /// Where BlueZ lives.
    pub system: Connection,
    pub connector: Arc<dyn AapConnector>,
    /// `None`: MPRIS on `session`, routing from BlueZ.
    pub media_host: Option<Arc<dyn MediaHost>>,
    /// `None`: the Secret Service on `session`.
    pub keys: Option<Arc<dyn KeyStore>>,
    pub config: Arc<ConfigStore>,
    pub timing: Timing,
    /// `Some("/proc")` to look for other AAP clients.
    pub proc_root: Option<PathBuf>,
    /// Ask for `io.github.d101c.Pods` on `session`.
    pub request_name: bool,
    /// Spatial output. Production: [`crate::PipeWireRenderer`].
    pub renderer: Arc<dyn Renderer>,
    /// The AirPods' audio nodes. Production: [`crate::PipeWireSinks`].
    pub sinks: Arc<dyn AudioMonitor>,
}

pub(crate) struct DeviceEntry {
    pub(crate) ctx: Arc<DeviceCtx>,
    bluez_tx: watch::Sender<bool>,
}

pub struct Engine {
    pub(crate) session: Connection,
    connector: Arc<dyn AapConnector>,
    pub(crate) config: Arc<ConfigStore>,
    timing: Timing,
    bluez: Bluez,
    pub(crate) hooks: Arc<Hooks>,
    pub(crate) devices: tokio::sync::Mutex<HashMap<String, DeviceEntry>>,
    renderer: Arc<dyn Renderer>,
    sinks: Arc<dyn AudioMonitor>,
}

impl Engine {
    /// Serves the API and starts following BlueZ.
    pub async fn start(deps: EngineDeps) -> zbus::Result<Arc<Engine>> {
        let manager_ctx = Arc::new(ManagerCtx::default());
        let server = deps.session.object_server();
        server
            .at(
                api::MANAGER_PATH,
                ManagerIface {
                    ctx: manager_ctx.clone(),
                },
            )
            .await?;
        let manager: InterfaceRef<ManagerIface> = server.interface(api::MANAGER_PATH).await?;

        let bluez = Bluez::new(deps.system.clone());
        let battery = match BatteryProvider::new(deps.system.clone()).await {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                warn!(error = %e, "no battery provider");
                None
            }
        };
        let host = deps
            .media_host
            .unwrap_or_else(|| Arc::new(SystemHost::new(deps.session.clone(), bluez.clone())));
        let store = deps
            .keys
            .unwrap_or_else(|| Arc::new(SecretService::new(deps.session.clone())));
        let hooks = Arc::new(Hooks {
            manager,
            manager_ctx,
            battery,
            media: Arc::new(Media::new(host)),
            proc_root: deps.proc_root,
            keys: Arc::new(KeyRing::new(store)),
            case_refresh: Notify::new(),
        });

        if deps.request_name {
            deps.session.request_name(api::BUS_NAME).await?;
            info!(name = api::BUS_NAME, "serving");
        }

        let engine = Arc::new(Engine {
            session: deps.session,
            connector: deps.connector,
            config: deps.config,
            timing: deps.timing,
            bluez,
            hooks,
            devices: tokio::sync::Mutex::new(HashMap::new()),
            renderer: deps.renderer,
            sinks: deps.sinks,
        });
        engine.hooks_check_conflict().await;
        tokio::spawn(engine.clone().follow_clients());
        tokio::spawn(engine.clone().follow_bluez());
        tokio::spawn(engine.clone().follow_media());
        tokio::spawn(engine.clone().follow_ble(deps.system));
        Ok(engine)
    }

    /// Stops spatial audio and head tracking everywhere: the sensors get
    /// their stop frame, the renderer gives the default output back. For the
    /// end of the daemon.
    pub async fn shutdown(&self) {
        let waits: Vec<oneshot::Receiver<()>> = {
            let devices = self.devices.lock().await;
            devices
                .values()
                .map(|e| {
                    let (tx, rx) = oneshot::channel();
                    e.ctx.spatial(SpatialMsg::Shutdown(Some(tx)));
                    rx
                })
                .collect()
        };
        for w in waits {
            let _ = w.await;
        }
    }

    /// A client that leaves the bus stops watching `HeadPose`.
    async fn follow_clients(self: Arc<Self>) {
        let dbus = match zbus::fdo::DBusProxy::new(&self.session).await {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "cannot follow the bus clients");
                return;
            }
        };
        let mut changes = match dbus.receive_name_owner_changed().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "cannot follow the bus clients");
                return;
            }
        };
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.new_owner().is_some() || !args.name().starts_with(':') {
                continue;
            }
            let gone = args.name().to_string();
            let devices = self.devices.lock().await;
            for e in devices.values() {
                if e.ctx
                    .pose_watchers
                    .lock()
                    .expect("watchers lock")
                    .contains(&gone)
                {
                    debug!("a HeadPose watcher left the bus");
                    e.ctx.watch_pose(&gone, false);
                }
            }
        }
    }

    async fn hooks_check_conflict(&self) {
        if let Some(root) = &self.hooks.proc_root
            && let Some(found) = crate::conflict::scan(root)
        {
            warn!(client = found, "another AAP client is running");
            *self.hooks.manager_ctx.conflict.lock().expect("lock") = found.to_string();
            api::emit_manager(&self.hooks.manager, "Conflict").await;
        }
    }

    async fn follow_bluez(self: Arc<Self>) {
        loop {
            match self.bluez.watch().await {
                Ok(mut rx) => {
                    while let Some(ev) = rx.recv().await {
                        self.on_event(ev).await;
                    }
                    warn!("lost BlueZ; asking again");
                }
                Err(e) => warn!(error = %e, "BlueZ not available"),
            }
            tokio::time::sleep(BLUEZ_RETRY).await;
        }
    }

    /// `MediaPaused` follows the players the media logic holds, and lets
    /// go of them when the resume window is over.
    async fn follow_media(self: Arc<Self>) {
        let mut holding = self.hooks.media.holding();
        loop {
            let held = holding.borrow_and_update().clone();
            self.publish_media_paused(held.as_deref()).await;
            if held.is_some() {
                tokio::select! {
                    r = holding.changed() => if r.is_err() { return },
                    _ = tokio::time::sleep(RESUME_WINDOW + Duration::from_secs(3)) => {
                        self.hooks.media.expire().await;
                    }
                }
            } else if holding.changed().await.is_err() {
                return;
            }
        }
    }

    async fn publish_media_paused(&self, held: Option<&str>) {
        let devices = self.devices.lock().await;
        for entry in devices.values() {
            let changed = {
                let mut s = entry.ctx.state();
                let now = held == Some(s.bluez_path.as_str());
                std::mem::replace(&mut s.media_paused, now) != now
            };
            if changed {
                self.emit_device(&entry.ctx, &[Prop::MediaPaused]).await;
            }
        }
    }

    /// Sends `PropertiesChanged` for a device.
    pub(crate) async fn emit_device(&self, ctx: &DeviceCtx, props: &[Prop]) {
        if let Ok(iface) = self
            .session
            .object_server()
            .interface::<_, DeviceIface>(ctx.path.as_str())
            .await
        {
            api::emit_props(&iface, props).await;
        }
    }

    async fn on_event(&self, ev: BluezEvent) {
        match ev {
            BluezEvent::Changed(dev) if dev.paired || dev.connected => self.upsert(dev).await,
            BluezEvent::Changed(dev) | BluezEvent::Removed(dev) => self.remove(&dev.address).await,
        }
    }

    async fn upsert(&self, dev: BluezDevice) {
        let mut devices = self.devices.lock().await;
        if let Some(entry) = devices.get(&dev.address) {
            let props = {
                let mut s = entry.ctx.state();
                let mut props = s.set_alias(&dev.alias);
                props.extend(
                    s.set_modalias_product(dev.modalias.as_deref().and_then(product_from_modalias)),
                );
                s.bluez_path = dev.path.clone();
                props
            };
            if !props.is_empty() {
                self.emit_device(&entry.ctx, &props).await;
                self.hooks.case_refresh.notify_one();
            }
            if props.contains(&Prop::Model) {
                entry.ctx.spatial(SpatialMsg::Model);
            }
            entry
                .bluez_tx
                .send_if_modified(|c| std::mem::replace(c, dev.connected) != dev.connected);
            return;
        }

        let path = match OwnedObjectPath::try_from(api::device_path(&dev.address)) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "unusable address");
                return;
            }
        };
        let state = self.initial_state(&dev);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (spatial_tx, spatial_rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(DeviceCtx {
            path: path.clone(),
            state: Mutex::new(state),
            commands: cmd_tx,
            config: self.config.clone(),
            bluez: self.bluez.clone(),
            spatial: spatial_tx.clone(),
            pose_watchers: Mutex::new(Default::default()),
        });
        let server = self.session.object_server();
        if let Err(e) = server
            .at(path.as_str(), DeviceIface { ctx: ctx.clone() })
            .await
        {
            warn!(error = %e, "could not serve the device");
            return;
        }
        let iface = match server.interface::<_, DeviceIface>(path.as_str()).await {
            Ok(i) => i,
            Err(e) => {
                warn!(error = %e, "device object vanished");
                return;
            }
        };
        let spatial = SpatialTask::new(
            ctx.clone(),
            iface.clone(),
            self.renderer.clone(),
            (spatial_tx, spatial_rx),
            self.sinks.watch(&dev.address),
        );
        tokio::spawn(spatial.run());
        let (bluez_tx, bluez_rx) = watch::channel(dev.connected);
        let task = DeviceTask {
            ctx: ctx.clone(),
            iface,
            connector: self.connector.clone(),
            bluez_connected: bluez_rx,
            commands: cmd_rx,
            hooks: self.hooks.clone(),
            timing: self.timing,
        };
        tokio::spawn(task.run());
        // The keys from the keyring, so the case card works before the
        // first connection of this run.
        let keys = self.hooks.keys.clone();
        let address = dev.address.clone();
        tokio::spawn(async move { keys.load(&address).await });
        info!(model = %ctx.state().model().name, "AirPods found");
        devices.insert(dev.address.clone(), DeviceEntry { ctx, bluez_tx });
        self.publish_devices(&devices).await;
    }

    fn initial_state(&self, dev: &BluezDevice) -> DeviceState {
        let mut s = DeviceState::new(&dev.address, &dev.path);
        s.alias = dev.alias.clone();
        s.set_modalias_product(dev.modalias.as_deref().and_then(product_from_modalias));
        let config = self.config.get();
        s.low_battery_alerts = config.low_battery_alerts;
        let d = config
            .devices
            .get(&dev.address)
            .cloned()
            .unwrap_or_default();
        if let Some(on) = d.ear_detection {
            s.set_ear_detection(on);
        }
        s.pause_on_remove = d.pause_on_remove.unwrap_or(true);
        s.resume_on_return = d.resume_on_return.unwrap_or(true);
        let parse = |words: &[String]| -> Vec<NoiseMode> {
            words.iter().filter_map(|w| NoiseMode::parse(w)).collect()
        };
        let learned = d.press_hold_cycle.as_deref().map(parse).unwrap_or_default();
        // Only consistent stem presses may overturn the order kept.
        s.cycle = CycleLearner::new(Some(&learned));
        if let Some(words) = d.press_hold_modes {
            let modes = parse(&words);
            if modes.len() >= 2 {
                s.press_hold = Some(arrange(&modes, &learned));
            }
        } else if learned.len() >= 2 {
            s.press_hold = Some(canonical(&learned));
        }
        if let Some(color) = d.color {
            s.color = color;
        }
        s.spatial_mode = d
            .spatial_mode
            .as_deref()
            .and_then(SpatialMode::parse)
            .unwrap_or_default();
        s.follow_screen_on = d.follow_screen.unwrap_or(true);
        s
    }

    async fn remove(&self, address: &str) {
        let mut devices = self.devices.lock().await;
        let Some(entry) = devices.remove(address) else {
            return;
        };
        // Dropping the sender ends the device task.
        drop(entry.bluez_tx);
        entry.ctx.spatial(SpatialMsg::Shutdown(None));
        let _ = self
            .session
            .object_server()
            .remove::<DeviceIface, _>(entry.ctx.path.as_str())
            .await;
        info!("AirPods forgotten");
        self.publish_devices(&devices).await;
    }

    async fn publish_devices(&self, devices: &HashMap<String, DeviceEntry>) {
        let mut paths: Vec<OwnedObjectPath> =
            devices.values().map(|e| e.ctx.path.clone()).collect();
        paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        *self.hooks.manager_ctx.devices.lock().expect("lock") = paths;
        api::emit_manager(&self.hooks.manager, "Devices").await;
    }
}
