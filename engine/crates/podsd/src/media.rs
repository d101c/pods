// SPDX-License-Identifier: GPL-3.0-or-later
// The pause/resume decisions (`ear_action`, `parked_to_charge`, the settle
// delays, the mirror check on titles) are derived from podctl
// `src/bin/daemon/media.rs` (https://github.com/Rockykln/podctl),
// Copyright (c) 2026 pods contributors, MIT OR Apache-2.0. See engine/NOTICE.
// Changes: MPRIS only (no pactl), one-minute window, players remembered by
// their unique bus name, audio routing checked through BlueZ.
//! Pause what plays when a bud comes out, play it again when it is back.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, watch};
use tokio::time::Instant;
use tracing::{debug, info};
use zbus::Connection;

use pods_bt::{Bluez, BoxFuture};
use pods_proto::Wearing;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// Buds back after this long do not restart the music.
pub const RESUME_WINDOW: Duration = Duration::from_secs(60);
/// The in-ear sensor flickers while a bud is pulled out; right after a
/// pause the buds must stay in longer before the music comes back.
const RESUME_SETTLE_EARLY: Duration = Duration::from_secs(2);
const RESUME_SETTLE: Duration = Duration::from_millis(500);
const FLICKER_AFTER_PAUSE: Duration = Duration::from_secs(3);

/// A media player on the session bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    /// `org.mpris.MediaPlayer2.…`
    pub name: String,
    /// Unique name of its owner (`:1.42`): a player that restarted is not
    /// the one we paused.
    pub owner: String,
}

/// What the media logic asks of the system.
pub trait MediaHost: Send + Sync {
    fn players(&self) -> BoxFuture<'_, Vec<Player>>;
    fn status<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, Option<String>>;
    fn title<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, Option<String>>;
    fn call<'a>(&'a self, owner: &'a str, method: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// Whether audio flows to these buds right now.
    fn audio_to_buds<'a>(&'a self, bluez_path: &'a str) -> BoxFuture<'a, bool>;
}

/// Media settings of one device at the time of the change.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub ear_detection: bool,
    pub pause_on_remove: bool,
    pub resume_on_return: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum EarAction {
    Nothing,
    Pause,
    Resume,
}

fn count_in_ear(w: (Wearing, Wearing)) -> u8 {
    (w.0 == Wearing::Ear) as u8 + (w.1 == Wearing::Ear) as u8
}

/// One bud in the case, the other in an ear.
fn parked_to_charge(w: (Wearing, Wearing)) -> bool {
    matches!(
        w,
        (Wearing::Case, Wearing::Ear) | (Wearing::Ear, Wearing::Case)
    )
}

fn ear_action(
    prev: (Wearing, Wearing),
    now: (Wearing, Wearing),
    paused: bool,
    resume_at: u8,
) -> EarAction {
    let before = count_in_ear(prev);
    let after = count_in_ear(now);
    // Unknown → something is the first reading of a session, not a move.
    if prev == (Wearing::Unknown, Wearing::Unknown) {
        return EarAction::Nothing;
    }
    if after < before {
        // A second bud out must not move the resume target.
        return if paused {
            EarAction::Nothing
        } else {
            EarAction::Pause
        };
    }
    if !paused || after == before {
        return EarAction::Nothing;
    }
    if after < resume_at && !parked_to_charge(now) {
        return EarAction::Nothing;
    }
    EarAction::Resume
}

fn resume_settle(since_pause: Option<Duration>) -> Duration {
    if since_pause.is_some_and(|d| d < FLICKER_AFTER_PAUSE) {
        RESUME_SETTLE_EARLY
    } else {
        RESUME_SETTLE
    }
}

#[derive(Default)]
struct Inner {
    /// Owners of the players we paused.
    paused: Vec<String>,
    paused_at: Option<Instant>,
    resume_at_count: u8,
    /// Bumped on every change so a pending resume knows it was overtaken.
    generation: u64,
}

pub struct Media {
    host: Arc<dyn MediaHost>,
    inner: Mutex<Inner>,
    /// The BlueZ path of the buds we hold paused players for.
    holding: watch::Sender<Option<String>>,
}

impl Media {
    pub fn new(host: Arc<dyn MediaHost>) -> Self {
        Self {
            host,
            inner: Mutex::new(Inner::default()),
            holding: watch::channel(None).0,
        }
    }

    /// `Some(BlueZ path of the buds)` while the engine holds players it
    /// paused for them: the `MediaPaused` property.
    pub fn holding(&self) -> watch::Receiver<Option<String>> {
        self.holding.subscribe()
    }

    fn hold(&self, buds: Option<&str>) {
        let new = buds.map(str::to_string);
        self.holding.send_if_modified(|h| {
            let changed = *h != new;
            *h = new;
            changed
        });
    }

