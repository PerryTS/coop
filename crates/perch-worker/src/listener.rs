//! Unix socket listener + framed-JSON protocol handler.
//!
//! perch-daemon connects to our per-deployment socket at
//! `<sockets_dir>/<deployment>.sock`, sends `ClientHello`, then multiplexes
//! `Dispatch`/`Cron`/`Queue` requests over the same connection. We respond
//! in-order for the MVP; per-request IDs are echoed so the daemon can
//! match replies when we later add pipelining.
//!
//! The frame format is length-prefixed JSON:
//!
//! ```text
//! +-----------------+-----------------------+
//! | u32 length (BE) | JSON payload          |
//! +-----------------+-----------------------+
//! ```
//!
//! Max frame size is `perch_host_abi::MAX_FRAME_SIZE`. Frames larger than
//! that get a `ProtocolError` reply and the connection is closed.

use crate::host::DeploymentHost;
use anyhow::{anyhow, Context, Result};
use perch_host_abi::{
    AbiError, ClientHello, WorkerRequest, WorkerResponse, ABI_VERSION, MAX_FRAME_SIZE,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};

/// Listens for daemon connections on a Unix socket and dispatches to a
/// shared `DeploymentHost`.
pub struct Listener {
    deployment: String,
    socket_path: PathBuf,
    listener: UnixListener,
    host: Arc<DeploymentHost>,
}

impl Listener {
    pub fn bind(
        deployment: &str,
        socket_path: &Path,
        host: DeploymentHost,
    ) -> Result<Self> {
        // Remove any stale socket from a previous run.
        if socket_path.exists() {
            std::fs::remove_file(socket_path)
                .with_context(|| format!("removing stale socket {:?}", socket_path))?;
        }

        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding Unix socket at {:?}", socket_path))?;

        // Restrict access to the socket file. The daemon and workers run
        // as the same user, so 0600 is fine.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(socket_path, perms) {
                warn!(?e, "failed to set socket permissions to 0600");
            }
        }

        Ok(Self {
            deployment: deployment.to_string(),
            socket_path: socket_path.to_path_buf(),
            listener,
            host: Arc::new(host),
        })
    }

    /// Accept loop. Returns when the worker is told to shut down.
    pub async fn serve(self) -> Result<()> {
        let Self {
            deployment,
            socket_path,
            listener,
            host,
        } = self;

        // Track accepted connections so we can drain them on shutdown
        // later. For the MVP we don't actively drain on SIGTERM; we just
        // let the current connection handlers finish naturally.
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let host = host.clone();
                    let deployment = deployment.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(&deployment, stream, host).await {
                            warn!(deployment = %deployment, error = ?e, "connection error");
                        }
                    });
                }
                Err(e) => {
                    error!(deployment = %deployment, error = ?e, "accept failed");
                    // A brief backoff so we don't busy-spin on persistent
                    // accept failures.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }

            // If the socket file has been removed from under us (e.g. the
            // daemon cleaned it up during a redeploy), we're done.
            if !socket_path.exists() {
                info!(deployment = %deployment, "socket removed, shutting down listener");
                return Ok(());
            }
        }
    }
}

async fn handle_connection(
    deployment: &str,
    stream: UnixStream,
    host: Arc<DeploymentHost>,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = read_half;
    let mut writer = write_half;

    // First message must be a Hello.
    let first = read_frame(&mut reader).await?;
    let req: WorkerRequest = serde_json::from_slice(&first)
        .context("parsing first frame as WorkerRequest")?;

    match req {
        WorkerRequest::Hello(ClientHello {
            abi_version,
            client_name,
        }) => {
            if abi_version != ABI_VERSION {
                let err = WorkerResponse::ProtocolError {
                    message: format!(
                        "ABI version mismatch: client={}, worker={}",
                        abi_version, ABI_VERSION
                    ),
                };
                write_frame(&mut writer, &err).await?;
                return Err(
                    AbiError::VersionMismatch {
                        client: abi_version,
                        worker: ABI_VERSION,
                    }
                    .into(),
                );
            }

            debug!(
                deployment = deployment,
                client = %client_name,
                "client connected"
            );

            let hello = WorkerResponse::Hello {
                abi_version: ABI_VERSION,
                worker_name: format!("perch-worker/{}", env!("CARGO_PKG_VERSION")),
                deployment: deployment.to_string(),
            };
            write_frame(&mut writer, &hello).await?;
        }
        other => {
            let err = WorkerResponse::ProtocolError {
                message: format!("expected Hello as first frame, got {:?}", other),
            };
            write_frame(&mut writer, &err).await?;
            return Err(anyhow!("first frame was not Hello"));
        }
    }

    // Subsequent messages: Dispatch / Cron / Queue / Shutdown.
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(f) => f,
            Err(e) => {
                // EOF or read error — client went away, we're done with
                // this connection.
                debug!(deployment = deployment, error = ?e, "connection closed");
                return Ok(());
            }
        };

        let req: WorkerRequest = serde_json::from_slice(&frame)
            .context("parsing WorkerRequest")?;

        let response = match req {
            WorkerRequest::Hello(_) => WorkerResponse::ProtocolError {
                message: "Hello after handshake".to_string(),
            },
            WorkerRequest::Dispatch {
                request_id,
                request,
            } => match host.dispatch(request).await {
                Ok(response) => WorkerResponse::DispatchResult {
                    request_id,
                    response,
                },
                Err(e) => {
                    error!(deployment = deployment, error = ?e, "dispatch failed");
                    WorkerResponse::DispatchResult {
                        request_id,
                        response: perch_host_abi::DeploymentResponse {
                            status: 500,
                            headers: Default::default(),
                            body_base64: base64_encode(
                                format!("perch: dispatch error: {}", e).as_bytes(),
                            ),
                        },
                    }
                }
            },
            WorkerRequest::Cron {
                request_id,
                context,
            } => match host.fire_cron(context).await {
                Ok(()) => WorkerResponse::CronResult {
                    request_id,
                    message: None,
                    error: None,
                },
                Err(e) => WorkerResponse::CronResult {
                    request_id,
                    message: None,
                    error: Some(e.to_string()),
                },
            },
            WorkerRequest::Queue {
                request_id,
                message,
            } => match host.deliver_queue_message(message).await {
                Ok(disposition) => WorkerResponse::QueueResult {
                    request_id,
                    disposition,
                    error: None,
                },
                Err(e) => WorkerResponse::QueueResult {
                    request_id,
                    disposition: perch_host_abi::QueueDisposition::Nack,
                    error: Some(e.to_string()),
                },
            },
            WorkerRequest::Shutdown { grace_period_ms: _ } => {
                write_frame(&mut writer, &WorkerResponse::Goodbye).await?;
                return Ok(());
            }
        };

        write_frame(&mut writer, &response).await?;
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

async fn read_frame(stream: &mut tokio::net::unix::OwnedReadHalf) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > MAX_FRAME_SIZE {
        return Err(AbiError::FrameTooLarge(len as u64).into());
    }

    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_frame<T: serde::Serialize>(
    stream: &mut tokio::net::unix::OwnedWriteHalf,
    payload: &T,
) -> Result<()> {
    let body = serde_json::to_vec(payload)?;
    if body.len() > MAX_FRAME_SIZE {
        return Err(AbiError::FrameTooLarge(body.len() as u64).into());
    }
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
