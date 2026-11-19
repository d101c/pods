// SPDX-License-Identifier: GPL-3.0-or-later
//! Test rig: a private D-Bus daemon, a fake BlueZ on it (with the
//! advertisement monitor manager), a fake Secret Service, fake AirPods
//! behind a fake connector, a fake spatial renderer and a fake PipeWire
//! node monitor. Nothing here touches the real buses, the user's keyring,
//! the Bluetooth adapter or PipeWire.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, watch};
use zbus::fdo::ObjectManager;
use zbus::object_server::SignalEmitter;
use zbus::wire::{OwnedObjectPath, OwnedVariant, Variant};
use zbus::{Connection, MatchRule, MessageStream, interface};

use pods_bt::{AapConnector, AapLink, BoxFuture};
use pods_proto::ProximityKeys;
use pods_proto::ble::{AdvertFields, build_advert};
use podsd::config::ConfigStore;
use podsd::spatial::{RenderMode, RenderStatus, StatusSink};
use podsd::{AudioMonitor, Engine, EngineDeps, Renderer, Rendering, SinkState, Timing};

pub const ADDRESS: &str = "AA:BB:CC:DD:EE:01";
pub const BLUEZ_DEVICE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01";
pub const DEVICE: &str = "/io/github/d101c/Pods/devices/AA_BB_CC_DD_EE_01";
pub const DEVICE_IFACE: &str = "io.github.d101c.Pods1.Device";
pub const MANAGER_IFACE: &str = "io.github.d101c.Pods1.Manager";
pub const MANAGER: &str = "/io/github/d101c/Pods";
pub const BUS_NAME: &str = "io.github.d101c.Pods";

// ------------------------------------------------------------ private bus

pub struct PrivateBus {
    pub address: String,
    child: tokio::process::Child,
    _dir: tempfile::TempDir,
}

impl PrivateBus {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let conf = dir.path().join("bus.conf");
        std::fs::write(
            &conf,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.path().display()
            ),
        )
        .expect("bus config");
        let mut child = tokio::process::Command::new("dbus-daemon")
            .arg(format!("--config-file={}", conf.display()))
            .arg("--nofork")
            .arg("--print-address")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("dbus-daemon is needed for these tests");
        let stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .await
            .expect("bus address");
        Self {
            address: line.trim().to_string(),
            child,
            _dir: dir,
        }
    }

    pub async fn connect(&self) -> Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .build()
            .await
            .expect("connect to the private bus")
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

// -------------------------------------------------------------- fake BlueZ

pub struct FakeDevice {
    pub address: String,
    pub alias: String,
    pub connected: bool,
    pub paired: bool,
    pub modalias: String,
    pub uuids: Vec<String>,
}

#[interface(name = "org.bluez.Device1")]
impl FakeDevice {
    #[zbus(property)]
    fn address(&self) -> String {
        self.address.clone()
    }
    #[zbus(property)]
    fn alias(&self) -> String {
        self.alias.clone()
    }
    #[zbus(property)]
    async fn set_alias(&mut self, alias: String) {
        self.alias = alias;
    }
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.connected
    }
    #[zbus(property)]
    fn paired(&self) -> bool {
        self.paired
    }
    #[zbus(property)]
    fn modalias(&self) -> String {
        self.modalias.clone()
    }
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> Vec<String> {
        self.uuids.clone()
    }
    async fn disconnect(&mut self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) {
        self.connected = false;
        let _ = self.connected_changed(&e).await;
    }
    async fn connect(&mut self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) {
        self.connected = true;
        let _ = self.connected_changed(&e).await;
    }
}

#[derive(Default)]
pub struct FakeAdapter {
    pub providers: Arc<Mutex<Vec<(String, String)>>>,
}

#[interface(name = "org.bluez.BatteryProviderManager1")]
impl FakeAdapter {
    fn register_battery_provider(
        &self,
        provider: OwnedObjectPath,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        self.providers
            .lock()
            .unwrap()
            .push((sender, provider.to_string()));
    }
    fn unregister_battery_provider(&self, _provider: OwnedObjectPath) {}
}

