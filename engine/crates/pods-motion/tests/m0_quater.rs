// SPDX-License-Identifier: GPL-3.0-or-later
//! The spoken calibration of M0 quater (AirPods Pro, 1st generation,
//! 19 November 2026), replayed: the sensor frames exactly as received, then
//! the yaw of each step.
//!
//! Steps: face the screen, head to the left, centre, to the right, centre,
//! head up, centre, still for 20 s. The probe measured −83.5° to the left
//! and +86.3° to the right with its own reference (left negative there);
//! the engine's convention is the opposite: positive to the left.

use std::time::Duration;

use pods_motion::remotehid;
use pods_motion::{
    Event, HeadTracking, MotionConfig, Pose, SensorPath, SensorState, TrackerConfig,
};

const PRO_1: Option<u16> = Some(0x200E);

struct Line {
    tx: bool,
    t: Duration,
    bytes: Vec<u8>,
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// `m0-quater-setup.txt`: sensor frames from the HostConnect to the 25th report.
fn setup() -> Vec<Line> {
    include_str!("fixtures/m0-quater-setup.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut cols = l.split_whitespace();
            let tx = cols.next().unwrap() == "tx";
            let t = Duration::from_secs_f64(cols.next().unwrap().parse().unwrap());
            Line {
                tx,
                t,
                bytes: hex(cols.next().unwrap()),
            }
        })
        .collect()
}

/// `m0-quater-motion.bin`: every motion report, `u32 µs | u16 length | frame`.
fn motion() -> Vec<Line> {
    let mut b: &[u8] = include_bytes!("fixtures/m0-quater-motion.bin");
    let mut out = Vec::new();
    while b.len() >= 6 {
        let t = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let n = u16::from_le_bytes([b[4], b[5]]) as usize;
        out.push(Line {
            tx: false,
            t: Duration::from_micros(t as u64),
            bytes: b[6..6 + n].to_vec(),
        });
        b = &b[6 + n..];
    }
    out
}

/// `m0-quater-steps.txt`: (step, index of its first report).
fn steps() -> Vec<(String, usize)> {
    include_str!("fixtures/m0-quater-steps.txt")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let mut cols = l.split_whitespace();
            (
                cols.next().unwrap().to_string(),
                cols.next().unwrap().parse().unwrap(),
            )
        })
        .collect()
}

fn step(name: &str) -> (usize, usize) {
    let s = steps();
    let i = s.iter().position(|(n, _)| n == name).unwrap();
    (s[i].1, s[i + 1].1)
}

fn sent(events: &[Event]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Send(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn states(events: &[Event]) -> Vec<SensorState> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::State(s) => Some(*s),
            _ => None,
        })
        .collect()
}

fn is_announce(frame: &[u8]) -> bool {
    frame.windows(5).any(|w| w == b"<?xml")
}

fn is_report(frame: &[u8]) -> bool {
    remotehid::parse(frame)
        .unwrap_or_default()
        .iter()
        .any(|s| s.kind == remotehid::kind::INPUT_REPORT)
}

/// Engine started as in the probe: buds worn, sound already flowing, then
/// the announces and the acknowledgement of the interval.
fn started(config: MotionConfig) -> HeadTracking {
    let mut ht = HeadTracking::new(PRO_1, config);
    ht.set_enabled(true, Duration::from_secs(2));
    ht.set_audio(true, Duration::from_secs(2));
    for l in setup().iter().filter(|l| !l.tx && !is_report(&l.bytes)) {
        ht.handle_frame(&l.bytes, l.t);
    }
    ht
}

/// Replays every report; returns the latest pose after each one. `before`
/// runs before report `i` is fed (to press Recenter at a given moment).
fn replay(
    ht: &mut HeadTracking,
    mut before: impl FnMut(&mut HeadTracking, usize),
) -> Vec<Option<Pose>> {
    motion()
        .iter()
        .enumerate()
        .map(|(i, l)| {
            before(ht, i);
            ht.handle_frame(&l.bytes, l.t);
            ht.pose()
        })
        .collect()
}

fn mean(xs: impl Iterator<Item = f64>) -> f64 {
    let v: Vec<f64> = xs.collect();
    v.iter().sum::<f64>() / v.len() as f64
}

