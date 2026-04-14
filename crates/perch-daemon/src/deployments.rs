//! Deployment supervisor: watches the deployments directory, compiles
//! changes, spawns perch-worker children, and maintains the live
//! `RouterState`.
//!
//! Data flow:
//!
//! 1. At startup, scan `<deployments_dir>/` for subdirectories containing
//!    a `perch.toml` and load each as a deployment. For each, compile
//!    (if needed) and spawn a `perch-worker` child. Build the initial
//!    `RouterState`.
//!
//! 2. After startup, a `notify-debouncer-mini` watcher fires whenever a
//!    file changes anywhere under `deployments_dir`. We debounce
//!    ~500ms, then re-scan the changed deployment, recompile, and swap
//!    in a new worker via the drain-and-replace lifecycle.
//!
//! 3. Deployments that disappear (directory removed) have their worker
//!    killed and their Pull Zone (if any) torn down (Checkpoint 4).
//!
//! The supervisor holds two shared states:
//!
//! - `router: Arc<ArcSwap<RouterState>>` — swapped atomically on every
//!   reload so the HTTP listener always sees a consistent snapshot
//! - `workers: DashMap<String, Arc<WorkerClient>>` — one connection per
//!   deployment; rebuilt during reload

use crate::cdn;
use crate::config::{DeploymentConfig, RuntimeConfig};
use crate::router::{DeploymentRoutes, RouterState};
use crate::worker_client::WorkerClient;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// One loaded, running deployment.
pub struct LiveDeployment {
    pub name: String,
    pub config: DeploymentConfig,
    pub deployment_dir: PathBuf,
    pub dylib_path: PathBuf,
    pub socket_path: PathBuf,
    pub worker_child: tokio::process::Child,
    pub worker_pid: u32,
    pub worker_client: Arc<WorkerClient>,
}

/// Supervisor owns a lock-guarded map of name → LiveDeployment plus a
/// snapshot of the current router state. Reloads mutate both together.
pub struct DeploymentSupervisor {
    runtime_cfg: Arc<RuntimeConfig>,
    live: RwLock<HashMap<String, LiveDeployment>>,
    /// Current router state. Swapped atomically on reload (via RwLock
    /// write). The HTTP listener reads this via `current_router()`.
    router_state: RwLock<Arc<RouterState>>,
}

impl DeploymentSupervisor {
    pub fn new(runtime_cfg: Arc<RuntimeConfig>) -> Self {
        Self {
            runtime_cfg,
            live: RwLock::new(HashMap::new()),
            router_state: RwLock::new(Arc::new(RouterState::default())),
        }
    }

    /// A snapshot of the current router state. Returns an `Arc` so the
    /// caller can hold the snapshot through the life of a single request
    /// without blocking a reload.
    pub async fn current_router(&self) -> Arc<RouterState> {
        self.router_state.read().await.clone()
    }

    /// Fetch the current `WorkerClient` for a deployment.
    pub async fn client_for(&self, deployment: &str) -> Option<Arc<WorkerClient>> {
        self.live
            .read()
            .await
            .get(deployment)
            .map(|d| d.worker_client.clone())
    }

    /// Initial scan: load every deployment under `deployments_dir` and
    /// spawn workers for them. Called once at daemon startup. Failures
    /// for individual deployments are logged but don't block other
    /// deployments from loading.
    pub async fn initial_scan(&self) -> Result<()> {
        let deployments_dir = &self.runtime_cfg.paths.deployments_dir;
        if !deployments_dir.exists() {
            info!(
                dir = %deployments_dir.display(),
                "deployments directory does not exist yet, nothing to load"
            );
            return Ok(());
        }

        let entries = std::fs::read_dir(deployments_dir)
            .with_context(|| format!("reading {:?}", deployments_dir))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if let Err(e) = self.load_deployment(&name).await {
                error!(
                    deployment = %name,
                    error = ?e,
                    "failed to load deployment during initial scan"
                );
            }
        }