    /// Past the resume window the paused players are not ours to restart
    /// any more: let go of them.
    pub async fn expire(&self) {
        let mut inner = self.inner.lock().await;
        if inner
            .paused_at
            .is_some_and(|t| t.elapsed() >= RESUME_WINDOW + RESUME_SETTLE_EARLY)
        {
            debug!("resume window over; forgetting the paused players");
            inner.generation += 1;
            inner.paused.clear();
            inner.paused_at = None;
            self.hold(None);
        }
    }

    /// The buds moved. (left, right) before and after.
    pub async fn on_wearing(
        &self,
        bluez_path: &str,
        prev: (Wearing, Wearing),
        now: (Wearing, Wearing),
        policy: Policy,
    ) {
        if !policy.ear_detection || !policy.pause_on_remove {
            return;
        }
        let mut inner = self.inner.lock().await;
        inner.generation += 1;
        let my_gen = inner.generation;
        match ear_action(prev, now, !inner.paused.is_empty(), inner.resume_at_count) {
            EarAction::Nothing => return,
            EarAction::Pause => {
                if !self.host.audio_to_buds(bluez_path).await {
                    debug!("bud out, but no audio goes to the buds");
                    return;
                }
                let paused = self.pause_playing().await;
                if !paused.is_empty() {
                    info!(players = paused.len(), "paused on ear removal");
                    inner.paused = paused;
                    inner.paused_at = Some(Instant::now());
                    inner.resume_at_count = count_in_ear(prev);
                    self.hold(Some(bluez_path));
                }
                return;
            }
            EarAction::Resume => {}
        }
        if !policy.resume_on_return {
            inner.paused.clear();
            inner.paused_at = None;
            self.hold(None);
            return;
        }
        let settle = resume_settle(inner.paused_at.map(|t| t.elapsed()));
        drop(inner);
        tokio::time::sleep(settle).await;
        let mut inner = self.inner.lock().await;
        if inner.generation != my_gen || inner.paused.is_empty() {
            return;
        }
        let fresh = inner
            .paused_at
            .is_some_and(|t| t.elapsed() < RESUME_WINDOW + settle);
        let owners = std::mem::take(&mut inner.paused);
        inner.paused_at = None;
        self.hold(None);
        if !fresh {
            debug!("buds back after the resume window; leaving playback paused");
            return;
        }
        let mut resumed = 0;
        for owner in &owners {
            // Paused by someone else since, or gone.
            if self.host.status(owner).await.as_deref() != Some("Paused") {
                continue;
            }
            match self.host.call(owner, "Play").await {
                Ok(()) => resumed += 1,
                Err(e) => debug!(error = %e, "play failed"),
            }
        }
        if parked_to_charge(now) {
            inner.resume_at_count = 1;
        }
        info!(players = resumed, "resumed after the buds went back in");
    }

    /// The link closed: what we paused is no longer ours to resume.
    pub async fn link_dropped(&self) {
        let mut inner = self.inner.lock().await;
        inner.generation += 1;
        inner.paused.clear();
        inner.paused_at = None;
        self.hold(None);
    }

    /// Pauses every playing player, once per track: a browser integration
    /// mirrors another player's track and must not be toggled twice.
    async fn pause_playing(&self) -> Vec<String> {
        let mut paused = Vec::new();
        let mut titles: Vec<String> = Vec::new();
        for player in self.host.players().await {
            if self.host.status(&player.owner).await.as_deref() != Some("Playing") {
                continue;
            }
            let title = self.host.title(&player.owner).await;
            if title.as_ref().is_some_and(|t| titles.contains(t)) {
                debug!(player = %player.name, "mirrors a track already paused");
                continue;
            }
            match self.host.call(&player.owner, "Pause").await {
                Ok(()) => {
                    paused.push(player.owner.clone());
                    titles.extend(title);
                }
                Err(e) => debug!(player = %player.name, error = %e, "pause failed"),
            }
        }
        paused
    }
}

/// The real host: MPRIS on the session bus, BlueZ for the routing.
pub struct SystemHost {
    session: Connection,
    bluez: Bluez,
}

impl SystemHost {
    pub fn new(session: Connection, bluez: Bluez) -> Self {
        Self { session, bluez }
    }

    async fn proxy<'a>(&self, owner: &'a str) -> zbus::Result<zbus::Proxy<'a>> {
        zbus::proxy::Builder::new(&self.session)
            .destination(owner)
            .path(MPRIS_PATH)
            .interface(MPRIS_PLAYER)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
    }
}