/// `org.bluez.AdvertisementMonitorManager1` of hci0.
pub struct FakeMonitorManager {
    pub registered: Arc<Mutex<Vec<(String, String)>>>,
    pub types: Vec<String>,
}

#[interface(name = "org.bluez.AdvertisementMonitorManager1")]
impl FakeMonitorManager {
    fn register_monitor(
        &self,
        application: OwnedObjectPath,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let mut r = self.registered.lock().unwrap();
        let entry = (sender, application.to_string());
        if r.contains(&entry) {
            return Err(zbus::fdo::Error::Failed("AlreadyExists".into()));
        }
        r.push(entry);
        Ok(())
    }
    fn unregister_monitor(&self, _application: OwnedObjectPath) {}
    #[zbus(property)]
    fn supported_monitor_types(&self) -> Vec<String> {
        self.types.clone()
    }
    #[zbus(property)]
    fn supported_features(&self) -> Vec<String> {
        vec!["controller-patterns".into()]
    }
}

/// `org.bluez.Adapter1`, only to count discovery requests: there must
/// never be any.
pub struct FakeAdapter1 {
    pub discovery: Arc<AtomicUsize>,
}

#[interface(name = "org.bluez.Adapter1")]
impl FakeAdapter1 {
    fn start_discovery(&self) {
        self.discovery.fetch_add(1, Ordering::SeqCst);
    }
    fn stop_discovery(&self) {}
    fn set_discovery_filter(&self, _filter: HashMap<String, OwnedVariant>) {
        self.discovery.fetch_add(1, Ordering::SeqCst);
    }
}

/// A BLE advertiser BlueZ created an object for (the AirPods under their
/// rotating private address).
pub struct FakeLeDevice {
    pub address: String,
    pub rssi: i16,
    pub data: Vec<u8>,
}

