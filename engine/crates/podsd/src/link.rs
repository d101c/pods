// SPDX-License-Identifier: GPL-3.0-or-later
// The supervision ideas (retry after BlueZ reports a connection, reopen a
// mute link, the end-of-session packet) come from podctl
// `src/bin/daemon/{aap,link}.rs` (https://github.com/Rockykln/podctl),
// Copyright (c) 2026 pods contributors, MIT OR Apache-2.0. See engine/NOTICE.
//! One task per pair of AirPods: opens the AAP link while BlueZ says the
//! buds are connected, keeps it open, and turns what arrives into
//! properties and signals.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use zbus::object_server::InterfaceRef;

use pods_bt::{AapConnector, AapLink, BatteryProvider};
use pods_proto::{Message, decode, packet};

use crate::api::{self, Command, DeviceCtx, DeviceIface, ManagerCtx, ManagerIface};
use crate::conflict;
use crate::keys::KeyRing;
use crate::media::{Media, Policy};
use crate::spatial::SpatialMsg;
use crate::state::Prop;

/// Timings of the supervision. Tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// First wait after a failed attempt; doubles up to `backoff_max`.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// No frame for this long while BlueZ says connected: reopen. Off by default:
    /// AirPods Pro (1st generation) stay silent for minutes when nothing changes, and a
    /// dead link already shows as a socket error or as BlueZ dropping the device.
    pub watchdog: Option<Duration>,
    /// Leave the measured pauses between the opening packets.
    pub opening_pauses: bool,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(32),
            watchdog: None,
            opening_pauses: true,
        }
    }
}

/// 2, 4, 8, 16, 32, 32… seconds.
#[derive(Debug)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            next: min,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(self.max);
        d
    }

    pub fn reset(&mut self) {
        self.next = self.min;
    }
}

/// What the device tasks share with the rest of the engine.
pub struct Hooks {
    pub manager: InterfaceRef<ManagerIface>,
    pub manager_ctx: Arc<ManagerCtx>,
    pub battery: Option<Arc<BatteryProvider>>,
    pub media: Arc<Media>,
    /// Where to look for other AAP clients (`/proc`), `None` to not look.
    pub proc_root: Option<PathBuf>,
    /// The proximity keys of every pair, for the BLE monitor.
    pub keys: Arc<KeyRing>,
    /// Something the case card shows changed on the AAP side (connected,
    /// battery, name): an open card may need sending again.
    pub case_refresh: Notify,
}

impl Hooks {
    async fn set_active(
        &self,
        path: Option<&zbus::wire::OwnedObjectPath>,
        mine: &zbus::wire::OwnedObjectPath,
    ) {
        let changed = {
            let mut active = self.manager_ctx.active.lock().expect("lock");
            match path {
                Some(p) => {
                    let changed = active.as_ref() != Some(p);
                    *active = Some(p.clone());
                    changed
                }
                // Only clear it if it is still us.
                None if active.as_ref() == Some(mine) => {
                    *active = None;
                    true
                }
                None => false,
            }
        };
        if changed {
            api::emit_manager(&self.manager, "ActiveDevice").await;
        }
    }

    async fn check_conflict(&self) {
        let Some(root) = &self.proc_root else {
            return;
        };
        let found = conflict::scan(root).unwrap_or("").to_string();
        if !found.is_empty() {
            warn!(client = %found, "another AAP client is running; the AirPods answer only one");
        }
        let changed = {
            let mut c = self.manager_ctx.conflict.lock().expect("lock");
            std::mem::replace(&mut *c, found.clone()) != found
        };
        if changed {
            api::emit_manager(&self.manager, "Conflict").await;
        }
    }
}

enum End {
    /// The buds closed the channel, a read or write failed.
    Closed,
    /// Nothing heard for `watchdog`.
    Watchdog,
    /// BlueZ says the buds are gone.
    BluezGone,
    /// The device was removed or the engine stops.
    Shutdown,
}

