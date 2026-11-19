// SPDX-License-Identifier: GPL-3.0-or-later
//! `podsd`: owns the AAP link to the AirPods and serves
//! `io.github.d101c.Pods1` on the session bus. Started by systemd
//! (`pods-engine.service`) on the first call of the extension.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use pods_bt::L2capConnector;
use podsd::config::{ConfigStore, default_path};
use podsd::{Engine, EngineDeps, PipeWireRenderer, PipeWireSinks, Timing};

fn main() -> std::process::ExitCode {
    if std::env::args().any(|a| a == "--version") {
        println!("podsd {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .without_time()
        .init();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "no runtime");
            return std::process::ExitCode::FAILURE;
        }
    };
    match rt.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "podsd stops");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> zbus::Result<()> {
    let session = zbus::Connection::session().await?;
    let system = zbus::Connection::system().await?;
    let config = Arc::new(ConfigStore::load(default_path()));
    let mut timing = Timing::default();
    if let Some(secs) = config.get().watchdog_secs {
        timing.watchdog = Some(Duration::from_secs(secs.max(10)));
    }
    info!(config = %config.path().display(), "podsd {}", env!("CARGO_PKG_VERSION"));
    let engine = Engine::start(EngineDeps {
        session,
        system,
        connector: Arc::new(L2capConnector::default()),
        media_host: None,
        keys: None,
        config,
        timing,
        proc_root: Some(PathBuf::from("/proc")),
        request_name: true,
        renderer: Arc::new(PipeWireRenderer),
        sinks: Arc::new(PipeWireSinks::start()),
    })
    .await?;

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    info!("stopping");
    // Sensors off on the buds, default output given back.
    if tokio::time::timeout(Duration::from_secs(8), engine.shutdown())
        .await
        .is_err()
    {
        error!("spatial audio did not stop in time");
    }
    Ok(())
}
