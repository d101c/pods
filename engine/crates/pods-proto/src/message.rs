// SPDX-License-Identifier: GPL-3.0-or-later
// Battery and ear decoding derived from podctl `src/aap.rs`
// (https://github.com/Rockykln/podctl), Copyright (c) 2026 pods contributors,
// MIT OR Apache-2.0. See engine/NOTICE.
//! What the buds send, decoded.

use crate::ble::{self, ProximityKeys};
use crate::frame::{Frame, op};
use crate::info::DeviceInfo;
use crate::model::{Part, Side, Wearing};

/// One part of a battery notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatteryComponent {
    pub part: Part,
    /// 0–100, `None` when the part is not there (in a closed case, say).
    pub level: Option<u8>,
    pub charging: bool,
}

/// A received packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Reply to our handshake (`01 00 04 00 00 00 …`).
    HandshakeAck,
    /// `02 00 04 00 …`: the buds close the session; nothing more comes on
    /// this socket. Seen when another host takes the AAP channel.
    SessionEnd,
    Battery(Vec<BatteryComponent>),
    /// Ear status of the primary bud (the one carrying the microphone)
    /// and of the other one.
    Ear {
        primary: Wearing,
        secondary: Wearing,
    },
    /// Which bud is primary.
    PrimaryBud(Side),
    /// A control value, the four value bytes as sent.
    Control {
        id: u8,
        value: [u8; 4],
    },
    Info(Box<DeviceInfo>),
    /// The proximity keys (`0x31`), in reply to our `0x30`. Never logged:
    /// their `Debug` is withheld.
    Keys(ProximityKeys),
    /// Capabilities (`0x02`), raw; the Pro 1 never sends them.
    Capabilities(Vec<u8>),
    /// Anything else: opcode and payload length only, since several of
    /// those (0x0C, 0x0E, 0x2B, 0x2E) carry MAC addresses.
    Other {
        opcode: u8,
        len: usize,
    },
}

/// Battery part bytes: 1 headset (AirPods Max), 2 right, 4 left, 8 case.
fn part_from_wire(b: u8) -> Option<Part> {
    match b {
        0x02 => Some(Part::Right),
        0x04 => Some(Part::Left),
        0x08 => Some(Part::Case),
        _ => None,
    }
}

mod battery_status {
    pub const CHARGING: u8 = 0x01;
    pub const DISCONNECTED: u8 = 0x04;
}

/// `[count] ([part] 01 [level] [status] 01) × count`. Unknown parts are
/// skipped; a count larger than the payload is cut to what fits.
pub fn parse_battery(payload: &[u8]) -> Vec<BatteryComponent> {
    let Some((&declared, rest)) = payload.split_first() else {
        return Vec::new();
    };
    rest.as_chunks::<5>()
        .0
        .iter()
        .take(declared as usize)
        .filter_map(|c| {
            let part = part_from_wire(c[0])?;
            let status = c[3];
            let level = (status != battery_status::DISCONNECTED && c[2] <= 100).then_some(c[2]);
            Some(BatteryComponent {
                part,
                level,
                charging: status == battery_status::CHARGING,
            })
        })
        .collect()
}

