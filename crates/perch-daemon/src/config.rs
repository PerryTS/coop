//! TOML config shapes for `runtime.toml` (box-wide) and `perch.toml`
//! (per-deployment).
//!
//! The two types are separate because they have different lifetimes:
//! `RuntimeConfig` is loaded once at daemon startup and is constant for
//! the process lifetime. `DeploymentConfig` is loaded every time the
//! `notify` watcher fires for a changed deployment and fed into the
//! deployment lifecycle state machine.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ============================================================================
// runtime.toml — box-wide configuration
// ============================================================================

/// Top-level box configuration. Lives at `/var/lib/perch/runtime.toml` by
/// default. Loaded once at daemon startup via `RuntimeConfig::load`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub execution: ExecutionConfig,

    #[serde(default)]
    pub http: HttpConfig,

    #[serde(default)]
    pub paths: PathsConfig,

    #[serde(default)]
    pub postgres: Option<PostgresConfig>,

    #[serde(default)]
    pub queue_service: QueueServiceConfig,

    #[serde(default)]
    pub redis: Option<RedisConfig>,

    #[serde(default)]
    pub tls: TlsConfig,

    #[serde(default)]
    pub cdn: CdnConfig,

    #[serde(default)]
    pub admin: AdminConfig,

    #[serde(default)]
    pub logs: LogsConfig,
}

/// Durable queue activation policy. Queue handlers require the host-owned
/// PostgreSQL store by default. The opt-out exists for ABI-only tooling and
/// tests that load queue entries directly but never advertise consumption.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueServiceConfig {
    #[serde(default)]
    pub allow_handlers_without_store: bool,
}

/// How compiled application libraries are hosted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionConfig {
    /// `in_process` preloads every library into the daemon and dispatches
    /// directly to a thread-affine Perry executor. `worker` retains the
    /// process-isolated Unix-socket transport.
    #[serde(default)]
    pub mode: ExecutionMode,
    /// Provider integrity boundary. `full_hash` reads and hashes both provider
    /// files on every process start. `root_owned_immutable` skips that repeated
    /// read only for an unprivileged service loading a complete canonical path
    /// that is root-owned and not writable by the service, group, or others.
    #[serde(default)]
    pub provider_verification: PerryProviderVerification,
    /// Watch deployment source directories and trigger reloads after changes.
    /// Disable this for externally orchestrated, API-only deployment flows and
    /// controlled benchmarks where each activation must have one trigger.
    #[serde(default = "default_watch_deployments")]
    pub watch_deployments: bool,
    /// Maximum number of applications preloaded and warmed concurrently during
    /// the initial scan. Compilation has its own bound below.
    #[serde(default = "default_preload_concurrency")]
    pub preload_concurrency: usize,
    /// Perry compilation is memory-intensive. Keep it separately bounded so
    /// parallel preload cannot launch several multi-GiB builds by accident.
    #[serde(default = "default_compile_concurrency")]
    pub compile_concurrency: usize,
    /// Kill a compiler process group when a build exceeds this wall time.
    #[serde(default = "default_compile_timeout_seconds")]
    pub compile_timeout_seconds: u64,
    /// Kill a compiler process group when its sampled aggregate RSS exceeds
    /// this many MiB. Zero disables RSS enforcement but not wall enforcement.
    #[serde(default = "default_compile_max_rss_mb")]
    pub compile_max_rss_mb: u64,
    /// Maximum bytes retained from each compiler stdout/stderr stream. Output
    /// beyond the cap is drained and discarded so the compiler cannot block.
    #[serde(default = "default_compile_output_bytes")]
    pub compile_output_bytes: usize,
    /// Maximum number of dependency files dereferenced into one private
    /// compiler snapshot. This bounds hostile or accidentally enormous
    /// `node_modules` trees before Perry is started.
    #[serde(default = "default_compile_dependency_max_files")]
    pub compile_dependency_max_files: usize,
    /// Maximum aggregate dependency bytes dereferenced into one private
    /// compiler snapshot.
    #[serde(default = "default_compile_dependency_max_bytes")]
    pub compile_dependency_max_bytes: u64,
    /// Explicit Perry CPU baseline. Production defaults to `generic` so an
    /// artifact is not silently tied to the build host's instruction set.
    #[serde(default = "default_compile_march")]
    pub compile_march: String,
    /// Maximum number of files copied into one immutable static snapshot.
    #[serde(default = "default_artifact_max_static_files")]
    pub artifact_max_static_files: usize,
    /// Maximum aggregate static bytes copied into one immutable package.
    #[serde(default = "default_artifact_max_static_bytes")]
    pub artifact_max_static_bytes: u64,
    /// Explicit stack reservation for each thread-affine Perry executor.
    #[serde(default = "default_executor_stack_size_bytes")]
    pub executor_stack_size_bytes: usize,
    /// Maximum number of application commands waiting behind the command
    /// currently executing. Full queues reject new work immediately.
    #[serde(default = "default_command_queue_capacity")]
    pub command_queue_capacity: usize,
    /// Check per-application Perry arena growth after this many completed
    /// invocations. Zero disables host-boundary full-GC pacing.
    #[serde(default = "default_gc_reclaim_check_interval")]
    pub gc_reclaim_check_interval: usize,
    /// Minimum growth above the last post-full-GC live baseline before Perry
    /// performs a precise full collection at the executor boundary.
    #[serde(default = "default_gc_reclaim_growth_bytes")]
    pub gc_reclaim_growth_bytes: u64,
    /// Linux cgroup-v2 policy for dedicated workers. In `auto` mode the daemon
    /// uses the RSS watchdog when the configured hierarchy is not delegated;
    /// `required` makes deployment activation fail closed.
    #[serde(default)]
    pub cgroup: WorkerCgroupConfig,
    /// Bounded multi-application process-shard topology. Applications select
    /// it with `[isolation] class = "sharded"` or inherit `mode = "shard"`.
    #[serde(default)]
    pub shards: ShardExecutionConfig,
    /// Minimum number of application package generations retained per
    /// deployment, including the active generation.
    #[serde(default = "default_artifact_retention_count")]
    pub artifact_retention_count: usize,
    /// Preserve inactive application packages at least this many days even
    /// when the count floor has already been satisfied. Zero disables the age
    /// floor while retaining the configured count.
    #[serde(default = "default_artifact_retention_days")]
    pub artifact_retention_days: u32,
    /// Crash-leftover staging directories younger than this are not touched,
    /// so a concurrently running compiler cannot be collected.
    #[serde(default = "default_staging_reconcile_age_seconds")]
    pub staging_reconcile_age_seconds: u64,
}