#[interface(name = "org.bluez.Device1")]
impl FakeLeDevice {
    #[zbus(property)]
    fn address(&self) -> String {
        self.address.clone()
    }
    #[zbus(property)]
    fn address_type(&self) -> String {
        "random".into()
    }
    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> i16 {
        self.rssi
    }
    #[zbus(property)]
    fn paired(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn connected(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn manufacturer_data(&self) -> HashMap<u16, OwnedVariant> {
        HashMap::from([(
            0x004C,
            OwnedVariant::try_from(Variant::from(self.data.clone())).unwrap(),
        )])
    }
}

pub struct FakeTransport {
    pub state: String,
}

#[interface(name = "org.bluez.MediaTransport1")]
impl FakeTransport {
    #[zbus(property)]
    fn state(&self) -> String {
        self.state.clone()
    }
}

pub struct FakeBluez {
    pub conn: Connection,
    pub providers: Arc<Mutex<Vec<(String, String)>>>,
    /// (sender, root) of each `RegisterMonitor`.
    pub monitors: Arc<Mutex<Vec<(String, String)>>>,
    /// `StartDiscovery` and `SetDiscoveryFilter` calls.
    pub discovery: Arc<AtomicUsize>,
}

impl FakeBluez {
    pub async fn serve(bus: &PrivateBus, connected: bool) -> Self {
        Self::serve_model(bus, connected, "bluetooth:v004Cp200Ed0A4C").await
    }

    pub async fn serve_model(bus: &PrivateBus, connected: bool, modalias: &str) -> Self {
        let conn = bus.connect().await;
        let providers = Arc::new(Mutex::new(Vec::new()));
        let monitors = Arc::new(Mutex::new(Vec::new()));
        let discovery = Arc::new(AtomicUsize::new(0));
        let server = conn.object_server();
        server.at("/", ObjectManager).await.unwrap();
        server
            .at(
                "/org/bluez/hci0",
                FakeAdapter {
                    providers: providers.clone(),
                },
            )
            .await
            .unwrap();
        server
            .at(
                "/org/bluez/hci0",
                FakeMonitorManager {
                    registered: monitors.clone(),
                    types: vec!["or_patterns".into()],
                },
            )
            .await
            .unwrap();
        server
            .at(
                "/org/bluez/hci0",
                FakeAdapter1 {
                    discovery: discovery.clone(),
                },
            )
            .await
            .unwrap();
        server
            .at(
                BLUEZ_DEVICE,
                FakeDevice {
                    address: ADDRESS.into(),
                    alias: "AirPods Pro".into(),
                    connected,
                    paired: true,
                    modalias: modalias.into(),
                    uuids: vec![
                        "0000110b-0000-1000-8000-00805f9b34fb".into(),
                        pods_proto::AAP_UUID.into(),
                    ],
                },
            )
            .await
            .unwrap();
        server
            .at(
                format!("{BLUEZ_DEVICE}/fd0"),
                FakeTransport {
                    state: "active".into(),
                },
            )
            .await
            .unwrap();
        conn.request_name("org.bluez").await.unwrap();
        Self {
            conn,
            providers,
            monitors,
            discovery,
        }
    }

    /// The buds advertise `data` from the private `address`: BlueZ
    /// creates the object the first time, then updates its manufacturer
    /// data.
    pub async fn advertise(&self, address: &str, data: Vec<u8>, rssi: i16) -> String {
        let path = format!("/org/bluez/hci0/dev_{}", address.replace(':', "_"));
        let server = self.conn.object_server();
        match server.interface::<_, FakeLeDevice>(path.as_str()).await {
            Ok(iface) => {
                let changed = iface.get().await.data != data;
                iface.get_mut().await.data = data;
                if changed {
                    iface
                        .get()
                        .await
                        .manufacturer_data_changed(iface.signal_emitter())
                        .await
                        .unwrap();
                }
            }
            Err(_) => {
                server
                    .at(
                        path.as_str(),
                        FakeLeDevice {
                            address: address.to_string(),
                            rssi,
                            data,
                        },
                    )
                    .await
                    .unwrap();
            }
        }
        path
    }

    /// Calls `method(path)` on the registered monitor, as BlueZ does.
    pub async fn monitor_call(&self, method: &str, path: &str) {
        let (sender, root) = self.monitors.lock().unwrap()[0].clone();
        let om = zbus::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination(sender.clone())
            .path(root)
            .build()
            .await
            .unwrap();
        let objects = om.get_managed_objects().await.unwrap();
        let (monitor, _) = objects
            .iter()
            .find(|(_, ifaces)| {
                ifaces
                    .keys()
                    .any(|i| i.as_str() == "org.bluez.AdvertisementMonitor1")
            })
            .expect("a monitor under the root");
        let proxy = zbus::Proxy::new(
            &self.conn,
            sender,
            monitor.to_string(),
            "org.bluez.AdvertisementMonitor1",
        )
        .await
        .unwrap();
        let target = OwnedObjectPath::try_from(path.to_string()).unwrap();
        proxy.call_method(method, &(target,)).await.unwrap();
    }

    pub fn discovery_calls(&self) -> usize {
        self.discovery.load(Ordering::SeqCst)
    }

    pub async fn set_connected(&self, connected: bool) {
        let iface = self
            .conn
            .object_server()
            .interface::<_, FakeDevice>(BLUEZ_DEVICE)
            .await
            .unwrap();
        iface.get_mut().await.connected = connected;
        iface
            .get()
            .await
            .connected_changed(iface.signal_emitter())
            .await
            .unwrap();
    }

    pub async fn alias(&self) -> String {
        let iface = self
            .conn
            .object_server()
            .interface::<_, FakeDevice>(BLUEZ_DEVICE)
            .await
            .unwrap();
        iface.get().await.alias.clone()
    }
}

// ------------------------------------------------------- fake Secret Service

pub const SECRETS_SESSION: &str = "/org/freedesktop/secrets/session/s1";

#[derive(Clone, Debug, Default)]
pub struct SecretItem {
    pub path: String,
    pub label: String,
    pub attributes: HashMap<String, String>,
    pub value: Vec<u8>,
    pub content_type: String,
}

#[derive(Default)]
pub struct SecretsState {
    pub items: Vec<SecretItem>,
    pub locked: bool,
    pub algorithms: Vec<String>,
}

pub struct FakeSecretService {
    state: Arc<Mutex<SecretsState>>,
}

fn owned_path(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p.to_string()).unwrap()
}

#[interface(name = "org.freedesktop.Secret.Service")]
impl FakeSecretService {
    fn open_session(
        &self,
        algorithm: String,
        _input: OwnedVariant,
    ) -> zbus::fdo::Result<(OwnedVariant, OwnedObjectPath)> {
        self.state
            .lock()
            .unwrap()
            .algorithms
            .push(algorithm.clone());
        if algorithm != "plain" {
            return Err(zbus::fdo::Error::NotSupported(algorithm));
        }
        Ok((
            OwnedVariant::from(zbus::wire::Str::from("")),
            owned_path(SECRETS_SESSION),
        ))
    }