        self.rebuild_router_state().await;
        Ok(())
    }

    /// Load (or reload) a single deployment by name. If a worker for
    /// this deployment is already running, this does a drain-and-replace
    /// lifecycle; otherwise it spawns a fresh worker.
    pub async fn load_deployment(&self, name: &str) -> Result<()> {
        let deployment_dir = self.runtime_cfg.paths.deployments_dir.join(name);
        let config = DeploymentConfig::load(&deployment_dir)
            .with_context(|| format!("loading {}/perch.toml", name))?;

        if config.name != name {
            return Err(anyhow!(
                "deployment directory name {:?} does not match perch.toml name {:?}",
                name,
                config.name
            ));
        }

        // Compile (stub for Checkpoint 2 — uses the existing .dylib if
        // one is already present in the compiled_dir, otherwise errors).
        let dylib_path = self.compile_deployment(name, &deployment_dir).await?;

        // Ensure sockets dir exists and pick the socket path.
        std::fs::create_dir_all(&self.runtime_cfg.paths.sockets_dir)
            .with_context(|| {
                format!(
                    "creating sockets dir {:?}",
                    self.runtime_cfg.paths.sockets_dir
                )
            })?;
        let socket_path = self
            .runtime_cfg
            .paths
            .sockets_dir
            .join(format!("{}.sock", name));

        // Remove any stale socket from a crashed worker.
        let _ = std::fs::remove_file(&socket_path);

        // Spawn the worker process.
        let worker_binary = self.locate_worker_binary()?;
        info!(
            deployment = name,
            dylib = %dylib_path.display(),
            socket = %socket_path.display(),
            worker = %worker_binary.display(),
            "spawning perch-worker"
        );

        // Derive the Perry module name from the handler TS file so the
        // worker knows which symbol to dlsym. Perry v0.5 names symbols
        // after the SOURCE filename (e.g. contact.ts → contact_ts), not
        // the dylib filename.
        // Perry v0.5 uses just the LEAF FILENAME (not the dir path) for
        // symbol naming: handlers/contact.ts → perry_fn_contact_ts__handle.
        let module_name = config
            .handlers
            .first()
            .and_then(|h| {
                h.file.file_name()
                    .and_then(|f| f.to_str())
                    .map(|s| {
                        s.replace(|c: char| !c.is_alphanumeric() && c != '_', "_")
                            .trim_start_matches('_')
                            .to_string()
                    })
            });

        let mut cmd = tokio::process::Command::new(&worker_binary);
        cmd.arg("--deployment")
            .arg(name)
            .arg("--dylib")
            .arg(&dylib_path)
            .arg("--sockets-dir")
            .arg(&self.runtime_cfg.paths.sockets_dir)
            .arg("--compiled-dir")
            .arg(&self.runtime_cfg.paths.compiled_dir);

        if let Some(ref mn) = module_name {
            cmd.arg("--module-name").arg(mn);
        }

        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {:?}", worker_binary))?;

        // TODO(checkpoint 2+): pipe stdout/stderr into the daemon's
        // SQLite log store. For now we just let the child write to its
        // own pipes; the daemon reads them via the next tokio task.
        if let Some(stdout) = child.stdout.take() {
            let deployment_name = name.to_string();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stdout);
                let mut line = String::new();
                use tokio::io::AsyncBufReadExt;
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            info!(
                                target: "worker.stdout",
                                deployment = %deployment_name,
                                "{}",
                                line.trim_end()
                            );
                        }
                        Err(e) => {
                            warn!(
                                deployment = %deployment_name,
                                error = ?e,
                                "reading worker stdout"
                            );
                            break;
                        }
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let deployment_name = name.to_string();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stderr);
                let mut line = String::new();
                use tokio::io::AsyncBufReadExt;
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            warn!(
                                target: "worker.stderr",
                                deployment = %deployment_name,
                                "{}",
                                line.trim_end()
                            );
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Wait for the worker to bind its socket.
        self.wait_for_socket(&socket_path, Duration::from_secs(5))
            .await?;

        // Connect the worker client.
        let worker_client = Arc::new(WorkerClient::connect(name, &socket_path).await?);

        // Capture CDN opt-in before moving config into the live struct.
        let cdn_enabled = config.cdn.enabled;
        let config_for_cdn = config.clone();

        // Drain the previous instance if any. If replacing an existing
        // deployment, also purge the Bunny cache so stale content gets
        // evicted from edge POPs.
        {
            let mut live = self.live.write().await;
            if let Some(old) = live.remove(name) {
                self.drain_live_deployment(old).await;

                // Purge Bunny cache on redeploy.
                if let Some(bunny_cfg) = &self.runtime_cfg.cdn.bunny {
                    if let Err(e) = cdn::purge_deployment(bunny_cfg, name).await {
                        warn!(
                            deployment = %name,
                            error = ?e,
                            "CDN cache purge failed (stale content may persist until TTL expires)"
                        );
                    }
                }
            }

            let worker_pid = child.id().unwrap_or(0);
            live.insert(
                name.to_string(),
                LiveDeployment {
                    name: name.to_string(),
                    config,
                    deployment_dir,
                    dylib_path,
                    socket_path,
                    worker_child: child,
                    worker_pid,
                    worker_client,
                },
            );
        }

        // Bunny CDN: reconcile the Pull Zone for this deployment.
        if let Some(bunny_cfg) = &self.runtime_cfg.cdn.bunny {
            if cdn_enabled {
                let origin_port: u16 = self
                    .runtime_cfg
                    .http
                    .listen_origin
                    .rsplit(':')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(8081);
                // Use the box's public hostname as origin. In production
                // this comes from runtime.toml; for now fall back to
                // 127.0.0.1 (development).
                let origin_host = "127.0.0.1";

                if let Err(e) = cdn::reconcile_deployment(
                    bunny_cfg,
                    &config_for_cdn,
                    origin_host,
                    origin_port,
                )
                .await
                {
                    warn!(
                        deployment = %name,
                        error = ?e,
                        "Bunny CDN reconciliation failed (deployment still works without CDN)"
                    );
                }
            }
        }

        self.rebuild_router_state().await;
        Ok(())
    }

    /// Locate the perch-worker binary. Tries in order: runtime.toml
    /// explicit path, `$PATH`, the daemon's own directory.
    fn locate_worker_binary(&self) -> Result<PathBuf> {
        if let Some(p) = &self.runtime_cfg.paths.perch_worker_binary {
            return Ok(p.clone());
        }

        // Try the same directory as the daemon binary itself.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join("perch-worker");
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }

        // Fall back to PATH.
        Ok(PathBuf::from("perch-worker"))
    }

    /// Compile a deployment's handler TS files into a shared library via
    /// `perry compile --output-type dylib`. If the dylib already exists
    /// and is newer than the source files, skip compilation (incremental).
    async fn compile_deployment(
        &self,
        name: &str,
        deployment_dir: &Path,
    ) -> Result<PathBuf> {
        let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
        let dylib_path = self
            .runtime_cfg
            .paths
            .compiled_dir
            .join(format!("{}.{}", name, ext));

        // Collect all TS handler files from the deployment dir.
        let mut ts_files: Vec<PathBuf> = Vec::new();
        Self::collect_ts_files(deployment_dir, &mut ts_files);

        if ts_files.is_empty() {
            // No TS files — check if a pre-compiled dylib exists (e.g.
            // placed manually or from a previous compile).
            if dylib_path.exists() {
                info!(
                    deployment = name,
                    dylib = %dylib_path.display(),
                    "no TS files found, using existing compiled dylib"
                );
                return Ok(dylib_path);
            }
            return Err(anyhow!(
                "deployment {} has no .ts files and no pre-compiled dylib at {:?}",
                name,
                dylib_path
            ));
        }

        // Check if recompilation is needed (any source newer than dylib).
        let needs_compile = if dylib_path.exists() {
            let dylib_mtime = std::fs::metadata(&dylib_path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            ts_files.iter().any(|f| {
                std::fs::metadata(f)
                    .and_then(|m| m.modified())
                    .map(|t| t > dylib_mtime)
                    .unwrap_or(true)
            })
        } else {
            true
        };

        if !needs_compile {
            info!(
                deployment = name,
                dylib = %dylib_path.display(),
                "dylib is up-to-date, skipping compile"
            );
            return Ok(dylib_path);
        }

        let perry_binary = &self.runtime_cfg.paths.perry_binary;

        // Perry compiles all input files into a single dylib.
        // The first handler's filename determines the symbol names.
        let ts_args: Vec<String> = ts_files
            .iter()
            .map(|f| f.display().to_string())
            .collect();

        info!(
            deployment = name,
            perry = %perry_binary.display(),
            files = ?ts_args,
            output = %dylib_path.display(),
            "compiling deployment"
        );

        let output = tokio::process::Command::new(perry_binary.as_os_str())
            .arg("compile")
            .arg("--output-type")
            .arg("dylib")
            .arg("-o")
            .arg(&dylib_path)
            .args(&ts_args)
            .current_dir(deployment_dir)
            .output()
            .await
            .with_context(|| format!(
                "spawning perry compile for deployment {} (binary: {:?})",
                name,
                perry_binary
            ))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(anyhow!(
                "perry compile failed for deployment {} (exit {})\nstderr: {}\nstdout: {}",
                name,
                output.status,
                stderr.trim(),
                stdout.trim(),
            ));
        }

        if !dylib_path.exists() {
            return Err(anyhow!(
                "perry compile succeeded but dylib {:?} was not created",
                dylib_path
            ));
        }

        info!(
            deployment = name,
            dylib = %dylib_path.display(),
            "compilation succeeded"
        );

        Ok(dylib_path)
    }

    /// Recursively collect all .ts files under a directory.
    fn collect_ts_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip node_modules and hidden dirs.
                let name = entry.file_name();
                let name_str = name.to_str().unwrap_or("");
                if name_str.starts_with('.') || name_str == "node_modules" || name_str == "migrations" || name_str == "static" {
                    continue;
                }
                Self::collect_ts_files(&path, out);
            } else if path.extension().map(|e| e == "ts").unwrap_or(false) {
                out.push(path);
            }
        }
    }

    /// Wait for a worker to bind its socket. Polls existence up to the
    /// given timeout.
    async fn wait_for_socket(&self, socket_path: &Path, timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            if socket_path.exists() {
                return Ok(());
            }
            if start.elapsed() > timeout {
                return Err(anyhow!(
                    "timed out waiting for worker socket {:?} to appear",
                    socket_path
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Drain-and-kill an old LiveDeployment. Called during reload before
    /// the new worker becomes active.
    async fn drain_live_deployment(&self, mut live: LiveDeployment) {
        info!(
            deployment = %live.name,
            "draining previous worker instance"
        );

        // Ask the worker to drain gracefully.
        let _ = live
            .worker_client
            .shutdown(Duration::from_secs(10))
            .await;

        // Give the child a moment to exit on its own. If it's still
        // around after the grace period, kill it.
        let kill_deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(_)) = live.worker_child.try_wait() {
                break;
            }
            if std::time::Instant::now() > kill_deadline {
                warn!(
                    deployment = %live.name,
                    "worker did not exit within grace period, sending SIGKILL"
                );
                let _ = live.worker_child.kill().await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Clean up the socket file.
        let _ = std::fs::remove_file(&live.socket_path);
    }

    /// Rebuild the router state from the current `live` map.
    async fn rebuild_router_state(&self) {
        let live = self.live.read().await;
        let routes: Vec<DeploymentRoutes> = live
            .values()
            .map(|d| {
                DeploymentRoutes::from_config(&d.config, d.deployment_dir.clone())
            })
            .collect();

        let new_state = Arc::new(RouterState::build(routes));
        *self.router_state.write().await = new_state.clone();

        info!(
            deployments = new_state.deployment_count(),
            "router state rebuilt"
        );
    }

    /// Read a process's RSS in MB from /proc/<pid>/status (Linux only).
    /// Returns None on macOS (and other non-Linux platforms) — the RSS
    /// watchdog is a Linux-only feature for now.
    #[cfg(target_os = "linux")]
    fn read_rss_mb(pid: u32) -> Option<u64> {
        let path = format!("/proc/{}/status", pid);
        let content = std::fs::read_to_string(&path).ok()?;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let n: u64 = rest
                    .trim()
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()?;
                return Some(n / 1024); // KB → MB
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(dead_code)]
    fn read_rss_mb(_pid: u32) -> Option<u64> {
        None
    }

    /// Background task that periodically checks each worker's RSS and
    /// restarts any worker that exceeds its `max_worker_rss_mb` limit.
    /// Bounded the impact of ballooning deployments (e.g. one that
    /// builds large response buffers).
    pub fn spawn_rss_watchdog(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let to_restart: Vec<(String, u32, u64, u32)> = {
                    let live = self.live.read().await;
                    live.iter()
                        .filter_map(|(name, d)| {
                            let limit = d.config.limits.max_worker_rss_mb;
                            let rss = Self::read_rss_mb(d.worker_pid)?;
                            if rss > limit as u64 {
                                Some((name.clone(), d.worker_pid, rss, limit))
                            } else {
                                None
                            }
                        })
                        .collect()
                };
                for (name, pid, rss, limit) in to_restart {
                    warn!(
                        deployment = %name,
                        pid,
                        rss_mb = rss,
                        limit_mb = limit,
                        "worker RSS exceeded limit, restarting"
                    );
                    if let Err(e) = self.load_deployment(&name).await {
                        error!(deployment = %name, error = ?e, "RSS-triggered restart failed");
                    }
                }
            }
        })
    }

    /// Remove a deployment entirely (directory deleted, or explicit
    /// admin action). Drains the worker, tears down the Bunny Pull Zone,
    /// and rebuilds router state.
    pub async fn remove_deployment(&self, name: &str) {
        let old = {
            let mut live = self.live.write().await;
            live.remove(name)
        };
        if let Some(live) = old {
            self.drain_live_deployment(live).await;
        }

        // Bunny CDN: tear down the Pull Zone.
        if let Some(bunny_cfg) = &self.runtime_cfg.cdn.bunny {
            if let Err(e) = cdn::teardown_deployment(bunny_cfg, name).await {
                warn!(
                    deployment = %name,
                    error = ?e,
                    "failed to tear down Bunny Pull Zone"
                );
            }
        }

        self.rebuild_router_state().await;
    }
}
