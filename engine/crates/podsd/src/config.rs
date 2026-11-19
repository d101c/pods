// SPDX-License-Identifier: GPL-3.0-or-later
//! The engine's own settings, in `~/.config/pods/config.toml`.
//!
//! ```toml
//! low_battery_alerts = true
//! watchdog_secs = 60   # off unless set
//!
//! [devices."AA:BB:CC:DD:EE:FF"]
//! press_hold_modes = ["transparency", "anc"]
//! press_hold_cycle = ["off", "anc", "transparency"]
//! color = "white"
//! ear_detection = true
//! pause_on_remove = true
//! resume_on_return = true
//! spatial_mode = "head"     # "off", "fixed" or "head"
//! follow_screen = true
//! ```
//!
//! The engine is the only writer; the extension goes through D-Bus. The
//! proximity keys are never here: they live in the keyring.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::warn;

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "yes")]
    pub low_battery_alerts: bool,
    /// Seconds without a frame, while BlueZ says connected, before the
    /// AAP link is reopened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub devices: BTreeMap<String, DeviceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            low_battery_alerts: true,
            watchdog_secs: None,
            devices: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// What the user chose; rewritten to the buds at every connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press_hold_modes: Option<Vec<String>>,
    /// The press-and-hold cycle as last read from the buds or learned
    /// from the stem, in the order the buds walk it. The Pro 1 never
    /// reports it on connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press_hold_cycle: Option<Vec<String>>,
    /// Colour from the BLE advertisement, so it is known before the case
    /// is opened again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Last value written. The Pro 1 does not report it on connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ear_detection: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_on_remove: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_on_return: Option<bool>,
    /// "off", "fixed" or "head"; applied at every connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spatial_mode: Option<String>,
    /// On unless set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_screen: Option<bool>,
}

/// Where the file lives: systemd's `$CONFIGURATION_DIRECTORY` when the
/// unit sets `ConfigurationDirectory=`, else `$XDG_CONFIG_HOME/pods`,
/// else `~/.config/pods`.
pub fn default_path() -> PathBuf {
    let dir = std::env::var_os("CONFIGURATION_DIRECTORY")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|d| PathBuf::from(d).join("pods")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/pods")))
        .unwrap_or_else(|| PathBuf::from(".config/pods"));
    dir.join("config.toml")
}

/// The settings and the file behind them.
#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
    data: Mutex<Config>,
}

impl ConfigStore {
    /// Reads `path`. A missing file is the defaults. A file that does not
    /// parse is kept aside as `config.toml.broken` rather than overwritten.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let data = match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(c) => c,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "config does not parse; using defaults");
                    let _ = std::fs::rename(&path, path.with_extension("toml.broken"));
                    Config::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "config unreadable; using defaults");
                Config::default()
            }
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self) -> Config {
        self.data.lock().expect("config lock").clone()
    }

    pub fn device(&self, address: &str) -> DeviceConfig {
        self.get().devices.get(address).cloned().unwrap_or_default()
    }

    /// Changes the settings and writes them out.
    pub fn update(&self, f: impl FnOnce(&mut Config)) -> io::Result<()> {
        let snapshot = {
            let mut data = self.data.lock().expect("config lock");
            f(&mut data);
            data.clone()
        };
        write_atomically(&self.path, &snapshot)
    }

    pub fn update_device(
        &self,
        address: &str,
        f: impl FnOnce(&mut DeviceConfig),
    ) -> io::Result<()> {
        self.update(|c| f(c.devices.entry(address.to_string()).or_default()))
    }
}

fn write_atomically(path: &Path, config: &Config) -> io::Result<()> {
    let text = toml::to_string_pretty(config).map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::load(dir.path().join("pods/config.toml"));
        assert_eq!(store.get(), Config::default());
        assert!(store.get().low_battery_alerts);
    }

    #[test]
    fn writes_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pods/config.toml");
        let store = ConfigStore::load(&path);
        store
            .update_device("AA:BB:CC:DD:EE:FF", |d| {
                d.press_hold_modes = Some(vec!["off".into(), "anc".into()]);
                d.pause_on_remove = Some(false);
            })
            .unwrap();
        let again = ConfigStore::load(&path);
        let d = again.device("AA:BB:CC:DD:EE:FF");
        assert_eq!(d.press_hold_modes.unwrap(), ["off", "anc"]);
        assert_eq!(d.pause_on_remove, Some(false));
        assert_eq!(d.ear_detection, None);
        assert_eq!(again.device("00:00:00:00:00:00"), DeviceConfig::default());

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_broken_file_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "low_battery_alerts = [not toml").unwrap();
        let store = ConfigStore::load(&path);
        assert_eq!(store.get(), Config::default());
        assert!(dir.path().join("config.toml.broken").exists());
    }

    #[test]
    fn hand_written_settings_parse() {
        let c: Config = toml::from_str(
            "low_battery_alerts = false\nwatchdog_secs = 90\n[devices.\"AA:BB:CC:DD:EE:FF\"]\near_detection = false\n",
        )
        .unwrap();
        assert!(!c.low_battery_alerts);
        assert_eq!(c.watchdog_secs, Some(90));
        assert_eq!(c.devices["AA:BB:CC:DD:EE:FF"].ear_detection, Some(false));
        let c: Config = toml::from_str(
            "[devices.\"AA:BB:CC:DD:EE:FF\"]\npress_hold_cycle = [\"off\", \"anc\", \"transparency\"]\ncolor = \"black\"\n",
        )
        .unwrap();
        let d = &c.devices["AA:BB:CC:DD:EE:FF"];
        assert_eq!(
            d.press_hold_cycle.as_deref().unwrap(),
            ["off", "anc", "transparency"]
        );
        assert_eq!(d.color.as_deref(), Some("black"));
        assert_eq!(d.spatial_mode, None);
        let c: Config = toml::from_str(
            "[devices.\"AA:BB:CC:DD:EE:FF\"]\nspatial_mode = \"head\"\nfollow_screen = false\n",
        )
        .unwrap();
        let d = &c.devices["AA:BB:CC:DD:EE:FF"];
        assert_eq!(d.spatial_mode.as_deref(), Some("head"));
        assert_eq!(d.follow_screen, Some(false));
    }
}
