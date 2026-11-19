// SPDX-License-Identifier: GPL-3.0-or-later
//! A passive BLE advertisement monitor (`org.bluez.AdvertisementMonitor1`)
//! for the AirPods' proximity advertisement.
//!
//! BlueZ hands the pattern to the controller when it can
//! (`controller-patterns`): the host only wakes for Apple proximity
//! pairing messages. **No `StartDiscovery`**, no active scan: the monitor
//! stays registered for as long as the engine runs, connected or not.
//!
//! BlueZ reports a matching advertiser with `DeviceFound`, then keeps its
//! `org.bluez.Device1` object up to date; this module reads the
//! manufacturer data from those objects (`InterfacesAdded`,
//! `PropertiesChanged`) and turns it into [`BleEvent`]s. It knows nothing
//! about keys: telling our AirPods from the neighbour's is the engine's
//! job.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::fdo::{DBusProxy, ObjectManager, ObjectManagerProxy};
use zbus::names::InterfaceName;
use zbus::wire::{ObjectPath, OwnedObjectPath, OwnedVariant};
use zbus::{Connection, MatchRule, MessageStream, interface};

use pods_proto::ble::{AD_MANUFACTURER, ADVERT_LEN, APPLE, MONITOR_PATTERN, PROXIMITY_PAIRING};

use crate::bluez::{BLUEZ_SERVICE, DEVICE_IFACE};

/// Root of our monitor hierarchy, the path given to `RegisterMonitor`.
pub const MONITOR_ROOT: &str = "/io/github/d101c/Pods/monitor";
/// The one monitor under it.
pub const MONITOR_PATH: &str = "/io/github/d101c/Pods/monitor/airpods";
pub const MANAGER_IFACE: &str = "org.bluez.AdvertisementMonitorManager1";
pub const MONITOR_TYPE: &str = "or_patterns";

/// RSSI thresholds and timers handed to BlueZ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// A device is in range after `high_timeout` seconds at or above this.
    pub high_rssi: i16,
    pub high_timeout: u16,
    /// A device is lost after `low_timeout` seconds without a packet at
    /// or above this: BlueZ calls `DeviceLost`.
    pub low_rssi: i16,
    pub low_timeout: u16,
    /// 0: every packet of an in-range device is propagated.
    pub sampling: u16,
}

/// A case on the desk or in a bag next to the laptop reads -40 to -75 dBm;
/// AirPods in the next room or across an open space fall below -80.
/// `low_timeout` is the "8 s without an advertisement" that closes the case
/// card: BlueZ only signals advertisement *changes* on D-Bus, so the
/// silence is measured where every packet is seen, by BlueZ or the
/// controller.
pub const THRESHOLDS: Thresholds = Thresholds {
    high_rssi: -80,
    high_timeout: 1,
    low_rssi: -90,
    low_timeout: 8,
    sampling: 0,
};

/// What the monitor reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BleEvent {
    /// A proximity pairing advertisement (manufacturer data of company
    /// 0x004C, starting with `07 19`) from the BlueZ object `device`.
    Advert {
        device: String,
        address: String,
        rssi: Option<i16>,
        data: Vec<u8>,
    },
    /// BlueZ heard nothing from `device` for [`Thresholds::low_timeout`].
    Lost { device: String },
}

enum Call {
    Found(String),
    Lost(String),
}

struct MonitorObject {
    calls: mpsc::UnboundedSender<Call>,
    thresholds: Thresholds,
}

#[interface(name = "org.bluez.AdvertisementMonitor1")]
impl MonitorObject {
    fn release(&self) {
        info!("BlueZ released the advertisement monitor");
    }

    fn activate(&self) {
        info!("advertisement monitor active");
    }

    fn device_found(&self, device: OwnedObjectPath) {
        let _ = self.calls.send(Call::Found(device.to_string()));
    }

