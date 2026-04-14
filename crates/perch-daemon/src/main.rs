//! perch — the Perch supervisor daemon.
//!
//! Checkpoint 2 scope: bootstrap + runtime.toml parse + axum HTTP listener
//! + host-based routing + tower-http ServeDir + notify-based deployment
//! watcher + drain-and-replace worker lifecycle. No TLS yet (Checkpoint
//! 3), no Bunny CDN (Checkpoint 4), no admin UI / metrics / schema
//! provisioning (Checkpoint 5).
//!
//! The bare minimum flow:
//!
//! 1. Parse CLI + load runtime.toml
//! 2. Build DeploymentSupervisor
//! 3. Initial scan of deployments_dir → spawns workers, builds router
//! 4. Start axum listener
//! 5. Wait for SIGTERM, drain everything, exit
//!
//! The notify-based file watcher isn't wired in yet because it adds a
//! lot of surface area (debouncing, per-file change detection, reload
//! triggers) that we don't need for the Checkpoint 2 smoke test. It
//! lands as a follow-up within the same checkpoint once the basic
//! listener + worker spawn flow is validated.

mod admin;
mod cdn;
mod config;
mod deployments;
mod listener;
mod metrics;
mod proxy_headers;
mod router;
mod schema;
mod signals;
mod tls;
mod watcher;
mod worker_client;

use crate::config::RuntimeConfig;
use crate::deployments::DeploymentSupervisor;
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Debug, Parser)]
#[command(name = "perch", about = "Perch supervisor daemon")]
struct Cli {
    /// Path to runtime.toml. Defaults to ./var/perch/runtime.toml.
    #[arg(long, env = "PERCH_CONFIG")]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,perch_daemon=debug")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config_path = cli
        .config
        .unwrap_or_else(|| PathBuf::from("var/perch/runtime.toml"));

    info!(config = %config_path.display(), "perch starting");
    let runtime_cfg = Arc::new(RuntimeConfig::load(&config_path)?);

    // Validate and log TLS configuration.
    tls::validate_tls_config(&runtime_cfg)?;
    info!(tls = %tls::describe_tls_config(&runtime_cfg), "TLS configuration");

    // Initialize Prometheus metrics.
    metrics::init();

    // Create all the paths so the supervisor can write to them.
    ensure_dirs(&runtime_cfg)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("perch-daemon")
        .build()?;

    runtime.block_on(async move {
        let supervisor = Arc::new(DeploymentSupervisor::new(runtime_cfg.clone()));

        if let Err(e) = supervisor.initial_scan().await {
            error!(error = ?e, "initial deployment scan failed");
            // Keep running — an empty router returns 404 for everything,
            // which is still useful because the admin UI / metrics can
            // diagnose the problem.
        }

        // Start the deployment filesystem watcher. The returned join
        // handle lives for the life of the daemon; we don't wait on it
        // explicitly — it'll get cancelled when the tokio runtime
        // shuts down.
        match watcher::start(runtime_cfg.paths.deployments_dir.clone(), supervisor.clone()) {
            Ok(_handle) => {}
            Err(e) => {
                error!(error = ?e, "failed to start deployment watcher");
            }
        }

        // Start the RSS watchdog: periodically check each worker's
        // memory usage and restart any that exceed their configured
        // max_worker_rss_mb limit.
        let _watchdog = supervisor.clone().spawn_rss_watchdog();

        let shutdown_signal = signals::wait_for_shutdown();
        let serve_fut = listener::serve(runtime_cfg.clone(), supervisor.clone(), shutdown_signal);

        if let Err(e) = serve_fut.await {
            error!(error = ?e, "http listener exited with error");
        }

        info!("perch stopped");
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

fn ensure_dirs(cfg: &RuntimeConfig) -> Result<()> {
    let dirs = [
        &cfg.paths.deployments_dir,
        &cfg.paths.compiled_dir,
        &cfg.paths.sockets_dir,
        &cfg.paths.storage_dir,
        &cfg.paths.logs_dir,
        &cfg.paths.acme_cache_dir,
    ];
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating directory {:?}", dir))?;
    }
    if let Some(parent) = cfg.paths.state_db.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state db parent {:?}", parent))?;
        }
    }
    Ok(())
}
