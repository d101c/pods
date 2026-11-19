//! Spatial audio for the pods engine (docs/ARCHITECTURE.md, Spatial audio).
//!
//! Two modes share one virtual sink, `spatial_airpods` ("AirPods Pro ·
//! Spatial"), whose binaural output goes to the AirPods node only. Both run
//! the same renderer in this process, in a `pw_filter` fed by a null sink:
//! KEMAR HRIRs (libmysofa) with a diffuse-field equalisation and the bass
//! passed dry (see [`eq`]), FFT block convolution, and warm crossfades on
//! every angle change (see [`render`]).
//!
//! - [`Mode::Fixed`]: virtual speakers at ±30°, the angle never changes.
//! - [`Mode::HeadTracked`]: the scene follows [`Spatial::set_yaw`] (an
//!   atomic, callable from any thread at sensor rate).
//!
//! In both modes the supervisor makes the sink the default output, restores
//! the previous one at the end, and pauses while Listen Together shares the
//! sound (see [`policy`]). It talks to PipeWire through the `pipewire`
//! crate only (metadata `default.configured.audio.sink`, registry), never
//! through `pactl`/`wpctl`.
//!
//! ```no_run
//! use pods_spatial::{Mode, Options, Spatial, Target};
//! let target = Target::new("bluez_output.AA_BB_CC_DD_EE_FF.1", "AirPods Pro");
//! let opts = Options::new();
//! let spatial = Spatial::start(Mode::HeadTracked, target, opts, |s| println!("{s:?}"))?;
//! spatial.set_yaw(12.5); // from the head-tracking thread, 25-50 Hz
//! spatial.stop();
//! # Ok::<(), pods_spatial::Error>(())
//! ```

pub mod angle;
pub mod conf;
pub mod eq;
mod filter;
pub mod hrir;
pub mod policy;
pub mod render;
pub mod sinks;
mod supervisor;
pub mod xover;

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub use filter::{RenderStats, StatsSnapshot};
pub use policy::{PauseReason, Status, StopReason};
pub use sinks::{NodeRun, SinkWatch};