    fn device_lost(&self, device: OwnedObjectPath) {
        let _ = self.calls.send(Call::Lost(device.to_string()));
    }

    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        MONITOR_TYPE.to_string()
    }

    #[zbus(property, name = "RSSIHighThreshold")]
    fn rssi_high_threshold(&self) -> i16 {
        self.thresholds.high_rssi
    }

    #[zbus(property, name = "RSSIHighTimeout")]
    fn rssi_high_timeout(&self) -> u16 {
        self.thresholds.high_timeout
    }

    #[zbus(property, name = "RSSILowThreshold")]
    fn rssi_low_threshold(&self) -> i16 {
        self.thresholds.low_rssi
    }

    #[zbus(property, name = "RSSILowTimeout")]
    fn rssi_low_timeout(&self) -> u16 {
        self.thresholds.low_timeout
    }

    #[zbus(property, name = "RSSISamplingPeriod")]
    fn rssi_sampling_period(&self) -> u16 {
        self.thresholds.sampling
    }

    /// (start position, AD type, content).
    #[zbus(property)]
    fn patterns(&self) -> Vec<(u8, u8, Vec<u8>)> {
        vec![(0, AD_MANUFACTURER, MONITOR_PATTERN.to_vec())]
    }
}

#[zbus::proxy(
    interface = "org.bluez.AdvertisementMonitorManager1",
    default_service = "org.bluez"
)]
trait AdvertisementMonitorManager1 {
    fn register_monitor(&self, application: &ObjectPath<'_>) -> zbus::Result<()>;
}

fn as_strings(v: &OwnedVariant) -> Option<Vec<String>> {
    Vec::<String>::try_from(v.try_clone().ok()?).ok()
}

/// The Apple proximity pairing bytes in a `ManufacturerData` value.
fn apple_bytes(value: &OwnedVariant) -> Option<Vec<u8>> {
    let map: HashMap<u16, OwnedVariant> = value.try_clone().ok()?.try_into().ok()?;
    let bytes: Vec<u8> = map.get(&APPLE)?.try_clone().ok()?.try_into().ok()?;
    (bytes.len() >= ADVERT_LEN && bytes[0] == PROXIMITY_PAIRING && bytes[1] == MONITOR_PATTERN[3])
        .then_some(bytes)
}

/// `AA:BB:…` from a BlueZ device path `/org/bluez/hci0/dev_AA_BB_…`.
pub fn address_from_path(path: &str) -> Option<String> {
    let dev = path.rsplit_once("/dev_")?.1;
    (dev.len() == 17).then(|| dev.replace('_', ":"))
}

/// Per BlueZ device: the last RSSI seen, and which objects were read in
/// full. `PropertiesChanged` and `InterfacesAdded` come on two streams, so
/// a change can be read before the object it belongs to: an object never
/// read in full is read (`GetAll`) before its first change counts.
#[derive(Default)]
struct Seen {
    rssi: HashMap<String, i16>,
    known: std::collections::HashSet<String>,
}

impl Seen {
    fn full(&mut self, path: &str, props: &HashMap<String, OwnedVariant>) -> Option<BleEvent> {
        self.known.insert(path.to_string());
        self.props(path, props)
    }

    /// Turns the properties of one device into an event, if they carry an
    /// advertisement.
    fn props(&mut self, path: &str, props: &HashMap<String, OwnedVariant>) -> Option<BleEvent> {
        if let Some(r) = props
            .get("RSSI")
            .and_then(|v| i16::try_from(v.try_clone().ok()?).ok())
        {
            self.rssi.insert(path.to_string(), r);
        }
        let data = apple_bytes(props.get("ManufacturerData")?)?;
        let address = props
            .get("Address")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .or_else(|| address_from_path(path))?;
        Some(BleEvent::Advert {
            device: path.to_string(),
            address,
            rssi: self.rssi.get(path).copied(),
            data,
        })
    }
}

/// Registers our monitor root with every adapter that has the monitor
/// manager and supports `or_patterns`. Returns how many accepted.
async fn register_all(conn: &Connection) -> usize {
    let om = match ObjectManagerProxy::builder(conn)
        .destination(BLUEZ_SERVICE)
        .path("/")
        .build()
        .await
    {
        Ok(p) => p,
        Err(e) => {
            debug!(error = %e, "no BlueZ object manager");
            return 0;
        }
    };
    let objects = match om.get_managed_objects().await {
        Ok(o) => o,
        Err(e) => {
            debug!(error = %e, "BlueZ objects unreadable");
            return 0;
        }
    };
    let mut done = 0;
    for (path, ifaces) in objects {
        let Some(props) = ifaces.get(MANAGER_IFACE) else {
            continue;
        };
        if register(conn, path.as_str(), props).await {
            done += 1;
        }
    }
    done
}

async fn register(conn: &Connection, adapter: &str, props: &HashMap<String, OwnedVariant>) -> bool {
    let types = props
        .get("SupportedMonitorTypes")
        .and_then(as_strings)
        .unwrap_or_default();
    if !types.iter().any(|t| t == MONITOR_TYPE) {
        warn!(adapter, "BlueZ offers no or_patterns monitor: no case card");
        return false;
    }
    let offload = props
        .get("SupportedFeatures")
        .and_then(as_strings)
        .unwrap_or_default()
        .iter()
        .any(|f| f == "controller-patterns");
    let proxy = match AdvertisementMonitorManager1Proxy::builder(conn)
        .path(adapter.to_string())
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(adapter, error = %e, "no monitor manager");
            return false;
        }
    };
    match proxy
        .register_monitor(&ObjectPath::from_static_str_unchecked(MONITOR_ROOT))
        .await
    {
        Ok(()) => {
            info!(adapter, offload, "advertisement monitor registered");
            true
        }
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.bluez.Error.AlreadyExists" =>
        {
            true
        }
        Err(e) => {
            warn!(adapter, error = %e, "BlueZ refused the advertisement monitor");
            false
        }
    }
}

