//! Daemon-side client for the perch-worker Unix-socket protocol.
//!
//! A `WorkerClient` owns a connection to a single running perch-worker
//! process (one per deployment). It exchanges a `Hello` handshake on
//! connect, then serializes `Dispatch` / `Cron` / `Queue` requests over
//! the same socket. For the MVP we keep a single persistent connection
//! per deployment and serialize requests through a tokio channel; a
//! future optimization is per-request multiplexing via the `request_id`
//! correlation ID we already bake into the protocol.
//!
//! If the socket ever disconnects (worker crash, drain, redeploy), the
//! client returns an error and the caller is expected to rebuild the
//! client against the new worker process the daemon just spawned.

use anyhow::{anyhow, Context, Result};
use perch_host_abi::{
    ClientHello, DeploymentRequest, DeploymentResponse, WorkerDeploymentSpec, WorkerRequest,
    WorkerResponse, ABI_VERSION, MAX_FRAME_SIZE,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

/// One connection to one perch-worker process.
pub struct WorkerClient {
    deployment: String,
    stream: Mutex<UnixStream>,
    next_request_id: AtomicU64,
    worker_name: String,
    /// A generation key selects one host inside a shared worker shard. It is
    /// absent for a dedicated one-deployment worker.
    runtime_id: Option<String>,
    /// Once a request deadline or framing error makes the stream state
    /// uncertain, this connection must never be reused. The supervisor will
    /// replace the worker generation.
    poisoned: AtomicBool,
    /// Requests waiting for the single ordered worker connection. This is
    /// separate from deployment admission and the application executor queue.
    transport_backlog: AtomicUsize,
    /// Exchanges holding the ordered connection. This is currently 0 or 1,
    /// but remains explicit if the protocol is multiplexed later.
    transport_in_flight: AtomicUsize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportPhase {
    Waiting,
    Active,
}

/// Fixed, bounded reasons why an ordered worker connection became unsafe to
/// reuse. Human-readable detail remains in logs; only this closed vocabulary
/// is exported as a Prometheus label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerPoisonCause {
    Cancelled,
    Deadline,
    ProcessExit,
    Protocol,
    RssLimit,
    ShardDomain,
    ShardLifecycle,
    StatusCheck,
    Transport,
}

impl WorkerPoisonCause {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Deadline => "deadline",
            Self::ProcessExit => "process_exit",
            Self::Protocol => "protocol",
            Self::RssLimit => "rss_limit",
            Self::ShardDomain => "shard_domain",
            Self::ShardLifecycle => "shard_lifecycle",
            Self::StatusCheck => "status_check",
            Self::Transport => "transport",
        }
    }
}

/// Cancellation-safe accounting for the ordered worker transport. Invocation
/// deadlines can drop a future while it is waiting for or holding the socket
/// lock, so gauges cannot rely on the normal return path.
struct TransportActivity<'a> {
    client: &'a WorkerClient,
    entrypoint: &'static str,
    phase: TransportPhase,
    phase_started: Instant,
    round_trip_recorded: bool,
}

impl<'a> TransportActivity<'a> {
    fn waiting(client: &'a WorkerClient, entrypoint: &'static str) -> Self {
        client.transport_backlog.fetch_add(1, Ordering::AcqRel);
        client.publish_transport_state();
        Self {
            client,
            entrypoint,
            phase: TransportPhase::Waiting,
            phase_started: Instant::now(),
            round_trip_recorded: false,
        }
    }

    fn acquired(&mut self) {
        debug_assert_eq!(self.phase, TransportPhase::Waiting);
        let previous = self.client.transport_backlog.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
        let previous = self
            .client
            .transport_in_flight
            .fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(previous, 0);
        crate::metrics::record_worker_transport_queue_wait(
            &self.client.deployment,
            self.entrypoint,
            "acquired",
            self.phase_started.elapsed().as_secs_f64(),
        );
        self.phase = TransportPhase::Active;
        self.phase_started = Instant::now();
        self.client.publish_transport_state();
    }