    fn search_items(
        &self,
        attributes: HashMap<String, String>,
    ) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
        let st = self.state.lock().unwrap();
        let found: Vec<OwnedObjectPath> = st
            .items
            .iter()
            .filter(|i| {
                attributes
                    .iter()
                    .all(|(k, v)| i.attributes.get(k) == Some(v))
            })
            .map(|i| owned_path(&i.path))
            .collect();
        if st.locked {
            (Vec::new(), found)
        } else {
            (found, Vec::new())
        }
    }

    fn unlock(&self, objects: Vec<OwnedObjectPath>) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
        if self.state.lock().unwrap().locked {
            (Vec::new(), owned_path("/org/freedesktop/secrets/prompt/p1"))
        } else {
            (objects, owned_path("/"))
        }
    }

    #[allow(clippy::type_complexity)]
    fn get_secrets(
        &self,
        items: Vec<OwnedObjectPath>,
        session: OwnedObjectPath,
    ) -> zbus::fdo::Result<HashMap<OwnedObjectPath, (OwnedObjectPath, Vec<u8>, Vec<u8>, String)>>
    {
        if session.as_str() != SECRETS_SESSION {
            return Err(zbus::fdo::Error::InvalidArgs("no such session".into()));
        }
        let st = self.state.lock().unwrap();
        Ok(items
            .into_iter()
            .filter_map(|p| {
                let item = st.items.iter().find(|i| i.path == p.as_str())?;
                Some((
                    p,
                    (
                        session.clone(),
                        Vec::new(),
                        item.value.clone(),
                        item.content_type.clone(),
                    ),
                ))
            })
            .collect())
    }
}

pub struct FakeCollection {
    state: Arc<Mutex<SecretsState>>,
}

