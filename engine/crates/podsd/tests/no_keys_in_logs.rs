// SPDX-License-Identifier: GPL-3.0-or-later
//! The proximity keys never reach the journal: every log line of the
//! engine, at the most verbose level, is captured through a whole key
//! exchange, a reconnection and a case card, then searched for the keys
//! in every way they could be printed.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use pods_proto::ProximityKeys;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Ways a key could show: hex either case, with or without separators,
/// either byte order, Rust's `Debug` of the bytes in decimal and in hex.
fn spellings(keys: &ProximityKeys) -> Vec<String> {
    let mut out = Vec::new();
    for key in [keys.irk, keys.enc] {
        let mut reversed = key;
        reversed.reverse();
        for k in [key, reversed] {
            let head = &k[..6];
            let hex = |sep: &str, upper: bool| {
                head.iter()
                    .map(|b| {
                        if upper {
                            format!("{b:02X}")
                        } else {
                            format!("{b:02x}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(sep)
            };
            for sep in ["", " ", ":"] {
                out.push(hex(sep, false));
                out.push(hex(sep, true));
            }
            out.push(format!("{:?}", head).trim_end_matches(']').to_string());
            out.push(format!("{:x?}", head).trim_end_matches(']').to_string());
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn the_keys_never_reach_the_log() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "podsd=trace,pods_bt=trace,pods_proto=trace",
        ))
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .init();

    let rig = Rig::start(true, fast_timing()).await;
    eventually("keys kept", || async { rig.secrets.items().len() == 1 }).await;
    rig.monitor_registered().await;
    rig.airpods.hang_up();
    eventually("reconnected", || async { rig.airpods.connects() == 2 }).await;

    let opened = rig.signals(MANAGER_IFACE, "CaseOpened").await;
    let keys = test_keys();
    let addr = keys.address_for([0x48, 0x01, 0x02]);
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, true, 1, 50), -50)
        .await;
    rig.bluez
        .advertise(&addr, pro1_advert(&keys, false, 2, 50), -50)
        .await;
    eventually("opened", || async { opened.lock().unwrap().len() == 1 }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    assert!(log.contains("proximity keys received"), "{log}");
    assert!(log.contains("proximity keys kept in the keyring"), "{log}");
    assert!(log.contains("case opened"), "{log}");
    for s in spellings(&keys) {
        assert!(!log.contains(&s), "key material ({s}) in the log:\n{log}");
    }
    // Nor the full private address the keys resolve.
    assert!(!log.contains(&addr), "{log}");
}
