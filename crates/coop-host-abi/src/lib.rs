//! Shared ABI types for Coop.
//!
//! Two protocols live here. They're intentionally in a standalone crate so
//! coop-daemon, coop-worker, and any tools that want to talk to the worker
//! (test harnesses, CLIs, etc.) can depend on the same vocabulary.
//!
//! **Daemon ↔ worker.** coop-daemon forwards HTTP requests over a Unix
//! domain socket per deployment. The framing is length-prefixed JSON: a
//! `u32` (big-endian) length followed by the JSON payload. Requests are
//! `WorkerRequest`, responses are `WorkerResponse`.
//!
//! **Host ↔ deployment dylib.** Applications exchange a compact `COOP`
//! frame in Perry `Buffer` values, retaining raw body bytes. This is the only
//! supported application-library HTTP ABI.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Current ABI version. Both sides check this on connect and refuse to talk
/// to each other on mismatch. Bump this on any breaking change.
pub const ABI_VERSION: u32 = 3;

/// Version of the on-disk Perry application-library contract. This is
/// independent from the daemon/worker socket protocol above.
// Bumped 2 -> 3 for the Coop rebrand: `APP_FRAME_MAGIC` changed from `PCH2` to
// `COOP`, so an image compiled against the old magic can no longer be parsed.
// The version bump is what turns that into a clean refusal at load with a named
// mismatch, instead of a frame decode failing somewhere inside a request.
// Every deployment must be recompiled.
pub const APP_LIBRARY_ABI_VERSION: u32 = 3;

/// Version of Coop's cached native-library boundary audit. This is separate
/// from the handler ABI: increasing it forces existing app images through the
/// strengthened audit before their cached result can be trusted.
pub const APP_LIBRARY_BOUNDARY_VERSION: u32 = 1;

/// Calling convention of the compiled handler export recorded in an app
/// library's sidecar manifest.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HandlerAbi {
    /// `extern "C" fn(f64) -> f64`
    Bare,
    /// `extern "C" fn(i64, f64) -> f64`, with a zero closure environment.
    Wrapped,
}

/// Versioned descriptor written next to every Perry application library.
///
/// Perry's generated code calls deep runtime internals through C symbols, so
/// compiler/runtime compatibility is exact rather than semver-compatible.
/// Coop refuses to load a descriptor built by another Perry version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppLibraryManifest {
    pub abi_version: u32,
    pub deployment: String,
    pub perry_version: String,
    pub perry_commit: String,
    pub compiler_sha256: String,
    pub target: String,
    pub init_symbol: String,
    /// Required HTTP entry point. It accepts and returns Perry `Buffer`
    /// values containing the compact frame defined below.
    pub handle_symbol: String,
    pub handler_abi: HandlerAbi,
    /// Optional cron entries keyed by their configured expression.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cron_entries: Vec<CronLibraryEntry>,
    /// Optional queue entries keyed by queue name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue_entries: Vec<QueueLibraryEntry>,
    /// A deployment-time boundary check can be cached safely when it is tied
    /// to the exact bytes of the application library.
    #[serde(default)]
    pub boundary_verified: bool,
    #[serde(default)]
    pub boundary_verification_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_size: Option<u64>,
    /// Digest of the sorted source paths and bytes plus the deployment config
    /// that selected and named every exported entry, the dereferenced external
    /// dependency tree, and the compiler invocation/environment identity. This
    /// is a build-cache input identity, separate from the compiled library's
    /// byte identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    /// Build identity for application code only. It excludes immutable static
    /// asset bytes/configuration, allowing a static-only deployment change to
    /// publish a new package around the already verified app image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_source_sha256: Option<String>,
    /// Exact logical-path/content digest of the dereferenced `node_modules`
    /// snapshot. Kept separately so operators can identify dependency-only
    /// rebuilds without exposing dependency contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_sha256: Option<String>,
    /// Digest of Coop's semantic Perry argv, explicitly propagated build
    /// environment, linker wrapper, provider identity, target, and CPU
    /// baseline. Concrete staging/cache paths are represented by stable tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiler_invocation_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronLibraryEntry {
    pub expression: String,
    pub symbol: String,
    pub handler_abi: HandlerAbi,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueLibraryEntry {
    pub queue_name: String,
    pub symbol: String,
    pub handler_abi: HandlerAbi,
}