async fn device_props(conn: &Connection, path: &str) -> Option<HashMap<String, OwnedVariant>> {
    let proxy = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(BLUEZ_SERVICE)
        .path(path.to_string())
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let iface = InterfaceName::from_static_str_unchecked(DEVICE_IFACE);
    proxy.get_all(iface).await.ok()
}

/// Serves the monitor on `conn` (the system bus), registers it with BlueZ
/// and reports what it sees until the receiver is dropped. Registers
/// again whenever BlueZ (re)starts or an adapter appears.
pub async fn start(conn: Connection) -> zbus::Result<mpsc::UnboundedReceiver<BleEvent>> {
    let (calls_tx, mut calls) = mpsc::unbounded_channel();
    let server = conn.object_server();
    server.at(MONITOR_ROOT, ObjectManager).await?;
    server
        .at(
            MONITOR_PATH,
            MonitorObject {
                calls: calls_tx,
                thresholds: THRESHOLDS,
            },
        )
        .await?;

    // Subscribe before registering so nothing falls in between. One
    // stream for every BlueZ signal: `InterfacesAdded` and the
    // `PropertiesChanged` that follow must be read in the order BlueZ sent
    // them, or a stale lid state could be read last.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(BLUEZ_SERVICE)
        .build()?;
    let mut signals =
        MessageStream::for_match_rule(rule, &conn, Some(512.try_into().expect("non-zero"))).await?;
    let dbus = DBusProxy::new(&conn).await?;
    let mut owners = dbus
        .receive_name_owner_changed_with_args(&[(0, BLUEZ_SERVICE)])
        .await?;

    if register_all(&conn).await == 0 {
        debug!("no adapter took the monitor yet");
    }

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut seen = Seen::default();
        loop {
            let event = tokio::select! {
                Some(msg) = signals.next() => {
                    let Ok(msg) = msg else { continue };
                    seen.signal(&conn, &msg).await
                }
                Some(sig) = owners.next() => {
                    let Ok(args) = sig.args() else { continue };
                    if args.new_owner().is_some() {
                        info!("BlueZ (re)started; registering the monitor again");
                        seen = Seen::default();
                        register_all(&conn).await;
                    }
                    None
                }
                Some(call) = calls.recv() => match call {
                    // Objects already followed through their signals need
                    // nothing more; others are read once.
                    Call::Found(path) if !seen.known.contains(&path) => {
                        debug!("BlueZ found an advertiser");
                        match device_props(&conn, &path).await {
                            Some(props) => seen.full(&path, &props),
                            None => None,
                        }
                    }
                    Call::Found(_) => None,
                    Call::Lost(path) => Some(BleEvent::Lost { device: path }),
                },
                else => {
                    warn!("BLE monitor streams ended");
                    return;
                }
            };
            if let Some(ev) = event
                && tx.send(ev).is_err()
            {
                return;
            }
        }
    });
    Ok(rx)
}

