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
    ClientHello, DeploymentRequest, DeploymentResponse, WorkerRequest, WorkerResponse,
    ABI_VERSION, MAX_FRAME_SIZE,
};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
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
}

/// How long the daemon waits for a worker to accept a new connection and
/// respond to the `Hello` handshake before giving up. If a worker spawn
/// takes longer than this, the daemon considers it failed and reports an
/// error to the user. 5 seconds is generous — healthy spawns complete in
/// well under a second.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-request dispatch timeout. The spec calls for 30 second wall clock
/// max per invocation; we pick a slightly higher daemon-side timeout so
/// the worker's own enforcement fires first with a nicer error.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);

impl WorkerClient {
    /// Connect to a worker that's already listening on the given socket.
    /// Sends `Hello` and waits for a matching `Hello` response. Returns
    /// a ready-to-use client.
    pub async fn connect(deployment: &str, socket_path: &Path) -> Result<Self> {
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

        timeout(HELLO_TIMEOUT, write_frame(&mut stream, &hello))
            .await
            .with_context(|| "timed out writing Hello")?
            .with_context(|| "writing Hello")?;

        let resp_bytes = timeout(HELLO_TIMEOUT, read_frame(&mut stream))
            .await
            .with_context(|| "timed out reading Hello response")?
            .with_context(|| "reading Hello response")?;

        let resp: WorkerResponse = serde_json::from_slice(&resp_bytes)
            .context("parsing Hello response")?;

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
                if worker_deployment != deployment {
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
                return Err(anyhow!(
                    "unexpected first frame from worker: {:?}",
                    other
                ));
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
        })
    }

    pub fn deployment(&self) -> &str {
        &self.deployment
    }

    pub fn worker_name(&self) -> &str {
        &self.worker_name
    }

    /// Dispatch an HTTP request to the worker. Returns the
    /// DeploymentResponse or an error.
    pub async fn dispatch(&self, request: DeploymentRequest) -> Result<DeploymentResponse> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let req = WorkerRequest::Dispatch {
            request_id,
            request,
        };

        let mut stream = self.stream.lock().await;

        timeout(REQUEST_TIMEOUT, write_frame(&mut *stream, &req))
            .await
            .context("timed out writing Dispatch")??;

        let resp_bytes = timeout(REQUEST_TIMEOUT, read_frame(&mut *stream))
            .await
            .context("timed out reading Dispatch response")??;

        let resp: WorkerResponse = serde_json::from_slice(&resp_bytes)
            .context("parsing Dispatch response")?;

        match resp {
            WorkerResponse::DispatchResult {
                request_id: got_id,
                response,
            } => {
                if got_id != request_id {
                    return Err(anyhow!(
                        "request ID mismatch: sent {}, got {}",
                        request_id,
                        got_id
                    ));
                }
                Ok(response)
            }
            WorkerResponse::ProtocolError { message } => {
                Err(anyhow!("worker protocol error: {}", message))
            }
            other => Err(anyhow!("unexpected dispatch response: {:?}", other)),
        }
    }

    /// Ask the worker to drain and exit gracefully. The worker responds
    /// with `Goodbye` once the last in-flight request finishes.
    pub async fn shutdown(&self, grace_period: Duration) -> Result<()> {
        let req = WorkerRequest::Shutdown {
            grace_period_ms: grace_period.as_millis() as u64,
        };
        let mut stream = self.stream.lock().await;
        timeout(grace_period * 2, write_frame(&mut *stream, &req))
            .await
            .context("timed out writing Shutdown")??;

        // Read the Goodbye (or whatever the worker sends back).
        let _ = timeout(grace_period * 2, read_frame(&mut *stream))
            .await
            .context("timed out reading Shutdown response")??;

        Ok(())
    }
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

async fn write_frame<T: serde::Serialize>(
    stream: &mut UnixStream,
    payload: &T,
) -> Result<()> {
    let body = serde_json::to_vec(payload)?;
    if body.len() > MAX_FRAME_SIZE {
        return Err(anyhow!("frame too large: {}", body.len()));
    }
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}
