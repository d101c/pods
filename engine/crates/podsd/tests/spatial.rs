// SPDX-License-Identifier: GPL-3.0-or-later
//! Spatial audio and head tracking (#23) on a private bus: fake BlueZ, fake
//! AirPods replaying the sensor frames of a real AirPods Pro (1st
//! generation) from M0 quater, a fake renderer instead of pods-spatial and
//! a fake PipeWire node instead of `bluez_output.<address>.1`.

mod common;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use pods_motion::remotehid;
use podsd::spatial::{PauseReason, RenderStatus, StopReason};

const NOT_SUPPORTED: &str = "io.github.d101c.Pods1.Error.NotSupported";
const NOT_CONNECTED: &str = "io.github.d101c.Pods1.Error.NotConnected";
const INVALID: &str = "io.github.d101c.Pods1.Error.InvalidArgument";
const AIRPODS_2: &str = "bluetooth:v004Cp200Fd0A4C";

/// `pods-motion/tests/fixtures/m0-quater-setup.txt`: (tx, frame).
fn setup_frames() -> Vec<(bool, Vec<u8>)> {
    include_str!("../../pods-motion/tests/fixtures/m0-quater-setup.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut cols = l.split_whitespace();
            let tx = cols.next().unwrap() == "tx";
            cols.next();
            (tx, hex_bytes(cols.next().unwrap()))
        })
        .collect()
}

/// `pods-motion/tests/fixtures/m0-quater-motion.bin`: every motion report.
fn motion_reports() -> Vec<Vec<u8>> {
    let mut b: &[u8] = include_bytes!("../../pods-motion/tests/fixtures/m0-quater-motion.bin");
    let mut out = Vec::new();
    while b.len() >= 6 {
        let n = u16::from_le_bytes([b[4], b[5]]) as usize;
        out.push(b[6..6 + n].to_vec());
        b = &b[6 + n..];
    }
    out
}

fn is_sensor_frame(f: &[u8]) -> bool {
    f.len() > 4 && f[4] == 0x17
}

fn sensor_frames_sent(rig: &Rig) -> Vec<Vec<u8>> {
    rig.airpods
        .received()
        .into_iter()
        .filter(|f| is_sensor_frame(f))
        .collect()
}

fn is_stop_frame(f: &[u8]) -> bool {
    (0..=255u8).any(|g| f == remotehid::set_interval(g, 7, 0).as_slice())
}

async fn connected(setup: Setup) -> Rig {
    let rig = Rig::with(setup).await;
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
    eventually("keys kept", || async { rig.secrets.items().len() == 1 }).await;
    rig
}

async fn pro1() -> Rig {
    connected(Setup::default()).await
}

async fn prop_is(rig: &Rig, name: &'static str, want: &str) {
    eventually(&format!("{name} = {want}"), || async {
        rig.device_prop::<String>(name).await == want
    })
    .await;
}

/// Head mode rendering, sound flowing, the sensors announced and streaming.
async fn streaming(rig: &Rig) {
    rig.call("SetSpatialMode", &("head",)).await.unwrap();
    prop_is(rig, "SpatialStatus", "active").await;
    prop_is(rig, "HeadTracking", "idle").await;
    rig.sinks.set(true, true);
    prop_is(rig, "HeadTracking", "starting").await;
    for (tx, frame) in setup_frames() {
        if !tx {
            rig.airpods.inject(frame);
        }
    }
    prop_is(rig, "HeadTracking", "active").await;
}

