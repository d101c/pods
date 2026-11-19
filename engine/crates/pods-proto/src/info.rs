// SPDX-License-Identifier: GPL-3.0-or-later
//! Device information (opcode `0x1D`).
//!
//! Field order is the one documented by LibrePods
//! (`AACPManager.parseInformationPacket`, GPL-3.0-or-later). The header and
//! the two records were measured on AirPods Pro (1st generation):
//!
//! ```text
//! 02 [len u16 LE] [record] 00  name \0 model number \0 manufacturer \0 serial \0
//!   version1 \0 version2 \0 hardware revision \0 updater id \0
//!   left serial \0 right serial \0 version3 \0 [binary…]
//! ```
//!
//! The Pro 1 sends two records on connect: `0x0D` (version3 `3344646`,
//! updater `…app.60-ANC`) and `0x04` (version3 `6619472`, the firmware
//! number other tools report for this model). Record `0x04` wins.

use std::fmt;

/// Record whose `version3` is the firmware of the buds.
pub const FIRMWARE_RECORD: u8 = 0x04;

#[derive(Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Record kind (byte 3 of the payload).
    pub record: u8,
    pub name: String,
    pub model_number: String,
    pub manufacturer: String,
    pub serial: String,
    pub version1: String,
    pub version2: String,
    pub hardware_revision: String,
    pub updater_id: String,
    pub serial_left: String,
    pub serial_right: String,
    pub version3: String,
}

impl DeviceInfo {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 6 {
            return None;
        }
        let record = payload[3];
        let mut fields = payload[5..]
            .split(|&b| b == 0)
            .map(|f| String::from_utf8_lossy(f).into_owned());
        let mut next = || fields.next().unwrap_or_default();
        let info = DeviceInfo {
            record,
            name: next(),
            model_number: next(),
            manufacturer: next(),
            serial: next(),
            version1: next(),
            version2: next(),
            hardware_revision: next(),
            updater_id: next(),
            serial_left: next(),
            serial_right: next(),
            version3: next(),
        };
        // A record without a printable name and model number is not one.
        let printable = |s: &str| !s.is_empty() && s.chars().all(|c| !c.is_control());
        (printable(&info.name) || printable(&info.model_number)).then_some(info)
    }

    /// The firmware version this record states, if any.
    pub fn firmware(&self) -> Option<&str> {
        let v = self.version3.trim();
        (!v.is_empty() && v.chars().all(|c| c.is_ascii_graphic())).then_some(v)
    }
}

/// Serial numbers identify the owner's buds: never in a log.
impl fmt::Debug for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceInfo")
            .field("record", &format_args!("0x{:02x}", self.record))
            .field("name", &self.name)
            .field("model_number", &self.model_number)
            .field("version1", &self.version1)
            .field("version3", &self.version3)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u8, fields: &[&str]) -> Vec<u8> {
        let mut body = Vec::new();
        for f in fields {
            body.extend_from_slice(f.as_bytes());
            body.push(0);
        }
        let len = (body.len() + 2) as u16;
        let mut p = vec![0x02, len as u8, (len >> 8) as u8, kind, 0x00];
        p.extend(body);
        p
    }

    #[test]
    fn fields_are_read_by_position() {
        let p = record(
            0x04,
            &[
                "Test AirPods",
                "A2084",
                "Apple Inc.",
                "XXXXXXXXXXXX",
                "65.1",
                "65.1",
                "1.0.0",
                "com.apple.accessory.updater.app.60",
                "LLLLLLLLLLLL",
                "RRRRRRRRRRRR",
                "6619472",
            ],
        );
        let info = DeviceInfo::parse(&p).unwrap();
        assert_eq!(info.record, 0x04);
        assert_eq!(info.name, "Test AirPods");
        assert_eq!(info.model_number, "A2084");
        assert_eq!(info.hardware_revision, "1.0.0");
        assert_eq!(info.serial_right, "RRRRRRRRRRRR");
        assert_eq!(info.firmware(), Some("6619472"));
    }

    #[test]
    fn an_empty_name_keeps_the_positions() {
        let p = record(0x04, &["", "A2084", "Apple Inc."]);
        let info = DeviceInfo::parse(&p).unwrap();
        assert_eq!(info.name, "");
        assert_eq!(info.model_number, "A2084");
        assert_eq!(info.firmware(), None);
    }

    #[test]
    fn short_or_empty_payloads_are_refused() {
        assert_eq!(DeviceInfo::parse(&[]), None);
        assert_eq!(DeviceInfo::parse(&[2, 0, 0, 4, 0]), None);
        assert_eq!(DeviceInfo::parse(&[2, 0, 0, 4, 0, 0, 0, 0]), None);
    }

    #[test]
    fn debug_hides_the_serial_numbers() {
        let p = record(0x04, &["N", "A2084", "Apple Inc.", "SERIALSERIAL"]);
        let shown = format!("{:?}", DeviceInfo::parse(&p).unwrap());
        assert!(!shown.contains("SERIAL"), "{shown}");
        assert!(shown.contains("A2084"));
    }
}