#[interface(name = "org.freedesktop.Secret.Collection")]
impl FakeCollection {
    fn create_item(
        &self,
        properties: HashMap<String, OwnedVariant>,
        secret: (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
        replace: bool,
    ) -> (OwnedObjectPath, OwnedObjectPath) {
        let mut st = self.state.lock().unwrap();
        if st.locked {
            return (
                owned_path("/"),
                owned_path("/org/freedesktop/secrets/prompt/p2"),
            );
        }
        let attributes: HashMap<String, String> = properties
            .get("org.freedesktop.Secret.Item.Attributes")
            .and_then(|v| HashMap::<String, String>::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default();
        let label = properties
            .get("org.freedesktop.Secret.Item.Label")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default();
        if replace {
            st.items.retain(|i| i.attributes != attributes);
        }
        let path = format!(
            "/org/freedesktop/secrets/collection/login/{}",
            st.items.len() + 1
        );
        st.items.push(SecretItem {
            path: path.clone(),
            label,
            attributes,
            value: secret.2,
            content_type: secret.3,
        });
        (owned_path(&path), owned_path("/"))
    }
}

pub struct FakeSecretSession;

#[interface(name = "org.freedesktop.Secret.Session")]
impl FakeSecretSession {
    fn close(&self) {}
}

pub struct FakeSecrets {
    pub conn: Connection,
    pub state: Arc<Mutex<SecretsState>>,
}

impl FakeSecrets {
    pub async fn serve(bus: &PrivateBus) -> Self {
        let conn = bus.connect().await;
        let state = Arc::new(Mutex::new(SecretsState::default()));
        let server = conn.object_server();
        server
            .at(
                "/org/freedesktop/secrets",
                FakeSecretService {
                    state: state.clone(),
                },
            )
            .await
            .unwrap();
        server
            .at(
                "/org/freedesktop/secrets/aliases/default",
                FakeCollection {
                    state: state.clone(),
                },
            )
            .await
            .unwrap();
        server.at(SECRETS_SESSION, FakeSecretSession).await.unwrap();
        conn.request_name("org.freedesktop.secrets").await.unwrap();
        Self { conn, state }
    }

    pub fn items(&self) -> Vec<SecretItem> {
        self.state.lock().unwrap().items.clone()
    }

    /// Keys stored before the engine starts (an earlier run).
    pub fn seed(&self, address: &str, keys: &ProximityKeys) {
        let mut st = self.state.lock().unwrap();
        let n = st.items.len() + 1;
        st.items.push(SecretItem {
            path: format!("/org/freedesktop/secrets/collection/login/{n}"),
            label: "seeded".into(),
            attributes: HashMap::from([
                ("xdg:schema".to_string(), podsd::keys::SCHEMA.to_string()),
                ("address".to_string(), address.to_string()),
            ]),
            value: keys.to_bytes().to_vec(),
            content_type: "application/octet-stream".into(),
        });
    }

    pub fn lock(&self, locked: bool) {
        self.state.lock().unwrap().locked = locked;
    }
}

// ------------------------------------------------------------ fake AirPods

/// What the Pro 1 sent after the notification request in M0 (masked).
pub fn m0_burst() -> Vec<Vec<u8>> {
    include_str!("../../../pods-proto/tests/fixtures/m0-pro1-one-bud.txt")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let dir = cols.next()?;
            let t: f64 = cols.next()?.parse().ok()?;
            let hex = cols.next()?;
            (dir == "rx" && t < 5.0 && !hex.starts_with("01000400")).then(|| hex_bytes(hex))
        })
        .collect()
}

pub fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[derive(Default)]
pub struct FakeInner {
    pub connects: AtomicUsize,
    pub refuse: AtomicUsize,
    /// Accept the link, then never say a word.
    pub mute: AtomicBool,
    pub received: Mutex<Vec<Vec<u8>>>,
    to_engine: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    /// What the buds answer to `0x30`, if anything.
    pub keys: Mutex<Option<ProximityKeys>>,
}

#[derive(Clone)]
pub struct FakeAirPods {
    pub inner: Arc<FakeInner>,
}

impl Default for FakeAirPods {
    fn default() -> Self {
        let inner = FakeInner::default();
        *inner.keys.lock().unwrap() = Some(test_keys());
        Self {
            inner: Arc::new(inner),
        }
    }
}

/// The proximity keys of the fake AirPods. Made up: nobody's real keys.
pub fn test_keys() -> ProximityKeys {
    ProximityKeys {
        irk: core::array::from_fn(|i| 0x31 + 3 * i as u8),
        enc: core::array::from_fn(|i| 0xC4 ^ (5 * i as u8)),
    }
}

/// Another pair nearby.
pub fn neighbour_keys() -> ProximityKeys {
    ProximityKeys {
        irk: core::array::from_fn(|i| 0x77 ^ i as u8),
        enc: [0x5A; 16],
    }
}

/// The `0x31` reply carrying `keys`.
pub fn keys_frame(keys: &ProximityKeys) -> Vec<u8> {
    let mut f = vec![4, 0, 4, 0, 0x31, 0];
    f.extend(pods_proto::ble::keys_payload(keys));
    f
}

