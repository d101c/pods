// SPDX-License-Identifier: GPL-3.0-or-later
//! M2 end to end on a private bus: the passive BLE monitor, the keys and
//! the keyring, the case card, Connect, conversation awareness and the
//! press-and-hold cycle read from the stem.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::*;
use zbus::wire::{OwnedObjectPath, OwnedVariant};

type Card = HashMap<String, OwnedVariant>;

fn card_of(m: &zbus::Message) -> (String, Card) {
    let (path, card): (OwnedObjectPath, Card) = m.body().deserialize().unwrap();
    (path.to_string(), card)
}

fn byte(card: &Card, key: &str) -> Option<u8> {
    card.get(key)
        .map(|v| u8::try_from(v.try_clone().unwrap()).unwrap())
}

fn flag(card: &Card, key: &str) -> Option<bool> {
    card.get(key)
        .map(|v| bool::try_from(v.try_clone().unwrap()).unwrap())
}

fn text(card: &Card, key: &str) -> Option<String> {
    card.get(key)
        .map(|v| String::try_from(v.try_clone().unwrap()).unwrap())
}

/// A private address of our test AirPods.
fn our_address(prand: u8) -> String {
    test_keys().address_for([prand, 0x22, 0x33])
}

async fn listed(rig: &Rig) {
    eventually("device listed", || async {
        rig.device()
            .await
            .get_property::<String>("Address")
            .await
            .is_ok()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_monitor_is_passive_and_registered_for_good() {
    let rig = Rig::start(true, fast_timing()).await;
    rig.monitor_registered().await;
    let (sender, root) = rig.bluez.monitors.lock().unwrap()[0].clone();
    assert_eq!(root, pods_bt::monitor::MONITOR_ROOT);
    assert_eq!(
        sender,
        rig.engine_system.unique_name().unwrap().to_string(),
        "served on the bus BlueZ lives on"
    );

    // What BlueZ reads under the root.
    let om = zbus::fdo::ObjectManagerProxy::builder(&rig.client)
        .destination(sender)
        .path(root)
        .build()
        .await
        .unwrap();
    let objects = om.get_managed_objects().await.unwrap();
    assert_eq!(objects.len(), 1);
    let (path, ifaces) = objects.iter().next().unwrap();
    assert_eq!(path.as_str(), pods_bt::monitor::MONITOR_PATH);
    let props = ifaces
        .iter()
        .find(|(i, _)| i.as_str() == "org.bluez.AdvertisementMonitor1")
        .map(|(_, p)| p)
        .expect("AdvertisementMonitor1");
    let get = |k: &str| props.get(k).unwrap().try_clone().unwrap();
    assert_eq!(String::try_from(get("Type")).unwrap(), "or_patterns");
    let patterns: Vec<(u8, u8, Vec<u8>)> = get("Patterns").try_into().unwrap();
    assert_eq!(patterns, [(0, 0xFF, vec![0x4C, 0x00, 0x07, 0x19])]);
    assert_eq!(i16::try_from(get("RSSIHighThreshold")).unwrap(), -80);
    assert_eq!(i16::try_from(get("RSSILowThreshold")).unwrap(), -90);
    assert_eq!(u16::try_from(get("RSSILowTimeout")).unwrap(), 8);
    assert_eq!(u16::try_from(get("RSSIHighTimeout")).unwrap(), 1);
    assert_eq!(u16::try_from(get("RSSISamplingPeriod")).unwrap(), 0);

    // Still registered once the link is up, and no scan ever.
    eventually("connected", || async {
        rig.device_prop::<bool>("Connected").await
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.bluez.monitors.lock().unwrap().len(), 1);
    assert_eq!(rig.bluez.discovery_calls(), 0, "no StartDiscovery");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_keys_are_asked_once_and_kept_in_the_keyring_only() {
    let rig = Rig::start(true, fast_timing()).await;
    eventually("keys kept", || async { rig.secrets.items().len() == 1 }).await;
    let item = rig.secrets.items()[0].clone();
    assert_eq!(item.value, test_keys().to_bytes());
    assert_eq!(item.attributes["address"], ADDRESS);
    assert_eq!(item.attributes["xdg:schema"], podsd::keys::SCHEMA);
    assert!(!item.label.contains(ADDRESS), "label: {}", item.label);
    let algorithms = rig.secrets.state.lock().unwrap().algorithms.clone();
    assert!(!algorithms.is_empty() && algorithms.iter().all(|a| a == "plain"));
    let requests = || {
        rig.airpods
            .received()
            .iter()
            .filter(|p| **p == pods_proto::packet::request_keys())
            .count()
    };
    assert_eq!(requests(), 1);

    // Reconnecting asks no more: the keyring has them.
    rig.airpods.hang_up();
    eventually("reconnected", || async { rig.airpods.connects() == 2 }).await;
    eventually("open again", || async {
        rig.device_prop::<bool>("Connected").await
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(requests(), 1);

    // Never in the configuration file.
    rig.call("SetPauseOnRemove", &(false,)).await.unwrap();
    let saved = std::fs::read_to_string(rig.config_path()).unwrap();
    let hex: String = test_keys()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(!saved.contains(&hex[..16]), "{saved}");
    assert!(!saved.contains(&hex[64 - 16..]), "{saved}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_known_pair_is_not_asked_again() {
    let rig = Rig::with(Setup {
        keys_in_keyring: true,
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    eventually("open", || async {
        rig.device_prop::<String>("Firmware").await == "6619472"
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !rig.airpods
            .received()
            .contains(&pods_proto::packet::request_keys())
    );
    assert_eq!(rig.secrets.items().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_lid_opens_and_closes_the_card() {
    let rig = Rig::with(Setup {
        connected: false,
        keys_in_keyring: true,
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    rig.monitor_registered().await;
    let opened = rig.signals(MANAGER_IFACE, "CaseOpened").await;
    let closed = rig.signals(MANAGER_IFACE, "CaseClosed").await;
    let keys = test_keys();
    let addr = our_address(0x41);

    // The first advertisement after the case wakes still says closed.
    let ble = rig
        .bluez
        .advertise(&addr, pro1_advert(&keys, true, 2, 62), -50)
        .await;
    rig.bluez.monitor_call("DeviceFound", &ble).await;
    // Then open, counter moved: the card at once.
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 3, 62), -50)
        .await;
    eventually("CaseOpened", || async { opened.lock().unwrap().len() == 1 }).await;
    let (path, card) = card_of(&opened.lock().unwrap()[0]);
    assert_eq!(path, DEVICE);
    assert_eq!(text(&card, "name").as_deref(), Some("AirPods Pro"));
    assert_eq!(text(&card, "model").as_deref(), Some("0x200E"));
    assert_eq!(text(&card, "color").as_deref(), Some("white"));
    assert_eq!(flag(&card, "connected"), Some(false));
    // Exact levels from the encrypted tail.
    assert_eq!(byte(&card, "right"), Some(93));
    assert_eq!(byte(&card, "left"), Some(84));
    assert_eq!(byte(&card, "case"), Some(62));
    assert_eq!(flag(&card, "rightCharging"), Some(true));
    assert_eq!(flag(&card, "caseCharging"), Some(false));
    assert_eq!(byte(&card, "budsInCase"), Some(2));
    assert_eq!(rig.device_prop::<String>("Color").await, "white");

    // The lid closes.
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, true, 3, 62), -50)
        .await;
    eventually("CaseClosed", || async { closed.lock().unwrap().len() == 1 }).await;
    let (path,): (OwnedObjectPath,) = closed.lock().unwrap()[0].body().deserialize().unwrap();
    assert_eq!(path.as_str(), DEVICE);

    // Opened again, then BlueZ hears nothing for 8 s: DeviceLost closes.
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 4, 61), -50)
        .await;
    eventually("opened again", || async {
        opened.lock().unwrap().len() == 2
    })
    .await;
    rig.bluez.monitor_call("DeviceLost", &ble).await;
    eventually("closed by DeviceLost", || async {
        closed.lock().unwrap().len() == 2
    })
    .await;
    let saved = std::fs::read_to_string(rig.config_path()).unwrap();
    assert!(saved.contains("color = \"white\""), "{saved}");
    assert_eq!(rig.bluez.discovery_calls(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_moving_counter_the_open_lid_must_hold() {
    let rig = Rig::with(Setup {
        connected: false,
        keys_in_keyring: true,
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    rig.monitor_registered().await;
    let opened = rig.signals(MANAGER_IFACE, "CaseOpened").await;
    let keys = test_keys();
    let addr = our_address(0x52);
    // An open lid read once, then closed within 300 ms: nothing.
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 5, 50), -50)
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, true, 5, 50), -50)
        .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(opened.lock().unwrap().is_empty());
    // Open again and left so: the card after the debounce.
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 5, 50), -50)
        .await;
    eventually("opened after holding", || async {
        opened.lock().unwrap().len() == 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn other_peoples_airpods_never_open_a_card() {
    let rig = Rig::with(Setup {
        connected: false,
        keys_in_keyring: true,
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    rig.monitor_registered().await;
    let opened = rig.signals(MANAGER_IFACE, "CaseOpened").await;
    let theirs = neighbour_keys();
    let addr = theirs.address_for([0x61, 0x02, 0x03]);
    rig.bluez
        .advertise(&addr, pro1_advert(&theirs, true, 1, 40), -45)
        .await;
    rig.bluez
        .advertise(&addr, pro1_advert(&theirs, false, 2, 40), -45)
        .await;
    // Ours, but far away (below the RSSI threshold).
    let far = our_address(0x63);
    rig.bluez
        .advertise(&far, pro1_advert(&test_keys(), true, 1, 40), -88)
        .await;
    rig.bluez
        .advertise(&far, pro1_advert(&test_keys(), false, 2, 40), -88)
        .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(opened.lock().unwrap().is_empty());
    assert_eq!(rig.device_prop::<String>("Color").await, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changing_card_is_sent_again_at_most_every_two_seconds() {
    let rig = Rig::with(Setup {
        connected: false,
        keys_in_keyring: true,
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    rig.monitor_registered().await;
    let opened = rig.signals(MANAGER_IFACE, "CaseOpened").await;
    let keys = test_keys();
    let addr = our_address(0x44);
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, true, 1, 70), -50)
        .await;
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 2, 70), -50)
        .await;
    eventually("opened", || async { opened.lock().unwrap().len() == 1 }).await;
    let t0 = std::time::Instant::now();
    // The case level changes twice quickly: one update, after 2 s.
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 2, 71), -50)
        .await;
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 2, 72), -50)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(opened.lock().unwrap().len(), 1);
    eventually("update", || async { opened.lock().unwrap().len() == 2 }).await;
    assert!(t0.elapsed() >= Duration::from_millis(1500));
    let (_, card) = card_of(&opened.lock().unwrap()[1]);
    assert_eq!(byte(&card, "case"), Some(72));
    // Nothing changes: nothing more.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    assert_eq!(opened.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_from_the_card_goes_through_bluez() {
    let rig = Rig::with(Setup {
        connected: false,
        keys_in_keyring: true,
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    rig.monitor_registered().await;
    let opened = rig.signals(MANAGER_IFACE, "CaseOpened").await;
    let keys = test_keys();
    let addr = our_address(0x45);
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, true, 6, 55), -50)
        .await;
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 7, 55), -50)
        .await;
    eventually("opened", || async { opened.lock().unwrap().len() == 1 }).await;
    assert_eq!(
        flag(&card_of(&opened.lock().unwrap()[0]).1, "connected"),
        Some(false)
    );

    rig.call("Connect", &()).await.unwrap();
    eventually("connected", || async {
        rig.device_prop::<bool>("Connected").await
    })
    .await;
    assert_eq!(rig.airpods.connects(), 1);
    // The open card follows: connected now.
    eventually("card says connected", || async {
        opened
            .lock()
            .unwrap()
            .iter()
            .any(|m| flag(&card_of(m).1, "connected") == Some(true))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_keyring_still_gives_a_card_this_run() {
    let rig = Rig::with(Setup {
        connected: true,
        keyring_locked: true,
        ..Setup::default()
    })
    .await;
    rig.monitor_registered().await;
    let opened = rig.signals(MANAGER_IFACE, "CaseOpened").await;
    eventually("keys asked", || async {
        rig.airpods
            .received()
            .contains(&pods_proto::packet::request_keys())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        rig.secrets.items().is_empty(),
        "nothing written while locked"
    );
    let keys = test_keys();
    let addr = our_address(0x46);
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, true, 1, 30), -50)
        .await;
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 2, 30), -50)
        .await;
    eventually("opened", || async { opened.lock().unwrap().len() == 1 }).await;
    assert_eq!(
        flag(&card_of(&opened.lock().unwrap()[0]).1, "connected"),
        Some(true)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_awareness_on_a_model_that_has_it() {
    let rig = Rig::with(Setup {
        modalias: "bluetooth:v004Cp2024d0A4C",
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    eventually("open", || async {
        rig.device_prop::<String>("Firmware").await == "6619472"
    })
    .await;
    eventually("keys kept", || async { rig.secrets.items().len() == 1 }).await;
    assert!(
        rig.device_prop::<Vec<String>>("Capabilities")
            .await
            .contains(&"conversation".to_string())
    );
    assert!(!rig.device_prop::<bool>("ConversationAwareness").await);
    rig.call("SetConversationAwareness", &(true,))
        .await
        .unwrap();
    assert_eq!(
        rig.airpods.received().last().unwrap(),
        &pods_proto::packet::set_conversation_awareness(true)
    );
    assert!(rig.device_prop::<bool>("ConversationAwareness").await);
    // The buds report it off (changed on the phone).
    rig.airpods
        .inject(pods_proto::packet::set_conversation_awareness(false));
    eventually("reported off", || async {
        !rig.device_prop::<bool>("ConversationAwareness").await
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stem_shows_the_real_press_and_hold_cycle() {
    use pods_proto::NoiseMode::{Anc, Off, Transparency};
    use pods_proto::packet::set_noise_mode;
    let rig = Rig::start(true, fast_timing()).await;
    listed(&rig).await;
    eventually("open", || async {
        rig.device_prop::<String>("NoiseMode").await == "anc"
    })
    .await;
    eventually("keys kept", || async { rig.secrets.items().len() == 1 }).await;
    assert_eq!(
        rig.device_prop::<Vec<String>>("PressHoldModes").await,
        ["transparency", "anc"]
    );
    // What the real Pro 1 does: ANC → transparency → off → ANC.
    rig.airpods.inject(set_noise_mode(Transparency));
    rig.airpods.inject(set_noise_mode(Off));
    rig.airpods.inject(set_noise_mode(Anc));
    eventually("followed", || async {
        rig.device_prop::<String>("NoiseMode").await == "anc"
    })
    .await;
    // Not a full tour yet (the first press only starts the run).
    assert_eq!(
        rig.device_prop::<Vec<String>>("PressHoldModes").await,
        ["transparency", "anc"]
    );
    // Back to transparency: the tour is complete.
    rig.airpods.inject(set_noise_mode(Transparency));
    eventually("learned", || async {
        rig.device_prop::<Vec<String>>("PressHoldModes").await == ["off", "anc", "transparency"]
    })
    .await;
    eventually("kept in the configuration", || async {
        let saved = std::fs::read_to_string(rig.config_path()).unwrap_or_default();
        let flat: String = saved.split_whitespace().collect();
        flat.contains(r#"press_hold_cycle=["off","anc","transparency""#)
    })
    .await;
    // A click on the tile goes where the stem would: transparency → off.
    rig.call("CycleNoiseMode", &()).await.unwrap();
    assert_eq!(rig.airpods.received().last().unwrap(), &set_noise_mode(Off));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_learned_cycle_is_known_at_the_next_start() {
    let rig = Rig::with(Setup {
        connected: false,
        config: Some(
            "[devices.\"AA:BB:CC:DD:EE:01\"]\npress_hold_cycle = [\"off\", \"anc\", \"transparency\"]\ncolor = \"black\"\n",
        ),
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    assert_eq!(
        rig.device_prop::<Vec<String>>("PressHoldModes").await,
        ["off", "anc", "transparency"]
    );
    assert_eq!(rig.device_prop::<String>("Color").await, "black");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keyring_locked_at_start_is_asked_again() {
    let rig = Rig::with(Setup {
        connected: false,
        keys_in_keyring: true,
        keyring_locked: true,
        ..Setup::default()
    })
    .await;
    listed(&rig).await;
    rig.monitor_registered().await;
    let opened = rig.signals(MANAGER_IFACE, "CaseOpened").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The session unlocks the keyring; the first advertisement nobody
    // resolves sends the engine back to it.
    rig.secrets.lock(false);
    let keys = test_keys();
    let addr = our_address(0x47);
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, true, 1, 45), -50)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 2, 45), -50)
        .await;
    eventually("opened", || async { opened.lock().unwrap().len() == 1 }).await;
}
