// SPDX-License-Identifier: GPL-3.0-or-later
//! The objects the engine serves must match `docs/io.github.d101c.Pods1.xml`
//! member for member: names, signatures, access, argument names and
//! directions. The extension is written against that file.

mod common;

use std::collections::BTreeMap;

use common::*;

const CONTRACT: &str = include_str!("../../../../docs/io.github.d101c.Pods1.xml");

/// interface → member key → description.
type Shape = BTreeMap<String, BTreeMap<String, String>>;

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// A small reader for introspection XML: enough for this format, no more.
fn shape(xml: &str, only_prefix: &str) -> Shape {
    // Comments may hold `<…>` text: drop them first.
    let mut text = String::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<!--") {
        text.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find("-->").expect("closed comment") + 3;
        rest = &rest[end..];
    }
    text.push_str(rest);

    let mut out = Shape::new();
    let mut iface: Option<String> = None;
    let mut member: Option<(String, String, Vec<String>)> = None; // kind, name, args
    let mut pos = 0;
    while let Some(open) = text[pos..].find('<') {
        let start = pos + open;
        let end = start + text[start..].find('>').expect("closed tag");
        let tag = &text[start + 1..end];
        pos = end + 1;
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        let closing = tag.starts_with('/');
        let self_closing = tag.ends_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        match (name, closing) {
            ("interface", false) => {
                let n = attr(tag, "name").unwrap();
                iface = n.starts_with(only_prefix).then_some(n);
            }
            ("interface", true) => iface = None,
            ("method" | "signal", false) => {
                let n = attr(tag, "name").unwrap();
                member = Some((name.to_string(), n, Vec::new()));
                if self_closing {
                    finish(&mut out, &iface, member.take());
                }
            }
            ("method" | "signal", true) => finish(&mut out, &iface, member.take()),
            ("arg", false) => {
                if let Some((kind, _, args)) = member.as_mut() {
                    let dir = attr(tag, "direction").unwrap_or_else(|| {
                        if kind == "method" {
                            "in".into()
                        } else {
                            "out".into()
                        }
                    });
                    let dir = if kind == "signal" { "out".into() } else { dir };
                    args.push(format!(
                        "{} {}:{}",
                        dir,
                        attr(tag, "name").unwrap_or_default(),
                        attr(tag, "type").unwrap()
                    ));
                }
            }
            ("property", false) => {
                if let Some(i) = &iface {
                    out.entry(i.clone()).or_default().insert(
                        format!("property {}", attr(tag, "name").unwrap()),
                        format!(
                            "{} {}",
                            attr(tag, "type").unwrap(),
                            attr(tag, "access").unwrap()
                        ),
                    );
                }
            }
            _ => {}
        }
    }
    out
}

fn finish(out: &mut Shape, iface: &Option<String>, member: Option<(String, String, Vec<String>)>) {
    if let (Some(i), Some((kind, name, args))) = (iface, member) {
        out.entry(i.clone())
            .or_default()
            .insert(format!("{kind} {name}"), args.join(", "));
    }
}

async fn introspect(rig: &Rig, path: &str) -> String {
    let p = rig.proxy(path, "org.freedesktop.DBus.Introspectable").await;
    p.call("Introspect", &()).await.unwrap()
}

#[test]
fn the_reader_reads_the_contract() {
    let s = shape(CONTRACT, "io.github.d101c.Pods1");
    let dev = &s["io.github.d101c.Pods1.Device"];
    assert_eq!(dev["property Battery"], "a{s(yb)} read");
    assert_eq!(dev["method SetNoiseMode"], "in mode:s");
    assert_eq!(dev["method CycleNoiseMode"], "");
    assert_eq!(dev["signal NoiseModeChanged"], "out mode:s, out source:s");
    assert_eq!(s["io.github.d101c.Pods1.Manager"].len(), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_served_api_is_the_contract() {
    let rig = Rig::start(false, fast_timing()).await;
    eventually("device listed", || async {
        rig.device()
            .await
            .get_property::<String>("Address")
            .await
            .is_ok()
    })
    .await;
    let expected = shape(CONTRACT, "io.github.d101c.Pods1");
    let mut served = shape(&introspect(&rig, MANAGER).await, "io.github.d101c.Pods1");
    served.extend(shape(
        &introspect(&rig, DEVICE).await,
        "io.github.d101c.Pods1",
    ));
    for (iface, members) in &expected {
        let got = served
            .get(iface)
            .unwrap_or_else(|| panic!("{iface} not served"));
        for (k, v) in members {
            assert_eq!(got.get(k), Some(v), "{iface} {k}");
        }
        for k in got.keys() {
            assert!(
                members.contains_key(k),
                "{iface} serves {k}, which is not in the contract"
            );
        }
    }
    assert_eq!(served.len(), expected.len());
}