/// Mean yaw and tilt over the second half of a step (the head has arrived).
fn plateau(poses: &[Option<Pose>], name: &str) -> (f64, f64) {
    let (a, b) = step(name);
    let held = &poses[a + (b - a) / 2..b];
    (
        mean(held.iter().map(|p| p.unwrap().yaw_deg)),
        mean(held.iter().map(|p| p.unwrap().tilt_deg)),
    )
}

/// Least-squares slope (°/min) and residual σ (°) of the yaw over a range.
fn drift(poses: &[Option<Pose>], from: usize, to: usize) -> (f64, f64) {
    let pts: Vec<(f64, f64)> = poses[from..to]
        .iter()
        .map(|p| p.unwrap())
        .map(|p| (p.time.as_secs_f64(), p.yaw_deg))
        .collect();
    let mx = mean(pts.iter().map(|p| p.0));
    let my = mean(pts.iter().map(|p| p.1));
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let slope = sxy / sxx;
    let var = mean(pts.iter().map(|p| (p.1 - my - slope * (p.0 - mx)).powi(2)));
    (slope * 60.0, var.sqrt())
}

/// Index of the last report of "01-ecran": the user faces the screen.
fn facing() -> usize {
    step("01-ecran").1 - 1
}

#[test]
fn the_probe_session_is_reproduced_frame_for_frame() {
    let lines = setup();
    let tx: Vec<&Line> = lines.iter().filter(|l| l.tx).collect();
    let mut ht = HeadTracking::new(PRO_1, MotionConfig::default());

    // Buds worn but the output idle: nothing is asked (M0 quater, 17:05).
    assert!(ht.set_enabled(true, Duration::from_secs(2)).is_empty());
    let ev = ht.set_audio(true, Duration::from_secs(2));
    assert_eq!(sent(&ev), vec![tx[0].bytes.clone()], "HostConnect");
    assert_eq!(
        states(&ev),
        vec![SensorState::Waiting(SensorPath::RemoteHid)]
    );

    let mut out = Vec::new();
    let mut poses = 0;
    for l in lines.iter().filter(|l| !l.tx) {
        out.extend(ht.handle_frame(&l.bytes, l.t));
        poses += (ht.pose().map(|p| p.time) == Some(l.t)) as usize;
    }
    // The interval right after the devmotion6 announce, byte for byte.
    assert_eq!(sent(&out), vec![tx[1].bytes.clone()], "SetReport 20 000 µs");
    assert_eq!(
        states(&out),
        vec![SensorState::Streaming(SensorPath::RemoteHid)]
    );
    let names: Vec<&str> = ht
        .sensors()
        .devices()
        .values()
        .map(|s| s.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "kylo",
            "obi",
            "obirelay",
            "accel",
            "gyro",
            "cma",
            "devmotion6",
            "iedrelay"
        ]
    );
    assert_eq!(ht.sensors().motion_device(), Some(7));
    // 25 reports, the first six sent before the fusion has an attitude.
    assert_eq!(poses, 19);

    // Stopping: interval 0 to the same device, as the probe did.
    let ev = ht.stop();
    assert_eq!(sent(&ev), vec![tx[2].bytes.clone()], "SetReport 0");
    assert_eq!(states(&ev), vec![SensorState::Off]);
}

#[test]
fn a_reannounce_rearms_the_stream() {
    let mut ht = started(MotionConfig::default());
    let reports = motion();
    for l in &reports[..100] {
        ht.handle_frame(&l.bytes, l.t);
    }
    let before = ht.pose().unwrap();
    assert_eq!(ht.state(), SensorState::Streaming(SensorPath::RemoteHid));

    // The buds announce again (primary switch): the interval goes out again.
    let devmotion = setup()
        .into_iter()
        .find(|l| is_announce(&l.bytes) && l.bytes.windows(10).any(|w| w == b"devmotion6"))
        .unwrap();
    let t = reports[100].t;
    let ev = ht.handle_frame(&devmotion.bytes, t);
    assert_eq!(sent(&ev), vec![remotehid::set_interval(3, 7, 20_000)]);
    assert_eq!(
        states(&ev),
        vec![SensorState::Waiting(SensorPath::RemoteHid)]
    );

    // The yaw carries on from where it was, with no jump.
    let ev = ht.handle_frame(&reports[100].bytes, t);
    assert_eq!(
        states(&ev),
        vec![SensorState::Streaming(SensorPath::RemoteHid)]
    );
    assert!((ht.pose().unwrap().yaw_deg - before.yaw_deg).abs() < 1.0);
}

