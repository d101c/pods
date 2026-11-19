// SPDX-License-Identifier: GPL-3.0-or-later
//! The AirPods' proximity keys (IRK and ENC), kept in the user's keyring
//! through the Secret Service (`org.freedesktop.secrets`, GNOME Keyring),
//! and in memory while the engine runs.
//!
//! Never in a file, never in the journal: [`ProximityKeys`] prints as
//! "withheld", and nothing here logs a value or an attribute beyond the
//! address prefix.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::wire::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

use pods_bt::BoxFuture;
use pods_proto::ProximityKeys;

/// Where the keys live.
pub trait KeyStore: Send + Sync {
    fn load<'a>(&'a self, address: &'a str)
    -> BoxFuture<'a, Result<Option<ProximityKeys>, String>>;
    fn save<'a>(
        &'a self,
        address: &'a str,
        keys: &'a ProximityKeys,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// The `xdg:schema` attribute of our keyring items (what Seahorse shows).
pub const SCHEMA: &str = "io.github.d101c.Pods.ProximityKeys";
const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Service",
    default_service = "org.freedesktop.secrets",
    default_path = "/org/freedesktop/secrets"
)]
trait Service {
    fn open_session(
        &self,
        algorithm: &str,
        input: &Variant<'_>,
    ) -> zbus::Result<(OwnedVariant, OwnedObjectPath)>;

    fn search_items(
        &self,
        attributes: HashMap<&str, &str>,
    ) -> zbus::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)>;

    fn unlock(
        &self,
        objects: &[ObjectPath<'_>],
    ) -> zbus::Result<(Vec<OwnedObjectPath>, OwnedObjectPath)>;

    #[allow(clippy::type_complexity)]
    fn get_secrets(
        &self,
        items: &[ObjectPath<'_>],
        session: &ObjectPath<'_>,
    ) -> zbus::Result<HashMap<OwnedObjectPath, (OwnedObjectPath, Vec<u8>, Vec<u8>, String)>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Collection",
    default_service = "org.freedesktop.secrets"
)]
trait Collection {
    fn create_item(
        &self,
        properties: HashMap<&str, Variant<'_>>,
        secret: &(ObjectPath<'_>, Vec<u8>, Vec<u8>, &str),
        replace: bool,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Session",
    default_service = "org.freedesktop.secrets"
)]
trait Session {
    fn close(&self) -> zbus::Result<()>;
}

/// The Secret Service on the session bus, spoken directly with zbus.
///
/// The `plain` session algorithm: the secret crosses the user's own
/// session bus only. A locked keyring is not unlocked by a prompt from a
/// background service; the keys then stay in memory until the next
/// connection asks again.
pub struct SecretService {
    conn: Connection,
}

impl SecretService {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    fn attributes(address: &str) -> HashMap<&str, &str> {
        HashMap::from([("xdg:schema", SCHEMA), ("address", address)])
    }

    async fn service(&self) -> zbus::Result<ServiceProxy<'static>> {
        ServiceProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
    }

    async fn session(&self, service: &ServiceProxy<'_>) -> zbus::Result<OwnedObjectPath> {
        let (_, path) = service.open_session("plain", &Variant::from("")).await?;
        Ok(path)
    }

    async fn close(&self, session: &OwnedObjectPath) {
        if let Ok(p) = SessionProxy::builder(&self.conn)
            .path(session.clone())
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
        {
            let _ = p.close().await;
        }
    }

    async fn load_inner(&self, address: &str) -> zbus::Result<Option<ProximityKeys>> {
        let service = self.service().await?;
        let (mut unlocked, locked) = service.search_items(Self::attributes(address)).await?;
        if unlocked.is_empty() && !locked.is_empty() {
            let objects: Vec<ObjectPath<'_>> = locked.iter().map(|p| p.as_ref()).collect();
            let (now_unlocked, prompt) = service.unlock(&objects).await?;
            if prompt.as_str() != "/" {
                warn!("the keyring is locked; the case card waits for it");
                return Ok(None);
            }
            unlocked = now_unlocked;
        }
        let Some(item) = unlocked.first() else {
            return Ok(None);
        };
        let session = self.session(&service).await?;
        let secrets = service
            .get_secrets(&[item.as_ref()], &session.as_ref())
            .await;
        self.close(&session).await;
        let secrets = secrets?;
        Ok(secrets
            .get(item)
            .and_then(|(_, _, value, _)| ProximityKeys::from_bytes(value)))
    }

    async fn save_inner(&self, address: &str, keys: &ProximityKeys) -> zbus::Result<bool> {
        let service = self.service().await?;
        let session = self.session(&service).await?;
        let collection = CollectionProxy::builder(&self.conn)
            .path(DEFAULT_COLLECTION)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let short: String = address.chars().take(8).collect();
        let label = format!("AirPods proximity keys ({short}…)");
        let attributes: HashMap<String, String> = Self::attributes(address)
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let properties = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", Variant::from(label)),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Variant::from(attributes),
            ),
        ]);
        let secret = (
            session.as_ref(),
            Vec::new(),
            keys.to_bytes().to_vec(),
            "application/octet-stream",
        );
        let created = collection.create_item(properties, &secret, true).await;
        self.close(&session).await;
        let (_, prompt) = created?;
        Ok(prompt.as_str() == "/")
    }
}

