// SPDX-License-Identifier: GPL-3.0-or-later
//! Frames captured on AirPods Pro (1st generation) during M0, replayed.

use pods_proto::control::id;
use pods_proto::packet;
use pods_proto::{Message, NoiseMode, Part, Side, Wearing, decode, model_for_number};

struct Line {
    dir: String,
    bytes: Vec<u8>,
}

fn load(text: &str) -> Vec<Line> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut cols = l.split_whitespace();
            let dir = cols.next().unwrap().to_string();
            let _t = cols.next().unwrap();
            let hex = cols.next().unwrap();
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            Line { dir, bytes }
        })
        .collect()
}

fn received(text: &str) -> Vec<Message> {
    load(text)
        .into_iter()
        .filter(|l| l.dir == "rx")
        .map(|l| decode(&l.bytes).expect("every received packet is AAP"))
        .collect()
}

const ONE_BUD: &str = include_str!("fixtures/m0-pro1-one-bud.txt");
const TWO_BUDS: &str = include_str!("fixtures/m0-pro1-two-buds.txt");

#[test]
fn we_send_what_the_probe_sent() {
    let sent: Vec<Vec<u8>> = load(ONE_BUD)
        .into_iter()
        .filter(|l| l.dir == "tx")
        .map(|l| l.bytes)
        .collect();
    let opening: Vec<Vec<u8>> = packet::opening().iter().map(|(p, _)| p.to_vec()).collect();
    assert_eq!(sent[..3], opening[..]);
    assert_eq!(sent[3], packet::set_noise_mode(NoiseMode::Transparency));
    assert_eq!(sent[4], packet::set_noise_mode(NoiseMode::Anc));
}

#[test]
fn the_connect_burst_decodes() {
    let msgs = received(ONE_BUD);
    assert_eq!(msgs[0], Message::HandshakeAck);

    // Two information records, firmware in record 0x04.
    let infos: Vec<_> = msgs
        .iter()
        .filter_map(|m| match m {
            Message::Info(i) => Some(i),
            _ => None,
        })
        .collect();
    assert_eq!(infos.len(), 2);
    assert!(infos.iter().all(|i| i.model_number == "A2084"));
    assert!(infos.iter().all(|i| i.name == "Test AirPods Pro 1"));
    let fw = infos
        .iter()
        .find(|i| i.record == pods_proto::info::FIRMWARE_RECORD)
        .and_then(|i| i.firmware());
    assert_eq!(fw, Some("6619472"));
    assert_eq!(model_for_number("A2084"), Some(0x200E));

    // The settings the Pro 1 reports on connect, in order.
    let controls: Vec<u8> = msgs
        .iter()
        .filter_map(|m| match m {
            Message::Control { id, .. } => Some(*id),
            _ => None,
        })
        .take(6)
        .collect();
    assert_eq!(
        controls,
        [
            id::LISTENING_MODE,
            id::ONE_BUD_ANC,
            id::CALL_MANAGEMENT,
            id::CLICK_HOLD_INTERVAL,
            id::DOUBLE_CLICK_INTERVAL,
            id::CHIME_VOLUME
        ]
    );
    assert!(msgs.contains(&Message::Control {
        id: id::LISTENING_MODE,
        value: [2, 0, 0, 0]
    }));
    // No capabilities reply on this model.
    assert!(!msgs.iter().any(|m| matches!(m, Message::Capabilities(_))));
}

#[test]
fn one_bud_worn_the_other_in_the_case() {
    let msgs = received(ONE_BUD);
    let battery = msgs
        .iter()
        .find_map(|m| match m {
            Message::Battery(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    let level = |p: Part| battery.iter().find(|c| c.part == p).unwrap().level;
    assert_eq!(level(Part::Right), Some(86));
    assert_eq!(level(Part::Left), None);
    assert_eq!(level(Part::Case), None);

    assert!(msgs.contains(&Message::Ear {
        primary: Wearing::Ear,
        secondary: Wearing::Case
    }));
    assert!(msgs.contains(&Message::PrimaryBud(Side::Right)));
}

#[test]
fn the_noise_mode_round_trip_is_echoed() {
    let modes: Vec<NoiseMode> = received(ONE_BUD)
        .iter()
        .filter_map(|m| match m {
            Message::Control {
                id: id::LISTENING_MODE,
                value,
            } => NoiseMode::from_wire(value[0]),
            _ => None,
        })
        .collect();
    assert_eq!(
        modes,
        [NoiseMode::Anc, NoiseMode::Transparency, NoiseMode::Anc]
    );
}

#[test]
fn both_buds_worn() {
    let msgs = received(TWO_BUDS);
    assert!(msgs.contains(&Message::Ear {
        primary: Wearing::Ear,
        secondary: Wearing::Ear
    }));
    let battery = msgs
        .iter()
        .find_map(|m| match m {
            Message::Battery(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    let level = |p: Part| battery.iter().find(|c| c.part == p).unwrap().level;
    assert_eq!(level(Part::Right), Some(81));
    assert_eq!(level(Part::Left), Some(95));
    assert_eq!(level(Part::Case), None);
}

/// MAC-bearing opcodes stay opaque.
#[test]
fn address_frames_are_not_interpreted() {
    let others: Vec<u8> = received(ONE_BUD)
        .iter()
        .filter_map(|m| match m {
            Message::Other { opcode, .. } => Some(*opcode),
            _ => None,
        })
        .collect();
    assert_eq!(others, [0x2B, 0x0C, 0x0C, 0x0E, 0x2E]);
}
