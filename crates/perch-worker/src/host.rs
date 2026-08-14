//! Thread-affine host for one preloaded Perry application library.
//!
//! Perry owns substantial thread-local runtime state. A library must therefore
//! be loaded, initialized, invoked, pumped, and unloaded on the same OS thread.
//! `DeploymentHost` enforces that rule by keeping `LoadedPlugin` inside a
//! dedicated executor thread and exchanging owned Rust values over a channel.

use crate::plugin_host::{load_deployment, LoadedPlugin};
use anyhow::{anyhow, Context, Result};
use perch_host_abi::{
    decode_cron_response, decode_http_response, decode_queue_response, encode_cron_request,
    encode_http_request, encode_queue_request, CronContext, DeploymentRequest, DeploymentResponse,
    HttpDispatchRequest, HttpDispatchResponse, QueueDispatchMessage, QueueDisposition,
    QueueMessage,
};
use std::path::{Path, PathBuf};
use std::sync::{mpsc as std_mpsc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

type Reply<T> = oneshot::Sender<std::result::Result<T, String>>;
const APP_WARM_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_EXECUTOR_STACK_SIZE_BYTES: usize = 1024 * 1024;
pub const DEFAULT_COMMAND_QUEUE_CAPACITY: usize = 256;
// Host-forced full collection is opt-in until the realistic imported-handler
// promotion gate is green on the pinned Perry provider. The tiny ABI fixture
// is safe, but the Worker-shaped JSON fixture currently corrupts a response
// after repeated synchronous full collections on Linux/aarch64.
pub const DEFAULT_GC_RECLAIM_CHECK_INTERVAL: usize = 0;
pub const DEFAULT_GC_RECLAIM_GROWTH_BYTES: u64 = 256 * 1024;
const MIN_EXECUTOR_STACK_SIZE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct DeploymentHostOptions {
    pub executor_stack_size_bytes: usize,
    pub command_queue_capacity: usize,
    /// Sample Perry arena growth after this many completed invocations. Zero
    /// disables host-boundary reclaim pacing and is the production default
    /// until Perry passes the imported-handler full-GC promotion gate.
    pub gc_reclaim_check_interval: usize,
    /// Minimum uncollected growth above the last full-GC live baseline before
    /// a precise host-boundary full collection is requested.
    pub gc_reclaim_growth_bytes: u64,
    /// Opaque host-assigned deployment ID used only by provider callbacks.
    pub deployment_context_id: u64,
}

impl Default for DeploymentHostOptions {
    fn default() -> Self {
        Self {
            executor_stack_size_bytes: DEFAULT_EXECUTOR_STACK_SIZE_BYTES,
            command_queue_capacity: DEFAULT_COMMAND_QUEUE_CAPACITY,
            gc_reclaim_check_interval: DEFAULT_GC_RECLAIM_CHECK_INTERVAL,
            gc_reclaim_growth_bytes: DEFAULT_GC_RECLAIM_GROWTH_BYTES,
            deployment_context_id: 0,
        }
    }
}

impl DeploymentHostOptions {
    fn validate(self) -> Result<Self> {
        if self.executor_stack_size_bytes < MIN_EXECUTOR_STACK_SIZE_BYTES {
            return Err(anyhow!(
                "Perry executor stack size must be at least {MIN_EXECUTOR_STACK_SIZE_BYTES} bytes"
            ));
        }
        if self.command_queue_capacity == 0 {
            return Err(anyhow!(
                "Perry executor command queue capacity must be positive"
            ));
        }
        if self.gc_reclaim_check_interval > 0 && self.gc_reclaim_growth_bytes == 0 {
            return Err(anyhow!(
                "Perry GC reclaim growth must be positive when pacing is enabled"
            ));
        }
        Ok(self)
    }
}

enum ExecutorCommand {
    DispatchHttp {
        request: HttpDispatchRequest,
        reply: Reply<HttpDispatchResponse>,
    },
    Cron {
        context: CronContext,
        reply: Reply<()>,
    },
    Queue {
        message: QueueDispatchMessage,
        reply: Reply<QueueDisposition>,
    },
    MemoryStats {
        reply: Reply<ApplicationMemoryStats>,
    },
    MemoryPressure {
        level: u32,
        reply: Reply<u32>,
    },
    Shutdown {
        reply: Option<oneshot::Sender<()>>,
    },
}

/// Memory directly attributable to one application's thread-local Perry heap.
/// Process RSS also contains shared providers, stacks, code pages, allocator
/// retention, and the daemon, so it cannot provide this attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplicationMemoryStats {
    pub arena_live_bytes: u64,
    pub arena_reserved_bytes: u64,
}