    fn finish(&mut self, outcome: &'static str) {
        debug_assert_eq!(self.phase, TransportPhase::Active);
        crate::metrics::record_worker_transport_round_trip(
            &self.client.deployment,
            self.entrypoint,
            outcome,
            self.phase_started.elapsed().as_secs_f64(),
        );
        self.round_trip_recorded = true;
    }
}

impl Drop for TransportActivity<'_> {
    fn drop(&mut self) {
        match self.phase {
            TransportPhase::Waiting => {
                let previous = self.client.transport_backlog.fetch_sub(1, Ordering::AcqRel);
                debug_assert!(previous > 0);
                crate::metrics::record_worker_transport_queue_wait(
                    &self.client.deployment,
                    self.entrypoint,
                    "cancelled",
                    self.phase_started.elapsed().as_secs_f64(),
                );
                crate::metrics::record_worker_transport_cancellation(
                    &self.client.deployment,
                    self.entrypoint,
                    "waiting",
                );
            }
            TransportPhase::Active => {
                let previous = self
                    .client
                    .transport_in_flight
                    .fetch_sub(1, Ordering::AcqRel);
                debug_assert_eq!(previous, 1);
                if !self.round_trip_recorded {
                    crate::metrics::record_worker_transport_round_trip(
                        &self.client.deployment,
                        self.entrypoint,
                        "cancelled",
                        self.phase_started.elapsed().as_secs_f64(),
                    );
                    crate::metrics::record_worker_transport_cancellation(
                        &self.client.deployment,
                        self.entrypoint,
                        "active",
                    );
                    // Dropping a future after it acquired the ordered stream
                    // can interrupt either a write or a read. The next caller
                    // must never attempt to interpret that uncertain framing.
                    self.client.mark_poisoned(
                        WorkerPoisonCause::Cancelled,
                        "worker transport future dropped during an active exchange",
                    );
                }
            }
        }
        self.client.publish_transport_state();
    }
}

/// How long the daemon waits for a worker to accept a new connection and
/// respond to the `Hello` handshake before giving up. If a worker spawn
/// takes longer than this, the daemon considers it failed and reports an
/// error to the user. 5 seconds is generous — healthy spawns complete in
/// well under a second.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Lifecycle control is deployment-path work, but it still must be bounded so
/// a wedged native initializer cannot stall the deployment supervisor forever.
const SHARD_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const SHARD_LOAD_ATTEMPTS: usize = 3;
const SHARD_LOAD_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, thiserror::Error)]
pub(crate) enum ShardLoadError {
    #[error("shard rejected application load: {0}")]
    Rejected(String),
    #[error("shard application load outcome is uncertain after bounded retries: {0}")]
    Uncertain(String),
}

impl ShardLoadError {
    pub(crate) const fn is_uncertain(&self) -> bool {
        matches!(self, Self::Uncertain(_))
    }
}

impl WorkerClient {
    /// Connect to a worker that's already listening on the given socket.
    /// Sends `Hello` and waits for a matching `Hello` response. Returns
    /// a ready-to-use client.
    pub async fn connect(deployment: &str, socket_path: &Path) -> Result<Self> {
        Self::connect_inner(deployment, None, socket_path).await
    }

    pub async fn connect_shard(
        deployment: &str,
        runtime_id: &str,
        socket_path: &Path,
    ) -> Result<Self> {
        Self::connect_inner(deployment, Some(runtime_id), socket_path).await
    }

