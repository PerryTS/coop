//! perch-worker — per-deployment host process.
//!
//! Spawned by perch-daemon with `--deployment <name>`. Responsibilities:
//!
//! 1. Load the separately packaged Perry runtime and stdlib providers.
//! 2. Preload the deployment's compiled `.dylib` on a dedicated Perry
//!    executor thread and call its required `handle(Buffer): Buffer` export.
//!
//! 3. Listen on `/var/lib/perch/sockets/<deployment>.sock` for framed
//!    requests from perch-daemon. Each message is length-prefixed JSON
//!    (see perch_host_abi).
//!
//! 4. Translate each socket `Dispatch` request to the compact application ABI
//!    and frame its `DeploymentResponse` back over the socket.

mod listener;
mod shard_listener;

use perch_app_host::{host, initialize_runtime_libraries_with_verification, ProviderVerification};

use anyhow::Context;
use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

#[derive(Debug, Parser)]
#[command(
    name = "perch-worker",
    about = "Dedicated or multi-application Perry host process"
)]
struct Cli {
    /// Deployment name. The worker listens on the assigned generation socket
    /// and loads the exact immutable library supplied with `--dylib`.
    #[arg(
        long,
        required_unless_present = "shard_id",
        conflicts_with = "shard_id"
    )]
    deployment: Option<String>,

    /// Run as a dynamic multi-application shard instead of a dedicated
    /// deployment worker.
    #[arg(long, conflicts_with = "deployment")]
    shard_id: Option<String>,

    /// Maximum distinct logical deployments resident in a shard. Replacement
    /// generations of the same deployment may overlap while draining.
    #[arg(long, default_value_t = 25, requires = "shard_id")]
    shard_max_apps: usize,

    /// Exact content-addressed application library. Dedicated workers require
    /// this explicitly; there is no mutable compiled-directory fallback.
    #[arg(long, required_unless_present = "shard_id")]
    dylib: Option<PathBuf>,

    /// Directory where the worker's Unix socket is created. Defaults to
    /// /var/lib/perch/sockets.
    #[arg(long, default_value = "/var/lib/perch/sockets")]
    sockets_dir: PathBuf,

    /// Exact generation-owned socket path assigned by the daemon. The
    /// directory-derived legacy name is retained only for manual invocation.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Override the Perry module name for symbol lookup. By default the
    /// module name is derived from the dylib filename stem. When the
    /// daemon compiles `handlers/contact.ts` and names the output
    /// `landing.dylib`, the symbol inside is `perry_fn_contact_ts__handle`
    /// (based on the TS filename), not `perry_fn_landing__handle`. Pass
    /// `--module-name contact_ts` to tell the worker the correct name.
    #[arg(long)]
    module_name: Option<String>,

    /// Separately packaged Perry runtime shared library.
    #[arg(long, env = "PERCH_PERRY_RUNTIME_LIBRARY")]
    perry_runtime: PathBuf,

    /// Separately packaged Perry stdlib shared library.
    #[arg(long, env = "PERCH_PERRY_STDLIB_LIBRARY")]
    perry_stdlib: PathBuf,

    /// Integrity boundary for the separately packaged provider pair. The
    /// immutable mode is accepted only when the running service cannot write
    /// any provider file or canonical parent directory.
    #[arg(
        long,
        default_value = "full_hash",
        value_parser = ["full_hash", "root_owned_immutable"]
    )]
    provider_verification: String,

    /// Stack reservation for the thread-affine Perry executor.
    #[arg(
        long,
        default_value_t = host::DEFAULT_EXECUTOR_STACK_SIZE_BYTES
    )]
    executor_stack_size_bytes: usize,

    /// Maximum application commands waiting behind the running command.
    #[arg(long, default_value_t = host::DEFAULT_COMMAND_QUEUE_CAPACITY)]
    command_queue_capacity: usize,

    /// Completed invocations between Perry arena-growth checks. Zero disables
    /// automatic host-boundary full-GC pacing.
    #[arg(long, default_value_t = host::DEFAULT_GC_RECLAIM_CHECK_INTERVAL)]
    gc_reclaim_check_interval: usize,

    /// Minimum uncollected arena growth before a precise host-boundary full GC.
    #[arg(long, default_value_t = host::DEFAULT_GC_RECLAIM_GROWTH_BYTES)]
    gc_reclaim_growth_bytes: u64,

    /// Opaque deployment context assigned by the daemon for host callbacks.
    #[arg(long, default_value_t = 0)]
    deployment_context_id: u64,

    /// Box-wide Redis URL from `runtime.toml` `[redis] url`. Supplied through
    /// the environment by the daemon so a password does not appear in process
    /// arguments. Absent leaves `@perch/runtime`'s `kv` unconfigured, and `kv`
    /// then throws on use rather than reaching a default server.
    #[arg(long, env = "PERCH_REDIS_URL")]
    redis_url: Option<String>,

    /// Box-wide object-storage root from `runtime.toml` `[paths] storage_dir`.
    /// The worker derives and creates this deployment's own subdirectory and
    /// exports it as `PERCH_STORAGE_DIR`; the application never sees the root.
    #[arg(long, env = "PERCH_STORAGE_ROOT")]
    storage_root: Option<PathBuf>,

    /// Host-owned queue database URL. Supplied through the environment by the
    /// daemon so it does not appear in process arguments.
    #[arg(long, env = "PERCH_QUEUE_POSTGRES_URL")]
    queue_postgres_url: Option<String>,

    #[arg(
        long,
        env = "PERCH_QUEUE_POSTGRES_MAX_CONNECTIONS",
        default_value_t = 2
    )]
    queue_postgres_max_connections: u32,

    #[arg(long, env = "PERCH_QUEUE_POLICIES_JSON")]
    queue_policies_json: Option<String>,

    /// Prepared cgroup-v2 membership file. The daemon creates and limits the
    /// generation before spawn; the worker attaches itself before loading any
    /// Perry or application code.
    #[arg(long, hide = true)]
    cgroup_procs: Option<PathBuf>,

    /// Daemon PID that owns this worker generation. Daemon-spawned workers
    /// terminate if they are reparented, preventing orphan application hosts
    /// after a daemon crash or SIGKILL. Manual workers may omit this.
    #[arg(long, hide = true)]
    parent_pid: Option<u32>,
}