/// A Pro 1 advertisement: both buds in the case, right primary at 93 %
/// (charging), left 84 % (charging), case 62 %, white, lid as given.
pub fn pro1_advert(keys: &ProximityKeys, closed: bool, counter: u8, case: u8) -> Vec<u8> {
    build_advert(
        &AdvertFields {
            model: 0x200E,
            status: 0x54,
            buds: 0x89,
            case: 0x30 | (case / 10),
            lid: (u8::from(closed) << 3) | (counter & 7),
            color: 0x00,
            connection: 0x00,
            plain: [
                0,
                0x80 | 93,
                0x80 | 84,
                case,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ],
        },
        keys,
    )
}

struct FakeLink {
    inner: Arc<FakeInner>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl FakeAirPods {
    pub fn connects(&self) -> usize {
        self.inner.connects.load(Ordering::SeqCst)
    }

    pub fn received(&self) -> Vec<Vec<u8>> {
        self.inner.received.lock().unwrap().clone()
    }

    /// Something the buds say on their own (a stem press, a battery change).
    pub fn inject(&self, packet: Vec<u8>) {
        if let Some(tx) = self.inner.to_engine.lock().unwrap().as_ref() {
            let _ = tx.send(packet);
        }
    }

    /// The buds close the channel.
    pub fn hang_up(&self) {
        self.inner.to_engine.lock().unwrap().take();
    }
}

impl AapConnector for FakeAirPods {
    fn connect<'a>(&'a self, _address: &'a str) -> BoxFuture<'a, io::Result<Box<dyn AapLink>>> {
        Box::pin(async move {
            let mut left = self.inner.refuse.load(Ordering::SeqCst);
            while left > 0 {
                match self.inner.refuse.compare_exchange(
                    left,
                    left - 1,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => return Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
                    Err(now) => left = now,
                }
            }
            self.inner.connects.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::unbounded_channel();
            *self.inner.to_engine.lock().unwrap() = Some(tx.clone());
            Ok(Box::new(FakeLink {
                inner: self.inner.clone(),
                rx: tokio::sync::Mutex::new(rx),
                tx,
            }) as Box<dyn AapLink>)
        })
    }
}

impl AapLink for FakeLink {
    fn send<'a>(&'a self, packet: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.inner.received.lock().unwrap().push(packet.to_vec());
            if self.inner.mute.load(Ordering::SeqCst) {
                return Ok(());
            }
            if packet[..4] == [0, 0, 4, 0] {
                let _ = self
                    .tx
                    .send(hex_bytes("010004000000010003000400b1e1040051e2"));
            } else if packet == pods_proto::packet::NOTIFICATIONS {
                for p in m0_burst() {
                    let _ = self.tx.send(p);
                }
            } else if packet.len() >= 8 && packet[4] == 0x09 && packet[6] == 0x0D {
                // The buds echo a listening mode they accept.
                let _ = self.tx.send(packet.to_vec());
            } else if packet == pods_proto::packet::request_keys().as_slice()
                && let Some(keys) = self.inner.keys.lock().unwrap().clone()
            {
                let _ = self.tx.send(keys_frame(&keys));
            }
            Ok(())
        })
    }

    fn recv(&self) -> BoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(async move {
            // The test can hang up by dropping its sender; ours stays, so
            // also stop when the shared slot is emptied.
            loop {
                let hung_up = self.inner.to_engine.lock().unwrap().is_none();
                if hung_up {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                let mut rx = self.rx.lock().await;
                match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                    Ok(Some(p)) => return Ok(p),
                    Ok(None) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                    Err(_) => continue,
                }
            }
        })
    }
}

// ------------------------------------------------------------- spatial

#[derive(Default)]
pub struct FakeRenderInner {
    /// "start fixed AA:BB:…", "start head AA:BB:…", "stop".
    pub log: Mutex<Vec<String>>,
    /// The status callback of the output started last.
    pub on_status: Mutex<Option<StatusSink>>,
    pub yaws: Mutex<Vec<f32>>,
    /// Outputs started and not stopped.
    pub running: AtomicUsize,
    /// The next start fails.
    pub refuse: AtomicBool,
    /// Do not report `Active` by itself after a start.
    pub hold_starting: AtomicBool,
}