/// Decode one packet. `None` for something that is not an AAP frame.
pub fn decode(buf: &[u8]) -> Option<Message> {
    let frame = Frame::parse(buf)?;
    match buf[0] {
        0x01 if frame.opcode == op::HANDSHAKE => return Some(Message::HandshakeAck),
        0x02 => return Some(Message::SessionEnd),
        0x04 => {}
        _ => return None,
    }
    let p = &frame.payload;
    Some(match frame.opcode {
        op::BATTERY => Message::Battery(parse_battery(p)),
        op::EAR if p.len() >= 2 => Message::Ear {
            primary: Wearing::from_wire(p[0]),
            secondary: Wearing::from_wire(p[1]),
        },
        // Pro 1: `02 00 00 00` with only the right bud out, `02 00 01 00`
        // with both. The first byte reads as a battery part byte (2 =
        // right), which is the bud that was primary in every capture.
        op::PRIMARY_BUD if !p.is_empty() && matches!(p[0], 0x02 | 0x04) => {
            Message::PrimaryBud(if p[0] == 0x04 {
                Side::Left
            } else {
                Side::Right
            })
        }
        op::CONTROL if !p.is_empty() => {
            let mut value = [0u8; 4];
            for (dst, src) in value.iter_mut().zip(&p[1..]) {
                *dst = *src;
            }
            Message::Control { id: p[0], value }
        }
        op::INFORMATION => match DeviceInfo::parse(p) {
            Some(info) => Message::Info(Box::new(info)),
            None => Message::Other {
                opcode: frame.opcode,
                len: p.len(),
            },
        },
        op::CAPABILITIES => Message::Capabilities(p.clone()),
        op::KEYS => match ble::parse_keys(p) {
            Some(keys) => Message::Keys(keys),
            None => Message::Other {
                opcode: frame.opcode,
                len: p.len(),
            },
        },
        opcode => Message::Other {
            opcode,
            len: p.len(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_battery_example() {
        // LibrePods "AAP Definitions": R 100 % discharging, L 99 % charging, case 17 %.
        let p = [
            0x03, 0x02, 0x01, 0x64, 0x02, 0x01, 0x04, 0x01, 0x63, 0x01, 0x01, 0x08, 0x01, 0x11,
            0x02, 0x01,
        ];
        let b = parse_battery(&p);
        assert_eq!(
            b,
            vec![
                BatteryComponent {
                    part: Part::Right,
                    level: Some(100),
                    charging: false
                },
                BatteryComponent {
                    part: Part::Left,
                    level: Some(99),
                    charging: true
                },
                BatteryComponent {
                    part: Part::Case,
                    level: Some(17),
                    charging: false
                },
            ]
        );
    }

    #[test]
    fn a_disconnected_part_has_no_level() {
        // Pro 1, left bud in the closed case: `04 01 00 04 01`.
        let p = [3, 2, 1, 86, 2, 1, 4, 1, 0, 4, 1, 8, 1, 0, 4, 1];
        let b = parse_battery(&p);
        assert_eq!(b[0].level, Some(86));
        assert_eq!(b[1].level, None);
        assert_eq!(b[2].level, None);
    }

    #[test]
    fn battery_survives_bad_input() {
        assert!(parse_battery(&[]).is_empty());
        assert_eq!(parse_battery(&[3, 2, 1, 50, 2, 1]).len(), 1);
        assert_eq!(parse_battery(&[255, 2, 1, 50, 2, 1, 4]).len(), 1);
        // Unknown part and impossible level.
        let b = parse_battery(&[2, 0x01, 1, 50, 2, 1, 0x02, 1, 200, 2, 1]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].level, None);
    }

    #[test]
    fn control_values_are_padded() {
        let m = decode(&[4, 0, 4, 0, 9, 0, 0x0D, 3]).unwrap();
        assert_eq!(
            m,
            Message::Control {
                id: 0x0D,
                value: [3, 0, 0, 0]
            }
        );
        let m = decode(&[4, 0, 4, 0, 9, 0, 0x1F, 0x50, 0x50, 0, 0, 0xEE]).unwrap();
        assert_eq!(
            m,
            Message::Control {
                id: 0x1F,
                value: [0x50, 0x50, 0, 0]
            }
        );
    }

    #[test]
    fn short_frames_become_other() {
        assert_eq!(
            decode(&[4, 0, 4, 0, 6, 0, 0]),
            Some(Message::Other { opcode: 6, len: 1 })
        );
        assert_eq!(
            decode(&[4, 0, 4, 0, 9, 0]),
            Some(Message::Other { opcode: 9, len: 0 })
        );
    }

    #[test]
    fn session_packets() {
        assert_eq!(
            decode(&[1, 0, 4, 0, 0, 0, 1, 0, 3, 0]),
            Some(Message::HandshakeAck)
        );
        assert_eq!(decode(&[2, 0, 4, 0, 0, 0]), Some(Message::SessionEnd));
        assert_eq!(decode(&[7, 0, 4, 0, 0, 0]), None);
        assert_eq!(decode(&[1, 0, 4, 0, 9, 0, 0x0D, 2]), None);
    }

    #[test]
    fn the_key_reply_decodes_without_showing() {
        let keys = ProximityKeys {
            irk: [0x11; 16],
            enc: [0x22; 16],
        };
        let mut frame = vec![4, 0, 4, 0, op::KEYS, 0];
        frame.extend(ble::keys_payload(&keys));
        let m = decode(&frame).unwrap();
        assert_eq!(m, Message::Keys(keys));
        let shown = format!("{m:?}");
        assert!(!shown.contains("11") && !shown.contains("22"), "{shown}");
        // A reply cut short is not keys.
        assert_eq!(
            decode(&frame[..20]),
            Some(Message::Other {
                opcode: op::KEYS,
                len: 14
            })
        );
    }

    #[test]
    fn primary_bud() {
        assert_eq!(
            decode(&[4, 0, 4, 0, 8, 0, 2, 0, 1, 0]),
            Some(Message::PrimaryBud(Side::Right))
        );
        assert_eq!(
            decode(&[4, 0, 4, 0, 8, 0, 4, 0, 1, 0]),
            Some(Message::PrimaryBud(Side::Left))
        );
        assert_eq!(
            decode(&[4, 0, 4, 0, 8, 0, 9]),
            Some(Message::Other { opcode: 8, len: 1 })
        );
    }
}