fn default_preload_concurrency() -> usize {
    if cfg!(target_os = "macos") {
        // dyld serializes image loading internally; competing preload threads
        // add contention and measured slower on the 20-library control.
        1
    } else {
        4
    }
}

fn default_watch_deployments() -> bool {
    true
}

fn default_compile_concurrency() -> usize {
    1
}

fn default_compile_timeout_seconds() -> u64 {
    5 * 60
}

fn default_compile_max_rss_mb() -> u64 {
    4 * 1024
}

fn default_compile_output_bytes() -> usize {
    1024 * 1024
}

fn default_compile_dependency_max_files() -> usize {
    250_000
}

fn default_compile_dependency_max_bytes() -> u64 {
    4 * 1024 * 1024 * 1024
}

fn default_compile_march() -> String {
    "generic".to_string()
}

fn default_artifact_max_static_files() -> usize {
    100_000
}

fn default_artifact_max_static_bytes() -> u64 {
    1024 * 1024 * 1024
}

fn default_executor_stack_size_bytes() -> usize {
    perch_app_host::host::DEFAULT_EXECUTOR_STACK_SIZE_BYTES
}

fn default_command_queue_capacity() -> usize {
    perch_app_host::host::DEFAULT_COMMAND_QUEUE_CAPACITY
}

fn default_gc_reclaim_check_interval() -> usize {
    perch_app_host::host::DEFAULT_GC_RECLAIM_CHECK_INTERVAL
}

fn default_gc_reclaim_growth_bytes() -> u64 {
    perch_app_host::host::DEFAULT_GC_RECLAIM_GROWTH_BYTES
}

fn default_artifact_retention_count() -> usize {
    3
}

fn default_artifact_retention_days() -> u32 {
    30
}