impl KeyStore for SecretService {
    fn load<'a>(
        &'a self,
        address: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProximityKeys>, String>> {
        Box::pin(async move { self.load_inner(address).await.map_err(|e| e.to_string()) })
    }

    fn save<'a>(
        &'a self,
        address: &'a str,
        keys: &'a ProximityKeys,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match self.save_inner(address, keys).await {
                Ok(true) => Ok(()),
                Ok(false) => Err("the keyring is locked".into()),
                Err(e) => Err(e.to_string()),
            }
        })
    }
}

/// In memory only: for tests, and when no Secret Service answers.
#[derive(Default)]
pub struct MemoryStore {
    keys: Mutex<HashMap<String, ProximityKeys>>,
}

impl KeyStore for MemoryStore {
    fn load<'a>(
        &'a self,
        address: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProximityKeys>, String>> {
        let k = self.keys.lock().expect("lock").get(address).cloned();
        Box::pin(async move { Ok(k) })
    }

    fn save<'a>(
        &'a self,
        address: &'a str,
        keys: &'a ProximityKeys,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.keys
            .lock()
            .expect("lock")
            .insert(address.to_string(), keys.clone());
        Box::pin(async { Ok(()) })
    }
}

/// The keys the engine knows, by AirPods address, backed by a [`KeyStore`].
pub struct KeyRing {
    store: Arc<dyn KeyStore>,
    cache: Mutex<HashMap<String, ProximityKeys>>,
    version: watch::Sender<u64>,
}

fn short(address: &str) -> String {
    address.chars().take(8).collect()
}

impl KeyRing {
    pub fn new(store: Arc<dyn KeyStore>) -> Self {
        Self {
            store,
            cache: Mutex::new(HashMap::new()),
            version: watch::channel(0).0,
        }
    }

    pub fn get(&self, address: &str) -> Option<ProximityKeys> {
        self.cache.lock().expect("lock").get(address).cloned()
    }

    /// Every pair of keys known, with its AirPods address.
    pub fn all(&self) -> Vec<(String, ProximityKeys)> {
        self.cache
            .lock()
            .expect("lock")
            .iter()
            .map(|(a, k)| (a.clone(), k.clone()))
            .collect()
    }

    /// Bumped whenever the set of keys changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    /// The current value of [`Self::subscribe`].
    pub fn version(&self) -> u64 {
        *self.version.borrow()
    }

    fn remember(&self, address: &str, keys: ProximityKeys) {
        let changed = self
            .cache
            .lock()
            .expect("lock")
            .insert(address.to_string(), keys.clone())
            .is_none_or(|old| old != keys);
        if changed {
            self.version.send_modify(|v| *v += 1);
        }
    }

    /// The keys of `address`, from memory or the store.
    pub async fn load(&self, address: &str) -> Option<ProximityKeys> {
        if let Some(k) = self.get(address) {
            return Some(k);
        }
        match self.store.load(address).await {
            Ok(Some(k)) => {
                debug!(address = %short(address), "proximity keys read from the keyring");
                self.remember(address, k.clone());
                Some(k)
            }
            Ok(None) => None,
            Err(e) => {
                debug!(error = %e, "keyring not readable");
                None
            }
        }
    }

    /// Keys the buds just sent: used at once, kept in the store.
    pub async fn save(&self, address: &str, keys: ProximityKeys) {
        self.remember(address, keys.clone());
        match self.store.save(address, &keys).await {
            Ok(()) => info!(address = %short(address), "proximity keys kept in the keyring"),
            Err(e) => {
                warn!(error = %e, "proximity keys not kept in the keyring (used until the engine stops)")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(b: u8) -> ProximityKeys {
        ProximityKeys {
            irk: [b; 16],
            enc: [b ^ 0xFF; 16],
        }
    }

    #[tokio::test]
    async fn the_ring_reads_through_and_tells_of_changes() {
        let store = Arc::new(MemoryStore::default());
        store.save("AA", &keys(1)).await.unwrap();
        let ring = KeyRing::new(store.clone());
        let mut v = ring.subscribe();
        assert_eq!(ring.get("AA"), None);
        assert_eq!(ring.load("AA").await, Some(keys(1)));
        assert!(v.has_changed().unwrap());
        v.borrow_and_update();
        assert_eq!(ring.load("BB").await, None);
        ring.save("BB", keys(2)).await;
        assert!(v.has_changed().unwrap());
        assert_eq!(store.load("BB").await.unwrap(), Some(keys(2)));
        let mut all = ring.all();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(all, [("AA".into(), keys(1)), ("BB".into(), keys(2))]);
        v.borrow_and_update();
        // The same keys again: no change.
        ring.save("BB", keys(2)).await;
        assert!(!v.has_changed().unwrap());
    }
}