impl Seen {
    /// One BlueZ signal.
    async fn signal(&mut self, conn: &Connection, msg: &zbus::Message) -> Option<BleEvent> {
        let header = msg.header();
        let member = header.member()?.as_str().to_string();
        let interface = header.interface()?.as_str().to_string();
        match (interface.as_str(), member.as_str()) {
            ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
                let path = header.path()?.to_string();
                let (iface, props, _) = msg
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
                    .ok()?;
                if iface != DEVICE_IFACE {
                    return None;
                }
                self.known.insert(path.clone());
                self.props(&path, &props)
            }
            ("org.freedesktop.DBus.ObjectManager", "InterfacesAdded") => {
                let (path, ifaces) = msg
                    .body()
                    .deserialize::<(
                        OwnedObjectPath,
                        HashMap<String, HashMap<String, OwnedVariant>>,
                    )>()
                    .ok()?;
                let path = path.to_string();
                if let Some(props) = ifaces.get(MANAGER_IFACE) {
                    register(conn, &path, props).await;
                }
                self.full(&path, ifaces.get(DEVICE_IFACE)?)
            }
            ("org.freedesktop.DBus.ObjectManager", "InterfacesRemoved") => {
                let (path, _) = msg
                    .body()
                    .deserialize::<(OwnedObjectPath, Vec<String>)>()
                    .ok()?;
                self.rssi.remove(path.as_str());
                self.known.remove(path.as_str());
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant_map(data: Vec<u8>) -> OwnedVariant {
        let inner = OwnedVariant::try_from(zbus::wire::Variant::from(data)).unwrap();
        let map: HashMap<u16, OwnedVariant> = HashMap::from([(APPLE, inner)]);
        OwnedVariant::try_from(zbus::wire::Variant::from(map)).unwrap()
    }

    fn advert() -> Vec<u8> {
        let mut d = vec![0x07, 0x19, 0x01, 0x0E, 0x20];
        d.resize(ADVERT_LEN, 0);
        d
    }

    #[test]
    fn only_proximity_pairing_counts() {
        assert_eq!(apple_bytes(&variant_map(advert())), Some(advert()));
        // Another Apple message (Find My, Nearby…).
        assert_eq!(apple_bytes(&variant_map(vec![0x12, 0x19, 0, 0])), None);
        // Too short.
        assert_eq!(apple_bytes(&variant_map(vec![0x07, 0x19, 1])), None);
    }

    #[test]
    fn addresses_from_paths() {
        assert_eq!(
            address_from_path("/org/bluez/hci0/dev_5A_11_22_33_44_55").as_deref(),
            Some("5A:11:22:33:44:55")
        );
        assert_eq!(address_from_path("/org/bluez/hci0"), None);
        assert_eq!(address_from_path("/org/bluez/hci0/dev_5A_11"), None);
    }

    #[test]
    fn device_properties_become_events() {
        let mut seen = Seen::default();
        let path = "/org/bluez/hci0/dev_5A_11_22_33_44_55";
        let rssi: HashMap<String, OwnedVariant> =
            HashMap::from([("RSSI".to_string(), OwnedVariant::from(-52i16))]);
        assert_eq!(seen.props(path, &rssi), None);
        let data: HashMap<String, OwnedVariant> =
            HashMap::from([("ManufacturerData".to_string(), variant_map(advert()))]);
        assert_eq!(
            seen.props(path, &data),
            Some(BleEvent::Advert {
                device: path.into(),
                address: "5A:11:22:33:44:55".into(),
                rssi: Some(-52),
                data: advert(),
            })
        );
    }

    #[test]
    fn thresholds_are_valid_for_bluez() {
        // adv_monitor.c: -127..20 dBm, high >= low, timeouts 1..300 s,
        // sampling at most 255.
        let t = THRESHOLDS;
        assert!((-127..=20).contains(&t.high_rssi) && (-127..=20).contains(&t.low_rssi));
        assert!(t.high_rssi >= t.low_rssi);
        assert!((1..=300).contains(&t.high_timeout) && (1..=300).contains(&t.low_timeout));
        assert!(t.sampling <= 255);
        assert_eq!(t.low_timeout, 8);
    }
}