#[derive(Deserialize)]
struct WorkerQueuePolicies {
    deployment: String,
    queues: Vec<WorkerQueuePolicy>,
}

#[derive(Deserialize)]
struct WorkerQueuePolicy {
    name: String,
    max_payload_bytes: usize,
    max_attempts: u32,
    max_delay_ms: u64,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .json()
        .init();

    let cli = Cli::parse();
    if let Some(parent_pid) = cli.parent_pid {
        bind_to_parent_process(parent_pid)?;
    }
    if let Some(cgroup_procs) = cli.cgroup_procs.as_deref() {
        attach_to_cgroup(cgroup_procs)?;
    }
    initialize_runtime_libraries_with_verification(
        &cli.perry_runtime,
        &cli.perry_stdlib,
        ProviderVerification::parse(&cli.provider_verification)?,
    )?;
    let worker_label = cli
        .shard_id
        .as_deref()
        .or(cli.deployment.as_deref())
        .expect("clap requires deployment or shard ID")
        .to_string();
    info!(
        worker = %worker_label,
        mode = if cli.shard_id.is_some() { "shard" } else { "dedicated" },
        cgroup_enforced = cli.cgroup_procs.is_some(),
        "perch-worker starting"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(format!("perch-worker-{worker_label}"))
        .build()?;

    runtime.block_on(async move {
        if let Some(url) = cli.queue_postgres_url.as_deref() {
            let store = Arc::new(
                perch_app_host::queue_store::QueueStore::connect(
                    url,
                    cli.queue_postgres_max_connections,
                )
                .await?,
            );
            perch_app_host::queue_store::initialize_queue_gateway(
                store,
                tokio::runtime::Handle::current(),
            )?;
        }

        if let Some(shard_id) = cli.shard_id.as_deref() {
            if cli.deployment_context_id != 0 || cli.queue_policies_json.is_some() {
                return Err(anyhow::anyhow!(
                    "shard queue contexts are supplied per loaded deployment"
                ));
            }
            // `kv` and `storage` are scoped by process environment, and one
            // shard process hosts many deployments. Accepting these here would
            // give every resident application the same key prefix and the same
            // object-storage directory — a data-isolation break, not a
            // degraded feature. Refuse rather than export something wrong.
            if cli.redis_url.is_some() || cli.storage_root.is_some() {
                return Err(anyhow::anyhow!(
                    "kv and storage are per-deployment capabilities and cannot be \
                     configured on a shared shard process; set isolation.class = \
                     \"dedicated\" for deployments that use them"
                ));
            }
            let socket_path = cli
                .socket
                .clone()
                .unwrap_or_else(|| cli.sockets_dir.join(format!("shard-{shard_id}.sock")));
            if let Some(parent) = socket_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let listener =
                shard_listener::ShardListener::bind(shard_id, &socket_path, cli.shard_max_apps)?;
            info!(
                shard = shard_id,
                socket = %socket_path.display(),
                max_apps = cli.shard_max_apps,
                "worker shard ready"
            );
            return listener.serve().await;
        }

        let deployment = cli
            .deployment
            .as_deref()
            .expect("dedicated worker requires deployment");
        if cli.deployment_context_id != 0 {
            if cli.queue_postgres_url.is_none() {
                return Err(anyhow::anyhow!(
                    "deployment context requires PERCH_QUEUE_POSTGRES_URL"
                ));
            }
            let raw_policies = cli.queue_policies_json.as_deref().ok_or_else(|| {
                anyhow::anyhow!("deployment context requires PERCH_QUEUE_POLICIES_JSON")
            })?;
            let policies: WorkerQueuePolicies = serde_json::from_str(raw_policies)?;
            if policies.deployment != deployment {
                return Err(anyhow::anyhow!(
                    "worker queue policy deployment mismatch: expected {:?}, got {:?}",
                    deployment,
                    policies.deployment
                ));
            }
            perch_app_host::queue_store::register_enqueue_context(
                cli.deployment_context_id,
                deployment.to_string(),
                policies
                    .queues
                    .into_iter()
                    .map(|queue| {
                        (
                            queue.name,
                            perch_app_host::queue_store::EnqueuePolicy {
                                max_payload_bytes: queue.max_payload_bytes,
                                max_attempts: queue.max_attempts,
                                max_delay: Duration::from_millis(queue.max_delay_ms),
                            },
                        )
                    })
                    .collect::<HashMap<_, _>>(),
            )?;
        }

        // `@perch/runtime`'s kv and storage capture their configuration in
        // module-level `const`s, which Perry evaluates inside
        // `perry_module_init()` while `DeploymentHost::load*` runs. Export the
        // deployment environment first or the application reads an empty one.
        let exported = perch_app_host::export_deployment_environment(
            deployment,
            &perch_app_host::DeploymentServices {
                redis_url: cli.redis_url.as_deref(),
                storage_root: cli.storage_root.as_deref(),
            },
        )?;
        info!(
            deployment,
            exported = exported.len(),
            kv_configured = cli.redis_url.is_some(),
            storage_configured = cli.storage_root.is_some(),
            "exported deployment capability environment"
        );

        let dylib_path = cli
            .dylib
            .clone()
            .expect("clap requires --dylib for dedicated workers");
        if !dylib_path.exists() {
            error!(dylib = %dylib_path.display(), "deployment dylib not found");
            return Err(anyhow::anyhow!(
                "deployment dylib does not exist: {}",
                dylib_path.display()
            ));
        }

        let host = host::DeploymentHost::load_with_options(
            deployment,
            &dylib_path,
            cli.module_name.as_deref(),
            host::DeploymentHostOptions {
                executor_stack_size_bytes: cli.executor_stack_size_bytes,
                command_queue_capacity: cli.command_queue_capacity,
                gc_reclaim_check_interval: cli.gc_reclaim_check_interval,
                gc_reclaim_growth_bytes: cli.gc_reclaim_growth_bytes,
                deployment_context_id: cli.deployment_context_id,
            },
        )?;

        let socket_path = cli
            .socket
            .clone()
            .unwrap_or_else(|| cli.sockets_dir.join(format!("{deployment}.sock")));
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = listener::Listener::bind(deployment, &socket_path, host)?;
        info!(
            socket = %socket_path.display(),
            "worker ready"
        );

        let result = listener.serve().await;
        perch_app_host::queue_store::unregister_enqueue_context(cli.deployment_context_id);
        result
    })?;

    Ok(())
}

#[cfg(unix)]
fn parent_process_matches(expected_parent_pid: u32) -> bool {
    expected_parent_pid != 0 && unsafe { libc::getppid() } == expected_parent_pid as libc::pid_t
}

/// Bind a daemon-spawned worker to its exact parent process. Linux gets a
/// kernel parent-death signal for immediate cleanup; the portable Unix watcher
/// covers macOS and also verifies the race where the parent exits before the
/// worker finishes initializing.
#[cfg(unix)]
fn bind_to_parent_process(expected_parent_pid: u32) -> anyhow::Result<()> {
    if expected_parent_pid == 0 || expected_parent_pid > libc::pid_t::MAX as u32 {
        anyhow::bail!("worker parent PID must fit a positive pid_t");
    }

    #[cfg(target_os = "linux")]
    {
        let result = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("installing worker parent-death signal");
        }
    }

