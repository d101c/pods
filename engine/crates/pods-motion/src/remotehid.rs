// SPDX-License-Identifier: GPL-3.0-or-later
// The wire format follows MousePods `docs/REMOTEHID-PROTOCOL.md` and podtrack
// `crates/podtrack-core/src/protocol.rs` (Apache-2.0). See engine/NOTICE.
//! RemoteHID over AAP: the sensor path of the H1 models (AirPods 3, AirPods
//! Pro 1st generation).
//!
//! ```text
//! frame  04 00 04 00 17 00 | 00 08 00 00 | len u16 LE | body
//! body   version 0 | generation | 0 | side flags | sub-packets…
//! sub    header u32 LE (device bits 0-6, length incl. header bits 7-16,
//!        kind bits 17-19, response bit 21) | payload
//! ```

/// Frame prefix: AAP header, opcode 0x17, descriptor 0x00000800 ("Sensor").
pub const PREFIX: [u8; 10] = [0x04, 0x00, 0x04, 0x00, 0x17, 0x00, 0x00, 0x08, 0x00, 0x00];

/// Sub-packet kinds.
pub mod kind {
    /// Host → device: HostConnect. Device → host: DeviceConnect (plist).
    pub const CONNECT: u8 = 0;
    pub const DISCONNECT: u8 = 1;
    pub const INPUT_REPORT: u8 = 2;
    pub const SET_REPORT: u8 = 3;
    pub const GET_REPORT: u8 = 4;
    /// The device asks the host to (re)connect: answered with HostConnect.
    pub const CONFIRM_CONNECT: u8 = 5;
}

/// HID report type of a SetReport.
pub const REPORT_FEATURE: u8 = 2;
/// Feature report 1 of a sensor: its report interval in microseconds.
pub const FEATURE_INTERVAL: u8 = 1;

/// `AccessoryService` of the device that carries the 58-byte motion block.
pub const MOTION_SERVICE: &str = "devmotion6";

/// One sub-packet of a received frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubPacket<'a> {
    pub device: u8,
    pub kind: u8,
    pub response: bool,
    pub payload: &'a [u8],
}

/// Sub-packets of a RemoteHID frame; `None` for any other frame. A length
/// field that runs past the body ends the walk (what came before is kept).
pub fn parse(frame: &[u8]) -> Option<Vec<SubPacket<'_>>> {
    if frame.len() < 12 || frame[..10] != PREFIX {
        return None;
    }
    let n = u16::from_le_bytes([frame[10], frame[11]]) as usize;
    let body = &frame[12..(12 + n).min(frame.len())];
    let mut out = Vec::new();
    let mut i = 4;
    while i + 4 <= body.len() {
        let h = u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
        let len = ((h >> 7) & 0x3FF) as usize;
        if len < 4 || i + len > body.len() {
            break;
        }
        out.push(SubPacket {
            device: (h & 0x7F) as u8,
            kind: ((h >> 17) & 7) as u8,
            response: h & (1 << 21) != 0,
            payload: &body[i + 4..i + len],
        });
        i += len;
    }
    Some(out)
}

fn sub_packet(device: u8, kind: u8, payload: &[u8]) -> Vec<u8> {
    let len = 4 + payload.len() as u32;
    let h = (device as u32 & 0x7F) | ((len & 0x3FF) << 7) | ((kind as u32 & 7) << 17);
    let mut v = h.to_le_bytes().to_vec();
    v.extend_from_slice(payload);
    v
}

/// A frame holding one sub-packet. `generation` goes up by one per frame.
pub fn encode(generation: u8, device: u8, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut body = vec![0, generation, 0, 0];
    body.extend_from_slice(&sub_packet(device, kind, payload));
    let mut frame = PREFIX.to_vec();
    frame.extend_from_slice(&(body.len() as u16).to_le_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// HostConnect: asks the AirPods to announce their sensors.
pub fn host_connect(generation: u8) -> Vec<u8> {
    encode(generation, 0, kind::CONNECT, &[])
}

/// SetReport feature 1: report interval in µs; 0 stops the stream.
pub fn set_interval(generation: u8, device: u8, interval_us: u32) -> Vec<u8> {
    let mut pl = vec![REPORT_FEATURE, FEATURE_INTERVAL];
    pl.extend_from_slice(&interval_us.to_le_bytes());
    encode(generation, device, kind::SET_REPORT, &pl)
}

/// The `AccessoryService` string of a DeviceConnect XML plist.
pub fn accessory_service(plist: &[u8]) -> Option<&str> {
    const KEY: &[u8] = b"<key>AccessoryService</key>";
    let start = find(plist, KEY)? + KEY.len();
    let rest = &plist[start..];
    let rest = &rest[rest.iter().position(|&c| c != b' ')?..];
    let rest = rest.strip_prefix(b"<string>")?;
    let end = find(rest, b"</string>")?;
    std::str::from_utf8(&rest[..end])
        .ok()
        .filter(|s| !s.is_empty())
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn frames_match_the_bytes_that_worked_on_the_pro_1() {
        // tx 2.6354 and tx 2.6924 of the M0 quater capture.
        assert_eq!(
            host_connect(1),
            hex("0400040017000008000008000001000000020000")
        );
        assert_eq!(
            set_interval(2, 7, 20_000),
            hex("040004001700000800000e0000020000070506000201204e0000")
        );
        assert_eq!(
            set_interval(3, 7, 0),
            hex("040004001700000800000e000003000007050600020100000000")
        );
    }

    #[test]
    fn a_set_report_acknowledgement_parses() {
        let f = hex("04000400170000080000090000960b008702260000");
        let subs = parse(&f).unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].device, 7);
        assert_eq!(subs[0].kind, kind::SET_REPORT);
        assert!(subs[0].response);
        assert_eq!(subs[0].payload, &[0]);
    }

    #[test]
    fn other_frames_and_truncation() {
        assert!(parse(&hex("0400040009000d02000000")).is_none());
        // Length field says 63 bytes, only 10 present: nothing is returned.
        let f = hex("04000400170000080000430000970b00871f04000001");
        assert_eq!(parse(&f).unwrap(), vec![]);
    }

    #[test]
    fn service_names() {
        let p = b"<dict><key>VendorID</key><integer>1452</integer><key>AccessoryService</key><string>devmotion6</string></dict>";
        assert_eq!(accessory_service(p), Some("devmotion6"));
        assert_eq!(accessory_service(b"<dict></dict>"), None);
    }
}