    async fn connect_inner(
        deployment: &str,
        runtime_id: Option<&str>,
        socket_path: &Path,
    ) -> Result<Self> {
        let stream = timeout(HELLO_TIMEOUT, UnixStream::connect(socket_path))
            .await
            .with_context(|| {
                format!(
                    "timed out connecting to worker socket {:?} for deployment {}",
                    socket_path, deployment
                )
            })?
            .with_context(|| format!("connecting to worker socket {:?}", socket_path))?;

        let mut stream = stream;

        let hello = WorkerRequest::Hello(ClientHello {
            abi_version: ABI_VERSION,
            client_name: format!("perch-daemon/{}", env!("CARGO_PKG_VERSION")),
        });

        let hello_bytes = timeout(HELLO_TIMEOUT, write_frame(&mut stream, &hello))
            .await
            .with_context(|| "timed out writing Hello")?
            .with_context(|| "writing Hello")?;
        crate::metrics::record_worker_transport_bytes(deployment, "hello", "sent", hello_bytes);

        let resp_bytes = timeout(HELLO_TIMEOUT, read_frame(&mut stream))
            .await
            .with_context(|| "timed out reading Hello response")?
            .with_context(|| "reading Hello response")?;
        crate::metrics::record_worker_transport_bytes(
            deployment,
            "hello",
            "received",
            resp_bytes.len() + 4,
        );

        let resp: WorkerResponse =
            serde_json::from_slice(&resp_bytes).context("parsing Hello response")?;

        let worker_name = match resp {
            WorkerResponse::Hello {
                abi_version,
                worker_name,
                deployment: worker_deployment,
            } => {
                if abi_version != ABI_VERSION {
                    return Err(anyhow!(
                        "ABI version mismatch: daemon={}, worker={}",
                        ABI_VERSION,
                        abi_version
                    ));
                }
                if runtime_id.is_none() && worker_deployment != deployment {
                    warn!(
                        expected = deployment,
                        got = %worker_deployment,
                        "worker reports a different deployment name than the daemon expected"
                    );
                }
                worker_name
            }
            WorkerResponse::ProtocolError { message } => {
                return Err(anyhow!("worker rejected Hello: {}", message));
            }
            other => {
                return Err(anyhow!("unexpected first frame from worker: {:?}", other));
            }
        };

        debug!(
            deployment = deployment,
            worker = %worker_name,
            "worker connection established"
        );

        Ok(Self {
            deployment: deployment.to_string(),
            stream: Mutex::new(stream),
            next_request_id: AtomicU64::new(1),
            worker_name,
            runtime_id: runtime_id.map(str::to_owned),
            poisoned: AtomicBool::new(false),
            transport_backlog: AtomicUsize::new(0),
            transport_in_flight: AtomicUsize::new(0),
        })
    }

    pub fn deployment(&self) -> &str {
        &self.deployment
    }

    pub fn worker_name(&self) -> &str {
        &self.worker_name
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    fn publish_transport_state(&self) {
        crate::metrics::set_worker_transport(
            &self.deployment,
            self.transport_backlog.load(Ordering::Acquire),
            self.transport_in_flight.load(Ordering::Acquire),
        );
    }

    #[cfg(test)]
    fn transport_state(&self) -> (usize, usize) {
        (
            self.transport_backlog.load(Ordering::Acquire),
            self.transport_in_flight.load(Ordering::Acquire),
        )
    }

    /// Prevent reuse after an invocation deadline. Dropping a future in the
    /// middle of a write/read exchange would otherwise leave the next caller
    /// consuming the previous response.
    pub fn mark_poisoned(&self, cause: WorkerPoisonCause, reason: &str) {
        if !self.poisoned.swap(true, Ordering::AcqRel) {
            crate::metrics::record_worker_transport_poisoned(&self.deployment, cause.as_str());
            warn!(
                deployment = %self.deployment,
                worker = %self.worker_name,
                cause = cause.as_str(),
                reason,
                "worker connection poisoned; generation replacement required"
            );
        }
    }

    async fn round_trip(
        &self,
        entrypoint: &'static str,
        request: &WorkerRequest,
    ) -> Result<WorkerResponse> {
        if self.is_poisoned() {
            return Err(anyhow!(
                "worker connection for {} is poisoned",
                self.deployment
            ));
        }

        let mut activity = TransportActivity::waiting(self, entrypoint);
        let mut stream = self.stream.lock().await;
        activity.acquired();
        // A caller may have been waiting for the stream lock when another
        // invocation timed out and poisoned the generation.
        if self.is_poisoned() {
            activity.finish("poisoned");
            return Err(anyhow!(
                "worker connection for {} is poisoned",
                self.deployment
            ));
        }

        let result = async {
            let sent = write_frame(&mut *stream, request).await?;
            crate::metrics::record_worker_transport_bytes(
                &self.deployment,
                entrypoint,
                "sent",
                sent,
            );
            let bytes = read_frame(&mut *stream).await?;
            crate::metrics::record_worker_transport_bytes(
                &self.deployment,
                entrypoint,
                "received",
                bytes.len() + 4,
            );
            serde_json::from_slice(&bytes).context("parsing worker response")
        }
        .await;
        activity.finish(if result.is_ok() { "success" } else { "failure" });
        if result.is_err() {
            self.mark_poisoned(
                WorkerPoisonCause::Transport,
                "worker transport or response framing failed",
            );
        }
        result
    }

    fn protocol_failure<T>(&self, message: impl Into<String>) -> Result<T> {
        let message = message.into();
        self.mark_poisoned(WorkerPoisonCause::Protocol, &message);
        Err(anyhow!(message))
    }

    /// Dispatch an HTTP request to the worker. Returns the
    /// DeploymentResponse or an error.
    pub async fn dispatch(&self, request: DeploymentRequest) -> Result<DeploymentResponse> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let req = WorkerRequest::Dispatch {
            request_id,
            runtime_id: self.runtime_id.clone(),
            request,
        };

        let resp = self.round_trip("http", &req).await?;

        match resp {
            WorkerResponse::DispatchResult {
                request_id: got_id,
                response,
            } => {
                if got_id != request_id {
                    return self.protocol_failure(format!(
                        "request ID mismatch: sent {}, got {}",
                        request_id, got_id
                    ));
                }
                Ok(response)
            }
            WorkerResponse::ProtocolError { message } => {
                self.protocol_failure(format!("worker protocol error: {message}"))
            }
            other => self.protocol_failure(format!("unexpected dispatch response: {other:?}")),
        }
    }