impl AppLibraryManifest {
    pub fn adjacent_path(library: &Path) -> PathBuf {
        library.with_extension("coop-lib.json")
    }

    pub fn load(library: &Path) -> Result<Option<Self>, std::io::Error> {
        let path = Self::adjacent_path(library);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn write(&self, library: &Path) -> Result<PathBuf, std::io::Error> {
        let path = Self::adjacent_path(library);
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temp_path, bytes)?;
        std::fs::rename(&temp_path, &path)?;
        Ok(path)
    }
}

/// The default relative path under `/var/lib/coop/` where per-deployment
/// Unix sockets live. The daemon creates `<sockets_dir>/<deployment>.sock`
/// and coop-worker listens on it.
pub const DEFAULT_SOCKETS_DIR: &str = "sockets";

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

/// One application generation loaded into a multi-application worker shard.
/// The daemon has already verified the package and provider identity; the
/// shard repeats application manifest/integrity checks while creating the
/// thread-affine Perry executor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerDeploymentSpec {
    pub deployment: String,
    pub runtime_id: String,
    pub dylib_path: PathBuf,
    pub module_name: Option<String>,
    pub executor_stack_size_bytes: usize,
    pub command_queue_capacity: usize,
    pub gc_reclaim_check_interval: usize,
    pub gc_reclaim_growth_bytes: u64,
    pub deployment_context_id: u64,
    #[serde(default)]
    pub queue_policies: Vec<WorkerQueuePolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerQueuePolicy {
    pub name: String,
    pub max_payload_bytes: usize,
    pub max_attempts: u32,
    pub max_delay_ms: u64,
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
        /// Required for a multi-application shard and absent for a dedicated
        /// worker, whose one preloaded host is implicit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_id: Option<String>,
        request: DeploymentRequest,
    },
    /// Fire a registered cron tool manually (used by the scheduler).
    Cron {
        request_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_id: Option<String>,
        context: CronContext,
    },
    /// Deliver a queue message to the deployment's queue handler.
    Queue {
        request_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_id: Option<String>,
        message: QueueMessage,
    },
    /// Eagerly load and initialize one application generation in a shard.
    LoadDeployment {
        request_id: u64,
        deployment: WorkerDeploymentSpec,
    },
    /// Drain and unload one application generation without stopping its shard.
    UnloadDeployment { request_id: u64, runtime_id: String },
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
    LoadResult {
        request_id: u64,
        runtime_id: String,
        error: Option<String>,
    },
    UnloadResult {
        request_id: u64,
        runtime_id: String,
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
    /// `@coop/runtime`'s `req.json()` / `req.text()` / `req.formData()`
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

// ============================================================================
// Daemon ↔ in-process app fast path (compact binary Perry Buffer)
// ============================================================================

/// Owned HTTP request used inside the daemon. Unlike `DeploymentRequest`, its
/// body remains raw bytes and duplicate headers are retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpDispatchRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub remote_addr: String,
    pub scheme: String,
    pub host: String,
    pub body: Vec<u8>,
}

/// Owned response returned by the application host. The listener never needs
/// to Base64-decode this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpDispatchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