/// A loaded deployment whose Perry state is permanently pinned to one thread.
pub struct DeploymentHost {
    deployment: String,
    plugin_name: String,
    tx: mpsc::Sender<ExecutorCommand>,
    executor: Mutex<Option<JoinHandle<()>>>,
}

impl DeploymentHost {
    /// Spawn the application executor, load the dylib on it, and wait until
    /// `perry_module_init` has completed. Returning `Ok` means the app is warm
    /// and no loader or module-initialization work remains on the request path.
    pub fn load(
        deployment: &str,
        dylib_path: &Path,
        module_name_override: Option<&str>,
    ) -> Result<Self> {
        Self::load_with_options(
            deployment,
            dylib_path,
            module_name_override,
            DeploymentHostOptions::default(),
        )
    }

    pub fn load_with_options(
        deployment: &str,
        dylib_path: &Path,
        module_name_override: Option<&str>,
        options: DeploymentHostOptions,
    ) -> Result<Self> {
        let total_started = Instant::now();
        let options = options.validate()?;
        if !crate::runtime_libraries_loaded() {
            return Err(anyhow!(
                "Perry runtime libraries must be initialized before loading deployment {deployment}"
            ));
        }

        let deployment_owned = deployment.to_string();
        let thread_deployment = deployment_owned.clone();
        let dylib_path = PathBuf::from(dylib_path);
        let module_name = module_name_override.map(str::to_owned);
        let (tx, rx) = mpsc::channel(options.command_queue_capacity);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);

        let executor = std::thread::Builder::new()
            .name(format!("perch-app-{deployment}"))
            .stack_size(options.executor_stack_size_bytes)
            .spawn(move || {
                crate::runtime_libraries::set_deployment_context(options.deployment_context_id);
                let load_started = Instant::now();
                let loaded = if let Some(name) = module_name.as_deref() {
                    LoadedPlugin::load(&dylib_path, name)
                        .with_context(|| format!("deployment={thread_deployment} module={name}"))
                } else {
                    load_deployment(&dylib_path)
                        .with_context(|| format!("deployment={thread_deployment}"))
                };
                let load_ms = load_started.elapsed().as_secs_f64() * 1_000.0;

                let mut plugin = match loaded {
                    Ok(mut plugin) => {
                        let init_started = Instant::now();
                        plugin.ensure_initialized();
                        let init_ms = init_started.elapsed().as_secs_f64() * 1_000.0;
                        let name = plugin.name().to_string();
                        let _ = ready_tx.send(Ok((name, load_ms, init_ms)));
                        plugin
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("{error:#}")));
                        return;
                    }
                };

