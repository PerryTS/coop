//! Shared ABI types for Perch.
//!
//! Two protocols live here. They're intentionally in a standalone crate so
//! perch-daemon, perch-worker, and any tools that want to talk to the worker
//! (test harnesses, CLIs, etc.) can depend on the same vocabulary.
//!
//! **Daemon ↔ worker.** perch-daemon forwards HTTP requests over a Unix
//! domain socket per deployment. The framing is length-prefixed JSON: a
//! `u32` (big-endian) length followed by the JSON payload. Requests are
//! `WorkerRequest`, responses are `WorkerResponse`.
//!
//! **Worker ↔ deployment dylib.** perch-worker invokes the dlopen'd
//! deployment's registered "route" tool via `perry_plugin_invoke_tool`. The
//! tool receives a NaN-boxed string containing the serialized
//! `DeploymentRequest` JSON and returns a NaN-boxed string containing the
//! serialized `DeploymentResponse` JSON. This is the interim wire protocol
//! until a nicer extern-function ABI is wired into Perry; it's ugly but it
//! works with existing Perry primitives.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Current ABI version. Both sides check this on connect and refuse to talk
/// to each other on mismatch. Bump this on any breaking change.
pub const ABI_VERSION: u32 = 1;

/// The default relative path under `/var/lib/perch/` where per-deployment
/// Unix sockets live. The daemon creates `<sockets_dir>/<deployment>.sock`
/// and perch-worker listens on it.
pub const DEFAULT_SOCKETS_DIR: &str = "sockets";

/// The default tool name that deployments register for HTTP dispatch.
/// Every deployment's plugin `activate()` calls
/// `api.registerTool("route", "...", handler)`. The handler receives a
/// NaN-boxed JSON string of `DeploymentRequest` and returns a NaN-boxed
/// JSON string of `DeploymentResponse`. `@perch/runtime` wraps this
/// convention so deployment authors write `api.registerRoute("POST /foo",
/// ...)` and the library builds a single dispatching tool internally.
pub const DEPLOYMENT_ROUTE_TOOL: &str = "route";

/// The default tool name deployments register for cron ticks.
/// perch-worker invokes this tool with a JSON-encoded `CronContext`.
pub const DEPLOYMENT_CRON_TOOL: &str = "cron";

/// The default tool name deployments register for queue message processing.
/// perch-worker invokes this tool with a JSON-encoded `QueueMessage`.
pub const DEPLOYMENT_QUEUE_TOOL: &str = "queue";

// ============================================================================
// Daemon ↔ worker framing (length-prefixed JSON over Unix socket)
// ============================================================================

/// First message a client sends after connecting. The worker replies with
/// `WorkerResponse::Hello` or closes the connection on version mismatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub abi_version: u32,
    pub client_name: String,
}

/// Envelope for every message from daemon to worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerRequest {
    /// Handshake. Must be the first message on a new connection.
    Hello(ClientHello),
    /// Dispatch an HTTP request to a registered route handler.
    Dispatch {
        /// Correlation ID — echoed in the response so the daemon can match
        /// responses to in-flight requests on a multiplexed connection.
        request_id: u64,
        request: DeploymentRequest,
    },
    /// Fire a registered cron tool manually (used by the scheduler).
    Cron {
        request_id: u64,
        context: CronContext,
    },
    /// Deliver a queue message to the deployment's queue handler.
    Queue {
        request_id: u64,
        message: QueueMessage,
    },
    /// Ask the worker to drain in-flight work and shut down. The worker
    /// replies with `Goodbye` once the last in-flight request finishes.
    Shutdown { grace_period_ms: u64 },
}

/// Envelope for every message from worker to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerResponse {
    /// Handshake response.
    Hello {
        abi_version: u32,
        worker_name: String,
        deployment: String,
    },
    /// Response to a `Dispatch` request.
    DispatchResult {
        request_id: u64,
        response: DeploymentResponse,
    },
    /// Response to a `Cron` request.
    CronResult {
        request_id: u64,
        /// Optional diagnostic string the worker returns — usually empty.
        message: Option<String>,
        error: Option<String>,
    },
    /// Response to a `Queue` request.
    QueueResult {
        request_id: u64,
        /// `ack` = processing succeeded, ack the message.
        /// `nack` = processing failed, re-enqueue with the provided delay.
        /// `dlq` = processing failed permanently, route to DLQ.
        disposition: QueueDisposition,
        error: Option<String>,
    },
    /// Worker is draining and won't accept new work.
    Goodbye,
    /// Something went wrong at the protocol level. Connection will close.
    ProtocolError { message: String },
}

/// Disposition for a processed queue message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueueDisposition {
    Ack,
    Nack,
    Dlq,
}

// ============================================================================
// Worker ↔ deployment dylib (serialized inside the "route" tool invocation)
// ============================================================================

/// The HTTP request shape the deployment's route tool receives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    /// Client IP as seen by the daemon. Will be the real client IP when
    /// the request came via a trusted proxy (Bunny edge), otherwise the
    /// direct connection peer.
    pub remote_addr: String,
    pub scheme: String,
    pub host: String,
    /// Body as base64 (so arbitrary bytes survive a JSON-string roundtrip).
    /// `@perch/runtime`'s `req.json()` / `req.text()` / `req.formData()`
    /// decode this into the right shape for the handler.
    pub body_base64: String,
}

/// The HTTP response shape the deployment's route tool returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    /// Body as base64 — same reason as the request side.
    pub body_base64: String,
}

/// Context object delivered to a cron tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronContext {
    /// The cron expression that fired this invocation.
    pub expression: String,
    /// Unix epoch milliseconds when the fire was scheduled.
    pub scheduled_at_ms: u64,
    /// Unix epoch milliseconds when the fire was actually dispatched.
    pub dispatched_at_ms: u64,
}

/// Queue message delivered to a queue tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueMessage {
    pub queue_name: String,
    pub message_id: String,
    /// Current attempt number (1-indexed). Used by the worker to decide
    /// whether to DLQ on failure.
    pub attempt: u32,
    pub max_retries: u32,
    /// Payload as base64 (arbitrary bytes).
    pub payload_base64: String,
}

// ============================================================================
// Error type
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum AbiError {
    #[error("ABI version mismatch: client={client}, worker={worker}")]
    VersionMismatch { client: u32, worker: u32 },
    #[error("frame too large: {0} bytes (max {MAX_FRAME_SIZE})")]
    FrameTooLarge(u64),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Maximum single-frame size on the Unix socket. Keeps the worker from
/// OOMing on a huge request; real static bodies should be served by the
/// daemon's ServeDir, not by the worker.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