const APP_FRAME_MAGIC: &[u8; 4] = b"COOP";
const HTTP_REQUEST_FRAME: u8 = 1;
const HTTP_RESPONSE_FRAME: u8 = 2;
const CRON_REQUEST_FRAME: u8 = 3;
const CRON_RESPONSE_FRAME: u8 = 4;
const QUEUE_REQUEST_FRAME: u8 = 5;
const QUEUE_RESPONSE_FRAME: u8 = 6;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HttpFrameError {
    #[error("HTTP ABI frame exceeds {MAX_FRAME_SIZE} bytes")]
    TooLarge,
    #[error("invalid HTTP ABI frame: {0}")]
    Invalid(&'static str),
    #[error("HTTP ABI text field is not UTF-8")]
    Utf8,
}

/// Encode a raw HTTP request for `handle(Buffer): Buffer`.
pub fn encode_http_request(request: &HttpDispatchRequest) -> Result<Vec<u8>, HttpFrameError> {
    let mut writer = FrameWriter::new(HTTP_REQUEST_FRAME);
    writer.text(&request.method)?;
    writer.text(&request.path)?;
    writer.text(&request.query)?;
    writer.text(&request.remote_addr)?;
    writer.text(&request.scheme)?;
    writer.text(&request.host)?;
    writer.headers(&request.headers)?;
    writer.bytes(&request.body)?;
    writer.finish()
}

/// Exact encoded size of an HTTP request frame without allocating it.
/// Hosts use this to reject oversized work before it reaches an executor.
pub fn http_request_frame_len(request: &HttpDispatchRequest) -> Result<usize, HttpFrameError> {
    let mut size = APP_FRAME_MAGIC.len() + 1;
    for value in [
        request.method.as_bytes(),
        request.path.as_bytes(),
        request.query.as_bytes(),
        request.remote_addr.as_bytes(),
        request.scheme.as_bytes(),
        request.host.as_bytes(),
    ] {
        add_sized_field(&mut size, value.len())?;
    }
    add_sized_headers(&mut size, &request.headers)?;
    add_sized_field(&mut size, request.body.len())?;
    Ok(size)
}

/// Decode a request frame. This is also useful to non-Perry hosts and tests.
pub fn decode_http_request(frame: &[u8]) -> Result<HttpDispatchRequest, HttpFrameError> {
    let mut reader = FrameReader::new(frame, HTTP_REQUEST_FRAME)?;
    let request = HttpDispatchRequest {
        method: reader.text()?,
        path: reader.text()?,
        query: reader.text()?,
        remote_addr: reader.text()?,
        scheme: reader.text()?,
        host: reader.text()?,
        headers: reader.headers()?,
        body: reader.bytes()?.to_vec(),
    };
    reader.finish()?;
    Ok(request)
}

/// Encode a raw HTTP response returned by `handle`.
pub fn encode_http_response(response: &HttpDispatchResponse) -> Result<Vec<u8>, HttpFrameError> {
    let mut writer = FrameWriter::new(HTTP_RESPONSE_FRAME);
    writer.u16(response.status);
    writer.headers(&response.headers)?;
    writer.bytes(&response.body)?;
    writer.finish()
}

/// Exact encoded size of an HTTP response frame without allocating it.
pub fn http_response_frame_len(response: &HttpDispatchResponse) -> Result<usize, HttpFrameError> {
    let mut size = APP_FRAME_MAGIC.len() + 1 + std::mem::size_of::<u16>();
    add_sized_headers(&mut size, &response.headers)?;
    add_sized_field(&mut size, response.body.len())?;
    Ok(size)
}

/// Decode a `handle` response without JSON or Base64.
pub fn decode_http_response(frame: &[u8]) -> Result<HttpDispatchResponse, HttpFrameError> {
    let mut reader = FrameReader::new(frame, HTTP_RESPONSE_FRAME)?;
    let response = HttpDispatchResponse {
        status: reader.u16()?,
        headers: reader.headers()?,
        body: reader.bytes()?.to_vec(),
    };
    reader.finish()?;
    Ok(response)
}

fn add_sized_headers(size: &mut usize, headers: &[(String, String)]) -> Result<(), HttpFrameError> {
    u32::try_from(headers.len()).map_err(|_| HttpFrameError::TooLarge)?;
    *size = size.checked_add(4).ok_or(HttpFrameError::TooLarge)?;
    for (name, value) in headers {
        add_sized_field(size, name.len())?;
        add_sized_field(size, value.len())?;
    }
    Ok(())
}

fn add_sized_field(size: &mut usize, field_len: usize) -> Result<(), HttpFrameError> {
    u32::try_from(field_len).map_err(|_| HttpFrameError::TooLarge)?;
    *size = size
        .checked_add(4)
        .and_then(|value| value.checked_add(field_len))
        .ok_or(HttpFrameError::TooLarge)?;
    if *size > MAX_FRAME_SIZE {
        return Err(HttpFrameError::TooLarge);
    }
    Ok(())
}

struct FrameWriter {
    bytes: Vec<u8>,
}

impl FrameWriter {
    fn new(kind: u8) -> Self {
        let mut bytes = Vec::with_capacity(512);
        bytes.extend_from_slice(APP_FRAME_MAGIC);
        bytes.push(kind);
        Self { bytes }
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: usize) -> Result<(), HttpFrameError> {
        let value = u32::try_from(value).map_err(|_| HttpFrameError::TooLarge)?;
        self.bytes.extend_from_slice(&value.to_be_bytes());
        Ok(())
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), HttpFrameError> {
        let final_len = self
            .bytes
            .len()
            .checked_add(4)
            .and_then(|length| length.checked_add(value.len()))
            .ok_or(HttpFrameError::TooLarge)?;
        if final_len > MAX_FRAME_SIZE {
            return Err(HttpFrameError::TooLarge);
        }
        self.u32(value.len())?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn text(&mut self, value: &str) -> Result<(), HttpFrameError> {
        self.bytes(value.as_bytes())
    }

    fn headers(&mut self, headers: &[(String, String)]) -> Result<(), HttpFrameError> {
        self.u32(headers.len())?;
        for (name, value) in headers {
            self.text(name)?;
            self.text(value)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, HttpFrameError> {
        if self.bytes.len() > MAX_FRAME_SIZE {
            Err(HttpFrameError::TooLarge)
        } else {
            Ok(self.bytes)
        }
    }
}

struct FrameReader<'a> {
    frame: &'a [u8],
    position: usize,
}

impl<'a> FrameReader<'a> {
    fn new(frame: &'a [u8], kind: u8) -> Result<Self, HttpFrameError> {
        if frame.len() > MAX_FRAME_SIZE {
            return Err(HttpFrameError::TooLarge);
        }
        if frame.get(..4) != Some(APP_FRAME_MAGIC.as_slice()) || frame.get(4) != Some(&kind) {
            return Err(HttpFrameError::Invalid("wrong magic or frame kind"));
        }
        Ok(Self { frame, position: 5 })
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], HttpFrameError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(HttpFrameError::Invalid("length overflow"))?;
        let value = self
            .frame
            .get(self.position..end)
            .ok_or(HttpFrameError::Invalid("truncated field"))?;
        self.position = end;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, HttpFrameError> {
        let value: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| HttpFrameError::Invalid("truncated u16"))?;
        Ok(u16::from_be_bytes(value))
    }

    fn u32(&mut self) -> Result<usize, HttpFrameError> {
        let value: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| HttpFrameError::Invalid("truncated u32"))?;
        Ok(u32::from_be_bytes(value) as usize)
    }

    fn u64(&mut self) -> Result<u64, HttpFrameError> {
        let value: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| HttpFrameError::Invalid("truncated u64"))?;
        Ok(u64::from_be_bytes(value))
    }

    fn u8(&mut self) -> Result<u8, HttpFrameError> {
        Ok(*self
            .take(1)?
            .first()
            .ok_or(HttpFrameError::Invalid("truncated u8"))?)
    }

    fn bytes(&mut self) -> Result<&'a [u8], HttpFrameError> {
        let count = self.u32()?;
        self.take(count)
    }

    fn text(&mut self) -> Result<String, HttpFrameError> {
        std::str::from_utf8(self.bytes()?)
            .map(str::to_owned)
            .map_err(|_| HttpFrameError::Utf8)
    }

    fn headers(&mut self) -> Result<Vec<(String, String)>, HttpFrameError> {
        let count = self.u32()?;
        // Every pair needs at least two zero-length prefixes. Reject absurd
        // counts before attempting an allocation.
        if count > self.frame.len().saturating_sub(self.position) / 8 {
            return Err(HttpFrameError::Invalid("invalid header count"));
        }
        let mut headers = Vec::with_capacity(count);
        for _ in 0..count {
            headers.push((self.text()?, self.text()?));
        }
        Ok(headers)
    }

    fn finish(&self) -> Result<(), HttpFrameError> {
        if self.position == self.frame.len() {
            Ok(())
        } else {
            Err(HttpFrameError::Invalid("trailing bytes"))
        }
    }
}