fn default_staging_reconcile_age_seconds() -> u64 {
    60 * 60
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            mode: ExecutionMode::InProcess,
            provider_verification: PerryProviderVerification::FullHash,
            watch_deployments: default_watch_deployments(),
            preload_concurrency: default_preload_concurrency(),
            compile_concurrency: default_compile_concurrency(),
            compile_timeout_seconds: default_compile_timeout_seconds(),
            compile_max_rss_mb: default_compile_max_rss_mb(),
            compile_output_bytes: default_compile_output_bytes(),
            compile_dependency_max_files: default_compile_dependency_max_files(),
            compile_dependency_max_bytes: default_compile_dependency_max_bytes(),
            compile_march: default_compile_march(),
            artifact_max_static_files: default_artifact_max_static_files(),
            artifact_max_static_bytes: default_artifact_max_static_bytes(),
            executor_stack_size_bytes: default_executor_stack_size_bytes(),
            command_queue_capacity: default_command_queue_capacity(),
            gc_reclaim_check_interval: default_gc_reclaim_check_interval(),
            gc_reclaim_growth_bytes: default_gc_reclaim_growth_bytes(),
            cgroup: WorkerCgroupConfig::default(),
            shards: ShardExecutionConfig::default(),
            artifact_retention_count: default_artifact_retention_count(),
            artifact_retention_days: default_artifact_retention_days(),
            staging_reconcile_age_seconds: default_staging_reconcile_age_seconds(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    InProcess,
    Worker,
    Shard,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PerryProviderVerification {
    #[default]
    FullHash,
    RootOwnedImmutable,
}

impl PerryProviderVerification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FullHash => "full_hash",
            Self::RootOwnedImmutable => "root_owned_immutable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerCgroupConfig {
    #[serde(default)]
    pub mode: CgroupMode,
    #[serde(default = "default_cgroup_root")]
    pub root: PathBuf,
}

impl Default for WorkerCgroupConfig {
    fn default() -> Self {
        Self {
            mode: CgroupMode::default(),
            root: default_cgroup_root(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardExecutionConfig {
    /// Number of bounded placement slots. Processes are started lazily when
    /// their first application is activated. Each deployment has a stable
    /// hash preference and probes the remaining slots if that shard is full.
    #[serde(default = "default_shard_count")]
    pub count: usize,
    /// Maximum distinct deployments in one process. Replacement generations
    /// of the same deployment may overlap during activation and drain.
    #[serde(default = "default_shard_max_apps")]
    pub max_apps: usize,
    /// Aggregate cgroup/RSS budget for the complete shard failure domain.
    #[serde(default = "default_shard_max_rss_mb")]
    pub max_rss_mb: u32,
    #[serde(default = "default_shard_max_cpu_percent")]
    pub max_cpu_percent: u32,
    #[serde(default = "default_shard_max_pids")]
    pub max_pids: u32,
}

fn default_shard_count() -> usize {
    4
}

fn default_shard_max_apps() -> usize {
    25
}

fn default_shard_max_rss_mb() -> u32 {
    1024
}

fn default_shard_max_cpu_percent() -> u32 {
    400
}

fn default_shard_max_pids() -> u32 {
    256
}

impl Default for ShardExecutionConfig {
    fn default() -> Self {
        Self {
            count: default_shard_count(),
            max_apps: default_shard_max_apps(),
            max_rss_mb: default_shard_max_rss_mb(),
            max_cpu_percent: default_shard_max_cpu_percent(),
            max_pids: default_shard_max_pids(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CgroupMode {
    Disabled,
    Auto,
    Required,
}

impl Default for CgroupMode {
    fn default() -> Self {
        if cfg!(target_os = "linux") {
            Self::Auto
        } else {
            Self::Disabled
        }
    }
}

fn default_cgroup_root() -> PathBuf {
    PathBuf::from("/sys/fs/cgroup/perch")
}

impl ExecutionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProcess => "in_process",
            Self::Worker => "worker",
            Self::Shard => "shard",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Public HTTP listener. Handles inbound plain HTTP, ACME HTTP-01
    /// challenges, and redirects to HTTPS for non-Bunny hostnames.
    #[serde(default = "default_http_listen")]
    pub listen_http: String,

    /// Public HTTPS listener. Uses `rustls-acme` in `tls.mode = "acme"`,
    /// static certs in `manual`, or is unused in `off`.
    #[serde(default = "default_https_listen")]
    pub listen_https: String,

    /// Private origin listener for CDN pull traffic. Bunny's edge calls
    /// this port on a cache miss to fetch content from the box. Trusted
    /// proxy headers (`X-Forwarded-*`) are honored here only when the
    /// source IP is in the configured Bunny allowlist. **Must not be
    /// exposed to the public internet**; firewall it to CDN edge IPs only.
    #[serde(default = "default_origin_listen")]
    pub listen_origin: String,
}

fn default_http_listen() -> String {
    "0.0.0.0:80".to_string()
}
fn default_https_listen() -> String {
    "0.0.0.0:443".to_string()
}
fn default_origin_listen() -> String {
    "127.0.0.1:8081".to_string()
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen_http: default_http_listen(),
            listen_https: default_https_listen(),
            listen_origin: default_origin_listen(),
        }
    }
}

/// Filesystem layout the daemon owns. All paths default to a `var/` layout
/// that works for local development (`./var/perch/...`); production installs
/// typically override to `/var/lib/perch/...`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsConfig {
    #[serde(default = "default_deployments_dir")]
    pub deployments_dir: PathBuf,
    #[serde(default = "default_compiled_dir")]
    pub compiled_dir: PathBuf,
    #[serde(default = "default_sockets_dir")]
    pub sockets_dir: PathBuf,
    #[serde(default = "default_storage_dir")]
    pub storage_dir: PathBuf,
    #[serde(default = "default_logs_dir")]
    pub logs_dir: PathBuf,
    #[serde(default = "default_state_db")]
    pub state_db: PathBuf,
    #[serde(default = "default_acme_cache_dir")]
    pub acme_cache_dir: PathBuf,
    #[serde(default = "default_perry_binary")]
    pub perry_binary: PathBuf,
    /// Process-wide Perry runtime provider loaded before any application.
    #[serde(default = "default_perry_runtime_library")]
    pub perry_runtime_library: PathBuf,
    /// Process-wide Perry standard-library provider loaded after runtime.
    #[serde(default = "default_perry_stdlib_library")]
    pub perry_stdlib_library: PathBuf,
    /// Path to the perch-worker binary. If not set, we locate it by
    /// searching `$PATH` and the daemon binary's own directory.
    #[serde(default)]
    pub perch_worker_binary: Option<PathBuf>,
}

fn default_deployments_dir() -> PathBuf {
    PathBuf::from("var/perch/deployments")
}
fn default_compiled_dir() -> PathBuf {
    PathBuf::from("var/perch/compiled")
}
fn default_sockets_dir() -> PathBuf {
    PathBuf::from("var/perch/sockets")
}
fn default_storage_dir() -> PathBuf {
    PathBuf::from("var/perch/storage")
}
fn default_logs_dir() -> PathBuf {
    PathBuf::from("var/perch/logs")
}
fn default_state_db() -> PathBuf {
    PathBuf::from("var/perch/state.sqlite")
}
fn default_acme_cache_dir() -> PathBuf {
    PathBuf::from("var/perch/acme")
}
fn default_perry_binary() -> PathBuf {
    PathBuf::from("perry")
}
fn default_perry_runtime_library() -> PathBuf {
    PathBuf::from(if cfg!(target_os = "macos") {
        "var/perch/lib/libperry_runtime.dylib"
    } else {
        "var/perch/lib/libperry_runtime.so"
    })
}
fn default_perry_stdlib_library() -> PathBuf {
    PathBuf::from(if cfg!(target_os = "macos") {
        "var/perch/lib/libperry_stdlib.dylib"
    } else {
        "var/perch/lib/libperry_stdlib.so"
    })
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            deployments_dir: default_deployments_dir(),
            compiled_dir: default_compiled_dir(),
            sockets_dir: default_sockets_dir(),
            storage_dir: default_storage_dir(),
            logs_dir: default_logs_dir(),
            state_db: default_state_db(),
            acme_cache_dir: default_acme_cache_dir(),
            perry_binary: default_perry_binary(),
            perry_runtime_library: default_perry_runtime_library(),
            perry_stdlib_library: default_perry_stdlib_library(),
            perch_worker_binary: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresConfig {
    pub url: String,
    #[serde(default = "default_pg_max_connections")]
    pub max_connections: u32,
}

fn default_pg_max_connections() -> u32 {
    16
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    pub url: String,
}

/// TLS mode. `off` = plain HTTP only (development or behind external
/// proxy). `acme` = rustls-acme (Checkpoint 3). `manual` = static cert
/// files from disk (Checkpoint 3).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub mode: TlsMode,
    #[serde(default)]
    pub acme_contact: Option<String>,
    #[serde(default)]
    pub acme_directory: Option<String>,
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    #[default]
    Off,
    Acme,
    Manual,
}

/// Box-wide CDN configuration. Bunny is the only supported provider in v0.
/// If `cdn.bunny.api_key` is set, every deployment is opted into Bunny by
/// default (individual deployments can opt out via `perch.toml` `[cdn]
/// enabled = false`). See Checkpoint 4 for implementation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CdnConfig {
    #[serde(default)]
    pub bunny: Option<BunnyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BunnyConfig {
    pub api_key: String,
    #[serde(default = "default_bunny_cache_duration")]
    pub default_cache_duration_secs: u32,
}

fn default_bunny_cache_duration() -> u32 {
    86400 // 1 day
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminConfig {
    #[serde(default = "default_admin_path")]
    pub path: String,
    /// Optional bcrypt hash of the admin password. When unset, admin UI
    /// is reachable without auth — only safe for local development.
    #[serde(default)]
    pub password_hash: Option<String>,
}

fn default_admin_path() -> String {
    "/_perch/admin".to_string()
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            path: default_admin_path(),
            password_hash: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsConfig {
    #[serde(default = "default_logs_retention")]
    pub retention_days: u32,
}

fn default_logs_retention() -> u32 {
    30
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            retention_days: default_logs_retention(),
        }
    }
}

impl RuntimeConfig {
    /// Load `runtime.toml` from disk. Returns a default-populated config
    /// if the file doesn't exist, which is the right behavior for first
    /// boot and for tests that don't care about config.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::info!(
                config = %path.display(),
                "runtime.toml not found, using defaults"
            );
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading runtime config {:?}", path))?;
        let cfg: Self = toml::from_str(&contents)
            .with_context(|| format!("parsing runtime config {:?}", path))?;
        if cfg.execution.executor_stack_size_bytes < 256 * 1024 {
            return Err(anyhow!(
                "execution.executor_stack_size_bytes must be at least {}",
                256 * 1024
            ));
        }
        if cfg.execution.command_queue_capacity == 0 {
            return Err(anyhow!("execution.command_queue_capacity must be positive"));
        }
        if cfg.execution.gc_reclaim_check_interval > 0 && cfg.execution.gc_reclaim_growth_bytes == 0
        {
            return Err(anyhow!(
                "execution.gc_reclaim_growth_bytes must be positive when GC pacing is enabled"
            ));
        }
        if cfg.execution.preload_concurrency == 0 {
            return Err(anyhow!("execution.preload_concurrency must be positive"));
        }
        if cfg.execution.compile_concurrency == 0 {
            return Err(anyhow!("execution.compile_concurrency must be positive"));
        }
        if cfg.execution.compile_timeout_seconds == 0 {
            return Err(anyhow!(
                "execution.compile_timeout_seconds must be positive"
            ));
        }
        if cfg.execution.compile_output_bytes == 0 {
            return Err(anyhow!("execution.compile_output_bytes must be positive"));
        }
        if cfg.execution.compile_dependency_max_files == 0 {
            return Err(anyhow!(
                "execution.compile_dependency_max_files must be positive"
            ));
        }
        if cfg.execution.compile_dependency_max_bytes == 0 {
            return Err(anyhow!(
                "execution.compile_dependency_max_bytes must be positive"
            ));
        }
        if cfg.execution.compile_march.trim().is_empty() {
            return Err(anyhow!("execution.compile_march must not be empty"));
        }
        if cfg.execution.artifact_max_static_files == 0 {
            return Err(anyhow!(
                "execution.artifact_max_static_files must be positive"
            ));
        }
        if cfg.execution.artifact_max_static_bytes == 0 {
            return Err(anyhow!(
                "execution.artifact_max_static_bytes must be positive"
            ));
        }
        if cfg.execution.artifact_retention_count == 0 {
            return Err(anyhow!(
                "execution.artifact_retention_count must be positive"
            ));
        }
        if cfg.execution.shards.count == 0 || cfg.execution.shards.count > 1024 {
            return Err(anyhow!("execution.shards.count must be between 1 and 1024"));
        }
        if cfg.execution.shards.max_apps == 0 || cfg.execution.shards.max_apps > 10_000 {
            return Err(anyhow!(
                "execution.shards.max_apps must be between 1 and 10000"
            ));
        }
        if cfg.execution.shards.max_rss_mb == 0 {
            return Err(anyhow!("execution.shards.max_rss_mb must be positive"));
        }
        if cfg.execution.shards.max_cpu_percent == 0
            || cfg.execution.shards.max_cpu_percent > 10_000
        {
            return Err(anyhow!(
                "execution.shards.max_cpu_percent must be between 1 and 10000"
            ));
        }
        if cfg.execution.shards.max_pids == 0 {
            return Err(anyhow!("execution.shards.max_pids must be positive"));
        }
        if cfg.execution.cgroup.mode != CgroupMode::Disabled
            && !cfg.execution.cgroup.root.is_absolute()
        {
            return Err(anyhow!("execution.cgroup.root must be an absolute path"));
        }
        Ok(cfg)
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            execution: ExecutionConfig::default(),
            http: HttpConfig::default(),
            paths: PathsConfig::default(),
            postgres: None,
            queue_service: QueueServiceConfig::default(),
            redis: None,
            tls: TlsConfig::default(),
            cdn: CdnConfig::default(),
            admin: AdminConfig::default(),
            logs: LogsConfig::default(),
        }
    }
}

// ============================================================================
// perch.toml — per-deployment configuration
// ============================================================================

/// Per-deployment manifest. Lives at
/// `<deployments_dir>/<name>/perch.toml`. The daemon reads this every
/// time the `notify` watcher fires for that deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentConfig {
    /// Deployment name. Must match the directory name under
    /// `deployments_dir/`. The daemon validates this on load.
    pub name: String,

    #[serde(default)]
    pub version: Option<String>,

    /// Hosts this deployment answers for. Used by the daemon's host-based
    /// router to dispatch incoming requests. At least one hostname is
    /// required; if you want your deployment reachable via path-prefix
    /// routing under the box IP, set `domains = []` (empty list is
    /// allowed for development).
    #[serde(default)]
    pub hosts: HostsConfig,

    /// CDN opt-out. If Bunny is configured at the box level (runtime.toml
    /// `[cdn.bunny]`), every deployment is opted IN by default. Set
    /// `[cdn] enabled = false` to opt this deployment out.
    #[serde(default)]
    pub cdn: DeploymentCdnConfig,

    #[serde(default)]
    pub database: Option<DeploymentDatabaseConfig>,

    /// Deployment-specific failure-domain policy. `inherit` preserves the
    /// box-wide execution default; explicit classes choose the daemon, a
    /// bounded multi-app shard, or one supervised process per deployment.
    #[serde(default)]
    pub isolation: DeploymentIsolationConfig,

    #[serde(rename = "handlers", default)]
    pub handlers: Vec<HandlerConfig>,

    #[serde(rename = "static", default)]
    pub static_blocks: Vec<StaticConfig>,

    #[serde(rename = "crons", default)]
    pub crons: Vec<CronConfig>,

    #[serde(rename = "queues", default)]
    pub queues: Vec<QueueConfig>,

    /// Optional pre-publication warmup and health contract. The configured
    /// request is dispatched directly to the initialized replacement runtime;
    /// it is never exposed through the public router until every probe passes.
    #[serde(default)]
    pub activation: ActivationConfig,

    #[serde(default)]
    pub capabilities: CapabilitiesConfig,

    #[serde(default)]
    pub limits: DeploymentLimitsConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentIsolationConfig {
    #[serde(default)]
    pub class: IsolationClass,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationClass {
    /// Use the box-wide `execution.mode` default.
    #[default]
    Inherit,
    /// Run in the daemon address space. This is the minimum-latency/density
    /// class and is only appropriate for mutually trusted application code.
    Trusted,
    /// Give this deployment one supervised worker process. This contains a
    /// native crash and permits hard process termination after a deadline.
    Dedicated,
    /// Share one supervised process with a bounded deterministic group of
    /// deployments. A crash or hard timeout restarts the whole shard.
    Sharded,
}

impl IsolationClass {
    pub const fn requested_label(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Trusted => "trusted",
            Self::Dedicated => "dedicated",
            Self::Sharded => "sharded",
        }
    }

    pub const fn resolve(self, default_mode: ExecutionMode) -> ExecutionMode {
        match self {
            Self::Inherit => default_mode,
            Self::Trusted => ExecutionMode::InProcess,
            Self::Dedicated => ExecutionMode::Worker,
            Self::Sharded => ExecutionMode::Shard,
        }
    }

    pub const fn effective_label(self, default_mode: ExecutionMode) -> &'static str {
        match self.resolve(default_mode) {
            ExecutionMode::InProcess => "trusted",
            ExecutionMode::Worker => "dedicated",
            ExecutionMode::Shard => "sharded",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostsConfig {
    #[serde(default)]
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentCdnConfig {
    /// Defaults to true; set false to opt out if Bunny is configured at
    /// the box level.
    #[serde(default = "default_cdn_enabled")]
    pub enabled: bool,
}

fn default_cdn_enabled() -> bool {
    true
}

impl Default for DeploymentCdnConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentDatabaseConfig {
    #[serde(default)]
    pub migrations: Option<PathBuf>,
    #[serde(default = "default_pg_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_db_max_query_rows")]
    pub max_query_rows: u32,
    #[serde(default = "default_db_max_query_duration_ms")]
    pub max_query_duration_ms: u32,
}

fn default_db_max_query_rows() -> u32 {
    10_000
}
fn default_db_max_query_duration_ms() -> u32 {
    5_000
}

/// A single HTTP handler entry. Each handler is a separate TypeScript
/// file exporting a `default function` (or a Perry plugin registering a
/// named tool); the daemon builds a routing table from all `[[handlers]]`
/// blocks across all loaded deployments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerConfig {
    /// Relative path to the TS file from the deployment root.
    pub file: PathBuf,
    /// URL path to match.
    pub path: String,
    /// HTTP method. Case-insensitive. If omitted, matches any method.
    #[serde(default)]
    pub method: Option<String>,
    /// Historical route label. If omitted, the daemon derives a name from the
    /// file path. ABI-v2 application libraries use one required Buffer entry
    /// point, so this is metadata rather than a separately registered tool.
    #[serde(default)]
    pub tool: Option<String>,
}

/// A static-file block. The daemon serves files from `directory` under
/// the `path` prefix via `tower-http::services::ServeDir`. Multiple
/// `[[static]]` blocks are allowed; they're checked in order for the
/// first prefix match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticConfig {
    /// Relative path to the directory from the deployment root.
    pub directory: PathBuf,
    /// URL prefix to mount this directory under. `/` means "everything".
    #[serde(default = "default_static_path")]
    pub path: String,
}

fn default_static_path() -> String {
    "/".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronConfig {
    pub file: PathBuf,
    /// Standard cron expression (minute hour day-of-month month day-of-week).
    pub schedule: String,
    /// `skip` is the production default: one slow fire cannot overlap the
    /// same schedule. `allow` still remains bounded by deployment admission.
    #[serde(default)]
    pub overlap: CronOverlapPolicy,
    /// A scheduler wakeup later than `max_lateness_ms` is either skipped or
    /// dispatched once. Daemon downtime is intentionally never caught up.
    #[serde(default)]
    pub late: CronLatePolicy,
    #[serde(default = "default_cron_max_lateness_ms")]
    pub max_lateness_ms: u64,
    /// Cron evaluation currently has one explicit, portable timezone.
    #[serde(default = "default_cron_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CronOverlapPolicy {
    #[default]
    Skip,
    Allow,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CronLatePolicy {
    #[default]
    Skip,
    RunOnce,
}

fn default_cron_max_lateness_ms() -> u64 {
    30_000
}

fn default_cron_timezone() -> String {
    "UTC".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueConfig {
    pub file: PathBuf,
    pub name: String,
    #[serde(default = "default_queue_concurrency")]
    pub concurrency: u32,
    #[serde(default = "default_queue_max_retries")]
    pub max_retries: u32,
    /// Duration of a delivery lease. This must exceed the deployment wall
    /// deadline so the same message cannot run concurrently after expiry.
    #[serde(default = "default_queue_visibility_timeout_ms")]
    pub visibility_timeout_ms: u64,
    /// Empty-queue polling interval for each configured consumer.
    #[serde(default = "default_queue_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// Initial retry delay before deterministic jitter is applied.
    #[serde(default = "default_queue_retry_base_delay_ms")]
    pub retry_base_delay_ms: u64,
    /// Upper bound for exponential retry delay and jitter.
    #[serde(default = "default_queue_retry_max_delay_ms")]
    pub retry_max_delay_ms: u64,
    /// Largest producer-requested delivery delay.
    #[serde(default = "default_queue_max_enqueue_delay_ms")]
    pub max_enqueue_delay_ms: u64,
    /// Delete dead letters older than this many days. Zero retains them until
    /// an operator explicitly removes or replays them.
    #[serde(default = "default_queue_dlq_retention_days")]
    pub dlq_retention_days: u32,
    #[serde(default)]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationConfig {
    /// HTTP path invoked before a generation becomes active. `None` disables
    /// activation probing while retaining eager library/module initialization.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_activation_method")]
    pub method: String,
    /// Repetition count used to populate lazy application caches. Kept small
    /// and sequential so activation cannot create an unbounded load burst.
    #[serde(default = "default_activation_requests")]
    pub requests: u16,
    #[serde(default = "default_activation_expected_status")]
    pub expected_status: u16,
    /// Optional exact SHA-256 of the response body for a semantic health gate.
    #[serde(default)]
    pub expected_body_sha256: Option<String>,
}

fn default_activation_method() -> String {
    "GET".to_string()
}

fn default_activation_requests() -> u16 {
    1
}

fn default_activation_expected_status() -> u16 {
    200
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            path: None,
            method: default_activation_method(),
            requests: default_activation_requests(),
            expected_status: default_activation_expected_status(),
            expected_body_sha256: None,
        }
    }
}

fn default_queue_concurrency() -> u32 {
    1
}
fn default_queue_max_retries() -> u32 {
    5
}
fn default_queue_visibility_timeout_ms() -> u64 {
    60_000
}
fn default_queue_poll_interval_ms() -> u64 {
    100
}
fn default_queue_retry_base_delay_ms() -> u64 {
    1_000
}
fn default_queue_retry_max_delay_ms() -> u64 {
    60_000
}
fn default_queue_max_enqueue_delay_ms() -> u64 {
    7 * 24 * 60 * 60 * 1_000
}
fn default_queue_dlq_retention_days() -> u32 {
    30
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilitiesConfig {
    #[serde(default)]
    pub fetch: Option<FetchCapability>,
    #[serde(default)]
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchCapability {
    #[serde(default)]
    pub allowlist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentLimitsConfig {
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb_per_invocation: u32,
    #[serde(default = "default_max_wall_clock_ms")]
    pub max_wall_clock_ms: u32,
    #[serde(default = "default_max_concurrent_invocations")]
    pub max_concurrent_invocations: u32,
    /// Maximum raw HTTP request body accepted before application dispatch.
    /// The complete ABI frame remains subject to `MAX_FRAME_SIZE` as well.
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
    /// Maximum combined HTTP request header-name/value bytes.
    #[serde(default = "default_max_request_header_bytes")]
    pub max_request_header_bytes: usize,
    /// Maximum raw HTTP response body accepted from an application.
    #[serde(default = "default_max_response_body_bytes")]
    pub max_response_body_bytes: usize,
    /// Maximum combined HTTP response header-name/value bytes.
    #[serde(default = "default_max_response_header_bytes")]
    pub max_response_header_bytes: usize,
    /// Maximum raw payload delivered to a queue handler.
    #[serde(default = "default_max_queue_payload_bytes")]
    pub max_queue_payload_bytes: usize,
    /// Hard RSS limit per worker process in MB. When exceeded, the
    /// daemon kills + respawns the worker. Default 512 MB. This bounds
    /// the impact of a ballooning deployment (test5 grew to 196 MB
    /// during stress testing because Perry's arena allocator doesn't
    /// release pages back to the OS aggressively).
    #[serde(default = "default_max_worker_rss_mb")]
    pub max_worker_rss_mb: u32,
    /// Dedicated-worker CPU quota as a percentage of one CPU. Values above
    /// 100 allow multiple cores for runtime/background work.
    #[serde(default = "default_max_worker_cpu_percent")]
    pub max_worker_cpu_percent: u32,
    /// Maximum tasks (processes + threads) in a dedicated worker cgroup.
    #[serde(default = "default_max_worker_pids")]
    pub max_worker_pids: u32,
}

fn default_max_memory_mb() -> u32 {
    16
}
fn default_max_wall_clock_ms() -> u32 {
    30_000
}
fn default_max_concurrent_invocations() -> u32 {
    1000
}
fn default_max_request_body_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_max_request_header_bytes() -> usize {
    64 * 1024
}
fn default_max_response_body_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_max_response_header_bytes() -> usize {
    64 * 1024
}
fn default_max_queue_payload_bytes() -> usize {
    1024 * 1024
}
fn default_max_worker_rss_mb() -> u32 {
    512
}
fn default_max_worker_cpu_percent() -> u32 {
    100
}
fn default_max_worker_pids() -> u32 {
    64
}

impl Default for DeploymentLimitsConfig {
    fn default() -> Self {
        Self {
            max_memory_mb_per_invocation: default_max_memory_mb(),
            max_wall_clock_ms: default_max_wall_clock_ms(),
            max_concurrent_invocations: default_max_concurrent_invocations(),
            max_request_body_bytes: default_max_request_body_bytes(),
            max_request_header_bytes: default_max_request_header_bytes(),
            max_response_body_bytes: default_max_response_body_bytes(),
            max_response_header_bytes: default_max_response_header_bytes(),
            max_queue_payload_bytes: default_max_queue_payload_bytes(),
            max_worker_rss_mb: default_max_worker_rss_mb(),
            max_worker_cpu_percent: default_max_worker_cpu_percent(),
            max_worker_pids: default_max_worker_pids(),
        }
    }
}

impl DeploymentConfig {
    /// Load `perch.toml` from a deployment directory. The file must be
    /// named `perch.toml` and live directly under `deployment_dir`.
    pub fn load(deployment_dir: &Path) -> Result<Self> {
        let toml_path = deployment_dir.join("perch.toml");
        if !toml_path.exists() {
            return Err(anyhow!("deployment {:?} has no perch.toml", deployment_dir));
        }
        let contents = std::fs::read_to_string(&toml_path)
            .with_context(|| format!("reading {:?}", toml_path))?;
        let cfg: Self =
            toml::from_str(&contents).with_context(|| format!("parsing {:?}", toml_path))?;
        Ok(cfg)
    }

    /// Validate limits that must be safe before compilation or publication.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || self.name == "."
            || self.name == ".."
            || self.name.contains('/')
            || self.name.contains('\\')
        {
            return Err(anyhow!("deployment name is not a safe path component"));
        }
        for handler in &self.handlers {
            validate_deployment_path("handler file", &handler.file)?;
            if !handler.path.starts_with('/') {
                return Err(anyhow!("handler route paths must start with '/'"));
            }
        }
        for block in &self.static_blocks {
            validate_deployment_path("static directory", &block.directory)?;
            if !block.path.starts_with('/') {
                return Err(anyhow!("static mount paths must start with '/'"));
            }
        }
        for cron in &self.crons {
            validate_deployment_path("cron file", &cron.file)?;
            if !cron.timezone.eq_ignore_ascii_case("UTC") {
                return Err(anyhow!(
                    "cron timezone {:?} is unsupported; only UTC is currently enforceable",
                    cron.timezone
                ));
            }
            if cron.max_lateness_ms == 0 || cron.max_lateness_ms > 24 * 60 * 60 * 1_000 {
                return Err(anyhow!(
                    "cron max_lateness_ms must be between 1 and 86400000"
                ));
            }
        }
        if let Some(path) = self.activation.path.as_deref() {
            if !path.starts_with('/') {
                return Err(anyhow!("activation path must start with '/'"));
            }
            if path.contains('#') {
                return Err(anyhow!("activation path must not contain a fragment"));
            }
            if self.activation.method.is_empty()
                || self.activation.method.len() > 32
                || !self
                    .activation
                    .method
                    .bytes()
                    .all(|byte| byte.is_ascii_alphabetic() || byte == b'-')
            {
                return Err(anyhow!(
                    "activation method must contain only ASCII letters or '-'"
                ));
            }
            if !(1..=64).contains(&self.activation.requests) {
                return Err(anyhow!("activation requests must be between 1 and 64"));
            }
            if !(100..=599).contains(&self.activation.expected_status) {
                return Err(anyhow!(
                    "activation expected_status must be between 100 and 599"
                ));
            }
            if let Some(expected) = self.activation.expected_body_sha256.as_deref() {
                if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(anyhow!(
                        "activation expected_body_sha256 must be 64 hexadecimal characters"
                    ));
                }
            }
        }
        let mut queue_names = std::collections::HashSet::new();
        for queue in &self.queues {
            validate_deployment_path("queue file", &queue.file)?;
            if queue.name.is_empty()
                || queue.name.len() > 255
                || queue.name.contains('\0')
                || !queue_names.insert(queue.name.as_str())
            {
                return Err(anyhow!("queue names must be non-empty and unique"));
            }
            if queue.concurrency == 0 || queue.concurrency > 1024 {
                return Err(anyhow!("queue concurrency must be between 1 and 1024"));
            }
            if queue.max_retries >= i32::MAX as u32 {
                return Err(anyhow!(
                    "queue max_retries exceeds the durable schema range"
                ));
            }
            if queue.visibility_timeout_ms <= u64::from(self.limits.max_wall_clock_ms) {
                return Err(anyhow!(
                    "queue visibility_timeout_ms must exceed limits.max_wall_clock_ms"
                ));
            }
            if queue.poll_interval_ms == 0 {
                return Err(anyhow!("queue poll_interval_ms must be positive"));
            }
            if queue.retry_base_delay_ms == 0
                || queue.retry_max_delay_ms < queue.retry_base_delay_ms
            {
                return Err(anyhow!(
                    "queue retry delays must be positive and retry_max_delay_ms must be at least retry_base_delay_ms"
                ));
            }
            if queue.max_enqueue_delay_ms == 0 {
                return Err(anyhow!("queue max_enqueue_delay_ms must be positive"));
            }
        }
        if let Some(migrations) = self
            .database
            .as_ref()
            .and_then(|database| database.migrations.as_ref())
        {
            validate_deployment_path("database migrations", migrations)?;
        }
        if self.limits.max_wall_clock_ms == 0 {
            return Err(anyhow!("max_wall_clock_ms must be positive"));
        }
        if self.limits.max_concurrent_invocations == 0 {
            return Err(anyhow!("max_concurrent_invocations must be positive"));
        }
        if self.limits.max_worker_rss_mb == 0 {
            return Err(anyhow!("max_worker_rss_mb must be positive"));
        }
        if self.limits.max_worker_cpu_percent == 0 || self.limits.max_worker_cpu_percent > 10_000 {
            return Err(anyhow!(
                "max_worker_cpu_percent must be between 1 and 10000"
            ));
        }
        if self.limits.max_worker_pids == 0 {
            return Err(anyhow!("max_worker_pids must be positive"));
        }

        for (name, value) in [
            ("max_request_body_bytes", self.limits.max_request_body_bytes),
            (
                "max_request_header_bytes",
                self.limits.max_request_header_bytes,
            ),
            (
                "max_response_body_bytes",
                self.limits.max_response_body_bytes,
            ),
            (
                "max_response_header_bytes",
                self.limits.max_response_header_bytes,
            ),
            (
                "max_queue_payload_bytes",
                self.limits.max_queue_payload_bytes,
            ),
        ] {
            if value == 0 {
                return Err(anyhow!("{name} must be positive"));
            }
            if value > perch_host_abi::MAX_FRAME_SIZE {
                return Err(anyhow!(
                    "{name} ({value}) exceeds the global ABI frame limit ({})",
                    perch_host_abi::MAX_FRAME_SIZE
                ));
            }
        }
        Ok(())
    }

    pub const fn execution_mode(&self, runtime_default: ExecutionMode) -> ExecutionMode {
        self.isolation.class.resolve(runtime_default)
    }

    /// Derive a default tool name from a TS file path.
    /// `handlers/contact.ts` → `handlers_contact`.
    pub fn default_tool_name(file: &Path) -> String {
        let mut parts = Vec::new();
        for comp in file.components() {
            if let std::path::Component::Normal(s) = comp {
                if let Some(s) = s.to_str() {
                    parts.push(
                        s.trim_end_matches(".ts")
                            .trim_end_matches(".tsx")
                            .to_string(),
                    );
                }
            }
        }
        parts.join("_")
    }

    /// Resolve each handler's tool name: explicit `tool =` wins, else
    /// derive from the file path. Returns a map from `(method, path)` to
    /// tool name.
    pub fn handler_tool_map(&self) -> HashMap<(String, String), String> {
        let mut map = HashMap::new();
        for h in &self.handlers {
            let method = h.method.as_deref().unwrap_or("").to_uppercase();
            let tool = h
                .tool
                .clone()
                .unwrap_or_else(|| Self::default_tool_name(&h.file));
            map.insert((method, h.path.clone()), tool);
        }
        map
    }
}

fn validate_deployment_path(kind: &str, path: &Path) -> Result<()> {
    let mut has_component = false;
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => has_component = true,
            std::path::Component::CurDir => {}
            _ => return Err(anyhow!("{kind} must be a relative path without '..'")),
        }
    }
    if !has_component {
        return Err(anyhow!("{kind} must not be empty"));
    }
    Ok(())
}

impl Default for DeploymentConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: None,
            hosts: HostsConfig::default(),
            cdn: DeploymentCdnConfig::default(),
            database: None,
            isolation: DeploymentIsolationConfig::default(),
            handlers: Vec::new(),
            static_blocks: Vec::new(),
            crons: Vec::new(),
            queues: Vec::new(),
            activation: ActivationConfig::default(),
            capabilities: CapabilitiesConfig::default(),
            limits: DeploymentLimitsConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_minimal_runtime_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            r#"
[http]
listen_http = "127.0.0.1:8080"

[paths]
deployments_dir = "/tmp/d"
"#,
        )
        .unwrap();
        let cfg = RuntimeConfig::load(&path).unwrap();
        assert_eq!(cfg.http.listen_http, "127.0.0.1:8080");
        assert_eq!(cfg.paths.deployments_dir, PathBuf::from("/tmp/d"));
        assert_eq!(cfg.execution.mode, ExecutionMode::InProcess);
        assert_eq!(
            cfg.execution.provider_verification,
            PerryProviderVerification::FullHash
        );
        assert!(cfg.execution.watch_deployments);
        assert_eq!(
            cfg.execution.preload_concurrency,
            if cfg!(target_os = "macos") { 1 } else { 4 }
        );
        assert_eq!(cfg.execution.compile_concurrency, 1);
        assert_eq!(cfg.execution.executor_stack_size_bytes, 1024 * 1024);
        assert_eq!(cfg.execution.command_queue_capacity, 256);
        assert_eq!(cfg.execution.gc_reclaim_check_interval, 0);
        assert_eq!(cfg.execution.gc_reclaim_growth_bytes, 256 * 1024);
        assert_eq!(cfg.execution.shards.count, 4);
        assert_eq!(cfg.execution.shards.max_apps, 25);
        assert_eq!(
            cfg.execution.cgroup.mode,
            if cfg!(target_os = "linux") {
                CgroupMode::Auto
            } else {
                CgroupMode::Disabled
            }
        );
        assert!(cfg
            .paths
            .perry_runtime_library
            .to_string_lossy()
            .contains("libperry_runtime"));
        assert!(cfg
            .paths
            .perry_stdlib_library
            .to_string_lossy()
            .contains("libperry_stdlib"));
        // Defaults for unset fields
        assert_eq!(cfg.logs.retention_days, 30);
        assert!(!cfg.queue_service.allow_handlers_without_store);
    }

    #[test]
    fn loads_root_owned_immutable_provider_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            r#"
[execution]
provider_verification = "root_owned_immutable"
"#,
        )
        .unwrap();

        let cfg = RuntimeConfig::load(&path).unwrap();
        assert_eq!(
            cfg.execution.provider_verification,
            PerryProviderVerification::RootOwnedImmutable
        );
    }

    #[test]
    fn loads_gc_reclaim_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            r#"
