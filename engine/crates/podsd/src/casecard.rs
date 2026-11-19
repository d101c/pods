// SPDX-License-Identifier: GPL-3.0-or-later
//! The BLE side of the case card: advertisements from the monitor in,
//! `CaseOpened` / `CaseClosed` and the `Color` property out.
//!
//! An advertisement counts only when its private address resolves with
//! the IRK of a pair of AirPods paired here: the neighbour's AirPods never
//! open a card. The address resolution is cached per BlueZ object until
//! the set of keys changes.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::time::Instant;
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::wire::ObjectPath;

use pods_bt::BleEvent;
use pods_bt::monitor::{self, THRESHOLDS};
use pods_proto::ble::{Advert, Levels, parse_advert};

use crate::api::{DeviceCtx, ManagerIface};
use crate::case::{CardThrottle, LidChange, LidMachine, LidReading, Snapshot};
use crate::engine::Engine;
use crate::state::Prop;

/// One pair's card.
#[derive(Default)]
struct Card {
    lid: LidMachine,
    throttle: CardThrottle,
    /// The last advertisement and its levels.
    last: Option<(Advert, Levels)>,
    /// The BlueZ object it came from (the address rotates).
    ble_path: Option<String>,
}

impl Card {
    fn deadline(&self) -> Option<Instant> {
        [self.lid.deadline(), self.throttle.deadline()]
            .into_iter()
            .flatten()
            .min()
    }
}

/// How often keys missing at start are asked of the keyring again (it
/// may not have been unlocked yet when the engine started).
const KEYRING_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// Our AirPods' address for a BlueZ object, `None` for somebody else's,
/// with the version of the key ring it was worked out with: a "nobody"
/// goes stale as soon as new keys arrive.
type Owners = HashMap<String, (u64, Option<String>)>;

impl Engine {
    pub(crate) async fn follow_ble(self: Arc<Self>, system: Connection) {
        let mut events = match monitor::start(system).await {
            Ok(rx) => rx,
            Err(e) => {
                warn!(error = %e, "no BLE monitor: no case card");
                return;
            }
        };
        let mut keys_version = self.hooks.keys.subscribe();
        let mut owners = Owners::new();
        let mut keyring_asked: Option<Instant> = None;
        let mut cards: HashMap<String, Card> = HashMap::new();
        loop {
            let deadline = cards.values().filter_map(Card::deadline).min();
            tokio::select! {
                ev = events.recv() => match ev {
                    None => {
                        warn!("BLE monitor stopped");
                        return;
                    }
                    Some(BleEvent::Advert { device, address, rssi, data }) => {
                        if rssi.is_some_and(|r| r < THRESHOLDS.high_rssi) {
                            continue;
                        }
                        if !self
                            .on_advert(&mut cards, &mut owners, device, &address, &data)
                            .await
                        {
                            self.reload_missing_keys(&mut keyring_asked).await;
                        }
                    }
                    Some(BleEvent::Lost { device }) => {
                        for (address, card) in cards.iter_mut() {
                            if card.ble_path.as_deref() == Some(device.as_str())
                                && card.lid.lost() == Some(LidChange::Closed)
                            {
                                card.throttle.reset();
                                self.case_closed(address).await;
                            }
                        }
                    }
                },
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => self.poll_cards(&mut cards).await,
                r = keys_version.changed() => {
                    if r.is_err() {
                        return;
                    }
                    owners.clear();
                }
                _ = self.hooks.case_refresh.notified() => self.refresh_cards(&mut cards).await,
            }
        }
    }

    /// The pair whose IRK resolves `address`.
    fn owner(&self, owners: &mut Owners, ble_path: &str, address: &str) -> Option<String> {
        let version = self.hooks.keys.version();
        if let Some((v, known)) = owners.get(ble_path)
            && (known.is_some() || *v == version)
        {
            return known.clone();
        }
        let found = self
            .hooks
            .keys
            .all()
            .into_iter()
            .find(|(_, k)| k.resolves(address))
            .map(|(a, _)| a);
        owners.insert(ble_path.to_string(), (version, found.clone()));
        found
    }

    /// An advertisement nobody's keys resolve: perhaps ours, with keys the
    /// keyring could not give at start. Ask it again, now and then.
    async fn reload_missing_keys(&self, asked: &mut Option<Instant>) {
        if asked.is_some_and(|t| t.elapsed() < KEYRING_RETRY) {
            return;
        }
        *asked = Some(Instant::now());
        let missing: Vec<String> = self
            .devices
            .lock()
            .await
            .keys()
            .filter(|a| self.hooks.keys.get(a).is_none())
            .cloned()
            .collect();
        for address in missing {
            let keys = self.hooks.keys.clone();
            tokio::spawn(async move { keys.load(&address).await });
        }
    }

