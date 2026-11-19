// SPDX-License-Identifier: GPL-3.0-or-later
// Address resolution (`ah`, `resolves`, `parse_addr`) and the clear part of
// the advertisement are derived from podctl `src/ble.rs`
// (https://github.com/Rockykln/podctl), Copyright (c) 2026 pods contributors,
// MIT OR Apache-2.0. The key reply layout follows podctl `src/aap.rs`. The
// encrypted part, the colour and connection bytes are protocol facts from
// LibrePods Android (`BLEManager.kt`, GPL-3.0-or-later). See engine/NOTICE.
//! The proximity advertisement AirPods send over Bluetooth Low Energy,
//! and the keys that tell ours from everybody else's.
//!
//! ```text
//! manufacturer data of company 0x004C:
//!  0  07        proximity pairing
//!  1  19        length (25)
//!  2  01        paired
//!  3  model     u16 little endian (0E 20 = 0x200E)
//!  5  status    bit 5 primary is left, bit 6 this bud in the case,
//!               bit 2 both in the case, bit 4 at least one in the case,
//!               bits 1 and 3 in ear (swapped with the primary)
//!  6  buds      two nibbles, tens of percent, primary first, F unknown
//!  7  case      low nibble case level, high nibble charging flags
//!  8  lid       bit 3 closed, bits 0-2 opening counter
//!  9  colour
//! 10  connection state
//! 11  16 bytes  AES-128-ECB with the ENC key: byte 1 primary bud,
//!               byte 2 other bud, byte 3 case (bit 7 charging,
//!               bits 0-6 percent, FF unknown)
//! ```
//!
//! AirPods Pro (1st generation): the case does not advertise on its own;
//! the lid byte comes in the buds' advertisement.

use std::fmt;

use crate::aes;

/// Apple's company identifier.
pub const APPLE: u16 = 0x004C;
/// First byte of the manufacturer data: proximity pairing.
pub const PROXIMITY_PAIRING: u8 = 0x07;
/// Length byte of a proximity pairing message.
pub const PROXIMITY_LEN: u8 = 0x19;
/// Bytes of manufacturer data after the company id: type, length, 25.
pub const ADVERT_LEN: usize = 2 + PROXIMITY_LEN as usize;
/// The content an `or_patterns` monitor matches at position 0 of the
/// manufacturer specific AD (type 0xFF): company id, then type and length.
pub const MONITOR_PATTERN: [u8; 4] = [0x4C, 0x00, PROXIMITY_PAIRING, PROXIMITY_LEN];
/// AD type of manufacturer specific data.
pub const AD_MANUFACTURER: u8 = 0xFF;

// ------------------------------------------------------------------ keys

/// The two proximity keys of a pair of AirPods, byte for byte as the buds
/// hand them out in reply to `0x30`: the identity resolving key behind
/// their rotating address, and the key over the encrypted tail.
///
/// Never printed: `Debug` shows nothing of them.
#[derive(Clone, PartialEq, Eq)]
pub struct ProximityKeys {
    pub irk: [u8; 16],
    pub enc: [u8; 16],
}

impl fmt::Debug for ProximityKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProximityKeys(withheld)")
    }
}

impl ProximityKeys {
    /// Both keys, IRK first, as stored in the keyring.
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[..16].copy_from_slice(&self.irk);
        out[16..].copy_from_slice(&self.enc);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 32 {
            return None;
        }
        Some(Self {
            irk: bytes[..16].try_into().ok()?,
            enc: bytes[16..].try_into().ok()?,
        })
    }

    /// The IRK in the byte order of the Core Specification. The buds send
    /// it least significant byte first (podctl checked this against 16
    /// addresses of a real pair; LibrePods turns it around the same way).
    fn irk_spec(&self) -> [u8; 16] {
        let mut k = self.irk;
        k.reverse();
        k
    }

    /// Whether `address` (as BlueZ prints it) is one of these AirPods'
    /// resolvable private addresses.
    pub fn resolves(&self, address: &str) -> bool {
        parse_addr(address).is_some_and(|a| resolves(&self.irk_spec(), a))
    }

    /// A resolvable private address of these AirPods for `prand` (its two
    /// top bits are forced to `01`). For tests and fixtures.
    pub fn address_for(&self, prand: [u8; 3]) -> String {
        let prand = [(prand[0] & 0x3F) | 0x40, prand[1], prand[2]];
        let hash = ah(&self.irk_spec(), prand);
        [prand[0], prand[1], prand[2], hash[0], hash[1], hash[2]]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The decrypted tail of an advertisement. The ENC key is used as the
    /// buds send it (LibrePods does the same).
    pub fn decrypt(&self, data: &[u8]) -> Option<[u8; 16]> {
        if data.len() < ADVERT_LEN {
            return None;
        }
        let tail: [u8; 16] = data[data.len() - 16..].try_into().ok()?;
        Some(aes::decrypt_block(&self.enc, &tail))
    }

    /// The opposite of [`Self::decrypt`], to build test advertisements.
    pub fn encrypt(&self, plain: &[u8; 16]) -> [u8; 16] {
        aes::encrypt_block(&self.enc, plain)
    }
}