[execution]
gc_reclaim_check_interval = 64
gc_reclaim_growth_bytes = 131072
"#,
        )
        .unwrap();

        let cfg = RuntimeConfig::load(&path).unwrap();
        assert_eq!(cfg.execution.gc_reclaim_check_interval, 64);
        assert_eq!(cfg.execution.gc_reclaim_growth_bytes, 131_072);
    }

    #[test]
    fn loads_missing_runtime_toml_as_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.toml");
        let cfg = RuntimeConfig::load(&path).unwrap();
        assert_eq!(cfg.http.listen_http, "0.0.0.0:80");
    }

    #[test]
    fn validates_worker_cgroup_configuration_and_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            r#"
[execution.cgroup]
mode = "required"
root = "relative/cgroup"
"#,
        )
        .unwrap();
        assert!(RuntimeConfig::load(&path)
            .unwrap_err()
            .to_string()
            .contains("absolute"));

        let mut deployment = DeploymentConfig::default();
        deployment.name = "limits".into();
        deployment.limits.max_worker_cpu_percent = 0;
        assert!(deployment
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_worker_cpu_percent"));
        deployment.limits.max_worker_cpu_percent = 100;
        deployment.limits.max_worker_pids = 0;
        assert!(deployment
            .validate()
            .unwrap_err()
            .to_string()
            .contains("pids"));

        let invalid_shards = tmp.path().join("invalid-shards.toml");
        std::fs::write(
            &invalid_shards,
            r#"
[execution.shards]
count = 0
"#,
        )
        .unwrap();
        assert!(RuntimeConfig::load(&invalid_shards)
            .unwrap_err()
            .to_string()
            .contains("shards.count"));
    }

    #[test]
    fn loads_deployment_toml_with_hosts_handlers_static() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("perch.toml"),
            r#"
