// SPDX-License-Identifier: GPL-3.0-or-later
//! BlueZ over zbus: which AirPods are paired and connected, the alias,
//! disconnecting, and whether audio streams to them. No `bluetoothctl`,
//! no polling: one `GetManagedObjects`, then signals.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use zbus::fdo::ObjectManagerProxy;
use zbus::names::InterfaceName;
use zbus::wire::{OwnedObjectPath, OwnedVariant};
use zbus::{Connection, MatchRule, MessageStream};

pub const BLUEZ_SERVICE: &str = "org.bluez";
pub const DEVICE_IFACE: &str = "org.bluez.Device1";
pub const TRANSPORT_IFACE: &str = "org.bluez.MediaTransport1";

/// What the engine needs to know about a BlueZ device.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BluezDevice {
    pub path: String,
    pub address: String,
    pub alias: String,
    pub connected: bool,
    pub paired: bool,
    pub modalias: Option<String>,
    pub uuids: Vec<String>,
}

impl BluezDevice {
    /// True for a device that speaks AAP.
    pub fn is_aap(&self) -> bool {
        self.uuids
            .iter()
            .any(|u| u.eq_ignore_ascii_case(pods_proto::AAP_UUID))
    }

    /// Adapter object path: the parent of the device path.
    pub fn adapter(&self) -> &str {
        self.path.rsplit_once('/').map_or("/", |(parent, _)| parent)
    }

    fn from_props(path: &str, props: &HashMap<String, OwnedVariant>) -> Self {
        let mut dev = BluezDevice {
            path: path.to_string(),
            ..Default::default()
        };
        dev.update(props);
        dev
    }

    /// Applies changed properties; true when something the engine reads changed.
    pub fn update(&mut self, props: &HashMap<String, OwnedVariant>) -> bool {
        let before = self.clone();
        for (key, value) in props {
            match key.as_str() {
                "Address" => {
                    if let Some(s) = as_string(value) {
                        self.address = s;
                    }
                }
                "Alias" => {
                    if let Some(s) = as_string(value) {
                        self.alias = s;
                    }
                }
                "Connected" => {
                    if let Some(b) = as_bool(value) {
                        self.connected = b;
                    }
                }
                "Paired" => {
                    if let Some(b) = as_bool(value) {
                        self.paired = b;
                    }
                }
                "Modalias" => self.modalias = as_string(value),
                "UUIDs" => {
                    if let Some(v) = as_strings(value) {
                        self.uuids = v;
                    }
                }
                _ => {}
            }
        }
        *self != before
    }
}

fn as_string(v: &OwnedVariant) -> Option<String> {
    String::try_from(v.try_clone().ok()?).ok()
}

fn as_bool(v: &OwnedVariant) -> Option<bool> {
    bool::try_from(v.try_clone().ok()?).ok()
}

fn as_strings(v: &OwnedVariant) -> Option<Vec<String>> {
    Vec::<String>::try_from(v.try_clone().ok()?).ok()
}

/// A change to an AAP device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BluezEvent {
    /// Seen for the first time, or something changed.
    Changed(BluezDevice),
    /// Unpaired or the adapter went away.
    Removed(BluezDevice),
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub trait Device1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_alias(&self, alias: &str) -> zbus::Result<()>;
}

/// Handle on BlueZ (the system bus in production, a private bus in tests).
#[derive(Clone, Debug)]
pub struct Bluez {
    conn: Connection,
}

impl Bluez {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Every AAP device now, then every change, until the receiver is dropped.
    pub async fn watch(&self) -> zbus::Result<mpsc::UnboundedReceiver<BluezEvent>> {
        // Subscribe before the snapshot so nothing falls in between.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ_SERVICE)
            .interface("org.freedesktop.DBus.Properties")
            .member("PropertiesChanged")
            .path_namespace("/org/bluez")
            .arg(0, DEVICE_IFACE)
            .build()?;
        let mut changes = MessageStream::for_match_rule(rule, &self.conn, None).await?;
        let om = ObjectManagerProxy::builder(&self.conn)
            .destination(BLUEZ_SERVICE)
            .path("/")
            .build()
            .await?;
        let mut added = om.receive_interfaces_added().await?;
        let mut removed = om.receive_interfaces_removed().await?;
        let objects = om.get_managed_objects().await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let mut known: HashMap<String, BluezDevice> = HashMap::new();
        for (path, ifaces) in objects {
            if let Some(props) = ifaces.get(DEVICE_IFACE) {
                let dev = BluezDevice::from_props(path.as_str(), props);
                if dev.is_aap() {
                    let _ = tx.send(BluezEvent::Changed(dev.clone()));
                }
                known.insert(path.to_string(), dev);
            }
        }

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = changes.next() => {
                        let Ok(msg) = msg else { continue };
                        let Some(path) = msg.header().path().map(|p| p.to_string()) else { continue };
                        let Ok((_, props, _)) = msg
                            .body()
                            .deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
                        else {
                            continue;
                        };
                        let Some(dev) = known.get_mut(&path) else { continue };
                        if dev.update(&props) && dev.is_aap() && tx.send(BluezEvent::Changed(dev.clone())).is_err() {
                            return;
                        }
                    }
                    Some(sig) = added.next() => {
                        let Ok(args) = sig.args() else { continue };
                        let path = args.object_path().to_string();
                        let Some(props) = args
                            .interfaces_and_properties()
                            .get(&InterfaceName::from_static_str_unchecked(DEVICE_IFACE))
                        else {
                            continue;
                        };
                        let owned: HashMap<String, OwnedVariant> = props
                            .iter()
                            .filter_map(|(k, v)| Some((k.to_string(), v.try_to_owned().ok()?)))
                            .collect();
                        let dev = BluezDevice::from_props(&path, &owned);
                        debug!(path, aap = dev.is_aap(), "BlueZ device added");
                        if dev.is_aap() && tx.send(BluezEvent::Changed(dev.clone())).is_err() {
                            return;
                        }
                        known.insert(path, dev);
                    }
                    Some(sig) = removed.next() => {
                        let Ok(args) = sig.args() else { continue };
                        let gone = args.interfaces().iter().any(|i| i.as_str() == DEVICE_IFACE);
                        if !gone {
                            continue;
                        }
                        let path = args.object_path().to_string();
                        if let Some(dev) = known.remove(&path)
                            && dev.is_aap()
                            && tx.send(BluezEvent::Removed(dev)).is_err()
                        {
                            return;
                        }
                    }
                    else => {
                        warn!("BlueZ signal streams ended");
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }

    async fn device(&self, path: &str) -> zbus::Result<Device1Proxy<'static>> {
        Device1Proxy::builder(&self.conn)
            .path(OwnedObjectPath::try_from(path.to_string())?)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
    }

