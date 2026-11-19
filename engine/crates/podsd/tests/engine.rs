// SPDX-License-Identifier: GPL-3.0-or-later
//! The engine end to end on a private bus: fake BlueZ, fake AirPods
//! replaying the M0 capture of a Pro 1, a real D-Bus client.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use zbus::wire::OwnedObjectPath;

async fn connected_rig() -> Rig {
    let rig = Rig::start(true, fast_timing()).await;
    eventually("the link opens", || async {
        rig.device()
            .await
            .get_property::<bool>("Connected")
            .await
            .unwrap_or(false)
    })
    .await;
    eventually("the burst is read", || async {
        rig.device_prop::<String>("Firmware").await == "6619472"
    })
    .await;
    // First connection: the keys are asked for and kept. Waiting for it
    // keeps the request out of the way of what the tests send next.
    eventually("keys kept", || async { rig.secrets.items().len() == 1 }).await;
    rig
}

#[tokio::test(flavor = "multi_thread")]
async fn the_opening_sequence_and_what_it_reads() {
    let rig = connected_rig().await;
    let sent = rig.airpods.received();
    assert_eq!(sent[0], pods_proto::packet::HANDSHAKE);
    assert_eq!(sent[1], pods_proto::packet::FEATURE_FLAGS);
    assert_eq!(sent[2], pods_proto::packet::NOTIFICATIONS);
    // No press-and-hold rewrite without a choice; the key request of a
    // first connection.
    assert_eq!(sent[3], pods_proto::packet::request_keys());
    assert_eq!(sent.len(), 4);

    assert_eq!(rig.device_prop::<String>("Address").await, ADDRESS);
    assert_eq!(
        rig.device_prop::<String>("Name").await,
        "Test AirPods Pro 1"
    );
    assert_eq!(rig.device_prop::<String>("Model").await, "0x200E");
    assert_eq!(
        rig.device_prop::<String>("ModelName").await,
        "AirPods Pro (1st generation)"
    );
    assert_eq!(
        rig.device_prop::<Vec<String>>("Capabilities").await,
        [
            "noise.off",
            "noise.transparency",
            "noise.anc",
            "pressHold",
            "rename",
            "earDetection",
            "spatial.head"
        ]
    );
    assert_eq!(
        rig.device_prop::<Battery>("Battery").await,
        Battery::from([("right".to_string(), (86, false))])
    );
    assert_eq!(
        rig.device_prop::<(String, String)>("Wearing").await,
        ("case".to_string(), "ear".to_string())
    );
    assert_eq!(rig.device_prop::<String>("NoiseMode").await, "anc");
    assert_eq!(
        rig.device_prop::<Vec<String>>("NoiseModes").await,
        ["off", "transparency", "anc"]
    );
    assert_eq!(
        rig.device_prop::<Vec<String>>("PressHoldModes").await,
        ["transparency", "anc"]
    );
    assert!(rig.device_prop::<bool>("EarDetection").await);
    assert!(rig.device_prop::<bool>("PauseOnRemove").await);
    assert!(rig.device_prop::<bool>("ResumeOnReturn").await);
    // No advertisement seen yet; spatial audio off, Follow Screen on by
    // default, sensors known but not asked.
    assert_eq!(rig.device_prop::<String>("Color").await, "");
    assert!(!rig.device_prop::<bool>("MediaPaused").await);
    assert!(!rig.device_prop::<bool>("ConversationAwareness").await);
    assert_eq!(rig.device_prop::<String>("SpatialMode").await, "off");
    assert_eq!(rig.device_prop::<String>("SpatialStatus").await, "off");
    assert!(rig.device_prop::<bool>("FollowScreen").await);
    eventually("sensors known", || async {
        rig.device_prop::<String>("HeadTracking").await == "idle"
    })
    .await;

    let manager = rig.proxy(MANAGER, MANAGER_IFACE).await;
    let devices: Vec<OwnedObjectPath> = manager.get_property("Devices").await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].as_str(), DEVICE);
    let active: OwnedObjectPath = manager.get_property("ActiveDevice").await.unwrap();
    assert_eq!(active.as_str(), DEVICE);
    assert_eq!(
        manager.get_property::<String>("Conflict").await.unwrap(),
        ""
    );
    assert_eq!(
        manager
            .get_property::<String>("EngineVersion")
            .await
            .unwrap(),
        env!("CARGO_PKG_VERSION")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tile_and_stem_changes_are_told_apart() {
    let rig = connected_rig().await;
    let changes = rig.signals(DEVICE_IFACE, "NoiseModeChanged").await;

    rig.call("SetNoiseMode", &("transparency",)).await.unwrap();
    assert_eq!(
        rig.airpods.received().last().unwrap(),
        &pods_proto::packet::set_noise_mode(pods_proto::NoiseMode::Transparency)
    );
    eventually("tile change", || async {
        changes.lock().unwrap().len() == 1
    })
    .await;
    assert_eq!(
        string_args(&changes.lock().unwrap()[0]),
        ("transparency".into(), "tile".into())
    );
    assert_eq!(rig.device_prop::<String>("NoiseMode").await, "transparency");

    // The stem: the buds report ANC on their own.
    rig.airpods.inject(pods_proto::packet::set_noise_mode(
        pods_proto::NoiseMode::Anc,
    ));
    eventually("stem change", || async {
        changes.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        string_args(&changes.lock().unwrap()[1]),
        ("anc".into(), "stem".into())
    );

    // A click on the tile: next in [transparency, anc] after anc.
    rig.call("CycleNoiseMode", &()).await.unwrap();
    eventually("cycle change", || async {
        changes.lock().unwrap().len() == 3
    })
    .await;
    assert_eq!(
        string_args(&changes.lock().unwrap()[2]),
        ("transparency".into(), "tile".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_follow_the_contract() {
    let rig = connected_rig().await;
    let err = |r: Result<(), String>| r.unwrap_err();
    assert_eq!(
        err(rig.call("SetNoiseMode", &("loud",)).await),
        "io.github.d101c.Pods1.Error.InvalidArgument"
    );
    assert_eq!(
        err(rig.call("SetNoiseMode", &("adaptive",)).await),
        "io.github.d101c.Pods1.Error.NotSupported"
    );
    assert_eq!(
        err(rig.call("SetPressHoldModes", &(vec!["anc"],)).await),
        "io.github.d101c.Pods1.Error.InvalidArgument"
    );
    assert_eq!(
        err(rig
            .call("SetPressHoldModes", &(vec!["anc", "adaptive"],))
            .await),
        "io.github.d101c.Pods1.Error.InvalidArgument"
    );
    assert_eq!(
        err(rig.call("SetPressHoldModes", &(vec!["anc", "anc"],)).await),
        "io.github.d101c.Pods1.Error.InvalidArgument"
    );
    assert_eq!(
        err(rig.call("Rename", &("",)).await),
        "io.github.d101c.Pods1.Error.InvalidArgument"
    );
    let not_supported = "io.github.d101c.Pods1.Error.NotSupported";
    // The Pro 1 has no conversation awareness.
    assert_eq!(
        err(rig.call("SetConversationAwareness", &(true,)).await),
        not_supported
    );
    // Already connected: nothing to do.
    rig.call("Connect", &()).await.unwrap();
    // Spatial audio and head tracking: tests/spatial.rs.
}

#[tokio::test(flavor = "multi_thread")]
async fn press_hold_rename_and_settings() {
    let rig = connected_rig().await;

    rig.call("SetPressHoldModes", &(vec!["anc", "off"],))
        .await
        .unwrap();
    assert_eq!(
        rig.airpods.received().last().unwrap(),
        &[4, 0, 4, 0, 9, 0, 0x1A, 0x03, 0, 0, 0]
    );
    assert_eq!(
        rig.device_prop::<Vec<String>>("PressHoldModes").await,
        ["off", "anc"]
    );
    let saved = std::fs::read_to_string(rig.config_path()).unwrap();
    assert!(
        saved.contains("press_hold_modes = [") && saved.contains("\"off\""),
        "{saved}"
    );

    rig.call("Rename", &("  Studio  ",)).await.unwrap();
    assert_eq!(
        rig.airpods.received().last().unwrap(),
        &pods_proto::packet::rename("Studio").unwrap()
    );
    assert_eq!(rig.bluez.alias().await, "Studio");
    assert_eq!(rig.device_prop::<String>("Name").await, "Studio");

    rig.call("SetEarDetection", &(false,)).await.unwrap();
    assert_eq!(
        rig.airpods.received().last().unwrap(),
        &[4, 0, 4, 0, 9, 0, 0x0A, 2, 0, 0, 0]
    );
    assert!(!rig.device_prop::<bool>("EarDetection").await);

    rig.call("SetPauseOnRemove", &(false,)).await.unwrap();
    rig.call("SetResumeOnReturn", &(false,)).await.unwrap();
    assert!(!rig.device_prop::<bool>("PauseOnRemove").await);
    assert!(!rig.device_prop::<bool>("ResumeOnReturn").await);
    let saved = std::fs::read_to_string(rig.config_path()).unwrap();
    assert!(saved.contains("pause_on_remove = false"), "{saved}");
    assert!(saved.contains("ear_detection = false"), "{saved}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chosen_press_hold_is_written_back_at_each_connection() {
    let rig = connected_rig().await;
    rig.call("SetPressHoldModes", &(vec!["off", "transparency", "anc"],))
        .await
        .unwrap();
    rig.airpods.hang_up();
    eventually("reconnected", || async { rig.airpods.connects() == 2 }).await;
    eventually("rewritten", || async {
        let sent = rig.airpods.received();
        // handshake, flags, notifications, then the cycle.
        sent.windows(4).any(|w| {
            w[0] == pods_proto::packet::HANDSHAKE && w[3] == [4, 0, 4, 0, 9, 0, 0x1A, 0x07, 0, 0, 0]
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn low_battery_and_the_battery_provider() {
    let rig = connected_rig().await;
    let alerts = rig.signals(DEVICE_IFACE, "LowBattery").await;

    // BlueZ was given our provider, at the level of the lowest bud.
    eventually("provider registered", || async {
        !rig.bluez.providers.lock().unwrap().is_empty()
    })
    .await;
    let (sender, root) = rig.bluez.providers.lock().unwrap()[0].clone();
    assert_eq!(root, "/io/github/d101c/Pods/battery");
    let battery_path = format!("{root}/dev_AA_BB_CC_DD_EE_01");
    let provider = zbus::proxy::Builder::<zbus::Proxy>::new(&rig.client)
        .destination(sender)
        .path(battery_path)
        .interface("org.bluez.BatteryProvider1")
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    assert_eq!(provider.get_property::<u8>("Percentage").await.unwrap(), 86);
    let device: OwnedObjectPath = provider.get_property("Device").await.unwrap();
    assert_eq!(device.as_str(), BLUEZ_DEVICE);

    // Left 9 %, right 40 %: one alert for the left, provider at 9.
    let frame = vec![4, 0, 4, 0, 4, 0, 2, 4, 1, 9, 2, 1, 2, 1, 40, 2, 1];
    rig.airpods.inject(frame.clone());
    eventually("alert", || async { alerts.lock().unwrap().len() == 1 }).await;
    let (part, pct) = alerts.lock().unwrap()[0]
        .body()
        .deserialize::<(String, u8)>()
        .unwrap();
    assert_eq!((part.as_str(), pct), ("left", 9));
    eventually("provider follows", || async {
        provider.get_property::<u8>("Percentage").await.unwrap_or(0) == 9
    })
    .await;
    // Same reading again: no second alert.
    rig.airpods.inject(frame);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(alerts.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_link_is_reopened_with_backoff() {
    let rig = connected_rig().await;
    rig.airpods.inner.refuse.store(2, Ordering::SeqCst);
    rig.airpods.hang_up();
    eventually("seen as disconnected", || async {
        !rig.device_prop::<bool>("Connected").await
    })
    .await;
    assert!(rig.device_prop::<Battery>("Battery").await.is_empty());
    eventually("back after two refusals", || async {
        rig.device_prop::<bool>("Connected").await
    })
    .await;
    assert_eq!(rig.airpods.connects(), 2);
    assert_eq!(rig.airpods.inner.refuse.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_watchdog_reopens_a_mute_link_quietly() {
    let mut timing = fast_timing();
    timing.watchdog = Some(Duration::from_millis(400));
    let rig = Rig::start(true, timing).await;
    let connected_changes = rig
        .signals("org.freedesktop.DBus.Properties", "PropertiesChanged")
        .await;
    eventually("open", || async { rig.airpods.connects() >= 1 }).await;
    rig.airpods.inner.mute.store(true, Ordering::SeqCst);
    eventually("reopened twice", || async { rig.airpods.connects() >= 3 }).await;
    assert!(rig.device_prop::<bool>("Connected").await);
    // Connected never went false.
    let went_false = connected_changes.lock().unwrap().iter().any(|m| {
        m.body()
            .deserialize::<(
                String,
                std::collections::HashMap<String, zbus::OwnedVariant>,
                Vec<String>,
            )>()
            .ok()
            .and_then(|(_, props, _)| {
                props
                    .get("Connected")
                    .map(|v| bool::try_from(v.try_clone().unwrap()).unwrap())
            })
            == Some(false)
    });
    assert!(!went_false);
}

#[tokio::test(flavor = "multi_thread")]
async fn bluez_drives_the_link() {
    let rig = Rig::start(false, fast_timing()).await;
    eventually("device listed", || async {
        rig.device()
            .await
            .get_property::<String>("Address")
            .await
            .is_ok()
    })
    .await;
    assert!(!rig.device_prop::<bool>("Connected").await);
    assert_eq!(rig.airpods.connects(), 0);
    assert_eq!(
        rig.call("SetNoiseMode", &("anc",)).await.unwrap_err(),
        "io.github.d101c.Pods1.Error.NotConnected"
    );

    rig.bluez.set_connected(true).await;
    eventually("open", || async {
        rig.device_prop::<bool>("Connected").await
    })
    .await;

    rig.bluez.set_connected(false).await;
    eventually("closed", || async {
        !rig.device_prop::<bool>("Connected").await
    })
    .await;
    let manager = rig.proxy(MANAGER, MANAGER_IFACE).await;
    let active: OwnedObjectPath = manager.get_property("ActiveDevice").await.unwrap();
    assert_eq!(active.as_str(), "/");

    // Disconnect goes through BlueZ.
    rig.bluez.set_connected(true).await;
    eventually("open again", || async {
        rig.device_prop::<bool>("Connected").await
    })
    .await;
    rig.call("Disconnect", &()).await.unwrap();
    eventually("closed by Disconnect", || async {
        !rig.device_prop::<bool>("Connected").await
    })
    .await;
}

// ------------------------------------------------------------------ MPRIS

struct FakePlayer {
    status: Arc<std::sync::Mutex<String>>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.status.lock().unwrap().clone()
    }
    #[zbus(property)]
    fn metadata(&self) -> std::collections::HashMap<String, zbus::OwnedVariant> {
        std::collections::HashMap::from([(
            "xesam:title".to_string(),
            zbus::OwnedVariant::from(zbus::wire::Str::from("Song")),
        )])
    }
    fn pause(&self) {
        *self.status.lock().unwrap() = "Paused".into();
    }
    fn play(&self) {
        *self.status.lock().unwrap() = "Playing".into();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bud_out_pauses_and_back_resumes() {
    let rig = connected_rig().await;
    let status = Arc::new(std::sync::Mutex::new("Playing".to_string()));
    let player = rig.bus.connect().await;
    player
        .object_server()
        .at(
            "/org/mpris/MediaPlayer2",
            FakePlayer {
                status: status.clone(),
            },
        )
        .await
        .unwrap();
    player
        .request_name("org.mpris.MediaPlayer2.fake")
        .await
        .unwrap();

    // Both in, then one out.
    rig.airpods.inject(vec![4, 0, 4, 0, 6, 0, 0, 0]);
    eventually("both in", || async {
        rig.device_prop::<(String, String)>("Wearing").await == ("ear".into(), "ear".into())
    })
    .await;
    assert!(!rig.device_prop::<bool>("MediaPaused").await);
    rig.airpods.inject(vec![4, 0, 4, 0, 6, 0, 0, 1]);
    eventually("paused", || async { *status.lock().unwrap() == "Paused" }).await;
    eventually("MediaPaused", || async {
        rig.device_prop::<bool>("MediaPaused").await
    })
    .await;
    rig.airpods.inject(vec![4, 0, 4, 0, 6, 0, 0, 0]);
    eventually("resumed", || async { *status.lock().unwrap() == "Playing" }).await;
    eventually("MediaPaused cleared", || async {
        !rig.device_prop::<bool>("MediaPaused").await
    })
    .await;

    // Paused again, then the link drops: nothing is held any more.
    rig.airpods.inject(vec![4, 0, 4, 0, 6, 0, 0, 1]);
    eventually("paused again", || async {
        rig.device_prop::<bool>("MediaPaused").await
    })
    .await;
    rig.airpods.hang_up();
    eventually("let go with the link", || async {
        !rig.device_prop::<bool>("MediaPaused").await
    })
    .await;
}

// --------------------------------------------------------------- podsctl

#[tokio::test(flavor = "multi_thread")]
async fn podsctl_talks_to_the_api() {
    let rig = connected_rig().await;
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_podsctl"))
            .args(args)
            .env("DBUS_SESSION_BUS_ADDRESS", &rig.bus.address)
            .output()
            .expect("podsctl runs")
    };
    let out = tokio::task::block_in_place(|| run(&["status"]));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("Test AirPods Pro 1"), "{text}");
    assert!(text.contains("anc"), "{text}");
    assert!(text.contains("right 86%"), "{text}");

    let out = tokio::task::block_in_place(|| run(&["set-noise", "transparency"]));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    eventually("set by podsctl", || async {
        rig.device_prop::<String>("NoiseMode").await == "transparency"
    })
    .await;
    let out = tokio::task::block_in_place(|| run(&["cycle"]));
    assert!(out.status.success());
    eventually("cycled by podsctl", || async {
        rig.device_prop::<String>("NoiseMode").await == "anc"
    })
    .await;
    let out = tokio::task::block_in_place(|| run(&["set-noise", "adaptive"]));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("NotSupported"));
}
