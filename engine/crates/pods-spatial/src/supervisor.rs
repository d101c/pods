//! The PipeWire thread: watches the graph, runs the renderer's nodes (a null
//! sink and the `pw_filter`, the same in both modes) and applies the
//! [`Policy`](crate::policy::Policy) decisions to the `default` metadata.
//!
//! PipeWire objects are not `Send`, so everything here lives on one thread
//! that owns its main loop; the public handle talks to it through a
//! `pipewire::channel` and atomics.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::types::ObjectType;

use crate::conf::{self, OUTPUT_NODE_NAME, SINK_NODE_NAME};
use crate::filter::{DspFilter, FilterParams, RenderStats};
use crate::policy::{Action, Input, Policy, Status, default_json, parse_default_name};
use crate::render::{Renderer, SpectraTable};
use crate::{Error, Mode, Target};

const KEY_DEFAULT: &str = "default.audio.sink";
const KEY_CONFIGURED: &str = "default.configured.audio.sink";
const TICK: Duration = Duration::from_millis(250);
/// The sink must show up this soon after the backend starts.
const SINK_DEADLINE: Duration = Duration::from_secs(8);

pub(crate) enum Cmd {
    Stop,
}

pub(crate) struct Config {
    pub mode: Mode,
    pub target: Target,
    pub remote: Option<String>,
    pub require_core_name: Option<String>,
    pub table: Arc<SpectraTable>,
    pub yaw: Arc<AtomicU32>,
    pub stats: Arc<RenderStats>,
    pub status: Arc<Mutex<Status>>,
    pub on_status: Box<dyn Fn(Status) + Send>,
}

struct NodeInfo {
    name: String,
}

struct PortInfo {
    node: u32,
    input: bool,
    channel: String,
    monitor: bool,
}

struct Nodes {
    sink: Option<pw::node::Node>,
    filter: Option<DspFilter>,
    links: Vec<pw::link::Link>,
    linked: bool,
}

enum Backend {
    None,
    Running(Nodes),
}

struct State {
    mode: Mode,
    target: String,
    policy: Policy,
    nodes: HashMap<u32, NodeInfo>,
    ports: HashMap<u32, PortInfo>,
    target_id: Option<u32>,
    sink_id: Option<u32>,
    meta: Option<(u32, pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    /// Raw value of `default.configured.audio.sink`.
    configured: Option<String>,
    /// The configured default to put back (`Some(None)`: there was none).
    saved: Option<Option<String>>,
    sharing: bool,
    backend: Backend,
    started_at: Option<Instant>,
    core: pw::core::CoreRc,
    mainloop: pw::main_loop::MainLoopRc,
    reported: Status,
    status: Arc<Mutex<Status>>,
    on_status: Box<dyn Fn(Status) + Send>,
    tracking: Arc<AtomicBool>,
    stats: Arc<RenderStats>,
}

impl State {
    fn feed(&mut self, input: Input) {
        let actions = self.policy.handle(input, Instant::now());
        for a in actions {
            match a {
                Action::ClaimDefault => self.claim_default(),
                Action::RestoreDefault => self.restore_default(),
                // The nodes go once the loop is left (see `teardown`); the
                // renderer stops turning the sound now.
                Action::Teardown => self.tracking.store(false, Ordering::Relaxed),
            }
        }
        self.publish();
        if self.policy.is_stopped() {
            self.mainloop.quit();
        }
    }

    fn publish(&mut self) {
        let s = self.policy.status().clone();
        if s == self.reported {
            return;
        }
        tracing::info!("spatial audio: {s:?}");
        self.reported = s.clone();
        if let Ok(mut g) = self.status.lock() {
            *g = s.clone();
        }
        // Fixed mode: the renderer keeps yaw 0, never a transition.
        self.tracking.store(s == Status::Active && self.mode == Mode::HeadTracked, Ordering::Relaxed);
        (self.on_status)(s);
    }

    fn claim_default(&mut self) {
        if self.saved.is_none() {
            // A stale value naming our own sink (left by a crash) is no
            // default worth restoring.
            let prev = self
                .configured
                .clone()
                .filter(|v| parse_default_name(v).as_deref() != Some(SINK_NODE_NAME));
            self.saved = Some(prev);
        }
        if let Some((_, meta, _)) = &self.meta {
            meta.set_property(0, KEY_CONFIGURED, Some("Spa:String:JSON"), Some(&default_json(SINK_NODE_NAME)));
        }
    }