name = "landing"
version = "0.1.0"

[hosts]
domains = ["landing.test", "www.landing.test"]

[[handlers]]
file = "handlers/contact.ts"
path = "/contact"
method = "POST"

[[static]]
directory = "./static"
path = "/"
"#,
        )
        .unwrap();
        let cfg = DeploymentConfig::load(tmp.path()).unwrap();
        assert_eq!(cfg.name, "landing");
        assert_eq!(cfg.hosts.domains.len(), 2);
        assert_eq!(cfg.handlers.len(), 1);
        assert_eq!(cfg.handlers[0].path, "/contact");
        assert_eq!(cfg.static_blocks.len(), 1);
        assert_eq!(cfg.static_blocks[0].path, "/");
        assert_eq!(cfg.cdn.enabled, true);
        assert_eq!(cfg.isolation.class, IsolationClass::Inherit);
    }

    #[test]
    fn deployment_isolation_class_overrides_or_inherits_runtime_default() {
        let mut cfg = DeploymentConfig::default();
        assert_eq!(
            cfg.execution_mode(ExecutionMode::InProcess),
            ExecutionMode::InProcess
        );
        assert_eq!(
            cfg.execution_mode(ExecutionMode::Worker),
            ExecutionMode::Worker
        );
        assert_eq!(
            cfg.execution_mode(ExecutionMode::Shard),
            ExecutionMode::Shard
        );

        cfg.isolation.class = IsolationClass::Trusted;
        assert_eq!(
            cfg.execution_mode(ExecutionMode::Worker),
            ExecutionMode::InProcess
        );
        assert_eq!(
            cfg.isolation.class.effective_label(ExecutionMode::Worker),
            "trusted"
        );

        cfg.isolation.class = IsolationClass::Dedicated;
        assert_eq!(
            cfg.execution_mode(ExecutionMode::InProcess),
            ExecutionMode::Worker
        );
        assert_eq!(
            cfg.isolation
                .class
                .effective_label(ExecutionMode::InProcess),
            "dedicated"
        );

        cfg.isolation.class = IsolationClass::Sharded;
        assert_eq!(
            cfg.execution_mode(ExecutionMode::InProcess),
            ExecutionMode::Shard
        );
        assert_eq!(
            cfg.isolation
                .class
                .effective_label(ExecutionMode::InProcess),
            "sharded"
        );
    }

    #[test]
    fn parses_explicit_deployment_isolation_class() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("perch.toml"),
            r#"