impl MediaHost for SystemHost {
    fn players(&self) -> BoxFuture<'_, Vec<Player>> {
        Box::pin(async move {
            let Ok(dbus) = zbus::fdo::DBusProxy::new(&self.session).await else {
                return Vec::new();
            };
            let Ok(names) = dbus.list_names().await else {
                return Vec::new();
            };
            let mut players = Vec::new();
            for name in names {
                if !name.as_str().starts_with(MPRIS_PREFIX) {
                    continue;
                }
                let Ok(bus_name) = zbus::names::BusName::try_from(name.as_str()) else {
                    continue;
                };
                if let Ok(owner) = dbus.get_name_owner(bus_name).await {
                    players.push(Player {
                        name: name.to_string(),
                        owner: owner.to_string(),
                    });
                }
            }
            players
        })
    }

    fn status<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let proxy = self.proxy(owner).await.ok()?;
            proxy.get_property::<String>("PlaybackStatus").await.ok()
        })
    }

    fn title<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let proxy = self.proxy(owner).await.ok()?;
            let meta: std::collections::HashMap<String, zbus::OwnedVariant> =
                proxy.get_property("Metadata").await.ok()?;
            let t = String::try_from(meta.get("xesam:title")?.try_clone().ok()?).ok()?;
            (!t.is_empty()).then_some(t)
        })
    }

    fn call<'a>(&'a self, owner: &'a str, method: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let proxy = self.proxy(owner).await.map_err(|e| e.to_string())?;
            proxy
                .call_method(method, &())
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    fn audio_to_buds<'a>(&'a self, bluez_path: &'a str) -> BoxFuture<'a, bool> {
        // When BlueZ cannot be asked, assume yes: a pause too many is
        // better than music playing to nobody.
        Box::pin(async move { self.bluez.streaming(bluez_path).await.unwrap_or(true) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Wearing::{Case, Ear, Out, Unknown};
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct FakeHost {
        players: StdMutex<HashMap<String, (String, String)>>, // owner -> (status, title)
        calls: StdMutex<Vec<(String, String)>>,
        streaming: StdMutex<bool>,
    }

    impl FakeHost {
        fn with(players: &[(&str, &str, &str)]) -> Arc<Self> {
            let h = FakeHost::default();
            *h.streaming.lock().unwrap() = true;
            for (owner, status, title) in players {
                h.players
                    .lock()
                    .unwrap()
                    .insert(owner.to_string(), (status.to_string(), title.to_string()));
            }
            Arc::new(h)
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl MediaHost for FakeHost {
        fn players(&self) -> BoxFuture<'_, Vec<Player>> {
            let mut list: Vec<Player> = self
                .players
                .lock()
                .unwrap()
                .keys()
                .map(|o| Player {
                    name: format!("{MPRIS_PREFIX}{o}"),
                    owner: o.clone(),
                })
                .collect();
            list.sort_by(|a, b| a.owner.cmp(&b.owner));
            Box::pin(async move { list })
        }
        fn status<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, Option<String>> {
            let s = self.players.lock().unwrap().get(owner).map(|p| p.0.clone());
            Box::pin(async move { s })
        }
        fn title<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, Option<String>> {
            let s = self.players.lock().unwrap().get(owner).map(|p| p.1.clone());
            Box::pin(async move { s })
        }
        fn call<'a>(
            &'a self,
            owner: &'a str,
            method: &'a str,
        ) -> BoxFuture<'a, Result<(), String>> {
            self.calls
                .lock()
                .unwrap()
                .push((owner.to_string(), method.to_string()));
            if let Some(p) = self.players.lock().unwrap().get_mut(owner) {
                p.0 = if method == "Pause" {
                    "Paused"
                } else {
                    "Playing"
                }
                .to_string();
            }
            Box::pin(async { Ok(()) })
        }
        fn audio_to_buds<'a>(&'a self, _: &'a str) -> BoxFuture<'a, bool> {
            let s = *self.streaming.lock().unwrap();
            Box::pin(async move { s })
        }
    }

    const ON: Policy = Policy {
        ear_detection: true,
        pause_on_remove: true,
        resume_on_return: true,
    };

    #[test]
    fn decisions() {
        assert_eq!(
            ear_action((Ear, Ear), (Out, Ear), false, 0),
            EarAction::Pause
        );
        assert_eq!(
            ear_action((Out, Ear), (Out, Out), true, 2),
            EarAction::Nothing
        );
        assert_eq!(
            ear_action((Out, Out), (Out, Ear), true, 2),
            EarAction::Nothing
        );
        assert_eq!(
            ear_action((Out, Ear), (Ear, Ear), true, 2),
            EarAction::Resume
        );
        // Parked one bud to charge: resume on the one left.
        assert_eq!(
            ear_action((Out, Ear), (Case, Ear), true, 2),
            EarAction::Nothing
        );
        assert_eq!(
            ear_action((Out, Out), (Case, Ear), true, 2),
            EarAction::Resume
        );
        // Nothing paused, nothing to resume.
        assert_eq!(
            ear_action((Out, Ear), (Ear, Ear), false, 0),
            EarAction::Nothing
        );
        // First reading of a session.
        assert_eq!(
            ear_action((Unknown, Unknown), (Out, Out), false, 0),
            EarAction::Nothing
        );
    }

    #[tokio::test(start_paused = true)]
    async fn out_pauses_and_back_resumes_the_same_players() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song"), (":1.2", "Stopped", "")]);
        let media = Media::new(host.clone());
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), ON).await;
        assert_eq!(host.calls(), [(":1.1".into(), "Pause".into())]);
        tokio::time::advance(Duration::from_secs(10)).await;
        media.on_wearing("/dev", (Out, Ear), (Ear, Ear), ON).await;
        assert_eq!(
            host.calls(),
            [
                (":1.1".into(), "Pause".into()),
                (":1.1".into(), "Play".into())
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn media_paused_follows_what_the_engine_holds() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song")]);
        let media = Media::new(host.clone());
        let held = media.holding();
        assert_eq!(*held.borrow(), None);
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), ON).await;
        assert_eq!(held.borrow().as_deref(), Some("/dev"));
        media.on_wearing("/dev", (Out, Ear), (Ear, Ear), ON).await;
        assert_eq!(*held.borrow(), None);

        // Paused again, then the link drops.
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), ON).await;
        assert_eq!(held.borrow().as_deref(), Some("/dev"));
        media.link_dropped().await;
        assert_eq!(*held.borrow(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_hold_ends_with_the_resume_window() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song")]);
        let media = Media::new(host.clone());
        let held = media.holding();
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), ON).await;
        media.expire().await;
        assert_eq!(held.borrow().as_deref(), Some("/dev"), "too early");
        tokio::time::advance(RESUME_WINDOW + Duration::from_secs(3)).await;
        media.expire().await;
        assert_eq!(*held.borrow(), None);
        // And the buds coming back do not restart it.
        media.on_wearing("/dev", (Out, Ear), (Ear, Ear), ON).await;
        assert_eq!(host.calls().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_minute_the_music_stays_paused() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song")]);
        let media = Media::new(host.clone());
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), ON).await;
        tokio::time::advance(RESUME_WINDOW + Duration::from_secs(5)).await;
        media.on_wearing("/dev", (Out, Ear), (Ear, Ear), ON).await;
        assert_eq!(host.calls().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn mirrors_are_paused_once() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song"), (":1.2", "Playing", "Song")]);
        let media = Media::new(host.clone());
        media.on_wearing("/dev", (Ear, Ear), (Ear, Out), ON).await;
        assert_eq!(host.calls(), [(":1.1".into(), "Pause".into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_player_paused_by_hand_meanwhile_is_left_alone() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song")]);
        let media = Media::new(host.clone());
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), ON).await;
        // The player stopped (closed, or stopped by the user).
        host.players.lock().unwrap().get_mut(":1.1").unwrap().0 = "Stopped".into();
        media.on_wearing("/dev", (Out, Ear), (Ear, Ear), ON).await;
        assert_eq!(host.calls().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_without_audio_to_the_buds_or_with_the_setting_off() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song")]);
        *host.streaming.lock().unwrap() = false;
        let media = Media::new(host.clone());
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), ON).await;
        assert!(host.calls().is_empty());

        *host.streaming.lock().unwrap() = true;
        let off = Policy {
            pause_on_remove: false,
            ..ON
        };
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), off).await;
        let no_detection = Policy {
            ear_detection: false,
            ..ON
        };
        media
            .on_wearing("/dev", (Ear, Ear), (Out, Ear), no_detection)
            .await;
        assert!(host.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn resume_off_pauses_only() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song")]);
        let media = Media::new(host.clone());
        let p = Policy {
            resume_on_return: false,
            ..ON
        };
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), p).await;
        media.on_wearing("/dev", (Out, Ear), (Ear, Ear), p).await;
        assert_eq!(host.calls(), [(":1.1".into(), "Pause".into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_link_forgets_the_paused_players() {
        let host = FakeHost::with(&[(":1.1", "Playing", "Song")]);
        let media = Media::new(host.clone());
        media.on_wearing("/dev", (Ear, Ear), (Out, Ear), ON).await;
        media.link_dropped().await;
        media.on_wearing("/dev", (Out, Ear), (Ear, Ear), ON).await;
        assert_eq!(host.calls().len(), 1);
    }
}