    fn restore_default(&mut self) {
        let Some(saved) = self.saved.take() else { return };
        let current = self.configured.as_deref().and_then(parse_default_name);
        // Someone else (the user, Listen Together) picked an output since:
        // leave their choice alone.
        if current.as_deref() != Some(SINK_NODE_NAME) && self.configured != saved {
            return;
        }
        if let Some((_, meta, _)) = &self.meta {
            match &saved {
                Some(v) => meta.set_property(0, KEY_CONFIGURED, Some("Spa:String:JSON"), Some(v)),
                None => meta.set_property(0, KEY_CONFIGURED, None, None),
            }
        }
    }

    /// Removes our nodes and links, once the main loop is left and a round
    /// trip has brought every pending event in (issue #2):
    ///
    /// - dropping a proxy is enough: it asks the server to destroy the
    ///   object (`core.destroy`, opcode 7). Calling `Core::destroy_object`
    ///   as well sent that request twice, and the second one, for an id the
    ///   server had already freed, came back as "unknown resource N op:7";
    /// - when the AirPods node goes, the server destroys the links to it
    ///   after announcing the node gone. Released right then, their proxies
    ///   still looked alive and were destroyed a second time; after the
    ///   round trip their `remove_id` has arrived and dropping them sends
    ///   nothing.
    fn teardown(&mut self) {
        self.tracking.store(false, Ordering::Relaxed);
        if let Backend::Running(t) = std::mem::replace(&mut self.backend, Backend::None) {
            // Links first, then the nodes they join.
            drop(t.links);
            drop(t.filter);
            drop(t.sink);
        }
    }

    /// Lets go of the default metadata: its listener before its proxy.
    fn release_meta(&mut self) {
        if let Some((_, meta, listener)) = self.meta.take() {
            drop(listener);
            drop(meta);
        }
    }

    fn is_our_sink(&self, n: &NodeInfo) -> bool {
        if n.name != SINK_NODE_NAME {
            return false;
        }
        matches!(self.backend, Backend::Running(_))
    }

    fn after_graph_change(&mut self) {
        if self.started_at.is_some() && self.sink_id.is_none() {
            let found = self.nodes.iter().find(|(_, n)| self.is_our_sink(n)).map(|(id, _)| *id);
            if let Some(id) = found {
                self.sink_id = Some(id);
                self.feed(Input::SinkReady);
            }
        }
        let sharing = self.nodes.values().any(|n| n.name == "listen_together");
        if sharing != self.sharing {
            self.sharing = sharing;
            self.feed(Input::Sharing(sharing));
        }
        self.try_link();
    }