    if !parent_process_matches(expected_parent_pid) {
        anyhow::bail!(
            "worker parent changed before initialization: expected {expected_parent_pid}, current {}",
            unsafe { libc::getppid() }
        );
    }
    std::thread::Builder::new()
        .name("perch-parent-watch".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(100));
            if !parent_process_matches(expected_parent_pid) {
                // The daemon cannot supervise or reclaim this process
                // anymore. Avoid running application destructors in an
                // uncertain post-parent state.
                unsafe { libc::_exit(1) };
            }
        })
        .context("spawning worker parent watchdog")?;
    Ok(())
}

#[cfg(not(unix))]
fn bind_to_parent_process(_expected_parent_pid: u32) -> anyhow::Result<()> {
    anyhow::bail!("daemon parent binding is unsupported on this platform")
}

fn attach_to_cgroup(cgroup_procs: &std::path::Path) -> anyhow::Result<()> {
    if cgroup_procs.file_name().and_then(|name| name.to_str()) != Some("cgroup.procs") {
        return Err(anyhow::anyhow!(
            "worker cgroup membership path must name cgroup.procs"
        ));
    }
    std::fs::write(cgroup_procs, std::process::id().to_string()).map_err(|error| {
        anyhow::anyhow!(
            "attaching worker {} to cgroup through {}: {error}",
            std::process::id(),
            cgroup_procs.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{attach_to_cgroup, Cli};
    use clap::{CommandFactory, Parser};

    #[test]
    fn attaches_before_runtime_start_only_through_cgroup_procs() {
        let temp = tempfile::tempdir().unwrap();
        let procs = temp.path().join("cgroup.procs");
        attach_to_cgroup(&procs).unwrap();
        assert_eq!(
            std::fs::read_to_string(procs).unwrap(),
            std::process::id().to_string()
        );
        assert!(attach_to_cgroup(&temp.path().join("other")).is_err());
    }

    #[test]
    fn dedicated_worker_requires_an_exact_library_but_shard_does_not() {
        assert!(Cli::try_parse_from([
            "perch-worker",
            "--deployment",
            "alpha",
            "--perry-runtime",
            "/providers/runtime.so",
            "--perry-stdlib",
            "/providers/stdlib.so",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "perch-worker",
            "--deployment",
            "alpha",
            "--dylib",
            "/immutable/alpha/app.so",
            "--perry-runtime",
            "/providers/runtime.so",
            "--perry-stdlib",
            "/providers/stdlib.so",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "perch-worker",
            "--shard-id",
            "0",
            "--perry-runtime",
            "/providers/runtime.so",
            "--perry-stdlib",
            "/providers/stdlib.so",
        ])
        .is_ok());
    }

    /// The daemon passes these two through the environment rather than argv so
    /// a Redis password stays out of the process table (see
    /// `deployments.rs::spawn_dedicated_worker`). That only works if the names
    /// match exactly on both sides, and a mismatch is silent: the worker would
    /// simply see `None` and start a deployment with kv and storage
    /// unconfigured. Pin the names here so a rename has to break a test.
    #[test]
    fn capability_configuration_is_reachable_by_flag_and_by_environment() {
        let base = [
            "perch-worker",
            "--deployment",
            "alpha",
            "--dylib",
            "/immutable/alpha/app.so",
            "--perry-runtime",
            "/providers/runtime.so",
            "--perry-stdlib",
            "/providers/stdlib.so",
        ];

        let by_flag = Cli::try_parse_from(base.iter().copied().chain([
            "--redis-url",
            "redis://127.0.0.1:6379",
            "--storage-root",
            "/var/lib/perch/storage",
        ]))
        .expect("explicit flags parse");
        assert_eq!(by_flag.redis_url.as_deref(), Some("redis://127.0.0.1:6379"));
        assert_eq!(
            by_flag.storage_root.as_deref(),
            Some(std::path::Path::new("/var/lib/perch/storage"))
        );

        // Absent means "not configured", not "use a default". kv and storage
        // throw in that state rather than reaching a default server or a
        // relative directory shared with every other deployment.
        let unset = Cli::try_parse_from(base).expect("capabilities are optional");
        assert!(unset.redis_url.is_none() && unset.storage_root.is_none());

        assert_eq!(
            Cli::command()
                .get_arguments()
                .filter_map(|arg| arg.get_env().and_then(|env| env.to_str()))
                .filter(|env| *env == "PERCH_REDIS_URL" || *env == "PERCH_STORAGE_ROOT")
                .count(),
            2,
            "the daemon sets PERCH_REDIS_URL and PERCH_STORAGE_ROOT; both must \
             still be declared here or the worker silently sees no configuration"
        );
    }

    #[cfg(unix)]
    #[test]
    fn parent_identity_check_uses_the_exact_os_parent() {
        let parent = unsafe { libc::getppid() } as u32;
        assert!(super::parent_process_matches(parent));
        assert!(!super::parent_process_matches(0));
        assert!(!super::parent_process_matches(parent.saturating_add(1)));
    }
}
