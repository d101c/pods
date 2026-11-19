//! The head-tracked renderer as a PipeWire `pw_filter` node.
//!
//! pipewire-rs has no safe `pw_filter` binding, so this is a thin wrapper
//! over pipewire-sys: two DSP input ports (FL, FR), two DSP output ports, and
//! a process callback that runs in PipeWire's real-time data thread. The
//! callback only reads atomics and calls [`Renderer::process`] (no locks, no
//! allocation). Everything else (creation, destruction) happens on the
//! thread that owns the PipeWire main loop.

use std::ffi::{CString, c_char, c_void};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use pipewire as pw;
use pw::spa::sys as spa_sys;
use pw::sys as pw_sys;

use crate::Error;
use crate::conf::OUTPUT_NODE_NAME;
use crate::render::Renderer;

/// Largest cycle handled in one go (PipeWire's default quantum limit);
/// longer cycles are rendered in several calls.
const MAX_CYCLE: usize = 8192;

/// Counters written by the real-time thread, read by anyone.
#[derive(Debug, Default)]
pub struct RenderStats {
    pub cycles: AtomicU64,
    pub samples: AtomicU64,
    pub transitions: AtomicU64,
    /// Total time spent in the process callback, ns.
    pub busy_ns: AtomicU64,
    /// Longest process callback, ns.
    pub max_busy_ns: AtomicU64,
    /// Last graph rate seen (Hz).
    pub rate: AtomicU32,
    /// Last quantum seen (samples).
    pub quantum: AtomicU32,
    /// The filter went to the error state.
    pub failed: AtomicBool,
}

/// A copy of [`RenderStats`] at one instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub cycles: u64,
    pub samples: u64,
    pub transitions: u64,
    pub busy_ns: u64,
    pub max_busy_ns: u64,
    pub rate: u32,
    pub quantum: u32,
}

impl RenderStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            cycles: self.cycles.load(Ordering::Relaxed),
            samples: self.samples.load(Ordering::Relaxed),
            transitions: self.transitions.load(Ordering::Relaxed),
            busy_ns: self.busy_ns.load(Ordering::Relaxed),
            max_busy_ns: self.max_busy_ns.load(Ordering::Relaxed),
            rate: self.rate.load(Ordering::Relaxed),
            quantum: self.quantum.load(Ordering::Relaxed),
        }
    }
}

struct FilterData {
    /// in FL, in FR, out FL, out FR (port data returned by pw_filter_add_port).
    ports: [*mut c_void; 4],
    renderer: Renderer,
    yaw: Arc<AtomicU32>,
    tracking: Arc<AtomicBool>,
    stats: Arc<RenderStats>,
    in_buf: [Vec<f32>; 2],
}

pub(crate) struct DspFilter {
    raw: NonNull<pw_sys::pw_filter>,
    // The hook and the events table must outlive the filter.
    _hook: Box<spa_sys::spa_hook>,
    _events: Box<pw_sys::pw_filter_events>,
    data: *mut FilterData,
}

pub(crate) struct FilterParams {
    pub description: String,
    pub renderer: Renderer,
    pub yaw: Arc<AtomicU32>,
    pub tracking: Arc<AtomicBool>,
    pub stats: Arc<RenderStats>,
}

fn props(pairs: &[(&str, &str)]) -> pw::properties::PropertiesBox {
    let mut p = pw::properties::PropertiesBox::new();
    for (k, v) in pairs {
        p.insert(*k, *v);
    }
    p
}

impl DspFilter {
    /// Creates and connects the filter on `core`. Must run on the main-loop thread.
    pub fn new(core: &pw::core::Core, params: FilterParams) -> Result<Self, Error> {
        let node_props = props(&[
            ("node.name", OUTPUT_NODE_NAME),
            ("node.description", &params.description),
            ("media.type", "Audio"),
            ("media.category", "Filter"),
            ("media.role", "DSP"),
            ("node.autoconnect", "false"),
            // Like the Fixed output stream: our links never keep the graph
            // running, so the AirPods can suspend when nothing plays into the
            // sink (the link factory ignores `link.passive`, the node flag
            // is what counts). pw_filter would also process while idle.
            ("node.passive", "true"),
            ("node.always-process", "false"),
        ]);
        let name = CString::new("pods-spatial").expect("no NUL");
        // SAFETY: core is a live proxy owned by the caller; props ownership
        // moves to the filter.
        let raw = unsafe { pw_sys::pw_filter_new(core.as_raw_ptr(), name.as_ptr(), node_props.into_raw()) };
        let raw = NonNull::new(raw).ok_or_else(|| Error::PipeWire("pw_filter_new failed".into()))?;

        let data = Box::into_raw(Box::new(FilterData {
            ports: [ptr::null_mut(); 4],
            renderer: params.renderer,
            yaw: params.yaw,
            tracking: params.tracking,
            stats: params.stats,
            in_buf: [vec![0.0; MAX_CYCLE], vec![0.0; MAX_CYCLE]],
        }));

        let events = Box::new(pw_sys::pw_filter_events {
            version: pw_sys::PW_VERSION_FILTER_EVENTS,
            destroy: None,
            state_changed: Some(on_state_changed),
            io_changed: None,
            param_changed: None,
            add_buffer: None,
            remove_buffer: None,
            process: Some(on_process),
            drained: None,
            command: None,
        });
        // SAFETY: an all-zero spa_hook is the documented initial state.
        let mut hook: Box<spa_sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: the hook, events and data are heap allocations owned by
        // `this` below; the filter is destroyed in Drop before they are freed.
        unsafe {
            pw_sys::pw_filter_add_listener(raw.as_ptr(), &mut *hook as *mut _ as *mut _, &*events, data.cast());
        }
        // From here on, Drop cleans up on every error path.
        let this = Self { raw, _hook: hook, _events: events, data };

        let ports = [
            (spa_sys::SPA_DIRECTION_INPUT, "input_FL", "FL"),
            (spa_sys::SPA_DIRECTION_INPUT, "input_FR", "FR"),
            (spa_sys::SPA_DIRECTION_OUTPUT, "output_FL", "FL"),
            (spa_sys::SPA_DIRECTION_OUTPUT, "output_FR", "FR"),
        ];
        for (i, (dir, port_name, channel)) in ports.into_iter().enumerate() {
            let p = props(&[
                ("format.dsp", "32 bit float mono audio"),
                ("port.name", port_name),
                ("audio.channel", channel),
            ]);
            // SAFETY: live filter; props ownership moves to the port.
            let port = unsafe {
                pw_sys::pw_filter_add_port(
                    raw.as_ptr(),
                    dir,
                    pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                    std::mem::size_of::<u64>(),
                    p.into_raw(),
                    ptr::null_mut(),
                    0,
                )
            };
            if port.is_null() {
                return Err(Error::PipeWire(format!("cannot add port {port_name}")));
            }
            // SAFETY: data is valid; the RT thread does not run before connect.
            unsafe { (*data).ports[i] = port };
        }

        // SAFETY: live filter, no params.
        let res = unsafe {
            pw_sys::pw_filter_connect(raw.as_ptr(), pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS, ptr::null_mut(), 0)
        };
        if res < 0 {
            return Err(Error::PipeWire(format!("pw_filter_connect: error {res}")));
        }
        Ok(this)
    }

