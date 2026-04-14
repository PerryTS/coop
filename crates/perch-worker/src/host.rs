//! Deployment host — owns the loaded plugin and the per-request dispatch.
//!
//! `DeploymentHost` is the glue between the Unix socket listener
//! (`listener.rs`) and perry-runtime's plugin API (`plugin_host.rs`). A
//! single instance lives for the lifetime of the worker process; it holds
//! the `LoadedPlugin` and provides async-friendly wrappers for the three
//! operations a worker needs: HTTP dispatch, cron fire, queue message.

use crate::plugin_host::{load_deployment, LoadedPlugin};
use anyhow::{anyhow, Context, Result};
use perch_host_abi::{
    CronContext, DeploymentRequest, DeploymentResponse, QueueDisposition, QueueMessage,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

/// A loaded deployment, ready to dispatch requests to.
///
/// Safety & threading: all plugin invocations go through perry-runtime's
/// closure-call path, which uses a global registry and thread-local arena.
/// For the MVP we serialize plugin calls on a single dedicated thread to
/// avoid any threading hazards in perry-runtime. `dispatch_*` methods use
/// `tokio::task::spawn_blocking` internally so the async tokio runtime
/// isn't blocked while a plugin call runs.
pub struct DeploymentHost {
    deployment: String,
    /// Mutex because LoadedPlugin::invoke_handle takes &mut self (it calls
    /// ensure_initialized lazily). All calls go through spawn_blocking so
    /// contention is serial anyway.
    inner: Arc<Mutex<LoadedPlugin>>,
}

impl DeploymentHost {
    /// Load the deployment dylib. In v0.5, this dlopens the .dylib and
    /// looks up the `perry_fn_<module>__handle` symbol by name.
    ///
    /// `module_name_override`: if set, use this instead of deriving
    /// the module name from the dylib filename. Used when the daemon
    /// compiled a specific TS file (e.g. `contact.ts`) into a
    /// deployment-named dylib (e.g. `landing.dylib`).
    pub fn load(deployment: &str, dylib_path: &Path, module_name_override: Option<&str>) -> Result<Self> {
        let mut plugin = if let Some(name) = module_name_override {
            crate::plugin_host::LoadedPlugin::load(dylib_path, name)
                .with_context(|| format!("deployment={} module={}", deployment, name))?
        } else {
            load_deployment(dylib_path)
                .with_context(|| format!("deployment={}", deployment))?
        };
        // Initialize GC + string constants eagerly so the first request
        // doesn't pay the init cost.
        plugin.ensure_initialized();
        tracing::info!(
            deployment = deployment,
            plugin_name = plugin.name(),
            "deployment dylib loaded (v0.5 direct-call model)"
        );
        Ok(Self {
            deployment: deployment.to_string(),
            inner: Arc::new(Mutex::new(plugin)),
        })
    }

    pub fn deployment(&self) -> &str {
        &self.deployment
    }

    /// Dispatch an HTTP request to the deployment's registered `"route"`
    /// tool. The request is serialized to JSON and passed as the tool
    /// argument; the tool's return value is decoded as a
    /// `DeploymentResponse`.
    pub async fn dispatch(&self, request: DeploymentRequest) -> Result<DeploymentResponse> {
        let inner = self.inner.clone();
        let deployment = self.deployment.clone();
        tokio::task::spawn_blocking(move || {
            let request_json = serde_json::to_string(&request)
                .context("failed to serialize DeploymentRequest")?;

            debug!(
                deployment = %deployment,
                method = %request.method,
                path = %request.path,
                "dispatching to deployment handle function"
            );

            let mut plugin = inner.lock().map_err(|e| anyhow!("plugin lock poisoned: {}", e))?;
            let result = plugin
                .invoke_handle(&request_json)
                .context("invoking deployment handle function")?;

            match result {
                Some(response_json) => {
                    let response: DeploymentResponse = serde_json::from_str(&response_json)
                        .with_context(|| {
                            format!(
                                "deployment handle returned invalid JSON: {}",
                                truncate(&response_json, 200)
                            )
                        })?;
                    Ok(response)
                }
                None => {
                    warn!(
                        deployment = %deployment,
                        "handle function not found or returned non-string; responding 500"
                    );
                    Ok(DeploymentResponse {
                        status: 500,
                        headers: Default::default(),
                        body_base64: base64_encode(
                            b"perch: deployment has no 'handle' export",
                        ),
                    })
                }
            }
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join failed: {e}"))?
    }

    /// Fire a cron invocation. In v0.5, cron handlers are separate exported
    /// functions (`perry_fn_<module>__run`). For the MVP we route through
    /// the same `handle` function with a special `method = "CRON"` marker
    /// and the CronContext as the body. The deployment's @perch/runtime
    /// library dispatches based on method.
    pub async fn fire_cron(&self, context: CronContext) -> Result<()> {
        let inner = self.inner.clone();
        let deployment = self.deployment.clone();
        tokio::task::spawn_blocking(move || {
            let ctx_json = serde_json::to_string(&context)?;
            debug!(
                deployment = %deployment,
                expr = %context.expression,
                "firing cron"
            );
            let mut plugin = inner.lock().map_err(|e| anyhow!("lock: {}", e))?;
            let _ = plugin.invoke_handle(&ctx_json)?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join failed: {e}"))?
    }

    /// Deliver a queue message.
    pub async fn deliver_queue_message(
        &self,
        message: QueueMessage,
    ) -> Result<QueueDisposition> {
        let inner = self.inner.clone();
        let deployment = self.deployment.clone();
        tokio::task::spawn_blocking(move || {
            let msg_json = serde_json::to_string(&message)?;
            debug!(
                deployment = %deployment,
                queue = %message.queue_name,
                message_id = %message.message_id,
                attempt = message.attempt,
                "delivering queue message"
            );

            let mut plugin = inner.lock().map_err(|e| anyhow!("lock: {}", e))?;
            let result = plugin.invoke_handle(&msg_json)?;

            match result {
                Some(s) => {
                    #[derive(serde::Deserialize)]
                    struct Reply {
                        disposition: QueueDisposition,
                    }
                    match serde_json::from_str::<Reply>(&s) {
                        Ok(r) => Ok(r.disposition),
                        Err(_) => Ok(QueueDisposition::Nack),
                    }
                }
                None => Ok(QueueDisposition::Nack),
            }
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join failed: {e}"))?
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...(truncated, {} bytes total)", &s[..max], s.len())
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
