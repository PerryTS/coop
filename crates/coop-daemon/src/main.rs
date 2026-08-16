//! coop — the Coop supervisor daemon.
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
mod artifacts;
mod cdn;
mod cgroup;
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
use clap::{Parser, Subcommand};
use coop_app_host::{
    initialize_runtime_libraries_with_verification,
    queue_store::{initialize_queue_gateway, QueueStore},
    ProviderVerification,
};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Debug, Parser)]
#[command(name = "coop", about = "Coop supervisor daemon")]
struct Cli {
    /// Path to runtime.toml. Defaults to ./var/coop/runtime.toml.
    #[arg(long, env = "COOP_CONFIG", global = true)]
    config: Option<PathBuf>,
    /// Prepared cgroup-v2 membership file used by controlled benchmarks or an
    /// external launcher. Production service managers may attach the daemon
    /// themselves. Attachment happens before provider loading.
    #[arg(long, env = "COOP_SELF_CGROUP_PROCS", global = true, hide = true)]
    self_cgroup_procs: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Compile and verify immutable packages without activating them.
    Build {
        /// Deployment names. Omit only when --all is used.
        deployments: Vec<String>,
        /// Build every deployment directory in lexical order.
        #[arg(long, conflicts_with = "deployments")]
        all: bool,
    },
    /// Internal parent-bound wrapper for Perry compiler processes. The
    /// supervisor launches this in a private process group so a daemon crash
    /// cannot leave the compiler or any of its descendants running.
    #[cfg(unix)]
    #[command(hide = true, name = "compiler-guard")]
    CompilerGuard {
        #[arg(long)]
        parent_pid: u32,
        #[arg(
            required = true,
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<std::ffi::OsString>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,coop_daemon=debug")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    #[cfg(unix)]
    if let Some(CliCommand::CompilerGuard {
        parent_pid,
        command,
    }) = &cli.command
    {
        return run_compiler_guard(*parent_pid, command);
    }
    if let Some(cgroup_procs) = cli.self_cgroup_procs.as_deref() {
        attach_self_to_cgroup(cgroup_procs)?;
    }
    let config_path = cli
        .config
        .unwrap_or_else(|| PathBuf::from("var/coop/runtime.toml"));

    info!(config = %config_path.display(), "coop starting");
    let runtime_cfg = Arc::new(RuntimeConfig::load(&config_path)?);
    initialize_runtime_libraries_with_verification(
        &runtime_cfg.paths.perry_runtime_library,
        &runtime_cfg.paths.perry_stdlib_library,
        ProviderVerification::parse(runtime_cfg.execution.provider_verification.as_str())?,
    )?;

    // Validate and log TLS configuration.
    tls::validate_tls_config(&runtime_cfg)?;
    info!(tls = %tls::describe_tls_config(&runtime_cfg), "TLS configuration");

    // Initialize Prometheus metrics.
    metrics::init();

    // Create all the paths so the supervisor can write to them.
    ensure_dirs(&runtime_cfg)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("coop-daemon")
        .build()?;

    runtime.block_on(async move {
        // Explicit prebuild never needs the database. Serving mode proves the
        // durable queue schema before any queue-backed deployment is loaded.
        let queue_store = if cli.command.is_none() {
            match &runtime_cfg.postgres {
                Some(postgres) => Some(Arc::new(
                    QueueStore::connect(&postgres.url, postgres.max_connections)
                        .await
                        .context("initializing durable queue service")?,
                )),
                None => None,
            }
        } else {
            None
        };
        if let Some(store) = &queue_store {
            initialize_queue_gateway(store.clone(), tokio::runtime::Handle::current())
                .context("installing application queue enqueue gateway")?;
        }
        let supervisor = Arc::new(DeploymentSupervisor::with_queue_store(
            runtime_cfg.clone(),
            queue_store,
        ));

        if let Some(CliCommand::Build { deployments, all }) = cli.command {
            let names = if all {
                deployment_names(&runtime_cfg.paths.deployments_dir)?
            } else {
                if deployments.is_empty() {
                    return Err(anyhow::anyhow!(
                        "coop build requires at least one deployment or --all"
                    ));
                }
                deployments
            };
            for name in names {
                let started = std::time::Instant::now();
                let library = supervisor.prebuild_deployment(&name).await?;
                info!(
                    deployment = name,
                    library = %library.display(),
                    elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0,
                    "immutable application package prebuilt"
                );
            }
            return Ok::<(), anyhow::Error>(());
        }

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
        if runtime_cfg.execution.watch_deployments {
            match watcher::start(
                runtime_cfg.paths.deployments_dir.clone(),
                supervisor.clone(),
            ) {
                Ok(_handle) => {}
                Err(e) => {
                    error!(error = ?e, "failed to start deployment watcher");
                }
            }
        } else {
            info!("deployment watcher disabled; reloads require explicit orchestration");
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

        info!("coop stopped");
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

/// Keep the compiler in a private process group and watch the exact daemon
/// parent. `kill_on_drop` covers ordinary cancellation, while this wrapper
/// covers uncatchable daemon termination where Rust destructors never run.
#[cfg(unix)]
fn run_compiler_guard(parent_pid: u32, command: &[std::ffi::OsString]) -> Result<()> {
    use std::os::unix::process::ExitStatusExt;

    if parent_pid == 0 || parent_pid > libc::pid_t::MAX as u32 {
        anyhow::bail!("compiler guard parent PID must fit a positive pid_t");
    }
    let actual_parent = unsafe { libc::getppid() };
    if actual_parent != parent_pid as libc::pid_t {
        anyhow::bail!(
            "compiler guard parent changed before initialization: expected {parent_pid}, current {actual_parent}"
        );
    }
    let Some(program) = command.first() else {
        anyhow::bail!("compiler guard requires a command");
    };

    // The outer Tokio command also requests a fresh process group. Repeating
    // the operation here makes the invariant hold when the hidden command is
    // exercised directly by its OS-process regression test.
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("creating compiler guard process group");
    }

    let mut child = std::process::Command::new(program)
        .args(&command[1..])
        .spawn()
        .with_context(|| format!("compiler guard spawning {program:?}"))?;

    loop {
        if unsafe { libc::getppid() } != parent_pid as libc::pid_t {
            // The guard and every compiler descendant inherit this private
            // process group. Killing group zero cannot target the daemon or
            // the test runner because the guard is its group leader.
            unsafe {
                libc::kill(0, libc::SIGKILL);
            }
            std::process::exit(125);
        }
        if let Some(status) = child.try_wait().context("waiting for guarded compiler")? {
            if status.success() {
                return Ok(());
            }
            std::process::exit(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn attach_self_to_cgroup(cgroup_procs: &std::path::Path) -> Result<()> {
    if cgroup_procs.file_name().and_then(|name| name.to_str()) != Some("cgroup.procs") {
        anyhow::bail!("daemon cgroup membership path must name cgroup.procs");
    }
    std::fs::write(cgroup_procs, std::process::id().to_string()).with_context(|| {
        format!(
            "attaching daemon {} to cgroup through {}",
            std::process::id(),
            cgroup_procs.display()
        )
    })?;
    Ok(())
}

fn deployment_names(directory: &std::path::Path) -> Result<Vec<String>> {
    let mut names = std::fs::read_dir(directory)
        .with_context(|| format!("reading deployments directory {}", directory.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry
                .file_type()
                .ok()?
                .is_dir()
                .then(|| entry.file_name().to_str().map(str::to_string))?
        })
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
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
        std::fs::create_dir_all(dir).with_context(|| format!("creating directory {:?}", dir))?;
    }
    if let Some(parent) = cfg.paths.state_db.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state db parent {:?}", parent))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::attach_self_to_cgroup;

    #[test]
    fn benchmark_daemon_attaches_only_through_cgroup_procs() {
        let temp = tempfile::tempdir().unwrap();
        let procs = temp.path().join("cgroup.procs");
        attach_self_to_cgroup(&procs).unwrap();
        assert_eq!(
            std::fs::read_to_string(procs).unwrap(),
            std::process::id().to_string()
        );
        assert!(attach_self_to_cgroup(&temp.path().join("other")).is_err());
    }
}