    fn on_global(&mut self, st: &Rc<RefCell<State>>, registry: &pw::registry::RegistryRc, g: &GlobalRef) {
        let Some(props) = g.props else { return };
        match g.type_ {
            ObjectType::Node => {
                let name = props.get("node.name").unwrap_or_default().to_owned();
                if name == self.target {
                    self.target_id = Some(g.id);
                }
                self.nodes.insert(g.id, NodeInfo { name });
                self.after_graph_change();
            }
            ObjectType::Port => {
                let Some(node) = props.get("node.id").and_then(|v| v.parse().ok()) else { return };
                self.ports.insert(
                    g.id,
                    PortInfo {
                        node,
                        input: props.get("port.direction") == Some("in"),
                        channel: props.get("audio.channel").unwrap_or_default().to_owned(),
                        monitor: props.get("port.monitor") == Some("true"),
                    },
                );
                self.try_link();
            }
            ObjectType::Metadata if props.get("metadata.name") == Some("default") && self.meta.is_none() => {
                let meta: pw::metadata::Metadata = match registry.bind(g) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("cannot bind the default metadata: {e}");
                        return;
                    }
                };
                let weak = Rc::downgrade(st);
                let listener = meta
                    .add_listener_local()
                    .property(move |subject, key, _type, value| {
                        if let Some(st) = weak.upgrade() {
                            st.borrow_mut().on_meta(subject, key, value);
                        }
                        0
                    })
                    .register();
                self.meta = Some((g.id, meta, listener));
            }
            _ => {}
        }
    }

    fn on_global_remove(&mut self, id: u32) {
        if self.nodes.remove(&id).is_some() {
            if self.target_id == Some(id) {
                self.target_id = None;
                self.feed(Input::TargetGone);
            }
            if self.sink_id == Some(id) {
                self.sink_id = None;
                self.feed(Input::BackendFailed("the spatial sink disappeared".into()));
            }
            self.after_graph_change();
        }
        self.ports.remove(&id);
        if self.meta.as_ref().is_some_and(|(mid, _, _)| *mid == id) {
            self.release_meta();
        }
    }

    fn on_meta(&mut self, subject: u32, key: Option<&str>, value: Option<&str>) {
        if subject != 0 {
            return;
        }
        match key {
            None => {
                self.configured = None;
                self.feed(Input::DefaultSink(None));
            }
            Some(KEY_DEFAULT) => self.feed(Input::DefaultSink(value.and_then(parse_default_name))),
            Some(KEY_CONFIGURED) => self.configured = value.map(str::to_owned),
            _ => {}
        }
    }

    fn tick(&mut self) {
        if self.stats.failed.load(Ordering::Relaxed) {
            self.feed(Input::BackendFailed("the renderer filter failed".into()));
            return;
        }
        if self.sink_id.is_none() && self.started_at.is_some_and(|t| t.elapsed() > SINK_DEADLINE) {
            self.feed(Input::BackendFailed("the spatial sink did not appear".into()));
            return;
        }
        self.try_link();
        self.feed(Input::Tick);
    }

    /// Once every port exists, link
    /// sink monitor → filter inputs and filter outputs → AirPods.
    fn try_link(&mut self) {
        let Backend::Running(t) = &mut self.backend else { return };
        if t.linked {
            return;
        }
        let (Some(sink), Some(target)) = (self.sink_id, self.target_id) else { return };
        let Some(filter) = t.filter.as_ref().and_then(|f| f.node_id()) else { return };
        let ports = &self.ports;
        let find = |node: u32, input: bool, ch: &str, monitor: bool| {
            ports
                .iter()
                .find(|(_, p)| p.node == node && p.input == input && p.channel == ch && p.monitor == monitor)
                .map(|(id, _)| *id)
        };
        let mut pairs = Vec::with_capacity(4);
        for ch in ["FL", "FR"] {
            let (Some(a), Some(b)) = (find(sink, false, ch, true), find(filter, true, ch, false)) else { return };
            pairs.push((sink, a, filter, b));
            let (Some(c), Some(d)) = (find(filter, false, ch, false), find(target, true, ch, false)) else { return };
            pairs.push((filter, c, target, d));
        }
        for (on, op, inode, ip) in pairs {
            let mut p = pw::properties::PropertiesBox::new();
            p.insert("link.output.node", on.to_string());
            p.insert("link.output.port", op.to_string());
            p.insert("link.input.node", inode.to_string());
            p.insert("link.input.port", ip.to_string());
            // Passive: the AirPods may suspend when nothing plays into the sink.
            p.insert("link.passive", "true");
            p.insert("object.linger", "false");
            match self.core.create_object::<pw::link::Link>("link-factory", &p) {
                Ok(l) => t.links.push(l),
                Err(e) => tracing::warn!("cannot link the spatial renderer: {e}"),
            }
        }
        t.linked = true;
        tracing::debug!("spatial renderer linked: sink {sink} → filter {filter} → {target}");
    }

    fn start_backend(&mut self, cfg_target: &Target, table: Arc<SpectraTable>, yaw: Arc<AtomicU32>) -> Result<(), Error> {
        let desc = conf::sink_description(&cfg_target.description);
        let mut p = pw::properties::PropertiesBox::new();
        p.insert("factory.name", "support.null-audio-sink");
        p.insert("node.name", SINK_NODE_NAME);
        p.insert("node.description", desc.as_str());
        p.insert("media.class", "Audio/Sink");
        p.insert("audio.position", "FL,FR");
        // The sink volume applies to what we render.
        p.insert("monitor.channel-volumes", "true");
        p.insert("object.linger", "false");
        let sink: pw::node::Node = self.core.create_object("adapter", &p)?;
        let filter = DspFilter::new(
            &self.core,
            FilterParams {
                description: format!("{desc} (renderer)"),
                renderer: Renderer::new(table),
                yaw,
                tracking: self.tracking.clone(),
                stats: self.stats.clone(),
            },
        )?;
        self.backend = Backend::Running(Nodes { sink: Some(sink), filter: Some(filter), links: Vec::new(), linked: false });
        self.started_at = Some(Instant::now());
        Ok(())
    }
}

type GlobalRef<'a> = pw::registry::GlobalObject<&'a pw::spa::utils::dict::DictRef>;

/// Processes events until the server has answered everything sent so far.
fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc, timeout: Duration) -> Result<(), Error> {
    let done = Rc::new(Cell::new(false));
    let expired = Rc::new(Cell::new(false));
    let pending = core.sync(0)?;
    let _listener = core
        .add_listener_local()
        .done({
            let (done, ml) = (done.clone(), mainloop.clone());
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                    ml.quit();
                }
            }
        })
        .register();
    let timer = mainloop.loop_().add_timer({
        let (expired, ml) = (expired.clone(), mainloop.clone());
        move |_| {
            expired.set(true);
            ml.quit();
        }
    });
    timer.update_timer(Some(timeout), None).into_result().map_err(|e| Error::PipeWire(e.to_string()))?;
    while !done.get() && !expired.get() {
        mainloop.run();
    }
    if done.get() { Ok(()) } else { Err(Error::Timeout("waiting for PipeWire")) }
}

