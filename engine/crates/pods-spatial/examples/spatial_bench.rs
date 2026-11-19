//! Bench driver for pods-spatial. Only ever runs against the PRIVATE bench
//! PipeWire started by `bench/bench.sh` (it refuses any other instance).
//!
//!   spatial_bench run --mode fixed|tracked --target fake_airpods
//!                     [--tone eq|raw] [--traj static|sine|jump|hold] [--yaw DEG]
//!                     [--rate 50] [--seconds 15] [--stop-file F] [--stats F]
//!   spatial_bench watch-sinks [--prefix fake_airpods] [--seconds 30]
//!
//! Trajectories (head yaw, degrees, + = head turned left), as in the study:
//!   sine  45° · sin(2π · 0.5 Hz · t)   (141°/s at the peak: a brisk head shake)
//!   jump  0° / 60° alternating every 0.5 s
//!   hold  --yaw (default 0°) sent at the rate
//!   static  nothing sent

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pods_spatial::{Mode, Options, Spatial, Target, Tone};

const CORE_ENV: &str = "PODS_SPATIAL_BENCH_CORE";

fn die(msg: &str) -> ! {
    eprintln!("spatial_bench: {msg}");
    std::process::exit(2)
}

/// The bench core name, after checking we are pointed at a private instance.
fn bench_core() -> String {
    let core = std::env::var(CORE_ENV).unwrap_or_else(|_| die("SAFETY: run me from bench/bench.sh only"));
    let remote = std::env::var("PIPEWIRE_REMOTE").unwrap_or_default();
    let rundir = std::env::var("PIPEWIRE_RUNTIME_DIR").unwrap_or_default();
    if core.is_empty() || remote != core || rundir.is_empty() || rundir.starts_with("/run/user") {
        die("SAFETY: not pointed at the private bench PipeWire");
    }
    core
}

fn yaw_at(traj: &str, hold: f32, t: f64) -> f32 {
    match traj {
        "sine" => (45.0 * (2.0 * std::f64::consts::PI * 0.5 * t).sin()) as f32,
        "jump" if (t / 0.5) as u64 % 2 == 1 => 60.0,
        "jump" => 0.0,
        _ => hold,
    }
}

struct Args(Vec<String>);

impl Args {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().position(|a| a == key).and_then(|i| self.0.get(i + 1)).map(String::as_str)
    }
    fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_owned()
    }
    fn num(&self, key: &str, default: f64) -> f64 {
        self.get(key).map(|v| v.parse().unwrap_or_else(|_| die(&format!("bad {key}")))).unwrap_or(default)
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first().cloned() else { die("usage: spatial_bench run|watch-sinks ...") };
    let args = Args(argv);
    let core = bench_core();
    match cmd.as_str() {
        "run" => run(&args, core),
        "watch-sinks" => watch_sinks(&args, core),
        _ => die("usage: spatial_bench run|watch-sinks ..."),
    }
}

/// Prints what `SinkWatch` (the engine's "is sound flowing to the AirPods?")
/// reports, one `NODE <name> <Gone|Stopped|Running>` line per change.
fn watch_sinks(args: &Args, core: String) {
    let prefix = args.get_or("--prefix", "fake_airpods");
    let seconds = args.num("--seconds", 30.0);
    let watch = pods_spatial::SinkWatch::start(&prefix, Some(core), |name, run| {
        println!("NODE {name} {run:?}");
        let _ = std::io::stdout().flush();
    })
    .unwrap_or_else(|e| die(&e.to_string()));
    std::thread::sleep(Duration::from_secs_f64(seconds));
    watch.stop();
    println!("DONE");
}

fn run(args: &Args, core: String) {
    let mode = match args.get_or("--mode", "tracked").as_str() {
        "fixed" => Mode::Fixed,
        "tracked" => Mode::HeadTracked,
        m => die(&format!("unknown mode {m}")),
    };
    let target = args.get("--target").unwrap_or_else(|| die("--target is required"));
    let traj = args.get_or("--traj", "static");
    let rate = args.num("--rate", 50.0);
    let seconds = args.num("--seconds", 15.0);
    let stop_file = args.get("--stop-file").map(PathBuf::from);
    let hold = args.num("--yaw", 0.0) as f32;

    let mut opts = Options::new();
    opts.tone = match args.get_or("--tone", "eq").as_str() {
        "eq" => Tone::Equalised,
        "raw" => Tone::Raw,
        t => die(&format!("unknown tone {t}")),
    };
    opts.remote = Some(core.clone());
    opts.require_core_name = Some(core);
    if let Some(s) = args.get("--sofa") {
        opts.sofa = PathBuf::from(s);
    }
    let t0 = Instant::now();
    let on_status = move |s| {
        println!("STATUS {:.3} {:?}", t0.elapsed().as_secs_f64(), s);
        let _ = std::io::stdout().flush();
    };
    let spatial = Spatial::start(mode, Target::new(target, "AirPods Pro"), opts, on_status)
        .unwrap_or_else(|e| die(&format!("start failed: {e}")));
    println!("STARTED {:.3}", t0.elapsed().as_secs_f64());
    let _ = std::io::stdout().flush();

    let yaw = spatial.yaw_handle();
    let period = Duration::from_secs_f64(1.0 / rate.max(1.0));
    let start = Instant::now();
    let mut next = start;
    let mut sent = 0u64;
    loop {
        let t = start.elapsed();
        if t.as_secs_f64() >= seconds || stop_file.as_ref().is_some_and(|f| f.exists()) {
            break;
        }
        if matches!(spatial.status(), pods_spatial::Status::Stopped(_)) {
            break;
        }
        if traj != "static" && mode == Mode::HeadTracked {
            yaw.set(yaw_at(&traj, hold, t.as_secs_f64()));
            sent += 1;
        }
        next += if traj == "static" { Duration::from_millis(50) } else { period };
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    let stats = spatial.stats();
    let status = spatial.stop();
    println!("STOPPED {:.3} {:?}", t0.elapsed().as_secs_f64(), status);
    let line = format!(
        "{{\"yaw_updates_sent\":{sent},\"cycles\":{},\"samples\":{},\"transitions\":{},\"busy_ns\":{},\"max_busy_ns\":{},\"rate\":{},\"quantum\":{}}}",
        stats.cycles, stats.samples, stats.transitions, stats.busy_ns, stats.max_busy_ns, stats.rate, stats.quantum
    );
    println!("STATS {line}");
    if let Some(f) = args.get("--stats") {
        let _ = std::fs::write(f, line + "\n");
    }
}