/// Context object delivered to a cron tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronContext {
    /// The cron expression that fired this invocation.
    pub expression: String,
    /// Unix epoch milliseconds when the fire was scheduled.
    pub scheduled_at_ms: u64,
    /// Unix epoch milliseconds when the fire was actually dispatched.
    pub dispatched_at_ms: u64,
}

pub fn encode_cron_request(context: &CronContext) -> Result<Vec<u8>, HttpFrameError> {
    let mut writer = FrameWriter::new(CRON_REQUEST_FRAME);
    writer.text(&context.expression)?;
    writer.u64(context.scheduled_at_ms);
    writer.u64(context.dispatched_at_ms);
    writer.finish()
}

pub fn decode_cron_request(frame: &[u8]) -> Result<CronContext, HttpFrameError> {
    let mut reader = FrameReader::new(frame, CRON_REQUEST_FRAME)?;
    let context = CronContext {
        expression: reader.text()?,
        scheduled_at_ms: reader.u64()?,
        dispatched_at_ms: reader.u64()?,
    };
    reader.finish()?;
    Ok(context)
}

pub fn encode_cron_response() -> Vec<u8> {
    FrameWriter::new(CRON_RESPONSE_FRAME)
        .finish()
        .expect("empty cron response is bounded")
}

pub fn decode_cron_response(frame: &[u8]) -> Result<(), HttpFrameError> {
    FrameReader::new(frame, CRON_RESPONSE_FRAME)?.finish()
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

/// Queue delivery used by the in-process ABI. Unlike the worker-socket shape,
/// the payload remains raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDispatchMessage {
    pub queue_name: String,
    pub message_id: String,
    pub attempt: u32,
    pub max_retries: u32,
    pub payload: Vec<u8>,
}