pub(crate) fn run(cfg: Config, cmd_rx: pw::channel::Receiver<Cmd>, init_tx: SyncSender<Result<(), Error>>) {
    if let Err(e) = run_inner(cfg, cmd_rx, &init_tx) {
        let _ = init_tx.send(Err(e));
    }
}

fn run_inner(cfg: Config, cmd_rx: pw::channel::Receiver<Cmd>, init_tx: &SyncSender<Result<(), Error>>) -> Result<(), Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core_props = cfg.remote.as_ref().map(|r| {
        let mut p = pw::properties::PropertiesBox::new();
        p.insert("remote.name", r.as_str());
        p
    });
    let core = context.connect_rc(core_props)?;
    let registry = core.get_registry_rc()?;

    let Config { mode, target, remote: _, require_core_name, table, yaw, stats, status, on_status } = cfg;
    let st = Rc::new(RefCell::new(State {
        mode,
        target: target.node_name.clone(),
        policy: Policy::new(),
        nodes: HashMap::new(),
        ports: HashMap::new(),
        target_id: None,
        sink_id: None,
        meta: None,
        configured: None,
        saved: None,
        sharing: false,
        backend: Backend::None,
        started_at: None,
        core: core.clone(),
        mainloop: mainloop.clone(),
        reported: Status::Starting,
        status,
        on_status,
        tracking: Arc::new(AtomicBool::new(false)),
        stats,
    }));

    let core_name = Rc::new(RefCell::new(String::new()));
    let weak: Weak<RefCell<State>> = Rc::downgrade(&st);
    let _core_listener = core
        .add_listener_local()
        .info({
            let core_name = core_name.clone();
            move |info| *core_name.borrow_mut() = info.name().to_owned()
        })
        .error({
            let weak = weak.clone();
            move |id, _seq, res, msg| {
                tracing::warn!("PipeWire error on object {id}: {msg} ({res})");
                if id == pw::core::PW_ID_CORE
                    && res == -libc::EPIPE
                    && let Some(st) = weak.upgrade()
                {
                    st.borrow_mut().feed(Input::BackendFailed("PipeWire connection lost".into()));
                }
            }
        })
        .register();

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (weak, registry) = (weak.clone(), registry.clone());
            move |g| {
                if let Some(st) = weak.upgrade() {
                    st.borrow_mut().on_global(&st, &registry, g);
                }
            }
        })
        .global_remove({
            let weak = weak.clone();
            move |id| {
                if let Some(st) = weak.upgrade() {
                    st.borrow_mut().on_global_remove(id);
                }
            }
        })
        .register();

    // Globals first, then the metadata values of the bound default metadata.
    roundtrip(&mainloop, &core, Duration::from_secs(5))?;
    roundtrip(&mainloop, &core, Duration::from_secs(5))?;

    if let Some(expected) = &require_core_name {
        let found = core_name.borrow().clone();
        if &found != expected {
            return Err(Error::WrongCore { expected: expected.clone(), found });
        }
    }
    {
        let s = st.borrow();
        if s.target_id.is_none() {
            return Err(Error::TargetNotFound(target.node_name.clone()));
        }
        if s.nodes.values().any(|n| n.name == SINK_NODE_NAME || n.name == OUTPUT_NODE_NAME) {
            return Err(Error::AlreadyRunning);
        }
        if s.meta.is_none() {
            return Err(Error::NoDefaultMetadata);
        }
    }
    st.borrow_mut().start_backend(&target, table, yaw)?;

    let _cmd_rx = cmd_rx.attach(mainloop.loop_(), {
        let weak = weak.clone();
        move |cmd| match cmd {
            Cmd::Stop => {
                if let Some(st) = weak.upgrade() {
                    st.borrow_mut().feed(Input::StopRequested);
                }
            }
        }
    });
    let timer = mainloop.loop_().add_timer({
        let weak = weak.clone();
        move |_| {
            if let Some(st) = weak.upgrade() {
                st.borrow_mut().tick();
            }
        }
    });
    timer.update_timer(Some(TICK), Some(TICK)).into_result().map_err(|e| Error::PipeWire(e.to_string()))?;

    let _ = init_tx.send(Ok(()));
    st.borrow_mut().after_graph_change();
    while !st.borrow().policy.is_stopped() {
        mainloop.run();
    }

    // Make sure the restored default reached the server, and that what the
    // server destroyed on its own is known gone, before releasing anything.
    let _ = roundtrip(&mainloop, &core, Duration::from_secs(2));
    {
        let mut s = st.borrow_mut();
        s.teardown();
        s.release_meta();
    }
    let _ = roundtrip(&mainloop, &core, Duration::from_secs(1));
    Ok(())
}
