// SPDX-License-Identifier: GPL-3.0-or-later
// Derived from podtrack `crates/podtrack-core/src/protocol.rs`,
// Copyright 2026 René Onier, itself a
// port of MousePods `airpods_mouse_controller.py`
// (https://github.com/starman-tech/mousepods), Copyright 2026 Matteo
// Gaudioso; both Apache-2.0. Changes: borrowed slices, no serde. See
// engine/NOTICE.
//! SensorDataWX: the sensor path of the H2 models (AirPods Pro 2 and 3,
//! AirPods 4). Protobuf over opcode 0x17, descriptor 0x00100000.
//!
//! Silent on the AirPods Pro (1st generation): kept for the H2 models and as
//! the second path tried on a model nobody has mapped.

/// Frame prefix: AAP header, opcode 0x17, descriptor 0x00100000.
pub const PREFIX: [u8; 10] = [0x04, 0x00, 0x04, 0x00, 0x17, 0x00, 0x00, 0x00, 0x10, 0x00];

/// Services of a `ServiceSetting`.
pub const SERVICE_ACTIVITY: u8 = 0x0E;
pub const SERVICE_CMA: u8 = 0x0F;
pub const SERVICE_DEVMOTION6: u8 = 0x10;

/// Smallest block from which a pose can be read (gravity ends at byte 50).
pub const BLOCK_MIN_LEN: usize = 50;

/// `ServiceSetting{service, setting 2, interval in µs}`; 0 stops the stream.
/// `seq` is a 7-bit counter that goes up by one per request.
pub fn stream(seq: u8, service: u8, interval_us: u32) -> Vec<u8> {
    let mut body = vec![
        0x08,
        seq & 0x7F,
        0x42,
        0x0B,
        0x08,
        service,
        0x10,
        0x02,
        0x1A,
        0x05,
        0x01,
    ];
    body.extend_from_slice(&interval_us.to_le_bytes());
    let mut frame = PREFIX.to_vec();
    frame.extend_from_slice(&(body.len() as u16).to_le_bytes());
    frame.extend_from_slice(&body);
    frame
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn varint(b: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    while i < b.len() {
        let c = b[i];
        i += 1;
        if shift < 64 {
            v |= ((c & 0x7F) as u64) << shift;
        }
        if c & 0x80 == 0 {
            return Some((v, i));
        }
        shift += 7;
    }
    None
}

/// Fields of a protobuf message; `None` when a varint is cut short. An
/// unknown wire type ends the walk.
fn fields(buf: &[u8]) -> Option<Vec<(u64, Value<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let (key, j) = varint(buf, i)?;
        i = j;
        let (field, wire) = (key >> 3, key & 7);
        let value = match wire {
            0 => {
                let (v, j) = varint(buf, i)?;
                i = j;
                Value::Varint(v)
            }
            2 => {
                let (n, j) = varint(buf, i)?;
                let end = j.saturating_add(n as usize);
                let v = Value::Bytes(&buf[j.min(buf.len())..end.min(buf.len())]);
                i = end;
                v
            }
            1 | 5 => {
                let n = if wire == 5 { 4 } else { 8 };
                let v = Value::Bytes(&buf[i.min(buf.len())..(i + n).min(buf.len())]);
                i += n;
                v
            }
            _ => break,
        };
        out.push((field, value));
    }
    Some(out)
}

/// The motion blocks of a SensorDataWX frame (often several per frame);
/// `None` for any other frame.
pub fn motion_blocks(frame: &[u8]) -> Option<Vec<&[u8]>> {
    if frame.len() < 12 || frame[..10] != PREFIX {
        return None;
    }
    let n = u16::from_le_bytes([frame[10], frame[11]]) as usize;
    let body = &frame[12..(12 + n).min(frame.len())];
    let mut out = Vec::new();
    for (f, v) in fields(body).unwrap_or_default() {
        let (7, Value::Bytes(inner)) = (f, v) else {
            continue;
        };
        let Some(sub) = fields(inner) else { break };
        let mut service = None;
        let mut block = None;
        for (f2, v2) in sub {
            match (f2, v2) {
                (1, Value::Varint(s)) => service = Some(s),
                (3, Value::Bytes(b)) => block = Some(b),
                _ => {}
            }
        }
        if let (Some(s), Some(b)) = (service, block)
            && b.len() >= BLOCK_MIN_LEN
            && s <= 0xFF
            && [SERVICE_ACTIVITY, SERVICE_CMA, SERVICE_DEVMOTION6].contains(&(s as u8))
        {
            out.push(b);
        }
    }
    Some(out)
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
    fn stream_requests_match_the_probe() {
        // tx 70.8034 of the M0 quater capture: DEVMOTION6 stopped, seq 1.
        assert_eq!(
            stream(1, SERVICE_DEVMOTION6, 0),
            hex("040004001700000010000f000801420b081010021a050100000000")
        );
        // MousePods' first request, from podtrack's test.
        assert_eq!(
            stream(0x41, SERVICE_ACTIVITY, 40_000),
            hex("040004001700000010000f000841420b080e10021a0501409c0000")
        );
    }

    #[test]
    fn a_published_frame_yields_its_block() {
        // Public SensorDataWX vector of podtrack's tests (AirPods Pro 2).
        let f = hex(concat!(
            "04000400170000001000450008ff7f10013a3e08101a3a015021d8c7130d0000030043998d62bb00",
            "0000006cb9a83eb5fc0300020001005e1003d37706fdff000001008d020d035c0034aac63786ff0000"
        ));
        let blocks = motion_blocks(&f).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0][20..26], hex("6cb9a83eb5fc")[..]);
    }

    #[test]
    fn other_frames_are_not_ours() {
        assert!(motion_blocks(&hex("04000400170000080000090000960b008702260000")).is_none());
        assert_eq!(
            motion_blocks(&hex("040004001700000010000300ffffff")),
            Some(vec![])
        );
    }
}
