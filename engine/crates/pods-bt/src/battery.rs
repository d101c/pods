// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.bluez.BatteryProvider1`: gives BlueZ (and through it UPower and
//! GNOME's Power panel) one level for the AirPods, the lowest bud.

use std::collections::HashSet;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use zbus::fdo::ObjectManager;
use zbus::wire::{ObjectPath, OwnedObjectPath};
use zbus::{Connection, interface};

/// Root of our provider objects on the bus BlueZ listens to.
pub const PROVIDER_ROOT: &str = "/io/github/d101c/Pods/battery";

struct BatteryObject {
    device: OwnedObjectPath,
    percentage: u8,
}

#[interface(name = "org.bluez.BatteryProvider1")]
impl BatteryObject {
    #[zbus(property)]
    fn device(&self) -> OwnedObjectPath {
        self.device.clone()
    }

    #[zbus(property)]
    fn percentage(&self) -> u8 {
        self.percentage
    }

    #[zbus(property)]
    fn source(&self) -> String {
        "pods".to_string()
    }
}

#[zbus::proxy(
    interface = "org.bluez.BatteryProviderManager1",
    default_service = "org.bluez"
)]
trait BatteryProviderManager1 {
    fn register_battery_provider(&self, provider: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// Our battery provider, registered once per adapter on first use.
pub struct BatteryProvider {
    conn: Connection,
    registered: Mutex<HashSet<String>>,
}

impl BatteryProvider {
    /// Serves the object manager BlueZ reads the providers from.
    pub async fn new(conn: Connection) -> zbus::Result<Self> {
        conn.object_server()
            .at(PROVIDER_ROOT, ObjectManager)
            .await?;
        Ok(Self {
            conn,
            registered: Mutex::new(HashSet::new()),
        })
    }

    fn child(device_path: &str) -> String {
        let leaf = device_path.rsplit('/').next().unwrap_or("dev");
        format!("{PROVIDER_ROOT}/{leaf}")
    }

    /// Publishes `percent` for the device at `device_path` on `adapter`,
    /// or withdraws it with `None`.
    pub async fn set(&self, adapter: &str, device_path: &str, percent: Option<u8>) {
        let child = Self::child(device_path);
        let server = self.conn.object_server();
        let Some(percent) = percent else {
            match server.remove::<BatteryObject, _>(child.as_str()).await {
                Ok(true) => debug!(device = device_path, "battery withdrawn"),
                Ok(false) => {}
                Err(e) => debug!(error = %e, "battery withdraw failed"),
            }
            return;
        };
        let percent = percent.min(100);
        match server.interface::<_, BatteryObject>(child.as_str()).await {
            Ok(iface) => {
                if iface.get().await.percentage != percent {
                    iface.get_mut().await.percentage = percent;
                    let _ = iface
                        .get()
                        .await
                        .percentage_changed(iface.signal_emitter())
                        .await;
                }
            }
            Err(_) => {
                let Ok(device) = OwnedObjectPath::try_from(device_path.to_string()) else {
                    warn!(device = device_path, "not an object path");
                    return;
                };
                if let Err(e) = server
                    .at(
                        child.as_str(),
                        BatteryObject {
                            device,
                            percentage: percent,
                        },
                    )
                    .await
                {
                    warn!(error = %e, "could not publish the battery");
                    return;
                }
            }
        }
        self.register(adapter).await;
    }

    async fn register(&self, adapter: &str) {
        let mut done = self.registered.lock().await;
        if done.contains(adapter) {
            return;
        }
        // Tried once per adapter, whatever the outcome: a BlueZ without
        // the battery API will not grow one.
        done.insert(adapter.to_string());
        let proxy = BatteryProviderManager1Proxy::builder(&self.conn)
            .path(adapter.to_string())
            .build()
            .await;
        let result = match proxy {
            Ok(p) => {
                p.register_battery_provider(&ObjectPath::from_static_str_unchecked(PROVIDER_ROOT))
                    .await
            }
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => info!(adapter, "battery provider registered"),
            Err(e) => warn!(adapter, error = %e, "BlueZ refused the battery provider"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_paths() {
        assert_eq!(
            BatteryProvider::child("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
            "/io/github/d101c/Pods/battery/dev_AA_BB_CC_DD_EE_FF"
        );
    }
}
