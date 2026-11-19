// SPDX-License-Identifier: GPL-3.0-or-later
//! Control commands (opcode `0x09`): `04 00 04 00 09 00 [id] [v1] [v2] [v3] [v4]`.
//!
//! Identifiers and values are protocol facts documented by LibrePods
//! (`docs/control_commands.md`, GPL-3.0-or-later). Those marked "Pro 1"
//! were seen on AirPods Pro (1st generation), firmware 6619472.

use crate::model::NoiseMode;

/// Control identifiers.
pub mod id {
    pub const MIC_MODE: u8 = 0x01;
    pub const OWNS_CONNECTION: u8 = 0x06;
    /// Automatic ear detection: 1 on, 2 off. Not reported by the Pro 1 on connect.
    pub const EAR_DETECTION: u8 = 0x0A;
    /// Listening mode: 1 off, 2 ANC, 3 transparency, 4 adaptive. Pro 1: reported on connect.
    pub const LISTENING_MODE: u8 = 0x0D;
    /// What the press-and-hold does, right byte then left: 1 noise control, 5 Siri.
    pub const CLICK_HOLD_MODE: u8 = 0x16;
    /// Pro 1: reported on connect.
    pub const DOUBLE_CLICK_INTERVAL: u8 = 0x17;
    /// Pro 1: reported on connect.
    pub const CLICK_HOLD_INTERVAL: u8 = 0x18;
    /// Modes the press-and-hold cycles through, as a bit mask (see [`super::mask_of`]).
    pub const LISTENING_MODE_CONFIGS: u8 = 0x1A;
    /// Pro 1: reported on connect.
    pub const ONE_BUD_ANC: u8 = 0x1B;
    /// Pro 1: reported on connect.
    pub const CHIME_VOLUME: u8 = 0x1F;
    /// Pro 1: reported on connect.
    pub const CALL_MANAGEMENT: u8 = 0x24;
    /// Conversation awareness: 1 on, 2 off. Models with an H2 chip.
    pub const CONVERSATION_DETECT: u8 = 0x28;
    pub const ALLOW_OFF_OPTION: u8 = 0x34;
}

/// The value byte of an on/off control: 1 on, 2 off.
pub fn switch_byte(on: bool) -> u8 {
    if on { 1 } else { 2 }
}

/// 1 on, 2 off, anything else is no value.
pub fn switch_from_byte(b: u8) -> Option<bool> {
    match b {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Bit of a mode in the `LISTENING_MODE_CONFIGS` mask.
pub fn mask_bit(mode: NoiseMode) -> u8 {
    match mode {
        NoiseMode::Off => 0x01,
        NoiseMode::Anc => 0x02,
        NoiseMode::Transparency => 0x04,
        NoiseMode::Adaptive => 0x08,
    }
}

/// The mask for a set of modes.
pub fn mask_of(modes: &[NoiseMode]) -> u8 {
    modes.iter().fold(0, |m, &mode| m | mask_bit(mode))
}

/// The modes in a mask, in the order the buds cycle them
/// (off, transparency, ANC, adaptive — the order iOS shows them).
pub fn modes_of(mask: u8) -> Vec<NoiseMode> {
    NoiseMode::ALL
        .into_iter()
        .filter(|&m| mask & mask_bit(m) != 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values from the LibrePods table of press-and-hold transitions.
    #[test]
    fn masks_match_the_documented_bytes() {
        use NoiseMode::*;
        assert_eq!(mask_of(&[Off, Anc]), 0x03);
        assert_eq!(mask_of(&[Off, Transparency]), 0x05);
        assert_eq!(mask_of(&[Transparency, Anc]), 0x06);
        assert_eq!(mask_of(&[Off, Transparency, Anc]), 0x07);
        assert_eq!(mask_of(&[Off, Transparency, Anc, Adaptive]), 0x0F);
        assert_eq!(mask_of(&[Transparency, Anc, Adaptive]), 0x0E);
    }

    #[test]
    fn a_mask_reads_back_in_display_order() {
        use NoiseMode::*;
        assert_eq!(modes_of(0x06), vec![Transparency, Anc]);
        assert_eq!(modes_of(0x0F), vec![Off, Transparency, Anc, Adaptive]);
        assert_eq!(modes_of(0), Vec::<NoiseMode>::new());
        assert_eq!(modes_of(0xF0), Vec::<NoiseMode>::new());
    }

    #[test]
    fn switches() {
        assert_eq!(switch_byte(true), 1);
        assert_eq!(switch_byte(false), 2);
        assert_eq!(switch_from_byte(1), Some(true));
        assert_eq!(switch_from_byte(2), Some(false));
        assert_eq!(switch_from_byte(0), None);
    }
}