#[test]
fn announces_seen_before_the_sound_skip_the_host_connect() {
    let mut ht = HeadTracking::new(PRO_1, MotionConfig::default());
    for l in setup().iter().filter(|l| is_announce(&l.bytes)) {
        assert!(ht.handle_frame(&l.bytes, l.t).is_empty());
    }
    ht.set_enabled(true, Duration::from_secs(5));
    let ev = ht.set_audio(true, Duration::from_secs(5));
    assert_eq!(sent(&ev), vec![remotehid::set_interval(1, 7, 20_000)]);
}

#[test]
fn silence_stops_after_the_grace_period_and_sound_rearms() {
    let mut ht = started(MotionConfig::default());
    let reports = motion();
    for l in &reports[..50] {
        ht.handle_frame(&l.bytes, l.t);
    }
    let t = reports[49].t;
    assert!(ht.set_audio(false, t).is_empty());
    assert!(sent(&ht.tick(t + Duration::from_secs(5))).is_empty());
    let ev = ht.tick(t + Duration::from_secs(11));
    assert_eq!(sent(&ev), vec![remotehid::set_interval(3, 7, 0)]);
    assert_eq!(states(&ev), vec![SensorState::Off]);
    // Late reports after the stop are dropped.
    assert!(
        ht.handle_frame(&reports[50].bytes, t + Duration::from_secs(11))
            .is_empty()
    );

    let ev = ht.set_audio(true, t + Duration::from_secs(20));
    assert_eq!(sent(&ev), vec![remotehid::set_interval(4, 7, 20_000)]);
    assert_eq!(
        states(&ev),
        vec![SensorState::Waiting(SensorPath::RemoteHid)]
    );

    // Head tracking switched off: interval 0 at once.
    let ev = ht.set_enabled(false, t + Duration::from_secs(21));
    assert_eq!(sent(&ev), vec![remotehid::set_interval(5, 7, 0)]);
}

#[test]
fn a_stalled_stream_is_asked_again() {
    let mut ht = started(MotionConfig::default());
    let reports = motion();
    for l in &reports[..50] {
        ht.handle_frame(&l.bytes, l.t);
    }
    let t = reports[49].t;
    assert!(sent(&ht.tick(t + Duration::from_secs(1))).is_empty());
    let ev = ht.tick(t + Duration::from_secs(3));
    assert_eq!(sent(&ev), vec![remotehid::set_interval(3, 7, 20_000)]);
}

#[test]
fn the_calibration_plateaus() {
    let mut ht = started(MotionConfig::default());
    let face = facing();
    let poses = replay(&mut ht, |ht, i| {
        if i == face {
            ht.recenter();
        }
    });
    let (left, _) = plateau(&poses, "02-gauche");
    let (right, _) = plateau(&poses, "04-droite");
    let (up_yaw, up_tilt) = plateau(&poses, "06-haut");
    let centres = ["03-centre", "05-centre", "07-centre"].map(|s| plateau(&poses, s).0);
    eprintln!(
        "left {left:+.1}°, right {right:+.1}°, centres {centres:+.1?}, head up: yaw {up_yaw:+.1}° tilt {up_tilt:.1}°"
    );
    assert!((75.0..95.0).contains(&left), "left {left}");
    assert!((-95.0..-75.0).contains(&right), "right {right}");
    for c in centres {
        assert!(c.abs() < 3.0, "centre {c}");
    }
    // Looking up barely moves the yaw: the twist ignores the tilted bud.
    assert!(up_yaw.abs() < 8.0, "head up yaw {up_yaw}");
    assert!(up_tilt > 45.0, "head up tilt {up_tilt}");
}