/// Stands in for pods-spatial: records orders, reports what the test says.
#[derive(Clone, Default)]
pub struct FakeRenderer {
    pub inner: Arc<FakeRenderInner>,
}

impl FakeRenderer {
    pub fn log(&self) -> Vec<String> {
        self.inner.log.lock().unwrap().clone()
    }

    pub fn running(&self) -> usize {
        self.inner.running.load(Ordering::SeqCst)
    }

    pub fn yaws(&self) -> Vec<f32> {
        self.inner.yaws.lock().unwrap().clone()
    }

    /// What the PipeWire thread would report.
    pub fn report(&self, status: RenderStatus) {
        if let Some(cb) = self.inner.on_status.lock().unwrap().as_ref() {
            cb(status);
        }
    }
}

struct FakeRendering {
    inner: Arc<FakeRenderInner>,
}

impl Renderer for FakeRenderer {
    fn start(
        &self,
        mode: RenderMode,
        address: &str,
        _name: &str,
        on_status: StatusSink,
    ) -> Result<Box<dyn Rendering>, String> {
        let word = match mode {
            RenderMode::Fixed => "fixed",
            RenderMode::HeadTracked => "head",
        };
        self.inner
            .log
            .lock()
            .unwrap()
            .push(format!("start {word} {address}"));
        if self.inner.refuse.swap(false, Ordering::SeqCst) {
            return Err("target sink is not in the PipeWire graph".into());
        }
        // As the real one: Starting, then Active once the sink is the default.
        if !self.inner.hold_starting.load(Ordering::SeqCst) {
            on_status(RenderStatus::Active);
        }
        *self.inner.on_status.lock().unwrap() = Some(on_status);
        self.inner.running.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeRendering {
            inner: self.inner.clone(),
        }))
    }
}

impl Rendering for FakeRendering {
    fn set_yaw(&self, yaw_deg: f32) {
        self.inner.yaws.lock().unwrap().push(yaw_deg);
    }

    fn stop(self: Box<Self>) {
        self.inner.log.lock().unwrap().push("stop".into());
        self.inner.running.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Stands in for the PipeWire node monitor: the test moves the AirPods'
/// `bluez_output` node.
#[derive(Clone)]
pub struct FakeSinks {
    pub tx: Arc<watch::Sender<SinkState>>,
}

impl FakeSinks {
    pub fn new(present: bool) -> Self {
        let (tx, _) = watch::channel(SinkState {
            present,
            running: false,
            appearances: u64::from(present),
        });
        Self { tx: Arc::new(tx) }
    }

    pub fn set(&self, present: bool, running: bool) {
        self.tx.send_modify(|s| {
            if present && !s.present {
                s.appearances += 1;
            }
            s.present = present;
            s.running = present && running;
        });
    }
}

impl AudioMonitor for FakeSinks {
    fn watch(&self, _address: &str) -> watch::Receiver<SinkState> {
        self.tx.subscribe()
    }
}

// ----------------------------------------------------------------- rig

pub fn fast_timing() -> Timing {
    Timing {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(400),
        watchdog: Some(Duration::from_secs(30)),
        opening_pauses: false,
    }
}

pub struct Rig {
    pub bus: PrivateBus,
    pub bluez: FakeBluez,
    pub secrets: FakeSecrets,
    pub airpods: FakeAirPods,
    pub client: Connection,
    pub engine_system: Connection,
    pub config_dir: tempfile::TempDir,
    pub engine: Arc<Engine>,
    pub renderer: FakeRenderer,
    pub sinks: FakeSinks,
}

/// How a rig starts.
pub struct Setup {
    pub connected: bool,
    pub timing: Timing,
    pub modalias: &'static str,
    /// Keys already in the keyring from an earlier run.
    pub keys_in_keyring: bool,
    pub keyring_locked: bool,
    /// `config.toml` written before the engine starts.
    pub config: Option<&'static str>,
    /// The AirPods' PipeWire output node exists at start.
    pub sink_present: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            connected: true,
            timing: fast_timing(),
            modalias: "bluetooth:v004Cp200Ed0A4C",
            keys_in_keyring: false,
            keyring_locked: false,
            config: None,
            sink_present: true,
        }
    }
}