/// Key types in the `0x31` reply.
const KEY_IRK: u8 = 0x01;
const KEY_ENC: u8 = 0x04;

/// The keys in a `0x31` payload (after opcode and sub-opcode): a count,
/// then per key its type, a 16-bit big-endian length, one reserved byte
/// and the key. Type 1 is the IRK, type 4 the ENC key.
pub fn parse_keys(payload: &[u8]) -> Option<ProximityKeys> {
    let mut irk = None;
    let mut enc = None;
    let mut rest = payload.get(1..)?;
    while rest.len() >= 4 {
        let kind = rest[0];
        let len = u16::from_be_bytes([rest[1], rest[2]]) as usize;
        let body = rest.get(4..4 + len)?;
        if len == 16 {
            let key: [u8; 16] = body.try_into().ok()?;
            match kind {
                KEY_IRK => irk = Some(key),
                KEY_ENC => enc = Some(key),
                _ => {}
            }
        }
        rest = &rest[4 + len..];
    }
    Some(ProximityKeys {
        irk: irk?,
        enc: enc?,
    })
}

/// The `0x31` payload for `keys`: what the buds send. For tests.
pub fn keys_payload(keys: &ProximityKeys) -> Vec<u8> {
    let mut p = vec![2];
    for (kind, key) in [(KEY_IRK, &keys.irk), (KEY_ENC, &keys.enc)] {
        p.extend_from_slice(&[kind, 0x00, 0x10, 0x00]);
        p.extend_from_slice(key);
    }
    p
}

// ------------------------------------------------------- address resolution

/// The `ah` hash from the Core Specification, Vol 3 Part H 2.2.2:
/// AES-128 over the 24-bit input padded to a block, keyed with the IRK,
/// keeping the low 24 bits.
pub fn ah(irk: &[u8; 16], prand: [u8; 3]) -> [u8; 3] {
    let mut m = [0u8; 16];
    m[13..].copy_from_slice(&prand);
    let e = aes::encrypt_block(irk, &m);
    [e[13], e[14], e[15]]
}

/// Whether `addr` is a resolvable private address belonging to `irk`
/// (specification byte order). `addr` is most significant byte first, as
/// BlueZ prints it: the top three bytes are `prand`, the bottom three the
/// hash to check.
pub fn resolves(irk: &[u8; 16], addr: [u8; 6]) -> bool {
    // Bits 7..6 of the most significant byte mark an RPA (0b01).
    if addr[0] >> 6 != 0b01 {
        return false;
    }
    let prand = [addr[0], addr[1], addr[2]];
    ah(irk, prand) == [addr[3], addr[4], addr[5]]
}

/// Parse `AA:BB:CC:DD:EE:FF` into bytes, most significant first.
pub fn parse_addr(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut out {
        let p = parts.next()?;
        if p.len() != 2 {
            return None;
        }
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

// ---------------------------------------------------------- the advertisement

/// One part as the clear bytes give it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pod {
    /// Percent in steps of ten, or `None` while the part does not report.
    pub battery: Option<u8>,
    pub charging: bool,
    pub in_ear: bool,
}

/// The clear part of a proximity advertisement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advert {
    pub paired: bool,
    pub model: u16,
    pub primary_left: bool,
    pub this_in_case: bool,
    pub both_in_case: bool,
    /// Status bit 4: set whenever a bud lies in the case (both included).
    pub one_in_case: bool,
    pub left: Pod,
    pub right: Pod,
    pub case: Pod,
    /// Lid closed. Only meaningful while both buds are in the case: with
    /// one in an ear it reads "open" whatever the lid does (podctl).
    pub lid_closed: bool,
    /// Wraps at 8; a change means the lid moved even if the state reads
    /// the same.
    pub lid_counter: u8,
    pub color: u8,
    pub connection: u8,
}