pub fn encode_queue_request(message: &QueueDispatchMessage) -> Result<Vec<u8>, HttpFrameError> {
    let mut writer = FrameWriter::new(QUEUE_REQUEST_FRAME);
    writer.text(&message.queue_name)?;
    writer.text(&message.message_id)?;
    writer.u32(message.attempt as usize)?;
    writer.u32(message.max_retries as usize)?;
    writer.bytes(&message.payload)?;
    writer.finish()
}

pub fn decode_queue_request(frame: &[u8]) -> Result<QueueDispatchMessage, HttpFrameError> {
    let mut reader = FrameReader::new(frame, QUEUE_REQUEST_FRAME)?;
    let message = QueueDispatchMessage {
        queue_name: reader.text()?,
        message_id: reader.text()?,
        attempt: u32::try_from(reader.u32()?)
            .map_err(|_| HttpFrameError::Invalid("queue attempt overflow"))?,
        max_retries: u32::try_from(reader.u32()?)
            .map_err(|_| HttpFrameError::Invalid("queue retry overflow"))?,
        payload: reader.bytes()?.to_vec(),
    };
    reader.finish()?;
    Ok(message)
}

pub fn encode_queue_response(disposition: QueueDisposition) -> Vec<u8> {
    let mut writer = FrameWriter::new(QUEUE_RESPONSE_FRAME);
    writer.u8(match disposition {
        QueueDisposition::Ack => 0,
        QueueDisposition::Nack => 1,
        QueueDisposition::Dlq => 2,
    });
    writer
        .finish()
        .expect("queue disposition response is bounded")
}