                run_executor(&thread_deployment, &mut plugin, rx, options);
                crate::runtime_libraries::clear_deployment_context();
                // Perry emits this cleanup from executable exit epilogues, but
                // an application library never executes one.
                crate::runtime_libraries::release_current_thread_runtime_state();
                // `plugin` is dropped here, on the same thread that loaded it.
            })
            .with_context(|| format!("spawning Perry executor for {deployment}"))?;

        let (plugin_name, load_ms, init_ms) = match ready_rx.recv_timeout(APP_WARM_TIMEOUT) {
            Ok(Ok(ready)) => ready,
            Ok(Err(error)) => {
                let _ = executor.join();
                return Err(anyhow!(error));
            }
            Err(error) => {
                // Initialization may still complete after the timeout. Queue
                // shutdown before joining so that late success cannot leave
                // this caller waiting forever in `run_executor`.
                let _ = tx.try_send(ExecutorCommand::Shutdown { reply: None });
                let _ = executor.join();
                return Err(anyhow!(
                    "Perry executor for {deployment} did not initialize: {error}"
                ));
            }
        };

        tracing::info!(
            deployment,
            plugin_name,
            load_ms,
            init_ms,
            executor_stack_size_bytes = options.executor_stack_size_bytes,
            command_queue_capacity = options.command_queue_capacity,
            gc_reclaim_check_interval = options.gc_reclaim_check_interval,
            gc_reclaim_growth_bytes = options.gc_reclaim_growth_bytes,
            total_ms = total_started.elapsed().as_secs_f64() * 1_000.0,
            "application library preloaded on dedicated Perry thread"
        );

        Ok(Self {
            deployment: deployment_owned,
            plugin_name,
            tx,
            executor: Mutex::new(Some(executor)),
        })
    }

    pub fn deployment(&self) -> &str {
        &self.deployment
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Commands waiting for this application executor. The currently running
    /// command is not included.
    pub fn queue_depth(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    pub fn queue_capacity(&self) -> usize {
        self.tx.max_capacity()
    }

    pub async fn dispatch(&self, request: DeploymentRequest) -> Result<DeploymentResponse> {
        let response = self
            .dispatch_http(HttpDispatchRequest {
                method: request.method,
                path: request.path,
                query: request.query,
                headers: request.headers.into_iter().collect(),
                remote_addr: request.remote_addr,
                scheme: request.scheme,
                host: request.host,
                body: base64_decode(&request.body_base64)
                    .context("decoding worker HTTP request body")?,
            })
            .await?;
        Ok(DeploymentResponse {
            status: response.status,
            headers: response.headers.into_iter().collect(),
            body_base64: base64_encode(&response.body),
        })
    }

    pub async fn dispatch_http(
        &self,
        request: HttpDispatchRequest,
    ) -> Result<HttpDispatchResponse> {
        let (reply, response) = oneshot::channel();
        self.tx
            .try_send(ExecutorCommand::DispatchHttp { request, reply })
            .map_err(|error| self.enqueue_error(error))?;
        response
            .await
            .map_err(|_| anyhow!("Perry executor dropped its HTTP dispatch reply"))?
            .map_err(anyhow::Error::msg)
    }

    pub async fn fire_cron(&self, context: CronContext) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx
            .try_send(ExecutorCommand::Cron { context, reply })
            .map_err(|error| self.enqueue_error(error))?;
        response
            .await
            .map_err(|_| anyhow!("Perry executor dropped its cron reply"))?
            .map_err(anyhow::Error::msg)
    }

    pub async fn deliver_queue_message(&self, message: QueueMessage) -> Result<QueueDisposition> {
        self.deliver_queue(QueueDispatchMessage {
            queue_name: message.queue_name,
            message_id: message.message_id,
            attempt: message.attempt,
            max_retries: message.max_retries,
            payload: base64_decode(&message.payload_base64)
                .context("decoding worker queue payload")?,
        })
        .await
    }

    /// Deliver an in-process queue message without JSON/Base64 conversion.
    pub async fn deliver_queue(&self, message: QueueDispatchMessage) -> Result<QueueDisposition> {
        let (reply, response) = oneshot::channel();
        self.tx
            .try_send(ExecutorCommand::Queue { message, reply })
            .map_err(|error| self.enqueue_error(error))?;
        response
            .await
            .map_err(|_| anyhow!("Perry executor dropped its queue reply"))?
            .map_err(anyhow::Error::msg)
    }

    /// Query this application's thread-local Perry arena on its executor.
    /// Intended for diagnostics and capacity measurements, not the hot path.
    pub async fn memory_stats(&self) -> Result<ApplicationMemoryStats> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ExecutorCommand::MemoryStats { reply })
            .await
            .map_err(|_| anyhow!("Perry executor for {} has stopped", self.deployment))?;
        response
            .await
            .map_err(|_| anyhow!("Perry executor dropped its memory-statistics reply"))?
            .map_err(anyhow::Error::msg)
    }

    /// Ask Perry to collect at a thread-safe host boundary. Level 1 requests
    /// an ordinary collection and level 2 requests a full old-generation
    /// reclaim. The command runs on the deployment's executor after all raw
    /// request/response Buffer pointers have left the host call stack.
    pub async fn memory_pressure(&self, level: u32) -> Result<u32> {
        if level == 0 {
            return Err(anyhow!("Perry memory-pressure level must be positive"));
        }
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ExecutorCommand::MemoryPressure { level, reply })
            .await
            .map_err(|_| anyhow!("Perry executor for {} has stopped", self.deployment))?;
        response
            .await
            .map_err(|_| anyhow!("Perry executor dropped its memory-pressure reply"))?
            .map_err(anyhow::Error::msg)
    }

    fn enqueue_error(&self, error: mpsc::error::TrySendError<ExecutorCommand>) -> anyhow::Error {
        match error {
            mpsc::error::TrySendError::Full(_) => anyhow!(
                "Perry executor queue for {} is full (capacity {})",
                self.deployment,
                self.tx.max_capacity()
            ),
            mpsc::error::TrySendError::Closed(_) => {
                anyhow!("Perry executor for {} has stopped", self.deployment)
            }
        }
    }

    /// Drain commands already queued for this app and unload its library.
    pub async fn shutdown(&self) -> Result<()> {
        let handle = self
            .executor
            .lock()
            .map_err(|_| anyhow!("Perry executor join lock poisoned"))?
            .take();
        let Some(handle) = handle else {
            return Ok(());
        };

        let (reply, response) = oneshot::channel();
        if self
            .tx
            .send(ExecutorCommand::Shutdown { reply: Some(reply) })
            .await
            .is_ok()
        {
            let _ = response.await;
        }

        tokio::task::spawn_blocking(move || handle.join())
            .await
            .map_err(|error| anyhow!("joining Perry executor task: {error}"))?
            .map_err(|_| anyhow!("Perry executor panicked"))?;
        Ok(())
    }
}