#[derive(Debug)]
pub enum Error {
    /// The target node name is not a plain PipeWire node name.
    InvalidTarget(String),
    /// The SOFA file could not be read.
    Sofa(String),
    /// The AirPods sink is not in the graph.
    TargetNotFound(String),
    /// A `spatial_airpods` node already exists (another instance runs).
    AlreadyRunning,
    /// No `default` metadata: no session manager is running.
    NoDefaultMetadata,
    /// Connected to an unexpected PipeWire instance (bench safety check).
    WrongCore { expected: String, found: String },
    PipeWire(String),
    Io(std::io::Error),
    Timeout(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidTarget(t) => write!(f, "invalid target node name {t:?}"),
            Error::Sofa(e) => write!(f, "HRTF: {e}"),
            Error::TargetNotFound(t) => write!(f, "target sink {t} is not in the PipeWire graph"),
            Error::AlreadyRunning => write!(f, "a spatial_airpods sink already exists"),
            Error::NoDefaultMetadata => write!(f, "no 'default' metadata (is WirePlumber running?)"),
            Error::WrongCore { expected, found } => {
                write!(f, "connected to PipeWire core {found:?}, expected {expected:?}")
            }
            Error::PipeWire(e) => write!(f, "PipeWire: {e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Timeout(what) => write!(f, "timed out {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<pipewire::Error> for Error {
    fn from(e: pipewire::Error) -> Self {
        Error::PipeWire(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Virtual speakers at ±30°, no head tracking (the yaw stays 0).
    Fixed,
    /// The scene follows [`Spatial::set_yaw`].
    HeadTracked,
}

/// How the HRIRs are used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    /// Diffuse-field equalised, bass passed dry (see [`eq`]).
    #[default]
    Equalised,
    /// The HRIRs as measured, full band: what the PipeWire sofa filter
    /// plays. Kept for the bench and for listening comparisons.
    Raw,
}

/// The AirPods sink to render to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `node.name` of the sink, e.g. `bluez_output.AA_BB_CC_DD_EE_FF.1`.
    pub node_name: String,
    /// Device name for the sink description ("AirPods Pro" gives
    /// "AirPods Pro · Spatial").
    pub description: String,
}

impl Target {
    pub fn new(node_name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { node_name: node_name.into(), description: description.into() }
    }

    /// The BlueZ sink name for a device address (`AA:BB:…` → `bluez_output.AA_BB_….1`).
    pub fn bluez_node_name(address: &str) -> String {
        format!("bluez_output.{}.1", address.replace(':', "_").to_uppercase())
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// HRTF file (both modes).
    pub sofa: PathBuf,
    pub tone: Tone,
    /// PipeWire remote to connect to (`None`: `$PIPEWIRE_REMOTE` or the default).
    pub remote: Option<String>,
    /// Refuse to run unless the PipeWire core has this name (bench safety).
    pub require_core_name: Option<String>,
    /// Sample rate of the HRIR table (the graph rate, 48 kHz by default).
    pub rate: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            sofa: PathBuf::from(conf::DEFAULT_SOFA),
            tone: Tone::default(),
            remote: None,
            require_core_name: None,
            rate: 48_000,
        }
    }
}

impl Options {
    pub fn new() -> Self {
        Self::default()
    }

    /// The renderer's table for these options (reads the SOFA file; about
    /// 0.2 s for KEMAR with the equalisation).
    pub fn spectra(&self) -> Result<render::SpectraTable, Error> {
        let set = match self.tone {
            Tone::Raw => hrir::HrirSet::load_sofa(&self.sofa, self.rate)?,
            Tone::Equalised => {
                let (set, measurements) = hrir::HrirSet::load_sofa_with_measurements(&self.sofa, self.rate)?;
                let (set, report) = eq::equalise(&set, &measurements);
                tracing::debug!(?report, "HRIRs equalised");
                set
            }
        };
        Ok(render::SpectraTable::new(&set))
    }
}

/// Head yaw input, cheap to clone and to call from any thread.
#[derive(Debug, Clone, Default)]
pub struct YawHandle(Arc<AtomicU32>);

impl YawHandle {
    /// Head yaw in degrees, positive when the head turns left; any range
    /// (wrapped by the renderer). Ignored in Fixed mode and while paused.
    pub fn set(&self, yaw_deg: f32) {
        self.0.store(yaw_deg.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// A running spatial output. Dropping it stops it too.
pub struct Spatial {
    mode: Mode,
    yaw: YawHandle,
    stats: Arc<RenderStats>,
    status: Arc<Mutex<Status>>,
    cmd: pipewire::channel::Sender<supervisor::Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Spatial {
    /// Starts the spatial output for `target`. Returns once the backend is
    /// launched (the sink becomes the default shortly after; follow
    /// `on_status`, called from the PipeWire thread on every change).
    pub fn start(
        mode: Mode,
        target: Target,
        opts: Options,
        on_status: impl Fn(Status) + Send + 'static,
    ) -> Result<Self, Error> {
        conf::validate_node_name(&target.node_name)?;
        let table = Arc::new(opts.spectra()?);
        let yaw = YawHandle::default();
        let stats = Arc::new(RenderStats::default());
        let status = Arc::new(Mutex::new(Status::Starting));
        let (cmd_tx, cmd_rx) = pipewire::channel::channel();
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel(1);
        let cfg = supervisor::Config {
            mode,
            target,
            remote: opts.remote,
            require_core_name: opts.require_core_name,
            table,
            yaw: yaw.0.clone(),
            stats: stats.clone(),
            status: status.clone(),
            on_status: Box::new(on_status),
        };
        let thread = std::thread::Builder::new()
            .name("pods-spatial".into())
            .spawn(move || supervisor::run(cfg, cmd_rx, init_tx))?;
        match init_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self { mode, yaw, stats, status, cmd: cmd_tx, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = cmd_tx.send(supervisor::Cmd::Stop);
                Err(Error::Timeout("starting the spatial output"))
            }
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Sets the head yaw (degrees, positive to the left). Thread-safe and
    /// wait-free: one atomic store, read by the renderer once per cycle.
    pub fn set_yaw(&self, yaw_deg: f32) {
        self.yaw.set(yaw_deg);
    }

    /// A handle for the sensor thread.
    pub fn yaw_handle(&self) -> YawHandle {
        self.yaw.clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or(Status::Starting)
    }

    /// Renderer counters.
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// Stops the output: tears the backend down and gives the default
    /// output back. Blocks until done (a few hundred ms, ≤ 5 s).
    pub fn stop(mut self) -> Status {
        self.shutdown();
        self.status()
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = self.cmd.send(supervisor::Cmd::Stop);
            let _ = t.join();
        }
    }
}

impl Drop for Spatial {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluez_names() {
        assert_eq!(Target::bluez_node_name("aa:bb:cc:dd:ee:ff"), "bluez_output.AA_BB_CC_DD_EE_FF.1");
    }

    #[test]
    fn yaw_handle_round_trips() {
        let y = YawHandle::default();
        assert_eq!(y.get(), 0.0);
        y.set(-12.25);
        assert_eq!(y.clone().get(), -12.25);
    }
}