    async fn ctx(&self, address: &str) -> Option<Arc<DeviceCtx>> {
        self.devices
            .lock()
            .await
            .get(address)
            .map(|e| e.ctx.clone())
    }

    /// One advertisement. False when no known keys resolve its address.
    async fn on_advert(
        &self,
        cards: &mut HashMap<String, Card>,
        owners: &mut Owners,
        ble_path: String,
        address: &str,
        data: &[u8],
    ) -> bool {
        let Some(owner) = self.owner(owners, &ble_path, address) else {
            return false;
        };
        let Some(adv) = parse_advert(data) else {
            return true;
        };
        let Some(ctx) = self.ctx(&owner).await else {
            return true;
        };
        let plain = self.hooks.keys.get(&owner).and_then(|k| k.decrypt(data));
        let levels = adv.levels(plain.as_ref());

        let color = adv.color_name();
        if !color.is_empty() && ctx.state().set_color(color) {
            self.emit_device(&ctx, &[Prop::Color]).await;
            if let Err(e) = self
                .config
                .update_device(&owner, |d| d.color = Some(color.to_string()))
            {
                debug!(error = %e, "colour not saved");
            }
        }

        let card = cards.entry(owner.clone()).or_default();
        card.ble_path = Some(ble_path);
        card.last = Some((adv, levels));
        let now = Instant::now();
        match card.lid.reading(now, LidReading::from(&adv)) {
            Some(LidChange::Opened) => {
                let s = card
                    .throttle
                    .opened(now, ctx.state().snapshot(&adv, levels));
                info!("case opened");
                self.case_opened(&ctx, &s).await;
            }
            Some(LidChange::Closed) => {
                card.throttle.reset();
                info!("case closed");
                self.case_closed(&owner).await;
            }
            None if card.lid.is_open() => {
                let s = ctx.state().snapshot(&adv, levels);
                if let Some(s) = card.throttle.offer(now, s) {
                    self.case_opened(&ctx, &s).await;
                }
            }
            None => {}
        }
        true
    }

    async fn poll_cards(&self, cards: &mut HashMap<String, Card>) {
        let now = Instant::now();
        for (address, card) in cards.iter_mut() {
            match card.lid.poll(now) {
                Some(LidChange::Opened) => {
                    if let (Some(ctx), Some((adv, levels))) = (self.ctx(address).await, card.last) {
                        let s = card
                            .throttle
                            .opened(now, ctx.state().snapshot(&adv, levels));
                        info!("case opened");
                        self.case_opened(&ctx, &s).await;
                    }
                }
                Some(LidChange::Closed) => {
                    card.throttle.reset();
                    info!("case closed (silent)");
                    self.case_closed(address).await;
                }
                None => {}
            }
            if let Some(s) = card.throttle.poll(now)
                && let Some(ctx) = self.ctx(address).await
            {
                self.case_opened(&ctx, &s).await;
            }
        }
    }

    /// Something on the AAP side changed: open cards may show it.
    async fn refresh_cards(&self, cards: &mut HashMap<String, Card>) {
        let now = Instant::now();
        for (address, card) in cards.iter_mut() {
            if !card.lid.is_open() {
                continue;
            }
            let (Some(ctx), Some((adv, levels))) = (self.ctx(address).await, card.last) else {
                continue;
            };
            let s = ctx.state().snapshot(&adv, levels);
            if let Some(s) = card.throttle.offer(now, s) {
                self.case_opened(&ctx, &s).await;
            }
        }
    }

    async fn case_opened(&self, ctx: &DeviceCtx, s: &Snapshot) {
        let path = ObjectPath::from(&ctx.path);
        if let Err(e) =
            ManagerIface::case_opened(self.hooks.manager.signal_emitter(), path, s.to_dict()).await
        {
            debug!(error = %e, "CaseOpened not sent");
        }
    }

    async fn case_closed(&self, address: &str) {
        let Some(ctx) = self.ctx(address).await else {
            return;
        };
        let path = ObjectPath::from(&ctx.path);
        if let Err(e) = ManagerIface::case_closed(self.hooks.manager.signal_emitter(), path).await {
            debug!(error = %e, "CaseClosed not sent");
        }
    }
}