impl Drop for DeploymentHost {
    fn drop(&mut self) {
        let Ok(handle) = self.executor.get_mut() else {
            return;
        };
        if let Some(handle) = handle.take() {
            let _ = self.tx.try_send(ExecutorCommand::Shutdown { reply: None });
            // Dropping a JoinHandle detaches it. The shutdown command is FIFO,
            // so the executor still drains earlier work and unloads cleanly.
            drop(handle);
        }
    }
}

fn run_executor(
    deployment: &str,
    plugin: &mut LoadedPlugin,
    mut rx: mpsc::Receiver<ExecutorCommand>,
    options: DeploymentHostOptions,
) {
    let mut dispatch_metrics = DispatchMetrics::default();
    let mut gc_pacer = HostGcPacer::new(options);
    while let Some(command) = rx.blocking_recv() {
        match command {
            ExecutorCommand::DispatchHttp { request, reply } => {
                let result =
                    dispatch_http_on_executor(deployment, plugin, request, &mut dispatch_metrics)
                        .map_err(|error| format!("{error:#}"));
                let _ = reply.send(result);
                gc_pacer.after_invocation(deployment);
            }
            ExecutorCommand::Cron { context, reply } => {
                let result = fire_cron_on_executor(deployment, plugin, context)
                    .map_err(|error| format!("{error:#}"));
                let _ = reply.send(result);
                gc_pacer.after_invocation(deployment);
            }
            ExecutorCommand::Queue { message, reply } => {
                let result = deliver_queue_on_executor(deployment, plugin, message)
                    .map_err(|error| format!("{error:#}"));
                let _ = reply.send(result);
                gc_pacer.after_invocation(deployment);
            }
            ExecutorCommand::MemoryStats { reply } => {
                let (arena_live_bytes, arena_reserved_bytes) =
                    crate::runtime_libraries::current_thread_arena_stats();
                let _ = reply.send(Ok(ApplicationMemoryStats {
                    arena_live_bytes,
                    arena_reserved_bytes,
                }));
            }
            ExecutorCommand::MemoryPressure { level, reply } => {
                let result = unsafe {
                    (crate::runtime_libraries::runtime_api().js_gc_memory_pressure)(level)
                };
                if result == 2 {
                    gc_pacer.rebaseline();
                }
                let _ = reply.send(Ok(result));
            }
            ExecutorCommand::Shutdown { reply } => {
                if let Some(reply) = reply {
                    let _ = reply.send(());
                }
                break;
            }
        }
    }
}

struct HostGcPacer {
    check_interval: usize,
    growth_floor_bytes: u64,
    invocations: u64,
    live_baseline_bytes: u64,
}

impl HostGcPacer {
    fn new(options: DeploymentHostOptions) -> Self {
        let (live_baseline_bytes, _) = crate::runtime_libraries::current_thread_arena_stats();
        Self {
            check_interval: options.gc_reclaim_check_interval,
            growth_floor_bytes: options.gc_reclaim_growth_bytes,
            invocations: 0,
            live_baseline_bytes,
        }
    }

    fn rebaseline(&mut self) {
        let (live, _) = crate::runtime_libraries::current_thread_arena_stats();
        self.live_baseline_bytes = live;
    }

