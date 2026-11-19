// SPDX-License-Identifier: GPL-3.0-or-later
// Derived from podctl `src/aap.rs` (https://github.com/Rockykln/podctl),
// Copyright (c) 2026 pods contributors, MIT OR Apache-2.0. See engine/NOTICE.
//! The AAP data frame: `04 00 04 00 [opcode] [00] [payload…]`.

/// The header of every data frame in steady state.
pub const HEADER: [u8; 4] = [0x04, 0x00, 0x04, 0x00];

/// Opcodes this engine sends or reads.
pub mod op {
    pub const HANDSHAKE: u8 = 0x00;
    pub const CAPABILITIES_REQUEST: u8 = 0x01;
    pub const CAPABILITIES: u8 = 0x02;
    pub const BATTERY: u8 = 0x04;
    pub const EAR: u8 = 0x06;
    pub const PRIMARY_BUD: u8 = 0x08;
    pub const CONTROL: u8 = 0x09;
    /// Known devices / audio source (multipoint). Carries MAC addresses.
    pub const AUDIO_SOURCE_LIST: u8 = 0x0C;
    pub const AUDIO_SOURCE: u8 = 0x0E;
    pub const NOTIFICATIONS: u8 = 0x0F;
    pub const SENSOR: u8 = 0x17;
    pub const RENAME: u8 = 0x1A;
    pub const INFORMATION: u8 = 0x1D;
    pub const CONNECTED_DEVICES: u8 = 0x2E;
    pub const KEYS_REQUEST: u8 = 0x30;
    pub const KEYS: u8 = 0x31;
    pub const CONVERSATION_LEVEL: u8 = 0x4B;
    pub const FEATURE_FLAGS: u8 = 0x4D;
}

/// One decoded data frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub opcode: u8,
    pub subop: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(opcode: u8, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            opcode,
            subop: 0,
            payload: payload.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(6 + self.payload.len());
        buf.extend_from_slice(&HEADER);
        buf.push(self.opcode);
        buf.push(self.subop);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Reads a frame. The first byte is not checked: the handshake reply
    /// starts with `01 00 04 00` and the end-of-session packet with
    /// `02 00 04 00`, both otherwise shaped like data frames.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 6 || buf[1..4] != HEADER[1..4] {
            return None;
        }
        Some(Self {
            opcode: buf[4],
            subop: buf[5],
            payload: buf[6..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_goes_there_and_back() {
        let f = Frame::new(op::CONTROL, vec![0x0D, 0x02, 0, 0, 0]);
        let bytes = f.encode();
        assert_eq!(bytes, [4, 0, 4, 0, 9, 0, 0x0D, 2, 0, 0, 0]);
        assert_eq!(Frame::parse(&bytes), Some(f));
    }

    #[test]
    fn the_handshake_reply_parses_too() {
        let f = Frame::parse(&[1, 0, 4, 0, 0, 0, 1, 0, 3, 0]).unwrap();
        assert_eq!(f.opcode, op::HANDSHAKE);
    }

    #[test]
    fn garbage_is_refused() {
        assert_eq!(Frame::parse(&[]), None);
        assert_eq!(Frame::parse(&[0xff, 0xff, 0xff]), None);
        assert_eq!(Frame::parse(&[4, 0, 5, 0, 0, 0]), None);
        assert_eq!(Frame::parse(&[4, 0, 4, 0, 9]), None);
    }
}