pub struct DeviceTask {
    pub ctx: Arc<DeviceCtx>,
    pub iface: InterfaceRef<DeviceIface>,
    pub connector: Arc<dyn AapConnector>,
    pub bluez_connected: watch::Receiver<bool>,
    pub commands: mpsc::UnboundedReceiver<Command>,
    pub hooks: Arc<Hooks>,
    pub timing: Timing,
}

fn reject(cmd: Command) {
    let Command::Send { reply, .. } = cmd;
    let _ = reply.send(Err("the AirPods are not connected".into()));
}

impl DeviceTask {
    pub async fn run(mut self) {
        let mut backoff = Backoff::new(self.timing.backoff_min, self.timing.backoff_max);
        // Reopening after the watchdog: Connected stays true meanwhile.
        let mut silent = false;
        loop {
            if !self.wait_bluez().await {
                break;
            }
            let address = self.ctx.address();
            debug!(address = %short(&address), "opening the AAP link");
            let attempt = tokio::select! {
                r = self.connector.connect(&address) => Some(r),
                gone = bluez_goes_away(&mut self.bluez_connected) => {
                    if !gone { break; }
                    None
                }
            };
            let link = match attempt {
                None => {
                    self.closed(&mut silent).await;
                    backoff.reset();
                    continue;
                }
                Some(Err(e)) => {
                    debug!(error = %e, "AAP connect failed");
                    self.closed(&mut silent).await;
                    if !self.pause(backoff.next_delay()).await {
                        break;
                    }
                    continue;
                }
                Some(Ok(link)) => link,
            };
            backoff.reset();
            let end = self.session(link.as_ref(), silent).await;
            drop(link);
            self.ctx.spatial(SpatialMsg::Session(false));
            match end {
                End::Watchdog => {
                    info!("AAP link silent; reopening it");
                    silent = true;
                }
                End::Closed => {
                    info!("AAP link closed");
                    self.closed(&mut silent).await;
                    if !self.pause(backoff.next_delay()).await {
                        break;
                    }
                }
                End::BluezGone => {
                    info!("AirPods disconnected");
                    self.closed(&mut silent).await;
                }
                End::Shutdown => break,
            }
        }
        self.closed(&mut silent).await;
        debug!("device task ends");
    }

    /// Waits until BlueZ says connected, refusing orders meanwhile.
    /// False when the task must end.
    async fn wait_bluez(&mut self) -> bool {
        loop {
            if *self.bluez_connected.borrow_and_update() {
                return true;
            }
            tokio::select! {
                r = self.bluez_connected.changed() => if r.is_err() { return false },
                cmd = self.commands.recv() => match cmd {
                    Some(c) => reject(c),
                    None => return false,
                },
            }
        }
    }