/// Status bits.
mod status {
    pub const RIGHT_EAR: u8 = 0x02;
    pub const BOTH_IN_CASE: u8 = 0x04;
    pub const LEFT_EAR: u8 = 0x08;
    pub const ONE_IN_CASE: u8 = 0x10;
    pub const PRIMARY_LEFT: u8 = 0x20;
    pub const THIS_IN_CASE: u8 = 0x40;
}

/// Reads `data`, the manufacturer data of company 0x004C (starting with
/// `07 19`). `None` for any other Apple advertisement.
pub fn parse_advert(data: &[u8]) -> Option<Advert> {
    if data.len() < 11 || data[0] != PROXIMITY_PAIRING || data[1] != PROXIMITY_LEN {
        return None;
    }
    let model = u16::from_le_bytes([data[3], data[4]]);
    let st = data[5];
    let pods = data[6];
    let case_byte = data[7];
    let lid = data[8];

    let primary_left = st & status::PRIMARY_LEFT != 0;
    // Everything the buds report is ordered primary first, so the two
    // halves swap as soon as the right bud leads.
    let flipped = !primary_left;
    // Ten tenths is full; 15 is the documented "unknown", and the values
    // in between are not an encoding we know.
    let level = |nibble: u8| (nibble <= 10).then_some(nibble * 10);
    let (left_nib, right_nib) = if flipped {
        (pods >> 4, pods & 0x0f)
    } else {
        (pods & 0x0f, pods >> 4)
    };
    let flags = case_byte >> 4;
    let (left_charge, right_charge) = if flipped {
        (flags & 0x02 != 0, flags & 0x01 != 0)
    } else {
        (flags & 0x01 != 0, flags & 0x02 != 0)
    };
    let this_in_case = st & status::THIS_IN_CASE != 0;
    let ear_flipped = flipped ^ this_in_case;
    let (left_ear, right_ear) = if ear_flipped {
        (st & status::LEFT_EAR != 0, st & status::RIGHT_EAR != 0)
    } else {
        (st & status::RIGHT_EAR != 0, st & status::LEFT_EAR != 0)
    };

    Some(Advert {
        paired: data[2] == 0x01,
        model,
        primary_left,
        this_in_case,
        both_in_case: st & status::BOTH_IN_CASE != 0,
        one_in_case: st & status::ONE_IN_CASE != 0,
        left: Pod {
            battery: level(left_nib),
            charging: left_charge,
            in_ear: left_ear,
        },
        right: Pod {
            battery: level(right_nib),
            charging: right_charge,
            in_ear: right_ear,
        },
        case: Pod {
            battery: level(case_byte & 0x0f),
            charging: flags & 0x04 != 0,
            in_ear: false,
        },
        lid_closed: lid & 0x08 != 0,
        lid_counter: lid & 0x07,
        color: data[9],
        connection: data[10],
    })
}

/// A battery reading: percent and charging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub percent: u8,
    pub charging: bool,
}

/// Battery of the three parts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Levels {
    pub left: Option<Level>,
    pub right: Option<Level>,
    pub case: Option<Level>,
}

fn exact_level(b: u8) -> Option<Level> {
    if b == 0xFF {
        return None;
    }
    let percent = b & 0x7F;
    (percent <= 100).then_some(Level {
        percent,
        charging: b & 0x80 != 0,
    })
}

/// A decrypted level must fall within ten points of the clear one.
const AGREEMENT: u8 = 10;

impl Advert {
    /// How many buds lie in the case: 2, 1 or 0.
    pub fn buds_in_case(&self) -> u8 {
        if self.both_in_case {
            2
        } else if self.one_in_case || self.this_in_case {
            1
        } else {
            0
        }
    }

    /// Levels in steps of ten from the clear bytes.
    pub fn coarse_levels(&self) -> Levels {
        let l = |p: Pod| {
            p.battery.map(|percent| Level {
                percent,
                charging: p.charging,
            })
        };
        Levels {
            left: l(self.left),
            right: l(self.right),
            case: l(self.case),
        }
    }

