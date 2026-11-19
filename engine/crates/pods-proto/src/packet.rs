// SPDX-License-Identifier: GPL-3.0-or-later
// The opening packets are the ones podctl sends (`src/bin/daemon/aap.rs`,
// https://github.com/Rockykln/podctl, Copyright (c) 2026 pods contributors,
// MIT OR Apache-2.0); the notification mask and timings are the ones our M0
// probe used on the Pro 1. Renaming, the key request and conversation
// awareness follow LibrePods (GPL-3.0-or-later).
//! What the engine sends.

use crate::control::{self, id};
use crate::frame::{Frame, op};
use crate::model::NoiseMode;

/// Opens the AAP session. Not a data frame: its header is `00 00 04 00`.
pub const HANDSHAKE: [u8; 16] = [
    0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Feature flags `D7`: what makes the buds send battery notifications.
pub const FEATURE_FLAGS: [u8; 14] = [
    0x04, 0x00, 0x04, 0x00, 0x4D, 0x00, 0xD7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Ask for every notification. Four `FF` bytes: what the Pro 1 accepted.
pub const NOTIFICATIONS: [u8; 10] = [0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];

/// The opening sequence, in order, each with the pause to leave after it
/// (in milliseconds) before the next one. Measured on the Pro 1: the buds
/// answer about 150 ms after the last one with their whole state.
pub fn opening() -> [(&'static [u8], u64); 3] {
    [
        (&HANDSHAKE, 300),
        (&FEATURE_FLAGS, 150),
        (&NOTIFICATIONS, 150),
    ]
}

/// A control command with one value byte.
pub fn control(id: u8, value: u8) -> Vec<u8> {
    control4(id, [value, 0, 0, 0])
}

/// A control command with its four value bytes.
pub fn control4(id: u8, value: [u8; 4]) -> Vec<u8> {
    let mut payload = vec![id];
    payload.extend_from_slice(&value);
    Frame::new(op::CONTROL, payload).encode()
}

pub fn set_noise_mode(mode: NoiseMode) -> Vec<u8> {
    control(id::LISTENING_MODE, mode.wire())
}

/// The modes the press-and-hold cycles through. The buds want at least two.
pub fn set_press_hold_modes(modes: &[NoiseMode]) -> Vec<u8> {
    control(id::LISTENING_MODE_CONFIGS, control::mask_of(modes))
}

pub fn set_ear_detection(on: bool) -> Vec<u8> {
    control(id::EAR_DETECTION, control::switch_byte(on))
}

pub fn set_conversation_awareness(on: bool) -> Vec<u8> {
    control(id::CONVERSATION_DETECT, control::switch_byte(on))
}

/// Mask of key types asked for in [`request_keys`]: IRK (1) and ENC (4).
pub const KEY_MASK: u8 = 0x05;

/// Asks for the proximity keys (`30 00 05 00`); the buds answer `0x31`.
pub fn request_keys() -> Vec<u8> {
    Frame::new(op::KEYS_REQUEST, vec![KEY_MASK, 0x00]).encode()
}

/// Longest name accepted, in bytes. The length travels in one byte; iOS
/// stops well before that, so this is a guard, not Apple's limit.
pub const MAX_NAME_BYTES: usize = 64;

/// Renaming (opcode `0x1A`): `1A 00 01 [len] 00 [UTF-8 name]`.
/// `None` for an empty or too long name.
pub fn rename(name: &str) -> Option<Vec<u8>> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_NAME_BYTES {
        return None;
    }
    let mut payload = vec![0x01, bytes.len() as u8, 0x00];
    payload.extend_from_slice(bytes);
    Some(Frame::new(op::RENAME, payload).encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, decode};

    /// The exact bytes the M0 probe sent to the Pro 1 and that worked.
    #[test]
    fn the_opening_matches_the_m0_probe() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let seq = opening();
        assert_eq!(hex(seq[0].0), "00000400010002000000000000000000");
        assert_eq!(hex(seq[1].0), "040004004d00d700000000000000");
        assert_eq!(hex(seq[2].0), "040004000f00ffffffff");
    }

    #[test]
    fn noise_mode_frames_are_the_ones_the_pro_1_accepted() {
        // tx at 5.72 s in trames-20261119-163337.jsonl
        assert_eq!(
            set_noise_mode(NoiseMode::Transparency),
            [4, 0, 4, 0, 9, 0, 0x0D, 3, 0, 0, 0]
        );
        assert_eq!(
            set_noise_mode(NoiseMode::Anc),
            [4, 0, 4, 0, 9, 0, 0x0D, 2, 0, 0, 0]
        );
        assert_eq!(
            set_noise_mode(NoiseMode::Off),
            [4, 0, 4, 0, 9, 0, 0x0D, 1, 0, 0, 0]
        );
    }

    /// The buds echo a setting the way it was written: a frame we send
    /// must decode as the value we meant.
    #[test]
    fn what_we_send_reads_back() {
        for m in NoiseMode::ALL {
            let Some(Message::Control { id, value }) = decode(&set_noise_mode(m)) else {
                panic!("not a control");
            };
            assert_eq!(id, id::LISTENING_MODE);
            assert_eq!(NoiseMode::from_wire(value[0]), Some(m));
        }
        let Some(Message::Control { id, value }) = decode(&set_ear_detection(false)) else {
            panic!("not a control");
        };
        assert_eq!((id, value[0]), (id::EAR_DETECTION, 2));
    }

    #[test]
    fn press_hold_uses_the_mask() {
        use NoiseMode::*;
        assert_eq!(
            set_press_hold_modes(&[Transparency, Anc]),
            [4, 0, 4, 0, 9, 0, 0x1A, 0x06, 0, 0, 0]
        );
        assert_eq!(
            set_press_hold_modes(&[Off, Transparency, Anc]),
            [4, 0, 4, 0, 9, 0, 0x1A, 0x07, 0, 0, 0]
        );
    }

    /// podctl's and LibrePods' request, mask 05.
    #[test]
    fn key_request() {
        assert_eq!(request_keys(), [4, 0, 4, 0, 0x30, 0, 0x05, 0x00]);
    }

    #[test]
    fn conversation_awareness() {
        assert_eq!(
            set_conversation_awareness(true),
            [4, 0, 4, 0, 9, 0, 0x28, 1, 0, 0, 0]
        );
        assert_eq!(
            set_conversation_awareness(false),
            [4, 0, 4, 0, 9, 0, 0x28, 2, 0, 0, 0]
        );
    }

    #[test]
    fn rename_frame() {
        assert_eq!(
            rename("Pods").unwrap(),
            [4, 0, 4, 0, 0x1A, 0, 0x01, 4, 0, b'P', b'o', b'd', b's']
        );
        // UTF-8 length, not characters.
        assert_eq!(rename("é").unwrap()[7], 2);
        assert_eq!(rename(""), None);
        assert_eq!(rename(&"x".repeat(MAX_NAME_BYTES + 1)), None);
        assert!(rename(&"x".repeat(MAX_NAME_BYTES)).is_some());
    }
}