pub fn decode_queue_response(frame: &[u8]) -> Result<QueueDisposition, HttpFrameError> {
    let mut reader = FrameReader::new(frame, QUEUE_RESPONSE_FRAME)?;
    let disposition = match reader.u8()? {
        0 => QueueDisposition::Ack,
        1 => QueueDisposition::Nack,
        2 => QueueDisposition::Dlq,
        _ => return Err(HttpFrameError::Invalid("invalid queue disposition")),
    };
    reader.finish()?;
    Ok(disposition)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_http_request_round_trip_preserves_raw_body_and_headers() {
        let request = HttpDispatchRequest {
            method: "POST".into(),
            path: "/upload".into(),
            query: "part=1".into(),
            headers: vec![
                ("content-type".into(), "application/octet-stream".into()),
                ("x-repeat".into(), "one".into()),
                ("x-repeat".into(), "two".into()),
            ],
            remote_addr: "127.0.0.1".into(),
            scheme: "http".into(),
            host: "example.test".into(),
            body: vec![0, 1, 2, 0xff],
        };

        let frame = encode_http_request(&request).unwrap();
        assert_eq!(http_request_frame_len(&request).unwrap(), frame.len());
        assert_eq!(decode_http_request(&frame).unwrap(), request);
    }

    /// The Coop rebrand changed `APP_FRAME_MAGIC` from `PCH2` to `COOP`, and
    /// the safety story for that break is "an image built against the old magic
    /// is refused, not misread". That was asserted in a commit message and
    /// tested nowhere, so this pins it.
    ///
    /// The pair is what makes it discriminating. Rejecting a corrupt frame
    /// proves nothing on its own -- a decoder that rejected everything would
    /// pass that half. The same frame, differing only in its four magic bytes,
    /// must decode cleanly with `COOP` and be refused with `PCH2`.
    #[test]
    fn a_frame_carrying_the_retired_pch2_magic_is_refused_not_misread() {
        let request = HttpDispatchRequest {
            method: "GET".into(),
            path: "/health".into(),
            query: String::new(),
            headers: vec![("accept".into(), "application/json".into())],
            remote_addr: "127.0.0.1".into(),
            scheme: "http".into(),
            host: "example.test".into(),
            body: Vec::new(),
        };

        let frame = encode_http_request(&request).unwrap();
        assert_eq!(&frame[..4], b"COOP", "current magic");
        assert_eq!(
            decode_http_request(&frame).unwrap(),
            request,
            "the control half: this frame is otherwise entirely valid"
        );

        // Byte-for-byte identical apart from the retired magic -- exactly what a
        // still-deployed pre-rebrand application image would send.
        let mut stale = frame.clone();
        stale[..4].copy_from_slice(b"PCH2");
        assert_eq!(stale.len(), frame.len());

        let err = decode_http_request(&stale)
            .expect_err("a PCH2 frame must be refused, never decoded as a COOP one");
        assert!(
            matches!(err, HttpFrameError::Invalid(m) if m.contains("magic")),
            "the refusal should name the magic, got: {err:?}"
        );
    }

    #[test]
    fn binary_http_response_round_trip_preserves_raw_body() {
        let response = HttpDispatchResponse {
            status: 201,
            headers: vec![("content-type".into(), "application/json".into())],
            body: br#"{"ok":true}"#.to_vec(),
        };

        let frame = encode_http_response(&response).unwrap();
        assert_eq!(http_response_frame_len(&response).unwrap(), frame.len());
        assert_eq!(decode_http_response(&frame).unwrap(), response);
    }

    #[test]
    fn binary_http_size_checks_reject_oversized_bodies_without_encoding() {
        let request = HttpDispatchRequest {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            headers: vec![],
            remote_addr: String::new(),
            scheme: "http".into(),
            host: String::new(),
            body: vec![0; MAX_FRAME_SIZE],
        };
        assert_eq!(
            http_request_frame_len(&request),
            Err(HttpFrameError::TooLarge)
        );

        let response = HttpDispatchResponse {
            status: 200,
            headers: vec![],
            body: vec![0; MAX_FRAME_SIZE],
        };
        assert_eq!(
            http_response_frame_len(&response),
            Err(HttpFrameError::TooLarge)
        );
    }

    #[test]
    fn binary_http_decoder_rejects_trailing_and_truncated_frames() {
        let response = HttpDispatchResponse {
            status: 204,
            headers: vec![],
            body: vec![],
        };
        let mut frame = encode_http_response(&response).unwrap();
        frame.push(0);
        assert_eq!(
            decode_http_response(&frame),
            Err(HttpFrameError::Invalid("trailing bytes"))
        );
        frame.truncate(6);
        assert!(decode_http_response(&frame).is_err());
    }

    #[test]
    fn binary_cron_round_trip_and_response_are_strict() {
        let context = CronContext {
            expression: "*/5 * * * *".into(),
            scheduled_at_ms: 42,
            dispatched_at_ms: 47,
        };
        assert_eq!(
            decode_cron_request(&encode_cron_request(&context).unwrap()).unwrap(),
            context
        );
        let mut response = encode_cron_response();
        assert_eq!(decode_cron_response(&response), Ok(()));
        response.push(0);
        assert_eq!(
            decode_cron_response(&response),
            Err(HttpFrameError::Invalid("trailing bytes"))
        );
    }

    #[test]
    fn binary_queue_round_trip_preserves_payload_and_disposition() {
        let message = QueueDispatchMessage {
            queue_name: "mail".into(),
            message_id: "m-1".into(),
            attempt: 2,
            max_retries: 5,
            payload: vec![0, 1, 0xff],
        };
        assert_eq!(
            decode_queue_request(&encode_queue_request(&message).unwrap()).unwrap(),
            message
        );
        for disposition in [
            QueueDisposition::Ack,
            QueueDisposition::Nack,
            QueueDisposition::Dlq,
        ] {
            assert_eq!(
                decode_queue_response(&encode_queue_response(disposition)).unwrap(),
                disposition
            );
        }
        let mut invalid = encode_queue_response(QueueDisposition::Ack);
        invalid[5] = 9;
        assert_eq!(
            decode_queue_response(&invalid),
            Err(HttpFrameError::Invalid("invalid queue disposition"))
        );
    }

    #[test]
    fn shard_lifecycle_protocol_preserves_exact_runtime_identity() {
        let request = WorkerRequest::LoadDeployment {
            request_id: 41,
            deployment: WorkerDeploymentSpec {
                deployment: "alpha".into(),
                runtime_id: "runtime-7".into(),
                dylib_path: PathBuf::from("/immutable/alpha/app.so"),
                module_name: Some("alpha".into()),
                executor_stack_size_bytes: 262_144,
                command_queue_capacity: 64,
                gc_reclaim_check_interval: 256,
                gc_reclaim_growth_bytes: 262_144,
                deployment_context_id: 19,
                queue_policies: vec![WorkerQueuePolicy {
                    name: "events".into(),
                    max_payload_bytes: 1024,
                    max_attempts: 5,
                    max_delay_ms: 60_000,
                }],
            },
        };
        let decoded: WorkerRequest =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        let WorkerRequest::LoadDeployment {
            request_id,
            deployment,
        } = decoded
        else {
            panic!("expected load deployment request")
        };
        assert_eq!(request_id, 41);
        assert_eq!(deployment.deployment, "alpha");
        assert_eq!(deployment.runtime_id, "runtime-7");
        assert_eq!(deployment.deployment_context_id, 19);
        assert_eq!(deployment.queue_policies[0].name, "events");

        let unload = WorkerResponse::UnloadResult {
            request_id: 42,
            runtime_id: "runtime-7".into(),
            error: None,
        };
        let decoded: WorkerResponse =
            serde_json::from_slice(&serde_json::to_vec(&unload).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            WorkerResponse::UnloadResult {
                request_id: 42,
                runtime_id,
                error: None,
            } if runtime_id == "runtime-7"
        ));
    }
}