    pub async fn fire_cron(&self, context: perch_host_abi::CronContext) -> Result<()> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = WorkerRequest::Cron {
            request_id,
            runtime_id: self.runtime_id.clone(),
            context,
        };
        match self.round_trip("cron", &request).await? {
            WorkerResponse::CronResult {
                request_id: received,
                error,
                ..
            } if received == request_id => match error {
                Some(error) => Err(anyhow!("worker cron failed: {error}")),
                None => Ok(()),
            },
            response => self.protocol_failure(format!("unexpected cron response: {response:?}")),
        }
    }

    pub async fn deliver_queue(
        &self,
        message: perch_host_abi::QueueMessage,
    ) -> Result<perch_host_abi::QueueDisposition> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = WorkerRequest::Queue {
            request_id,
            runtime_id: self.runtime_id.clone(),
            message,
        };
        match self.round_trip("queue", &request).await? {
            WorkerResponse::QueueResult {
                request_id: received,
                disposition,
                error,
            } if received == request_id => match error {
                Some(error) => Err(anyhow!("worker queue delivery failed: {error}")),
                None => Ok(disposition),
            },
            response => self.protocol_failure(format!("unexpected queue response: {response:?}")),
        }
    }

    /// Load and initialize a generation before a routed client is connected.
    /// A short-lived control connection keeps lifecycle operations separate
    /// from request dispatch framing.
    pub async fn load_shard_deployment(
        socket_path: &Path,
        deployment: WorkerDeploymentSpec,
    ) -> std::result::Result<(), ShardLoadError> {
        let deployment_name = deployment.deployment.clone();
        let runtime_id = deployment.runtime_id.clone();
        let request_id = 1;
        let request = WorkerRequest::LoadDeployment {
            request_id,
            deployment,
        };
        let mut uncertain_failures = Vec::with_capacity(SHARD_LOAD_ATTEMPTS);
        for attempt in 1..=SHARD_LOAD_ATTEMPTS {
            match shard_control_round_trip(socket_path, &deployment_name, "load", request.clone())
                .await
            {
                Ok(WorkerResponse::LoadResult {
                    request_id: received,
                    runtime_id: received_runtime,
                    error,
                }) if received == request_id && received_runtime == runtime_id => {
                    return match error {
                        Some(error) => Err(ShardLoadError::Rejected(error)),
                        None => Ok(()),
                    };
                }
                Ok(WorkerResponse::ProtocolError { message }) => {
                    return Err(ShardLoadError::Rejected(message));
                }
                Ok(response) => uncertain_failures.push(format!(
                    "attempt {attempt} returned an uncorrelated response: {response:?}"
                )),
                Err(error) => uncertain_failures.push(format!("attempt {attempt}: {error:#}")),
            }
            if attempt < SHARD_LOAD_ATTEMPTS {
                tokio::time::sleep(SHARD_LOAD_RETRY_DELAY).await;
            }
        }
        Err(ShardLoadError::Uncertain(uncertain_failures.join("; ")))
    }

    pub async fn unload_shard_deployment(
        socket_path: &Path,
        deployment: &str,
        runtime_id: &str,
    ) -> Result<()> {
        let request_id = 1;
        let response = shard_control_round_trip(
            socket_path,
            deployment,
            "unload",
            WorkerRequest::UnloadDeployment {
                request_id,
                runtime_id: runtime_id.to_string(),
            },
        )
        .await?;
        match response {
            WorkerResponse::UnloadResult {
                request_id: received,
                runtime_id: received_runtime,
                error,
            } if received == request_id && received_runtime == runtime_id => match error {
                Some(error) => Err(anyhow!("shard application unload failed: {error}")),
                None => Ok(()),
            },
            response => Err(anyhow!("unexpected shard unload response: {response:?}")),
        }
    }

    /// Remove this client's application generation while leaving sibling
    /// applications and the shared shard process alive.
    pub async fn unload_shard_runtime(&self, grace_period: Duration) -> Result<()> {
        let runtime_id = self
            .runtime_id
            .as_ref()
            .ok_or_else(|| anyhow!("dedicated worker has no shard runtime to unload"))?
            .clone();
        if self.is_poisoned() {
            return Err(anyhow!(
                "shard connection for {} is poisoned",
                self.deployment
            ));
        }
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = WorkerRequest::UnloadDeployment {
            request_id,
            runtime_id: runtime_id.clone(),
        };
        let response = timeout(grace_period, self.round_trip("unload", &request))
            .await
            .context("timed out unloading shard application")??;
        match response {
            WorkerResponse::UnloadResult {
                request_id: received,
                runtime_id: received_runtime,
                error,
            } if received == request_id && received_runtime == runtime_id => match error {
                Some(error) => Err(anyhow!("shard application unload failed: {error}")),
                None => Ok(()),
            },
            response => {
                self.protocol_failure(format!("unexpected shard unload response: {response:?}"))
            }
        }
    }

    /// Ask the worker to drain and exit gracefully. The worker responds
    /// with `Goodbye` once the last in-flight request finishes.
    pub async fn shutdown(&self, grace_period: Duration) -> Result<()> {
        let drain_started = Instant::now();
        let result = self.shutdown_inner(grace_period).await;
        crate::metrics::record_worker_transport_drain(
            &self.deployment,
            if result.is_ok() { "success" } else { "failure" },
            drain_started.elapsed().as_secs_f64(),
        );
        result
    }

    async fn shutdown_inner(&self, grace_period: Duration) -> Result<()> {
        if self.runtime_id.is_some() {
            return Err(anyhow!(
                "shared shard processes must be stopped by their supervisor"
            ));
        }
        if self.is_poisoned() {
            return Err(anyhow!(
                "worker connection for {} is poisoned",
                self.deployment
            ));
        }
        let req = WorkerRequest::Shutdown {
            grace_period_ms: grace_period.as_millis() as u64,
        };
        let mut activity = TransportActivity::waiting(self, "shutdown");
        let mut stream = timeout(grace_period, self.stream.lock())
            .await
            .context("timed out waiting for worker connection during Shutdown")?;
        activity.acquired();
        let result = async {
            let sent = timeout(grace_period * 2, write_frame(&mut *stream, &req))
                .await
                .context("timed out writing Shutdown")??;
            crate::metrics::record_worker_transport_bytes(
                &self.deployment,
                "shutdown",
                "sent",
                sent,
            );

            // Read the Goodbye (or whatever the worker sends back).
            let response = timeout(grace_period * 2, read_frame(&mut *stream))
                .await
                .context("timed out reading Shutdown response")??;
            crate::metrics::record_worker_transport_bytes(
                &self.deployment,
                "shutdown",
                "received",
                response.len() + 4,
            );

            Ok(())
        }
        .await;
        activity.finish(if result.is_ok() { "success" } else { "failure" });
        result
    }
}

