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
    pub http: HttpConfig,

    #[serde(default)]
    pub paths: PathsConfig,

    #[serde(default)]
    pub postgres: Option<PostgresConfig>,

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
        Ok(cfg)
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            http: HttpConfig::default(),
            paths: PathsConfig::default(),
            postgres: None,
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

    #[serde(rename = "handlers", default)]
    pub handlers: Vec<HandlerConfig>,

    #[serde(rename = "static", default)]
    pub static_blocks: Vec<StaticConfig>,

    #[serde(rename = "crons", default)]
    pub crons: Vec<CronConfig>,

    #[serde(rename = "queues", default)]
    pub queues: Vec<QueueConfig>,

    #[serde(default)]
    pub capabilities: CapabilitiesConfig,

    #[serde(default)]
    pub limits: DeploymentLimitsConfig,
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
    /// Optional explicit tool name the deployment registers for this
    /// handler. If omitted, the daemon derives a name from the file path
    /// (e.g., `handlers/contact.ts` → `handlers_contact`). The routing
    /// tool contract is documented in `perch_host_abi::DEPLOYMENT_ROUTE_TOOL`:
    /// the MVP convention is ONE tool named `"route"` per deployment that
    /// internally dispatches based on `req.method` + `req.path`.
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
    #[serde(default)]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    pub file: PathBuf,
    pub name: String,
    #[serde(default = "default_queue_concurrency")]
    pub concurrency: u32,
    #[serde(default = "default_queue_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub tool: Option<String>,
}

fn default_queue_concurrency() -> u32 {
    1
}
fn default_queue_max_retries() -> u32 {
    5
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
    /// Hard RSS limit per worker process in MB. When exceeded, the
    /// daemon kills + respawns the worker. Default 512 MB. This bounds
    /// the impact of a ballooning deployment (test5 grew to 196 MB
    /// during stress testing because Perry's arena allocator doesn't
    /// release pages back to the OS aggressively).
    #[serde(default = "default_max_worker_rss_mb")]
    pub max_worker_rss_mb: u32,
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
fn default_max_worker_rss_mb() -> u32 {
    512
}

impl Default for DeploymentLimitsConfig {
    fn default() -> Self {
        Self {
            max_memory_mb_per_invocation: default_max_memory_mb(),
            max_wall_clock_ms: default_max_wall_clock_ms(),
            max_concurrent_invocations: default_max_concurrent_invocations(),
            max_worker_rss_mb: default_max_worker_rss_mb(),
        }
    }
}

impl DeploymentConfig {
    /// Load `perch.toml` from a deployment directory. The file must be
    /// named `perch.toml` and live directly under `deployment_dir`.
    pub fn load(deployment_dir: &Path) -> Result<Self> {
        let toml_path = deployment_dir.join("perch.toml");
        if !toml_path.exists() {
            return Err(anyhow!(
                "deployment {:?} has no perch.toml",
                deployment_dir
            ));
        }
        let contents = std::fs::read_to_string(&toml_path)
            .with_context(|| format!("reading {:?}", toml_path))?;
        let cfg: Self = toml::from_str(&contents)
            .with_context(|| format!("parsing {:?}", toml_path))?;
        Ok(cfg)
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
            let method = h
                .method
                .as_deref()
                .unwrap_or("")
                .to_uppercase();
            let tool = h
                .tool
                .clone()
                .unwrap_or_else(|| Self::default_tool_name(&h.file));
            map.insert((method, h.path.clone()), tool);
        }
        map
    }
}

impl Default for DeploymentConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: None,
            hosts: HostsConfig::default(),
            cdn: DeploymentCdnConfig::default(),
            database: None,
            handlers: Vec::new(),
            static_blocks: Vec::new(),
            crons: Vec::new(),
            queues: Vec::new(),
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
        // Defaults for unset fields
        assert_eq!(cfg.logs.retention_days, 30);
    }

    #[test]
    fn loads_missing_runtime_toml_as_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.toml");
        let cfg = RuntimeConfig::load(&path).unwrap();
        assert_eq!(cfg.http.listen_http, "0.0.0.0:80");
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
}