name = "isolated"

[isolation]
class = "dedicated"
"#,
        )
        .unwrap();
        let cfg = DeploymentConfig::load(tmp.path()).unwrap();
        assert_eq!(cfg.isolation.class, IsolationClass::Dedicated);
        assert_eq!(
            cfg.execution_mode(ExecutionMode::InProcess),
            ExecutionMode::Worker
        );
    }

    #[test]
    fn derives_tool_name_from_file_path() {
        assert_eq!(
            DeploymentConfig::default_tool_name(Path::new("handlers/contact.ts")),
            "handlers_contact"
        );
        assert_eq!(
            DeploymentConfig::default_tool_name(Path::new("crons/daily/aggregate.ts")),
            "crons_daily_aggregate"
        );
    }

    #[test]
    fn validates_invocation_and_byte_limits() {
        let mut cfg = DeploymentConfig::default();
        cfg.name = "limits".into();
        cfg.validate().unwrap();

        cfg.limits.max_concurrent_invocations = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("concurrent"));
        cfg.limits.max_concurrent_invocations = 1;
        cfg.limits.max_wall_clock_ms = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("wall_clock"));
        cfg.limits.max_wall_clock_ms = 1;
        cfg.limits.max_request_body_bytes = perch_host_abi::MAX_FRAME_SIZE + 1;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("global ABI frame limit"));
    }

    #[test]
    fn validates_bounded_activation_contract() {
        let mut cfg = DeploymentConfig::default();
        cfg.name = "warm".into();
        cfg.activation.path = Some("/health?deep=1".into());
        cfg.activation.method = "get".into();
        cfg.activation.requests = 2;
        cfg.activation.expected_body_sha256 = Some("ab".repeat(32));
        cfg.validate().unwrap();

        cfg.activation.requests = 65;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("between 1 and 64"));
        cfg.activation.requests = 1;
        cfg.activation.expected_body_sha256 = Some("not-a-digest".into());
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("64 hexadecimal"));
        cfg.activation.expected_body_sha256 = None;
        cfg.activation.path = Some("health".into());
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("start with '/'"));
    }
}