    /// Exact levels from the decrypted tail, when it agrees with the clear
    /// bytes: with a wrong key the tail decrypts to noise, and noise must
    /// not reach the screen. At least one part must be known in clear to
    /// check against; otherwise the coarse levels are all there is.
    pub fn levels(&self, plain: Option<&[u8; 16]>) -> Levels {
        let coarse = self.coarse_levels();
        let Some(plain) = plain else {
            return coarse;
        };
        let (first, second) = (exact_level(plain[1]), exact_level(plain[2]));
        let (left, right) = if self.primary_left {
            (first, second)
        } else {
            (second, first)
        };
        let exact = Levels {
            left,
            right,
            case: exact_level(plain[3]),
        };
        let pairs = [
            (exact.left, coarse.left),
            (exact.right, coarse.right),
            (exact.case, coarse.case),
        ];
        let mut checked = false;
        for (e, c) in pairs {
            if let Some(c) = c {
                checked = true;
                match e {
                    Some(e) if e.percent.abs_diff(c.percent) <= AGREEMENT => {}
                    _ => return coarse,
                }
            }
        }
        if checked { exact } else { coarse }
    }

    /// Colour name for the D-Bus `Color` property, "" when unknown.
    pub fn color_name(&self) -> &'static str {
        color_name(self.color)
    }
}

/// Colour byte → lower-case name (LibrePods' table).
pub fn color_name(b: u8) -> &'static str {
    match b {
        0x00 => "white",
        0x01 => "black",
        0x02 => "red",
        0x03 => "blue",
        0x04 => "pink",
        0x05 => "gray",
        0x06 => "silver",
        0x07 => "gold",
        0x08 => "rose-gold",
        0x09 => "space-gray",
        0x0A => "dark-blue",
        0x0B => "light-blue",
        0x0C => "yellow",
        _ => "",
    }
}

/// Fields of a test advertisement.
#[derive(Clone, Copy, Debug)]
pub struct AdvertFields {
    pub model: u16,
    pub status: u8,
    pub buds: u8,
    pub case: u8,
    pub lid: u8,
    pub color: u8,
    pub connection: u8,
    /// Decrypted tail: bytes 1, 2, 3 are primary, other, case.
    pub plain: [u8; 16],
}