impl Rig {
    pub async fn start(connected: bool, timing: Timing) -> Self {
        Self::with(Setup {
            connected,
            timing,
            ..Setup::default()
        })
        .await
    }

    pub async fn with(setup: Setup) -> Self {
        let connected = setup.connected;
        let timing = setup.timing;
        pods_bt::forbid_real_bluetooth();
        let bus = PrivateBus::start().await;
        let bluez = FakeBluez::serve_model(&bus, connected, setup.modalias).await;
        let secrets = FakeSecrets::serve(&bus).await;
        if setup.keys_in_keyring {
            secrets.seed(ADDRESS, &test_keys());
        }
        secrets.lock(setup.keyring_locked);
        let airpods = FakeAirPods::default();
        let config_dir = tempfile::tempdir().unwrap();
        if let Some(text) = setup.config {
            std::fs::write(config_dir.path().join("config.toml"), text).unwrap();
        }
        let session = bus.connect().await;
        let system = bus.connect().await;
        let renderer = FakeRenderer::default();
        let sinks = FakeSinks::new(setup.sink_present);
        let engine = Engine::start(EngineDeps {
            session,
            system: system.clone(),
            connector: Arc::new(airpods.clone()),
            media_host: None,
            keys: None,
            config: Arc::new(ConfigStore::load(config_dir.path().join("config.toml"))),
            timing,
            proc_root: None,
            request_name: true,
            renderer: Arc::new(renderer.clone()),
            sinks: Arc::new(sinks.clone()),
        })
        .await
        .expect("engine starts");
        let client = bus.connect().await;
        Self {
            bus,
            bluez,
            secrets,
            airpods,
            client,
            engine_system: system,
            config_dir,
            engine,
            renderer,
            sinks,
        }
    }

    /// Waits for the engine's monitor to be registered with BlueZ.
    pub async fn monitor_registered(&self) {
        eventually("monitor registered", || async {
            !self.bluez.monitors.lock().unwrap().is_empty()
        })
        .await;
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_dir.path().join("config.toml")
    }

    pub async fn proxy(&self, path: &str, iface: &str) -> zbus::Proxy<'static> {
        zbus::proxy::Builder::new(&self.client)
            .destination(BUS_NAME)
            .path(path.to_string())
            .interface(iface.to_string())
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    pub async fn device(&self) -> zbus::Proxy<'static> {
        self.proxy(DEVICE, DEVICE_IFACE).await
    }

    pub async fn device_prop<T>(&self, name: &str) -> T
    where
        T: serde::de::DeserializeOwned + zbus::Type,
    {
        self.device().await.get_property::<T>(name).await.unwrap()
    }

    /// Calls a device method, returning the D-Bus error name on failure.
    pub async fn call<B>(&self, method: &str, body: &B) -> Result<(), String>
    where
        B: serde::Serialize + zbus::wire::DynamicType,
    {
        self.device()
            .await
            .call_method(method, body)
            .await
            .map(|_| ())
            .map_err(|e| match e {
                zbus::Error::MethodError(name, _, _) => name.to_string(),
                other => other.to_string(),
            })
    }

    /// Collects every `member` signal of `iface` from now on.
    pub async fn signals(
        &self,
        iface: &'static str,
        member: &'static str,
    ) -> Arc<Mutex<Vec<zbus::Message>>> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(iface)
            .member(member)
            .build()
            .unwrap();
        let mut stream = MessageStream::for_match_rule(rule, &self.client, None)
            .await
            .unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        tokio::spawn(async move {
            while let Some(Ok(m)) = stream.next().await {
                sink.lock().unwrap().push(m);
            }
        });
        out
    }
}

/// Polls `check` until it holds, for up to five seconds.
pub async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if check().await {
            return;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub fn string_args(m: &zbus::Message) -> (String, String) {
    m.body().deserialize::<(String, String)>().unwrap()
}

pub type Battery = HashMap<String, (u8, bool)>;