#[test]
fn the_drift_at_rest() {
    let (a, b) = step("08-immobile");
    // Two seconds to settle after the spoken instruction.
    let from = a + 100;
    for follow in [false, true] {
        let mut ht = started(MotionConfig {
            tracker: TrackerConfig {
                follow_screen: follow,
                ..Default::default()
            },
            ..Default::default()
        });
        let face = facing();
        let poses = replay(&mut ht, |ht, i| {
            if i == face {
                ht.recenter();
            }
        });
        let (slope, sigma) = drift(&poses, from, b);
        eprintln!("rest, Follow Screen {follow}: drift {slope:+.2}°/min, noise σ {sigma:.2}°");
        assert!(slope.abs() < 5.0, "drift {slope}°/min");
        assert!(sigma < 0.3, "noise {sigma}°");
        if !follow {
            // What M0 measured on the raw yaw: −2.6°/min, i.e. +2.6 here.
            assert!(slope > 1.0, "drift {slope}°/min");
        }
    }
}

#[test]
fn follow_screen_brings_the_scene_back() {
    // Recenter pressed while looking to the left: the screen is then 80° to
    // the right. Follow Screen must bring it back without any help.
    let (_, left_end) = step("02-gauche");
    let (a, b) = step("08-immobile");
    let tail = b - 250; // the last 5 s of the still step
    let mut results = Vec::new();
    for follow in [true, false] {
        let mut ht = started(MotionConfig {
            tracker: TrackerConfig {
                follow_screen: follow,
                ..Default::default()
            },
            ..Default::default()
        });
        let poses = replay(&mut ht, |ht, i| {
            if i == left_end - 1 {
                ht.recenter();
            }
        });
        let glided = poses[left_end..b].iter().any(|p| p.unwrap().following);
        let end = mean(poses[tail..b].iter().map(|p| p.unwrap().yaw_deg));
        let worst = poses[tail..b]
            .iter()
            .map(|p| p.unwrap().yaw_deg.abs())
            .fold(0.0, f64::max);
        results.push((follow, glided, end, worst, poses[a].unwrap().yaw_deg));
    }
    eprintln!("Follow Screen (on, glided, end yaw, worst, at 08 start): {results:+.1?}");
    let (_, glided, end, worst, _) = results[0];
    assert!(glided);
    assert!(end.abs() < 3.0 && worst < 3.0, "end {end}, worst {worst}");
    let (_, glided, end, _, _) = results[1];
    assert!(!glided);
    assert!(
        end < -70.0,
        "without Follow Screen the scene stays off: {end}"
    );
}

#[test]
fn pose_rates_are_capped() {
    let mut ht = started(MotionConfig::default());
    ht.watch_ui(true);
    let reports = motion();
    let (mut render, mut ui) = (0usize, 0usize);
    for l in &reports {
        for e in ht.handle_frame(&l.bytes, l.t) {
            match e {
                Event::RenderPose(_) => render += 1,
                Event::UiPose(_) => ui += 1,
                _ => {}
            }
        }
    }
    let span = (reports.last().unwrap().t - reports[0].t).as_secs_f64();
    let (render_hz, ui_hz) = (render as f64 / span, ui as f64 / span);
    eprintln!(
        "{} reports in {span:.1} s ({:.1} Hz): render {render_hz:.1} Hz, interface {ui_hz:.1} Hz",
        reports.len(),
        reports.len() as f64 / span
    );
    // Bluetooth bunches reports: in a burst only the newest pose matters.
    assert!(render_hz <= 50.0 + 2.0 / span && render_hz > 45.0);
    assert!(ui_hz <= 30.0 + 2.0 / span && ui_hz > 29.0);

    ht.watch_ui(false);
    let ev = ht.handle_frame(&reports[0].bytes, reports.last().unwrap().t);
    assert!(!ev.iter().any(|e| matches!(e, Event::UiPose(_))));
}

#[test]
fn the_fixtures_carry_no_identifiers() {
    // Serial numbers and calibration blobs of the plists are masked, and no
    // frame other than opcode 0x17 is kept (0x1D, 0x0C, 0x0E, 0x2E, 0x2B
    // hold serial numbers and addresses).
    for l in setup().iter().chain(motion().iter()) {
        assert_eq!(l.bytes[4], 0x17);
        let text = String::from_utf8_lossy(&l.bytes);
        if let Some(i) = text.find("<key>SerialNumber</key><data>") {
            assert!(text[i + 29..].starts_with("AAAAAAAAAAAAAAAAAA"));
        }
    }
}