    pub async fn set_alias(&self, path: &str, alias: &str) -> zbus::Result<()> {
        self.device(path).await?.set_alias(alias).await
    }

    pub async fn disconnect(&self, path: &str) -> zbus::Result<()> {
        self.device(path).await?.disconnect().await
    }

    /// `Device1.Connect`: BlueZ connects the profiles of a paired device.
    pub async fn connect(&self, path: &str) -> zbus::Result<()> {
        self.device(path).await?.connect().await
    }

    /// True while a media transport of this device is active, that is,
    /// while audio actually flows to the buds.
    pub async fn streaming(&self, device_path: &str) -> zbus::Result<bool> {
        let om = ObjectManagerProxy::builder(&self.conn)
            .destination(BLUEZ_SERVICE)
            .path("/")
            .build()
            .await?;
        let prefix = format!("{device_path}/");
        let objects = om.get_managed_objects().await?;
        Ok(objects.iter().any(|(path, ifaces)| {
            path.as_str().starts_with(&prefix)
                && ifaces
                    .get(TRANSPORT_IFACE)
                    .and_then(|p| p.get("State"))
                    .and_then(as_string)
                    .is_some_and(|s| s == "active")
        }))
    }
}

/// D-Bus object path of a device for `address`, as BlueZ spells it.
pub fn device_path(adapter: &str, address: &str) -> String {
    format!("{adapter}/dev_{}", address.replace(':', "_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: Vec<(&str, OwnedVariant)>) -> HashMap<String, OwnedVariant> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn a_device_reads_its_properties() {
        let p = props(vec![
            (
                "Address",
                OwnedVariant::from(zbus::wire::Str::from("AA:BB:CC:DD:EE:FF")),
            ),
            ("Alias", OwnedVariant::from(zbus::wire::Str::from("Pods"))),
            ("Connected", OwnedVariant::from(true)),
            ("Paired", OwnedVariant::from(true)),
            (
                "UUIDs",
                OwnedVariant::try_from(zbus::wire::Variant::from(vec![
                    "0000110b-0000-1000-8000-00805f9b34fb".to_string(),
                    pods_proto::AAP_UUID.to_string(),
                ]))
                .unwrap(),
            ),
        ]);
        let dev = BluezDevice::from_props("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF", &p);
        assert_eq!(dev.address, "AA:BB:CC:DD:EE:FF");
        assert_eq!(dev.alias, "Pods");
        assert!(dev.connected && dev.paired && dev.is_aap());
        assert_eq!(dev.adapter(), "/org/bluez/hci0");

        let mut d2 = dev.clone();
        assert!(!d2.update(&props(vec![("Connected", OwnedVariant::from(true))])));
        assert!(d2.update(&props(vec![("Connected", OwnedVariant::from(false))])));
        assert!(!d2.connected);
        // Unrelated properties change nothing the engine reads.
        assert!(!d2.update(&props(vec![("RSSI", OwnedVariant::from(-40i16))])));
    }

    #[test]
    fn a_device_without_the_aap_uuid_is_not_ours() {
        let dev = BluezDevice {
            uuids: vec!["0000110b-0000-1000-8000-00805f9b34fb".into()],
            ..Default::default()
        };
        assert!(!dev.is_aap());
    }

    #[test]
    fn device_paths() {
        assert_eq!(
            device_path("/org/bluez/hci0", "AA:BB:CC:DD:EE:FF"),
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"
        );
    }
}