async fn shard_control_round_trip(
    socket_path: &Path,
    deployment: &str,
    entrypoint: &'static str,
    request: WorkerRequest,
) -> Result<WorkerResponse> {
    timeout(SHARD_CONTROL_TIMEOUT, async {
        let mut stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("connecting to shard socket {socket_path:?}"))?;
        let hello_sent = write_frame(
            &mut stream,
            &WorkerRequest::Hello(ClientHello {
                abi_version: ABI_VERSION,
                client_name: format!("perch-daemon-control/{}", env!("CARGO_PKG_VERSION")),
            }),
        )
        .await?;
        crate::metrics::record_worker_transport_bytes(deployment, "hello", "sent", hello_sent);
        match read_frame(&mut stream).await.and_then(|bytes| {
            crate::metrics::record_worker_transport_bytes(
                deployment,
                "hello",
                "received",
                bytes.len() + 4,
            );
            serde_json::from_slice::<WorkerResponse>(&bytes).context("parsing shard Hello response")
        })? {
            WorkerResponse::Hello { abi_version, .. } if abi_version == ABI_VERSION => {}
            WorkerResponse::Hello { abi_version, .. } => {
                return Err(anyhow!(
                    "shard ABI mismatch: daemon={}, worker={abi_version}",
                    ABI_VERSION
                ));
            }
            response => return Err(anyhow!("unexpected shard Hello response: {response:?}")),
        }
        let sent = write_frame(&mut stream, &request).await?;
        crate::metrics::record_worker_transport_bytes(deployment, entrypoint, "sent", sent);
        let bytes = read_frame(&mut stream).await?;
        crate::metrics::record_worker_transport_bytes(
            deployment,
            entrypoint,
            "received",
            bytes.len() + 4,
        );
        serde_json::from_slice(&bytes).context("parsing shard control response")
    })
    .await
    .with_context(|| {
        format!(
            "shard control exchange exceeded {} ms",
            SHARD_CONTROL_TIMEOUT.as_millis()
        )
    })?
}