/// The properties named in the `PropertiesChanged` signals collected.
fn changed(signals: &std::sync::Mutex<Vec<zbus::Message>>) -> Vec<String> {
    signals
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| {
            m.body()
                .deserialize::<(String, HashMap<String, zbus::OwnedVariant>, Vec<String>)>()
                .ok()
        })
        .flat_map(|(_, props, _)| props.into_keys())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn spatial_head_is_announced_for_models_with_sensors() {
    let rig = pro1().await;
    let caps = rig.device_prop::<Vec<String>>("Capabilities").await;
    assert_eq!(caps.last().map(String::as_str), Some("spatial.head"));
    prop_is(&rig, "HeadTracking", "idle").await;
    assert_eq!(rig.device_prop::<String>("SpatialMode").await, "off");
    assert_eq!(rig.device_prop::<String>("SpatialStatus").await, "off");
    assert!(rig.device_prop::<bool>("FollowScreen").await);

    let rig = connected(Setup {
        modalias: AIRPODS_2,
        ..Setup::default()
    })
    .await;
    let caps = rig.device_prop::<Vec<String>>("Capabilities").await;
    assert!(!caps.iter().any(|c| c == "spatial.head"), "{caps:?}");
    assert_eq!(
        rig.device_prop::<String>("HeadTracking").await,
        "unavailable"
    );
    assert!(!rig.device_prop::<bool>("FollowScreen").await);
    let err = |r: Result<(), String>| r.unwrap_err();
    assert_eq!(
        err(rig.call("SetSpatialMode", &("head",)).await),
        NOT_SUPPORTED
    );
    assert_eq!(
        err(rig.call("SetFollowScreen", &(true,)).await),
        NOT_SUPPORTED
    );
    assert_eq!(err(rig.call("Recenter", &()).await), NOT_SUPPORTED);
    assert_eq!(
        err(rig.call("WatchHeadPose", &(true,)).await),
        NOT_SUPPORTED
    );
    rig.call("WatchHeadPose", &(false,)).await.unwrap();
    assert_eq!(err(rig.call("SetSpatialMode", &("3d",)).await), INVALID);
    // The fixed scene needs no sensors.
    rig.call("SetSpatialMode", &("fixed",)).await.unwrap();
    prop_is(&rig, "SpatialStatus", "active").await;
    assert_eq!(rig.renderer.log(), [format!("start fixed {ADDRESS}")]);
    assert_eq!(
        rig.device_prop::<String>("HeadTracking").await,
        "unavailable"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_starts_without_the_airpods() {
    let rig = Rig::start(false, fast_timing()).await;
    eventually("device listed", || async {
        rig.device()
            .await
            .get_property::<String>("Address")
            .await
            .is_ok()
    })
    .await;
    let err = |r: Result<(), String>| r.unwrap_err();
    assert_eq!(
        err(rig.call("SetSpatialMode", &("fixed",)).await),
        NOT_CONNECTED
    );
    assert_eq!(
        err(rig.call("SetSpatialMode", &("off",)).await),
        NOT_CONNECTED
    );
    assert_eq!(err(rig.call("Recenter", &()).await), NOT_CONNECTED);
    assert_eq!(err(rig.call("SetSpatialMode", &("x",)).await), INVALID);
    // A setting: kept for the next connection.
    rig.call("SetFollowScreen", &(false,)).await.unwrap();
    assert!(!rig.device_prop::<bool>("FollowScreen").await);
    assert_eq!(rig.device_prop::<String>("SpatialStatus").await, "off");
    assert_eq!(
        rig.device_prop::<String>("HeadTracking").await,
        "unavailable"
    );
    assert!(rig.renderer.log().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn modes_start_switch_pause_and_stop_the_renderer() {
    let rig = pro1().await;
    let signals = rig
        .signals("org.freedesktop.DBus.Properties", "PropertiesChanged")
        .await;
    rig.renderer
        .inner
        .hold_starting
        .store(true, Ordering::SeqCst);
    rig.call("SetSpatialMode", &("fixed",)).await.unwrap();
    assert_eq!(rig.device_prop::<String>("SpatialMode").await, "fixed");
    prop_is(&rig, "SpatialStatus", "starting").await;
    eventually("renderer started", || async { rig.renderer.running() == 1 }).await;
    rig.renderer.report(RenderStatus::Active);
    prop_is(&rig, "SpatialStatus", "active").await;
    let saved = std::fs::read_to_string(rig.config_path()).unwrap();
    assert!(saved.contains("spatial_mode = \"fixed\""), "{saved}");

    // Listen Together shares the sound.
    rig.renderer
        .report(RenderStatus::Paused(PauseReason::Sharing));
    prop_is(&rig, "SpatialStatus", "paused").await;
    rig.renderer.report(RenderStatus::Active);
    prop_is(&rig, "SpatialStatus", "active").await;

    // Another mode: stop, then start.
    rig.renderer
        .inner
        .hold_starting
        .store(false, Ordering::SeqCst);
    rig.call("SetSpatialMode", &("head",)).await.unwrap();
    eventually("switched", || async { rig.renderer.log().len() == 3 }).await;
    assert_eq!(
        rig.renderer.log(),
        [
            format!("start fixed {ADDRESS}"),
            "stop".to_string(),
            format!("start head {ADDRESS}")
        ]
    );
    prop_is(&rig, "SpatialStatus", "active").await;
    assert_eq!(rig.renderer.running(), 1);
    // The same mode again changes nothing.
    rig.call("SetSpatialMode", &("head",)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.renderer.log().len(), 3);

    rig.call("SetSpatialMode", &("off",)).await.unwrap();
    prop_is(&rig, "SpatialStatus", "off").await;
    assert_eq!(rig.renderer.running(), 0);
    assert_eq!(rig.renderer.log().last().unwrap(), "stop");
    let saved = std::fs::read_to_string(rig.config_path()).unwrap();
    assert!(saved.contains("spatial_mode = \"off\""), "{saved}");

    let names = changed(&signals);
    for p in ["SpatialMode", "SpatialStatus"] {
        assert!(names.iter().any(|n| n == p), "{p} never changed: {names:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_the_node_and_the_users_choice() {
    let rig = pro1().await;
    rig.call("SetSpatialMode", &("fixed",)).await.unwrap();
    prop_is(&rig, "SpatialStatus", "active").await;

    // The backend dies: everything stops, the mode stays.
    rig.renderer
        .report(RenderStatus::Stopped(StopReason::BackendFailed(
            "client exited".into(),
        )));
    prop_is(&rig, "SpatialStatus", "failed").await;
    assert_eq!(rig.renderer.running(), 0);
    assert_eq!(rig.device_prop::<String>("SpatialMode").await, "fixed");
    // Setting the mode again tries again.
    rig.call("SetSpatialMode", &("fixed",)).await.unwrap();
    prop_is(&rig, "SpatialStatus", "active").await;
    assert_eq!(rig.renderer.running(), 1);

    // The AirPods' node goes away (profile switch): stop, wait, start again.
    rig.renderer
        .report(RenderStatus::Stopped(StopReason::TargetGone));
    eventually("stopped", || async { rig.renderer.running() == 0 }).await;
    rig.sinks.set(false, false);
    prop_is(&rig, "SpatialStatus", "starting").await;
    rig.sinks.set(true, false);
    prop_is(&rig, "SpatialStatus", "active").await;
    assert_eq!(rig.renderer.running(), 1);

    // A start that fails; a new appearance of the node retries.
    rig.renderer
        .report(RenderStatus::Stopped(StopReason::TargetGone));
    eventually("stopped", || async { rig.renderer.running() == 0 }).await;
    rig.renderer.inner.refuse.store(true, Ordering::SeqCst);
    rig.sinks.set(false, false);
    rig.sinks.set(true, false);
    prop_is(&rig, "SpatialStatus", "failed").await;
    rig.sinks.set(false, false);
    rig.sinks.set(true, false);
    prop_is(&rig, "SpatialStatus", "active").await;

    // The user picks the speakers: off, and it stays off.
    rig.renderer
        .report(RenderStatus::Stopped(StopReason::UserSwitched(
            "alsa_output.speakers".into(),
        )));
    prop_is(&rig, "SpatialStatus", "off").await;
    let starts = rig
        .renderer
        .log()
        .iter()
        .filter(|l| l.starts_with("start"))
        .count();
    rig.sinks.set(true, true);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        rig.renderer
            .log()
            .iter()
            .filter(|l| l.starts_with("start"))
            .count(),
        starts
    );
    assert_eq!(rig.device_prop::<String>("SpatialMode").await, "fixed");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mode_is_applied_again_at_every_connection() {
    let rig = connected(Setup {
        config: Some(
            "[devices.\"AA:BB:CC:DD:EE:01\"]\nspatial_mode = \"fixed\"\nfollow_screen = false\n",
        ),
        sink_present: false,
        ..Setup::default()
    })
    .await;
    assert_eq!(rig.device_prop::<String>("SpatialMode").await, "fixed");
    assert!(!rig.device_prop::<bool>("FollowScreen").await);
    // Connected, but the audio node is not there yet.
    prop_is(&rig, "SpatialStatus", "starting").await;
    assert!(rig.renderer.log().is_empty());
    rig.sinks.set(true, false);
    prop_is(&rig, "SpatialStatus", "active").await;

    // The AirPods go: everything stops.
    rig.bluez.set_connected(false).await;
    prop_is(&rig, "SpatialStatus", "off").await;
    prop_is(&rig, "HeadTracking", "unavailable").await;
    assert_eq!(rig.renderer.running(), 0);
    assert_eq!(rig.device_prop::<String>("SpatialMode").await, "fixed");

    // They come back: started again.
    rig.bluez.set_connected(true).await;
    prop_is(&rig, "SpatialStatus", "active").await;
    assert_eq!(rig.renderer.running(), 1);
    assert_eq!(
        rig.renderer
            .log()
            .iter()
            .filter(|l| l.starts_with("start"))
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn head_tracking_from_the_link_to_the_renderer_and_the_settings() {
    let rig = pro1().await;
    let poses = rig.signals(DEVICE_IFACE, "HeadPose").await;
    rig.call("WatchHeadPose", &(true,)).await.unwrap();

    rig.call("SetSpatialMode", &("head",)).await.unwrap();
    prop_is(&rig, "SpatialStatus", "active").await;
    // No sound to the AirPods: the sensors are not asked (M0 quater).
    prop_is(&rig, "HeadTracking", "idle").await;
    assert!(sensor_frames_sent(&rig).is_empty());

    let probe = setup_frames();
    let probe_tx: Vec<Vec<u8>> = probe
        .iter()
        .filter(|(tx, _)| *tx)
        .map(|(_, f)| f.clone())
        .collect();
    rig.sinks.set(true, true);
    prop_is(&rig, "HeadTracking", "starting").await;
    eventually("HostConnect", || async {
        sensor_frames_sent(&rig) == probe_tx[..1]
    })
    .await;

    // The announces and the first reports, as the Pro 1 sent them.
    for (tx, frame) in &probe {
        if !tx {
            rig.airpods.inject(frame.clone());
        }
    }
    prop_is(&rig, "HeadTracking", "active").await;
    // The interval right after the devmotion6 announce, byte for byte.
    eventually("SetReport", || async {
        sensor_frames_sent(&rig) == probe_tx[..2]
    })
    .await;
    eventually("render poses", || async { !rig.renderer.yaws().is_empty() }).await;
    eventually("HeadPose", || async { !poses.lock().unwrap().is_empty() }).await;
    let (yaw, pitch) = poses.lock().unwrap()[0]
        .body()
        .deserialize::<(f64, f64)>()
        .unwrap();
    assert!(
        yaw.abs() < 5.0 && (0.0..90.0).contains(&pitch),
        "{yaw} {pitch}"
    );

    // One second of reports at 50 Hz: at most 30 HeadPose (and a held one).
    let reports = motion_reports();
    let before = poses.lock().unwrap().len();
    let start = tokio::time::Instant::now();
    for r in &reports[100..150] {
        rig.airpods.inject(r.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let elapsed = start.elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let n = poses.lock().unwrap().len() - before;
    assert!(
        n >= 10 && n as f64 <= elapsed * 30.0 + 2.0,
        "{n} HeadPose in {elapsed:.2} s"
    );

    rig.call("Recenter", &()).await.unwrap();
    rig.call("SetFollowScreen", &(false,)).await.unwrap();
    assert!(!rig.device_prop::<bool>("FollowScreen").await);
    let saved = std::fs::read_to_string(rig.config_path()).unwrap();
    assert!(saved.contains("follow_screen = false"), "{saved}");

    // Nobody watches: no more HeadPose, the renderer still gets its poses.
    rig.call("WatchHeadPose", &(false,)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = poses.lock().unwrap().len();
    let yaws = rig.renderer.yaws().len();
    for r in &reports[150..200] {
        rig.airpods.inject(r.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    eventually("render poses go on", || async {
        rig.renderer.yaws().len() > yaws
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(poses.lock().unwrap().len(), before);

    // Mode off: the sensors get their interval 0, the renderer stops.
    rig.call("SetSpatialMode", &("off",)).await.unwrap();
    prop_is(&rig, "HeadTracking", "idle").await;
    eventually("stop frame", || async {
        sensor_frames_sent(&rig)
            .last()
            .is_some_and(|f| is_stop_frame(f))
    })
    .await;
    assert_eq!(rig.renderer.running(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sensors_wait_for_a_worn_bud_and_an_active_renderer() {
    let rig = pro1().await;
    rig.renderer
        .inner
        .hold_starting
        .store(true, Ordering::SeqCst);
    rig.sinks.set(true, true);
    rig.call("SetSpatialMode", &("head",)).await.unwrap();
    prop_is(&rig, "SpatialStatus", "starting").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sensor_frames_sent(&rig).is_empty());
    rig.renderer.report(RenderStatus::Active);
    prop_is(&rig, "HeadTracking", "starting").await;
    // Shared sound: the sensors stop.
    rig.renderer
        .report(RenderStatus::Paused(PauseReason::Sharing));
    prop_is(&rig, "HeadTracking", "idle").await;
    rig.renderer.report(RenderStatus::Active);
    prop_is(&rig, "HeadTracking", "starting").await;
    // Both buds back in the case (ear packet: primary, secondary = case).
    rig.airpods.inject(vec![4, 0, 4, 0, 6, 0, 2, 2]);
    prop_is(&rig, "HeadTracking", "idle").await;
    assert_eq!(
        rig.device_prop::<(String, String)>("Wearing").await,
        ("case".into(), "case".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_that_leaves_the_bus_stops_watching() {
    let rig = pro1().await;
    let poses = rig.signals(DEVICE_IFACE, "HeadPose").await;
    streaming(&rig).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(poses.lock().unwrap().is_empty(), "nobody watches yet");

    let other = rig.bus.connect().await;
    zbus::Proxy::new(&other, BUS_NAME, DEVICE, DEVICE_IFACE)
        .await
        .unwrap()
        .call_method("WatchHeadPose", &(true,))
        .await
        .unwrap();
    let reports = motion_reports();
    for r in &reports[100..120] {
        rig.airpods.inject(r.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    eventually("HeadPose", || async { !poses.lock().unwrap().is_empty() }).await;

    other.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = poses.lock().unwrap().len();
    for r in &reports[120..150] {
        rig.airpods.inject(r.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(poses.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnection_and_shutdown_stop_everything() {
    let rig = pro1().await;
    streaming(&rig).await;
    // The AirPods go: nothing can be sent, the renderer stops.
    let sent = sensor_frames_sent(&rig).len();
    rig.bluez.set_connected(false).await;
    prop_is(&rig, "HeadTracking", "unavailable").await;
    prop_is(&rig, "SpatialStatus", "off").await;
    assert_eq!(rig.renderer.running(), 0);
    assert_eq!(sensor_frames_sent(&rig).len(), sent);

    // Back, and streaming again (the buds announce anew).
    rig.bluez.set_connected(true).await;
    prop_is(&rig, "SpatialStatus", "active").await;
    for (tx, frame) in setup_frames() {
        if !tx {
            rig.airpods.inject(frame);
        }
    }
    prop_is(&rig, "HeadTracking", "active").await;

    // The daemon stops: interval 0 written before it answers, output given back.
    rig.engine.shutdown().await;
    assert!(
        sensor_frames_sent(&rig)
            .last()
            .is_some_and(|f| is_stop_frame(f))
    );
    assert_eq!(rig.renderer.running(), 0);
    assert_eq!(rig.renderer.log().last().unwrap(), "stop");
}