/// Builds the manufacturer data of an advertisement, its tail encrypted
/// with `keys`. For tests and fixtures: nothing in the engine sends one.
pub fn build_advert(f: &AdvertFields, keys: &ProximityKeys) -> Vec<u8> {
    let [lo, hi] = f.model.to_le_bytes();
    let mut d = vec![
        PROXIMITY_PAIRING,
        PROXIMITY_LEN,
        0x01,
        lo,
        hi,
        f.status,
        f.buds,
        f.case,
        f.lid,
        f.color,
        f.connection,
    ];
    d.extend_from_slice(&keys.encrypt(&f.plain));
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Core Specification Vol 3 Part H, sample data for `ah`.
    const IRK: [u8; 16] = [
        0xec, 0x02, 0x34, 0xa3, 0x57, 0xc8, 0xad, 0x05, 0x34, 0x10, 0x10, 0xa6, 0x0a, 0x39, 0x7d,
        0x9b,
    ];

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn adv(hex: &str) -> Advert {
        parse_advert(&bytes(hex)).unwrap()
    }

    /// A test pair: not anybody's real keys.
    fn test_keys() -> ProximityKeys {
        ProximityKeys {
            irk: core::array::from_fn(|i| 0x10 + i as u8),
            enc: core::array::from_fn(|i| 0xA0 ^ (i as u8 * 7)),
        }
    }

    // Captures from podctl's tests: AirPods Pro 2 USB-C (0x2024).

    /// Both buds in the ears, case out of reach.
    #[test]
    fn podctl_capture_both_buds_in_the_ears() {
        let a = adv("07190124200b778f1100055f5740de21491ea5bcb9b3a07f8e950d");
        assert_eq!(a.model, 0x2024);
        assert!(a.paired);
        assert!(a.left.in_ear && a.right.in_ear);
        assert_eq!((a.left.battery, a.right.battery), (Some(70), Some(70)));
        assert_eq!(a.case.battery, None);
        assert!(!a.both_in_case);
        assert_eq!(a.buds_in_case(), 0);
        assert!(!a.lid_closed);
        assert_eq!(a.color_name(), "white");
    }

    /// A second after both buds went into the case, lid closing.
    #[test]
    fn podctl_capture_case_and_closing_lid() {
        let a = adv("07190124207577b83a0005be5ec35a37dcf327b8478682685f4bd6");
        assert!(a.both_in_case);
        assert_eq!(a.buds_in_case(), 2);
        assert_eq!(a.case.battery, Some(80));
        assert!(a.left.charging && a.right.charging);
        assert!(a.lid_closed);
        assert_eq!(a.lid_counter, 2);
        assert_eq!(a.connection, 0x05);
    }

    /// The first advertisement after the lid opened again still carries
    /// the old lid state.
    #[test]
    fn podctl_capture_lid_opening() {
        let a = adv("07190124201487b8580000d0d43874863e0a2b0897ff63d15ff687");
        assert!(a.lid_closed);
        assert_eq!(a.lid_counter, 0);
        assert_eq!(a.left.battery, Some(80));
    }

    /// The case level is the LOW nibble of byte 7.
    #[test]
    fn the_case_is_the_low_nibble() {
        let a = adv("07190124207577b83a0005be5ec35a37dcf327b8478682685f4bd6");
        // 0xb8: level 8 below, flags b above (both buds charging, bit 2
        // clear: the case itself is not).
        assert_eq!(a.case.battery, Some(80));
        assert!(!a.case.charging);
        assert!(a.left.charging && a.right.charging);
    }

    #[test]
    fn rejects_battery_nibbles_above_ten() {
        for nibble in 11u8..=15 {
            let pods = nibble | (nibble << 4);
            let a = adv(&format!(
                "07190124200b{pods:02x}8f1100055f5740de21491ea5bcb9b3a07f8e950d"
            ));
            assert_eq!(a.left.battery, None, "nibble {nibble}");
            assert_eq!(a.right.battery, None, "nibble {nibble}");
        }
    }

    #[test]
    fn ignores_other_apple_advertisements() {
        assert!(parse_advert(&[0x10, 0x05, 0x01]).is_none());
        assert!(parse_advert(&[]).is_none());
        // Proximity pairing of another length.
        let mut d = bytes("07190124200b778f1100055f5740de21491ea5bcb9b3a07f8e950d");
        d[1] = 0x0F;
        assert!(parse_advert(&d).is_none());
    }

    #[test]
    fn spec_sample() {
        assert_eq!(ah(&IRK, [0x70, 0x81, 0x94]), [0x0d, 0xfb, 0xaa]);
        assert!(resolves(&IRK, parse_addr("70:81:94:0d:fb:aa").unwrap()));
        let mut other = IRK;
        other[0] ^= 1;
        assert!(!resolves(&other, parse_addr("70:81:94:0d:fb:aa").unwrap()));
    }

    /// The buds send the IRK least significant byte first.
    #[test]
    fn keys_resolve_in_the_byte_order_the_buds_use() {
        let mut sent = IRK;
        sent.reverse();
        let keys = ProximityKeys {
            irk: sent,
            enc: [0; 16],
        };
        assert!(keys.resolves("70:81:94:0D:FB:AA"));
        assert!(!keys.resolves("70:81:94:0D:FB:AB"));
        let addr = keys.address_for([0x12, 0x34, 0x56]);
        assert!(addr.starts_with("52:34:56:"), "{addr}");
        assert!(keys.resolves(&addr));
        assert!(!test_keys().resolves(&addr));
    }

    #[test]
    fn rejects_addresses_that_are_not_resolvable() {
        assert!(!resolves(&IRK, parse_addr("f0:81:94:0d:fb:aa").unwrap()));
        assert!(!resolves(&IRK, parse_addr("30:81:94:0d:fb:aa").unwrap()));
        assert!(!test_keys().resolves("garbage"));
    }

    #[test]
    fn parse_addr_wants_exactly_six_bytes() {
        assert_eq!(parse_addr("40:11:22:33:44:55").unwrap()[0], 0x40);
        assert_eq!(parse_addr("40:11:22:33:44:5A").unwrap()[5], 0x5a);
        assert!(parse_addr("40:11:22:33:44").is_none());
        assert!(parse_addr("40:11:22:33:44:55:00").is_none());
        assert!(parse_addr("zz:11:22:33:44:55").is_none());
        assert!(parse_addr("4:11:22:33:44:555").is_none());
    }

    /// The reply layout podctl parses: count, then type, BE length,
    /// reserved, key.
    #[test]
    fn the_key_reply_reads_back() {
        let k = test_keys();
        let payload = keys_payload(&k);
        assert_eq!(payload.len(), 1 + 2 * 20);
        assert_eq!(&payload[1..5], &[1, 0, 16, 0]);
        assert_eq!(parse_keys(&payload), Some(k.clone()));
        // ENC first, an unknown type in between: still read.
        let mut p = vec![3];
        p.extend_from_slice(&[4, 0, 16, 0]);
        p.extend_from_slice(&k.enc);
        p.extend_from_slice(&[9, 0, 2, 0, 0xAA, 0xBB]);
        p.extend_from_slice(&[1, 0, 16, 0]);
        p.extend_from_slice(&k.irk);
        assert_eq!(parse_keys(&p), Some(k));
        // Missing one, cut short.
        assert_eq!(parse_keys(&[1, 1, 0, 16, 0, 1, 2]), None);
        assert_eq!(parse_keys(&[]), None);
    }

    #[test]
    fn keys_never_print() {
        let k = test_keys();
        let shown = format!("{k:?}");
        assert_eq!(shown, "ProximityKeys(withheld)");
        assert_eq!(ProximityKeys::from_bytes(&k.to_bytes()), Some(k));
        assert_eq!(ProximityKeys::from_bytes(&[0; 31]), None);
    }

    fn pro1_case_open(keys: &ProximityKeys) -> Vec<u8> {
        build_advert(
            &AdvertFields {
                model: 0x200E,
                // Both in the case, at least one in the case, this bud in
                // the case, right is primary.
                status: 0x54,
                // Primary (right) 9 → 90 %, other (left) 8 → 80 %.
                buds: 0x89,
                // Both buds charging, case 6 → 60 %.
                case: 0x36,
                lid: 0x03,
                color: 0x00,
                connection: 0x00,
                plain: [
                    0x00,
                    0x80 | 93,
                    0x80 | 84,
                    62,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ],
            },
            keys,
        )
    }

    /// An advertisement made with a test key decrypts to exact levels.
    #[test]
    fn exact_levels_from_the_encrypted_tail() {
        let k = test_keys();
        let data = pro1_case_open(&k);
        assert_eq!(data.len(), ADVERT_LEN);
        let a = parse_advert(&data).unwrap();
        assert_eq!(a.model, 0x200E);
        assert!(!a.lid_closed);
        assert_eq!(a.lid_counter, 3);
        assert_eq!(a.buds_in_case(), 2);
        assert_eq!(a.right.battery, Some(90));
        assert_eq!(a.left.battery, Some(80));
        let plain = k.decrypt(&data).unwrap();
        let l = a.levels(Some(&plain));
        assert_eq!(
            l.right,
            Some(Level {
                percent: 93,
                charging: true
            })
        );
        assert_eq!(
            l.left,
            Some(Level {
                percent: 84,
                charging: true
            })
        );
        assert_eq!(
            l.case,
            Some(Level {
                percent: 62,
                charging: false
            })
        );
    }

    /// The wrong ENC key decrypts to noise: the clear levels stay.
    #[test]
    fn a_wrong_key_falls_back_to_tens() {
        let k = test_keys();
        let data = pro1_case_open(&k);
        let a = parse_advert(&data).unwrap();
        let mut wrong = k.clone();
        wrong.enc[0] ^= 0xFF;
        let noise = wrong.decrypt(&data).unwrap();
        assert_eq!(a.levels(Some(&noise)), a.coarse_levels());
        assert_eq!(a.levels(None).case.unwrap().percent, 60);
    }

    #[test]
    fn exact_bytes() {
        assert_eq!(exact_level(0xFF), None);
        assert_eq!(exact_level(0x7F), None);
        assert_eq!(
            exact_level(0x80 | 100),
            Some(Level {
                percent: 100,
                charging: true
            })
        );
    }

    #[test]
    fn colours() {
        assert_eq!(color_name(0), "white");
        assert_eq!(color_name(1), "black");
        assert_eq!(color_name(0x0C), "yellow");
        assert_eq!(color_name(0x42), "");
    }

    #[test]
    fn the_monitor_pattern_is_the_start_of_the_ad() {
        // Company id little endian, then type and length.
        assert_eq!(MONITOR_PATTERN, [0x4C, 0x00, 0x07, 0x19]);
        assert_eq!(
            APPLE.to_le_bytes(),
            [MONITOR_PATTERN[0], MONITOR_PATTERN[1]]
        );
    }
}