    /// Sleeps `delay` unless BlueZ drops the buds first. False when the task must end.
    async fn pause(&mut self, delay: Duration) -> bool {
        let deadline = tokio::time::sleep(delay);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return true,
                r = self.bluez_connected.changed() => {
                    if r.is_err() { return false; }
                    if !*self.bluez_connected.borrow() { return true; }
                }
                cmd = self.commands.recv() => match cmd {
                    Some(c) => reject(c),
                    None => return false,
                },
            }
        }
    }

    /// The link is down for good (not a silent reopen).
    async fn closed(&self, silent: &mut bool) {
        *silent = false;
        let props = self.ctx.state().set_connected(false);
        if props.is_empty() {
            return;
        }
        api::emit_props(&self.iface, &props).await;
        self.ctx.spatial(SpatialMsg::Disconnected);
        self.hooks.case_refresh.notify_one();
        self.hooks.set_active(None, &self.ctx.path).await;
        if let Some(b) = &self.hooks.battery {
            let (adapter, path) = self.bluez_paths();
            b.set(&adapter, &path, None).await;
        }
        self.hooks.media.link_dropped().await;
    }

    fn bluez_paths(&self) -> (String, String) {
        let path = self.ctx.state().bluez_path.clone();
        let adapter = path.rsplit_once('/').map_or("/", |(p, _)| p).to_string();
        (adapter, path)
    }

    async fn session(&mut self, link: &dyn AapLink, silent: bool) -> End {
        for (pkt, pause) in packet::opening() {
            if let Err(e) = link.send(pkt).await {
                debug!(error = %e, "opening write failed");
                return End::Closed;
            }
            if self.timing.opening_pauses {
                tokio::time::sleep(Duration::from_millis(pause)).await;
            }
        }
        // The press-and-hold cycle is reset by other hosts: write ours back.
        let rewrite = {
            let s = self.ctx.state();
            let has_choice = self
                .ctx
                .config
                .device(&s.address)
                .press_hold_modes
                .is_some();
            (s.model().press_hold && has_choice).then(|| s.press_hold_modes())
        };
        if let Some(modes) = rewrite
            && modes.len() >= 2
            && let Err(e) = link.send(&packet::set_press_hold_modes(&modes)).await
        {
            debug!(error = %e, "press-and-hold write failed");
            return End::Closed;
        }

        let props = {
            let mut s = self.ctx.state();
            s.begin_session();
            s.set_connected(true)
        };
        api::emit_props(&self.iface, &props).await;
        // Before the first frame is read: the spatial task sees every one.
        self.ctx.spatial(SpatialMsg::Session(true));
        self.hooks
            .set_active(Some(&self.ctx.path), &self.ctx.path)
            .await;
        if !silent {
            info!("AAP link open");
            self.hooks.check_conflict().await;
        }
        self.hooks.case_refresh.notify_one();
        self.ask_for_keys();

        let mut last_rx = Instant::now();
        loop {
            tokio::select! {
                r = link.recv() => match r {
                    Ok(packet) => {
                        last_rx = Instant::now();
                        if self.handle(&packet).await {
                            warn!("the AirPods ended the session (another host took them?)");
                            return End::Closed;
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "AAP read ended");
                        return End::Closed;
                    }
                },
                _ = async {
                    match self.timing.watchdog {
                        Some(w) => tokio::time::sleep_until(last_rx + w).await,
                        None => std::future::pending().await,
                    }
                } => return End::Watchdog,
                cmd = self.commands.recv() => match cmd {
                    Some(Command::Send { packet, reply }) => {
                        let r = link.send(&packet).await.map_err(|e| e.to_string());
                        let failed = r.is_err();
                        let _ = reply.send(r);
                        if failed {
                            return End::Closed;
                        }
                    }
                    None => return End::Shutdown,
                },
                r = self.bluez_connected.changed() => {
                    if r.is_err() {
                        return End::Shutdown;
                    }
                    if !*self.bluez_connected.borrow() {
                        return End::BluezGone;
                    }
                }
            }
        }
    }

    /// The first time these AirPods are seen (no keys in the keyring),
    /// ask them for their proximity keys. Off the reader's path: the
    /// keyring may take a moment to answer.
    fn ask_for_keys(&self) {
        let ctx = self.ctx.clone();
        let keys = self.hooks.keys.clone();
        tokio::spawn(async move {
            let address = ctx.address();
            if keys.load(&address).await.is_some() {
                return;
            }
            match ctx.send(packet::request_keys()).await {
                Ok(()) => info!("asked the AirPods for their proximity keys"),
                Err(e) => debug!(error = ?e, "key request not sent"),
            }
        });
    }

    /// One received packet. True when the buds ended the session.
    async fn handle(&self, packet: &[u8]) -> bool {
        // Every frame, sensor ones included (pods-motion picks its own).
        self.ctx.spatial(SpatialMsg::Frame(packet.to_vec()));
        let Some(msg) = decode(packet) else {
            debug!(len = packet.len(), "not an AAP packet");
            return false;
        };
        if let Message::Keys(k) = msg {
            info!("proximity keys received");
            let keys = self.hooks.keys.clone();
            let address = self.ctx.address();
            tokio::spawn(async move { keys.save(&address, k).await });
            return false;
        }
        let (outcome, policy, bluez_path, lowest) = {
            let mut s = self.ctx.state();
            let o = s.apply(&msg, Instant::now());
            let policy = Policy {
                ear_detection: s.ear_detection(),
                pause_on_remove: s.pause_on_remove,
                resume_on_return: s.resume_on_return,
            };
            (o, policy, s.bluez_path.clone(), s.lowest_bud())
        };
        api::emit_props(&self.iface, &outcome.props).await;
        if outcome.press_hold_learned {
            self.keep_press_hold();
        }
        if outcome.props.contains(&Prop::Model) {
            self.ctx.spatial(SpatialMsg::Model);
        }
        if outcome.wearing_changed.is_some() {
            self.ctx.spatial(SpatialMsg::Wearing);
        }
        if outcome
            .props
            .iter()
            .any(|p| matches!(p, Prop::Battery | Prop::Name | Prop::Model))
        {
            self.hooks.case_refresh.notify_one();
        }
        if let Some((mode, source)) = outcome.noise_changed {
            info!(
                mode = mode.as_str(),
                source = source.as_str(),
                "listening mode changed"
            );
            api::emit_noise_changed(&self.iface, mode, source).await;
        }
        for (part, pct) in &outcome.low_battery {
            info!(part = part.as_str(), pct, "low battery");
            api::emit_low_battery(&self.iface, part.as_str(), *pct).await;
        }
        if outcome.props.contains(&crate::state::Prop::Battery)
            && let Some(b) = &self.hooks.battery
        {
            let (adapter, path) = self.bluez_paths();
            b.set(&adapter, &path, lowest).await;
        }
        if let Some((before, after)) = outcome.wearing_changed {
            // Media calls may wait on players: never hold up the reader.
            let media = self.hooks.media.clone();
            tokio::spawn(async move { media.on_wearing(&bluez_path, before, after, policy).await });
        }
        outcome.session_end
    }
}