    fn after_invocation(&mut self, deployment: &str) {
        if self.check_interval == 0 {
            return;
        }
        self.invocations = self.invocations.saturating_add(1);
        if !self.invocations.is_multiple_of(self.check_interval as u64) {
            return;
        }

        let (before_live, before_reserved) = crate::runtime_libraries::current_thread_arena_stats();
        // A proportional band avoids quadratic full-GC work for applications
        // that intentionally retain a growing live set. Tiny transient heaps
        // use the configured floor; large live heaps receive 50% headroom.
        let growth_band = self.growth_floor_bytes.max(self.live_baseline_bytes / 2);
        if before_live.saturating_sub(self.live_baseline_bytes) < growth_band {
            return;
        }

        let started = Instant::now();
        let result = unsafe { (crate::runtime_libraries::runtime_api().js_gc_memory_pressure)(2) };
        if result != 2 {
            tracing::warn!(
                deployment,
                result,
                before_live,
                before_reserved,
                live_baseline_bytes = self.live_baseline_bytes,
                growth_band,
                "Perry host-boundary full GC was deferred"
            );
            return;
        }
        let (after_live, after_reserved) = crate::runtime_libraries::current_thread_arena_stats();
        self.live_baseline_bytes = after_live;
        tracing::info!(
            deployment,
            invocations = self.invocations,
            before_live,
            before_reserved,
            after_live,
            after_reserved,
            reclaimed_live_bytes = before_live.saturating_sub(after_live),
            duration_us = started.elapsed().as_secs_f64() * 1_000_000.0,
            "Perry host-boundary full GC completed"
        );
    }
}

#[derive(Default)]
struct DispatchMetrics {
    count: u64,
    encode_ns: u128,
    invoke_ns: u128,
    decode_ns: u128,
}

impl DispatchMetrics {
    fn record(&mut self, deployment: &str, encode: Duration, invoke: Duration, decode: Duration) {
        self.count += 1;
        self.encode_ns += encode.as_nanos();
        self.invoke_ns += invoke.as_nanos();
        self.decode_ns += decode.as_nanos();

        if self.count == 1 || (self.count >= 64 && self.count.is_power_of_two()) {
            let divisor = self.count as f64 * 1_000.0;
            tracing::info!(
                deployment,
                requests = self.count,
                encode_avg_us = self.encode_ns as f64 / divisor,
                invoke_avg_us = self.invoke_ns as f64 / divisor,
                decode_avg_us = self.decode_ns as f64 / divisor,
                "application dispatch phase sample"
            );
        }
    }
}

fn dispatch_http_on_executor(
    deployment: &str,
    plugin: &mut LoadedPlugin,
    request: HttpDispatchRequest,
    metrics: &mut DispatchMetrics,
) -> Result<HttpDispatchResponse> {
    let encode_started = Instant::now();
    let frame = encode_http_request(&request).context("encoding HTTP request frame")?;
    let encode_elapsed = encode_started.elapsed();

    let invoke_started = Instant::now();
    let response_frame = plugin
        .invoke_http(&frame)
        .context("invoking deployment HTTP handler")?;
    let invoke_elapsed = invoke_started.elapsed();

    let decode_started = Instant::now();
    let response = decode_http_response(&response_frame).context("decoding HTTP response frame")?;
    let decode_elapsed = decode_started.elapsed();
    metrics.record(deployment, encode_elapsed, invoke_elapsed, decode_elapsed);
    Ok(response)
}

fn fire_cron_on_executor(
    deployment: &str,
    plugin: &mut LoadedPlugin,
    context: CronContext,
) -> Result<()> {
    let expression = context.expression.clone();
    let request = encode_cron_request(&context).context("encoding cron request frame")?;
    let response = plugin
        .invoke_cron(&expression, &request)
        .with_context(|| format!("invoking cron handler for deployment {deployment}"))?;
    decode_cron_response(&response).context("decoding cron response frame")
}

fn deliver_queue_on_executor(
    deployment: &str,
    plugin: &mut LoadedPlugin,
    message: QueueDispatchMessage,
) -> Result<QueueDisposition> {
    let queue_name = message.queue_name.clone();
    let request = encode_queue_request(&message).context("encoding queue request frame")?;
    let response = plugin
        .invoke_queue(&queue_name, &request)
        .with_context(|| format!("invoking queue handler for deployment {deployment}"))?;
    decode_queue_response(&response).context("decoding queue response frame")
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .context("invalid Base64")
}
