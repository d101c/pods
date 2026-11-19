//! Follows the state of PipeWire nodes by name: is sound flowing to the
//! AirPods? The head sensors only run while it does (docs §9, M0 quater).
//!
//! One thread with its own main loop, like the supervisor: it binds every
//! node whose `node.name` starts with a prefix (`bluez_output.`) and reports
//! its state from the node's `info` events. No `pw-dump`, no subprocess.
//! When PipeWire goes away the nodes are reported gone and the thread
//! connects again every few seconds.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::types::ObjectType;

use crate::Error;

/// Wait between two connection attempts.
const RETRY: Duration = Duration::from_secs(3);

/// What a followed node is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRun {
    /// The node is not (or no longer) in the graph.
    Gone,
    /// In the graph, not processing (idle, suspended, creating, error).
    Stopped,
    /// Processing: sound flows through it.
    Running,
}

type OnChange = Rc<dyn Fn(&str, NodeRun)>;

struct Followed {
    name: String,
    run: Cell<NodeRun>,
    _node: pw::node::Node,
    _listener: pw::node::NodeListener,
}

/// A running watcher. Dropping it stops it.
pub struct SinkWatch {
    cmd: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl SinkWatch {
    /// Follows the nodes whose name starts with `prefix`. `on_change(name,
    /// run)` is called from the watcher's thread on every change, and with
    /// [`NodeRun::Gone`] for each node when PipeWire goes away. `remote`:
    /// the PipeWire remote (`None`: `$PIPEWIRE_REMOTE` or the default).
    pub fn start(
        prefix: &str,
        remote: Option<String>,
        on_change: impl Fn(&str, NodeRun) + Send + 'static,
    ) -> Result<Self, Error> {
        let (cmd, rx) = pw::channel::channel();
        let prefix = prefix.to_owned();
        let thread = std::thread::Builder::new()
            .name("pods-sinks".into())
            .spawn(move || run(&prefix, remote, Rc::new(on_change), rx))?;
        Ok(Self {
            cmd,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = self.cmd.send(());
            let _ = t.join();
        }
    }
}

impl Drop for SinkWatch {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(prefix: &str, remote: Option<String>, on_change: OnChange, rx: pw::channel::Receiver<()>) {
    pw::init();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("sink watcher: no main loop: {e}");
            return;
        }
    };
    let stop = Rc::new(Cell::new(false));
    let _rx = rx.attach(mainloop.loop_(), {
        let (stop, ml) = (stop.clone(), mainloop.clone());
        move |()| {
            stop.set(true);
            ml.quit();
        }
    });
    let mut warned = false;
    while !stop.get() {
        match session(&mainloop, prefix, remote.as_deref(), &on_change, &stop) {
            Ok(()) => warned = false,
            Err(e) if !warned => {
                tracing::info!("sink watcher: {e}; trying again every {}s", RETRY.as_secs());
                warned = true;
            }
            Err(_) => {}
        }
        if stop.get() {
            break;
        }
        let timer = mainloop.loop_().add_timer({
            let ml = mainloop.clone();
            move |_| ml.quit()
        });
        if timer.update_timer(Some(RETRY), None).into_result().is_err() {
            std::thread::sleep(RETRY);
            continue;
        }
        mainloop.run();
    }
}

/// One connection to PipeWire: runs until it breaks or `stop` is set.
fn session(
    mainloop: &pw::main_loop::MainLoopRc,
    prefix: &str,
    remote: Option<&str>,
    on_change: &OnChange,
    stop: &Rc<Cell<bool>>,
) -> Result<(), Error> {
    let context = pw::context::ContextRc::new(mainloop, None)?;
    let props = remote.map(|r| {
        let mut p = pw::properties::PropertiesBox::new();
        p.insert("remote.name", r);
        p
    });
    let core = context.connect_rc(props)?;
    let registry = core.get_registry_rc()?;
    let followed: Rc<RefCell<HashMap<u32, Followed>>> = Rc::default();
    let lost = Rc::new(Cell::new(false));

    let _core_listener = core
        .add_listener_local()
        .error({
            let (lost, ml) = (lost.clone(), mainloop.clone());
            move |id, _seq, res, msg| {
                if id == pw::core::PW_ID_CORE && res == -libc::EPIPE {
                    tracing::info!("sink watcher: PipeWire connection lost ({msg})");
                    lost.set(true);
                    ml.quit();
                }
            }
        })
        .register();

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (registry, followed, on_change) =
                (registry.clone(), followed.clone(), on_change.clone());
            let prefix = prefix.to_owned();
            move |g| {
                if g.type_ != ObjectType::Node {
                    return;
                }
                let Some(name) = g.props.and_then(|p| p.get("node.name")) else {
                    return;
                };
                if !name.starts_with(&prefix) {
                    return;
                }
                let node: pw::node::Node = match registry.bind(g) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!("sink watcher: cannot bind {name}: {e}");
                        return;
                    }
                };
                let id = g.id;
                let listener = node
                    .add_listener_local()
                    .info({
                        let (followed, on_change) = (Rc::downgrade(&followed), on_change.clone());
                        move |info| {
                            let run = match info.state() {
                                pw::node::NodeState::Running => NodeRun::Running,
                                _ => NodeRun::Stopped,
                            };
                            let Some(followed) = followed.upgrade() else {
                                return;
                            };
                            let name = {
                                let map = followed.borrow();
                                let Some(f) = map.get(&id) else { return };
                                if f.run.replace(run) == run {
                                    return;
                                }
                                f.name.clone()
                            };
                            on_change(&name, run);
                        }
                    })
                    .register();
                followed.borrow_mut().insert(
                    id,
                    Followed {
                        name: name.to_owned(),
                        run: Cell::new(NodeRun::Gone),
                        _node: node,
                        _listener: listener,
                    },
                );
            }
        })
        .global_remove({
            let (followed, on_change) = (followed.clone(), on_change.clone());
            move |id| {
                let gone = followed.borrow_mut().remove(&id);
                if let Some(f) = gone
                    && f.run.get() != NodeRun::Gone
                {
                    on_change(&f.name, NodeRun::Gone);
                }
            }
        })
        .register();

    while !lost.get() && !stop.get() {
        mainloop.run();
    }
    let left: Vec<Followed> = followed.borrow_mut().drain().map(|(_, f)| f).collect();
    for f in left {
        if f.run.get() != NodeRun::Gone {
            on_change(&f.name, NodeRun::Gone);
        }
    }
    if lost.get() {
        Err(Error::PipeWire("connection lost".into()))
    } else {
        Ok(())
    }
}