impl DeviceTask {
    /// The press-and-hold cycle the buds showed, for the next start.
    fn keep_press_hold(&self) {
        let (address, words) = {
            let s = self.ctx.state();
            let words: Vec<String> = s
                .press_hold_modes()
                .iter()
                .map(|m| m.as_str().to_string())
                .collect();
            (s.address.clone(), words)
        };
        info!(cycle = ?words, "press-and-hold cycle read from the AirPods");
        if let Err(e) = self
            .ctx
            .config
            .update_device(&address, |d| d.press_hold_cycle = Some(words))
        {
            warn!(error = %e, "could not save the press-and-hold cycle");
        }
    }
}

/// Resolves when BlueZ reports the buds gone (true) or the sender is
/// dropped (false).
async fn bluez_goes_away(rx: &mut watch::Receiver<bool>) -> bool {
    loop {
        if rx.changed().await.is_err() {
            return false;
        }
        if !*rx.borrow() {
            return true;
        }
    }
}

/// First three bytes of an address, for logs.
fn short(address: &str) -> String {
    address.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_ceiling() {
        let mut b = Backoff::new(Duration::from_secs(2), Duration::from_secs(32));
        let seq: Vec<u64> = (0..7).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(seq, [2, 4, 8, 16, 32, 32, 32]);
        b.reset();
        assert_eq!(b.next_delay().as_secs(), 2);
    }

    #[test]
    fn default_timings_are_the_documented_ones() {
        let t = Timing::default();
        assert_eq!(t.backoff_min, Duration::from_secs(2));
        assert_eq!(t.backoff_max, Duration::from_secs(32));
        assert_eq!(t.watchdog, None);
    }
}