    /// The node id once the filter is registered, if known.
    pub fn node_id(&self) -> Option<u32> {
        // SAFETY: live filter.
        let id = unsafe { pw_sys::pw_filter_get_node_id(self.raw.as_ptr()) };
        (id != spa_sys::SPA_ID_INVALID).then_some(id)
    }
}

impl Drop for DspFilter {
    fn drop(&mut self) {
        // SAFETY: destroying the filter stops the RT callbacks before
        // returning; only then is the data freed.
        unsafe {
            pw_sys::pw_filter_destroy(self.raw.as_ptr());
            drop(Box::from_raw(self.data));
        }
    }
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: pw_sys::pw_filter_state,
    state: pw_sys::pw_filter_state,
    error: *const c_char,
) {
    // SAFETY: data is the FilterData registered with the listener.
    let d = unsafe { &*(data as *const FilterData) };
    if state == pw_sys::pw_filter_state_PW_FILTER_STATE_ERROR {
        let msg = if error.is_null() {
            String::from("unknown error")
        } else {
            // SAFETY: PipeWire passes a NUL-terminated string.
            unsafe { std::ffi::CStr::from_ptr(error) }.to_string_lossy().into_owned()
        };
        tracing::warn!("spatial filter error: {msg}");
        d.stats.failed.store(true, Ordering::Relaxed);
    }
}

/// Real-time callback: no allocation, no lock, no logging.
unsafe extern "C" fn on_process(data: *mut c_void, position: *mut spa_sys::spa_io_position) {
    let started = Instant::now();
    // SAFETY: data is the FilterData registered with the listener; only this
    // thread touches it while the filter exists.
    let d = unsafe { &mut *(data as *mut FilterData) };
    if position.is_null() {
        return;
    }
    // SAFETY: PipeWire passes a valid position for the cycle.
    let clock = unsafe { &(*position).clock };
    let n = clock.duration as usize;
    if n == 0 {
        return;
    }
    // SAFETY: port data pointers come from pw_filter_add_port.
    let buf = |port: *mut c_void| unsafe { pw_sys::pw_filter_get_dsp_buffer(port, n as u32) as *mut f32 };
    let (in_l, in_r, out_l, out_r) = (buf(d.ports[0]), buf(d.ports[1]), buf(d.ports[2]), buf(d.ports[3]));
    if out_l.is_null() || out_r.is_null() {
        return;
    }
    let yaw = if d.tracking.load(Ordering::Relaxed) {
        f32::from_bits(d.yaw.load(Ordering::Relaxed))
    } else {
        0.0
    };
    let mut off = 0;
    while off < n {
        let m = (n - off).min(MAX_CYCLE);
        // Inputs are copied first: PipeWire may hand out aliasing buffers.
        for (ch, src) in [in_l, in_r].into_iter().enumerate() {
            let dst = &mut d.in_buf[ch][..m];
            if src.is_null() {
                dst.fill(0.0);
            } else {
                // SAFETY: the DSP buffer holds n samples.
                dst.copy_from_slice(unsafe { std::slice::from_raw_parts(src.add(off), m) });
            }
        }
        // SAFETY: each output DSP buffer holds n samples and is distinct.
        let (ol, or) = unsafe {
            (std::slice::from_raw_parts_mut(out_l.add(off), m), std::slice::from_raw_parts_mut(out_r.add(off), m))
        };
        d.renderer.process(yaw, &d.in_buf[0][..m], &d.in_buf[1][..m], ol, or);
        off += m;
    }
    let s = &d.stats;
    let busy = started.elapsed().as_nanos() as u64;
    s.cycles.fetch_add(1, Ordering::Relaxed);
    s.samples.fetch_add(n as u64, Ordering::Relaxed);
    s.transitions.store(d.renderer.transitions(), Ordering::Relaxed);
    s.busy_ns.fetch_add(busy, Ordering::Relaxed);
    s.max_busy_ns.fetch_max(busy, Ordering::Relaxed);
    s.rate.store(clock.rate.denom, Ordering::Relaxed);
    s.quantum.store(n as u32, Ordering::Relaxed);
}