// ---------------------------------------------------------------------------
// Framing helpers (mirror the worker-side listener framing)
// ---------------------------------------------------------------------------

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(anyhow!("frame too large: {}", len));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_frame<T: serde::Serialize>(stream: &mut UnixStream, payload: &T) -> Result<usize> {
    let body = serde_json::to_vec(payload)?;
    if body.len() > MAX_FRAME_SIZE {
        return Err(anyhow!("frame too large: {}", body.len()));
    }
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(body.len() + 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perch_host_abi::WorkerQueuePolicy;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn shard_spec() -> WorkerDeploymentSpec {
        WorkerDeploymentSpec {
            deployment: "alpha".into(),
            runtime_id: "runtime-1".into(),
            dylib_path: "/immutable/alpha/app.so".into(),
            module_name: Some("alpha".into()),
            executor_stack_size_bytes: 256 * 1024,
            command_queue_capacity: 8,
            gc_reclaim_check_interval: 256,
            gc_reclaim_growth_bytes: 256 * 1024,
            deployment_context_id: 17,
            queue_policies: vec![WorkerQueuePolicy {
                name: "events".into(),
                max_payload_bytes: 1024,
                max_attempts: 5,
                max_delay_ms: 60_000,
            }],
        }
    }

    async fn accept_shard_control(
        listener: &tokio::net::UnixListener,
    ) -> (UnixStream, WorkerRequest) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello: WorkerRequest =
            serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
        assert!(matches!(hello, WorkerRequest::Hello(_)));
        write_frame(
            &mut stream,
            &WorkerResponse::Hello {
                abi_version: ABI_VERSION,
                worker_name: "test-shard".into(),
                deployment: "shard:test".into(),
            },
        )
        .await
        .unwrap();
        let request = serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
        (stream, request)
    }

    #[tokio::test]
    async fn mismatched_response_poisoned_connection_is_never_reused() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("worker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello: WorkerRequest =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            assert!(matches!(hello, WorkerRequest::Hello(_)));
            write_frame(
                &mut stream,
                &WorkerResponse::Hello {
                    abi_version: ABI_VERSION,
                    worker_name: "test-worker".into(),
                    deployment: "test".into(),
                },
            )
            .await
            .unwrap();

            let request: WorkerRequest =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            let WorkerRequest::Dispatch { request_id, .. } = request else {
                panic!("expected dispatch")
            };
            write_frame(
                &mut stream,
                &WorkerResponse::DispatchResult {
                    request_id: request_id + 1,
                    response: DeploymentResponse {
                        status: 200,
                        headers: HashMap::new(),
                        body_base64: String::new(),
                    },
                },
            )
            .await
            .unwrap();
        });

        let client = WorkerClient::connect("test", &socket).await.unwrap();
        let request = DeploymentRequest {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            headers: HashMap::new(),
            remote_addr: String::new(),
            scheme: "http".into(),
            host: "test".into(),
            body_base64: String::new(),
        };
        let error = client.dispatch(request.clone()).await.unwrap_err();
        assert!(error.to_string().contains("request ID mismatch"));
        assert!(client.is_poisoned());
        let second = client.dispatch(request).await.unwrap_err();
        assert!(second.to_string().contains("poisoned"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn transport_backlog_is_cancellation_safe() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("worker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (first_received, first_received_rx) = tokio::sync::oneshot::channel();
        let (release_first, release_first_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello: WorkerRequest =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            assert!(matches!(hello, WorkerRequest::Hello(_)));
            write_frame(
                &mut stream,
                &WorkerResponse::Hello {
                    abi_version: ABI_VERSION,
                    worker_name: "test-worker".into(),
                    deployment: "test".into(),
                },
            )
            .await
            .unwrap();

            let request: WorkerRequest =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            let WorkerRequest::Dispatch { request_id, .. } = request else {
                panic!("expected dispatch")
            };
            first_received.send(()).unwrap();
            release_first_rx.await.unwrap();
            write_frame(
                &mut stream,
                &WorkerResponse::DispatchResult {
                    request_id,
                    response: DeploymentResponse {
                        status: 200,
                        headers: HashMap::new(),
                        body_base64: String::new(),
                    },
                },
            )
            .await
            .unwrap();
        });

        let client = Arc::new(WorkerClient::connect("test", &socket).await.unwrap());
        let request = DeploymentRequest {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            headers: HashMap::new(),
            remote_addr: String::new(),
            scheme: "http".into(),
            host: "test".into(),
            body_base64: String::new(),
        };
        let first_client = client.clone();
        let first_request = request.clone();
        let first = tokio::spawn(async move { first_client.dispatch(first_request).await });
        first_received_rx.await.unwrap();
        assert_eq!(client.transport_state(), (0, 1));

        let waiting_client = client.clone();
        let waiting = tokio::spawn(async move { waiting_client.dispatch(request).await });
        timeout(Duration::from_secs(1), async {
            while client.transport_state() != (1, 1) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second dispatch never entered the transport backlog");

        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        timeout(Duration::from_secs(1), async {
            while client.transport_state() != (0, 1) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled waiter remained in the transport backlog");
        assert!(
            !client.is_poisoned(),
            "cancelling before socket acquisition must leave framing reusable"
        );

        release_first.send(()).unwrap();
        assert_eq!(first.await.unwrap().unwrap().status, 200);
        assert_eq!(client.transport_state(), (0, 0));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn active_transport_cancellation_poisoned_connection_is_never_reused() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("worker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (request_received, request_received_rx) = tokio::sync::oneshot::channel();
        let (close_connection, close_connection_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello: WorkerRequest =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            assert!(matches!(hello, WorkerRequest::Hello(_)));
            write_frame(
                &mut stream,
                &WorkerResponse::Hello {
                    abi_version: ABI_VERSION,
                    worker_name: "test-worker".into(),
                    deployment: "test".into(),
                },
            )
            .await
            .unwrap();

            let request: WorkerRequest =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            assert!(matches!(request, WorkerRequest::Dispatch { .. }));
            request_received.send(()).unwrap();
            close_connection_rx.await.unwrap();
        });

        let client = Arc::new(WorkerClient::connect("test", &socket).await.unwrap());
        let request = DeploymentRequest {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            headers: HashMap::new(),
            remote_addr: String::new(),
            scheme: "http".into(),
            host: "test".into(),
            body_base64: String::new(),
        };
        let active_client = client.clone();
        let active = tokio::spawn(async move { active_client.dispatch(request.clone()).await });
        request_received_rx.await.unwrap();
        assert_eq!(client.transport_state(), (0, 1));

        active.abort();
        assert!(active.await.unwrap_err().is_cancelled());
        timeout(Duration::from_secs(1), async {
            while client.transport_state() != (0, 0) || !client.is_poisoned() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("active cancellation did not poison and release the transport");

        let second = client
            .dispatch(DeploymentRequest {
                method: "GET".into(),
                path: "/second".into(),
                query: String::new(),
                headers: HashMap::new(),
                remote_addr: String::new(),
                scheme: "http".into(),
                host: "test".into(),
                body_base64: String::new(),
            })
            .await
            .unwrap_err();
        assert!(second.to_string().contains("poisoned"));

        close_connection.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn shard_load_retries_the_exact_identity_after_a_lost_response() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("shard.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let expected = shard_spec();
        let server_expected = expected.clone();
        let server = tokio::spawn(async move {
            let (first, first_request) = accept_shard_control(&listener).await;
            let WorkerRequest::LoadDeployment {
                request_id: first_id,
                deployment: first_spec,
            } = first_request
            else {
                panic!("expected first load request")
            };
            assert_eq!(first_id, 1);
            assert_eq!(first_spec, server_expected);
            drop(first);

            let (mut second, second_request) = accept_shard_control(&listener).await;
            let WorkerRequest::LoadDeployment {
                request_id: second_id,
                deployment: second_spec,
            } = second_request
            else {
                panic!("expected retry load request")
            };
            assert_eq!(second_id, first_id);
            assert_eq!(second_spec, first_spec);
            write_frame(
                &mut second,
                &WorkerResponse::LoadResult {
                    request_id: second_id,
                    runtime_id: second_spec.runtime_id,
                    error: None,
                },
            )
            .await
            .unwrap();
        });

        WorkerClient::load_shard_deployment(&socket, expected)
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn definitive_shard_load_rejection_is_not_retried() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("shard.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, request) = accept_shard_control(&listener).await;
            let WorkerRequest::LoadDeployment {
                request_id,
                deployment,
            } = request
            else {
                panic!("expected load request")
            };
            write_frame(
                &mut stream,
                &WorkerResponse::LoadResult {
                    request_id,
                    runtime_id: deployment.runtime_id,
                    error: Some("application manifest rejected".into()),
                },
            )
            .await
            .unwrap();
        });

        let error = WorkerClient::load_shard_deployment(&socket, shard_spec())
            .await
            .unwrap_err();
        assert!(matches!(error, ShardLoadError::Rejected(_)));
        assert!(error.to_string().contains("application manifest rejected"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn exhausted_shard_load_retries_report_an_uncertain_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("shard.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..SHARD_LOAD_ATTEMPTS {
                let (stream, request) = accept_shard_control(&listener).await;
                assert!(matches!(request, WorkerRequest::LoadDeployment { .. }));
                drop(stream);
            }
        });

        let error = WorkerClient::load_shard_deployment(&socket, shard_spec())
            .await
            .unwrap_err();
        assert!(matches!(error, ShardLoadError::Uncertain(_)));
        assert!(error.is_uncertain());
        assert!(error.to_string().contains("attempt 3"));
        server.await.unwrap();
    }
}
