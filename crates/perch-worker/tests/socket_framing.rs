//! Integration test: verify the Unix-socket framed-JSON wire protocol end
//! to end. This test does NOT load a real plugin — it starts a
//! perch-worker listener backed by a stub dispatch function, connects to
//! it from the test, exchanges a Hello, sends a Dispatch request, and
//! verifies the response frame.
//!
//! Why split from the plugin test: we want the wire protocol to be
//! testable independently of Perry being in a working state. That way we
//! can iterate on the daemon and worker framing without depending on a
//! compilable Perry plugin, which is blocked on the gate-fix being
//! re-applied upstream.
//!
//! This test uses a local fork of the listener logic parameterized over a
//! "handler" closure instead of a real DeploymentHost. The real
//! DeploymentHost path is tested by plugin_roundtrip.rs.

use perch_host_abi::{
    ClientHello, DeploymentRequest, DeploymentResponse, WorkerRequest, WorkerResponse,
    ABI_VERSION, MAX_FRAME_SIZE,
};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

async fn read_frame(stream: &mut tokio::net::unix::OwnedReadHalf) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {}", len);
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_frame<T: serde::Serialize>(
    stream: &mut tokio::net::unix::OwnedWriteHalf,
    payload: &T,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(payload)?;
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

/// Start a stub listener that echoes back a fixed response for any
/// Dispatch request. Returns the socket path and a task handle.
async fn start_stub_listener(socket_path: PathBuf) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let listener = UnixListener::bind(&socket_path)?;
    let handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let (read_half, write_half) = stream.into_split();
            let mut reader = read_half;
            let mut writer = write_half;

            // Read hello
            let hello_bytes = match read_frame(&mut reader).await {
                Ok(b) => b,
                Err(_) => return,
            };
            let _hello: WorkerRequest = match serde_json::from_slice(&hello_bytes) {
                Ok(r) => r,
                Err(_) => return,
            };

            // Reply with Hello
            let hello_resp = WorkerResponse::Hello {
                abi_version: ABI_VERSION,
                worker_name: "stub".to_string(),
                deployment: "test".to_string(),
            };
            if write_frame(&mut writer, &hello_resp).await.is_err() {
                return;
            }

            // Read dispatch
            let dispatch_bytes = match read_frame(&mut reader).await {
                Ok(b) => b,
                Err(_) => return,
            };
            let req: WorkerRequest = match serde_json::from_slice(&dispatch_bytes) {
                Ok(r) => r,
                Err(_) => return,
            };

            // Extract request_id and build a fixed response
            let request_id = match req {
                WorkerRequest::Dispatch { request_id, .. } => request_id,
                _ => return,
            };

            let mut headers = HashMap::new();
            headers.insert("content-type".to_string(), "text/plain".to_string());

            let response = WorkerResponse::DispatchResult {
                request_id,
                response: DeploymentResponse {
                    status: 200,
                    headers,
                    body_base64: base64_encode(b"stub response"),
                },
            };
            let _ = write_frame(&mut writer, &response).await;
        }
    });
    Ok(handle)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn hello_then_dispatch_roundtrip() {
    // Pick a temp socket path
    let socket_path = std::env::temp_dir().join(format!("perch-test-{}.sock", std::process::id()));
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    let _server = start_stub_listener(socket_path.clone())
        .await
        .expect("failed to start stub listener");

    // Give the listener a moment to bind
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Connect as a client
    let client = UnixStream::connect(&socket_path)
        .await
        .expect("client connect failed");
    let (read_half, write_half) = client.into_split();
    let mut reader = read_half;
    let mut writer = write_half;

    // Send Hello
    let hello = WorkerRequest::Hello(ClientHello {
        abi_version: ABI_VERSION,
        client_name: "test-client".to_string(),
    });
    write_frame(&mut writer, &hello).await.unwrap();

    // Receive Hello
    let hello_bytes = read_frame(&mut reader).await.unwrap();
    let hello_resp: WorkerResponse = serde_json::from_slice(&hello_bytes).unwrap();
    match hello_resp {
        WorkerResponse::Hello { abi_version, .. } => {
            assert_eq!(abi_version, ABI_VERSION);
        }
        other => panic!("expected Hello response, got {:?}", other),
    }

    // Send Dispatch
    let mut headers = HashMap::new();
    headers.insert("host".to_string(), "test.local".to_string());

    let dispatch = WorkerRequest::Dispatch {
        request_id: 42,
        request: DeploymentRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: "".to_string(),
            headers,
            remote_addr: "127.0.0.1".to_string(),
            scheme: "http".to_string(),
            host: "test.local".to_string(),
            body_base64: "".to_string(),
        },
    };
    write_frame(&mut writer, &dispatch).await.unwrap();

    // Receive DispatchResult
    let result_bytes = read_frame(&mut reader).await.unwrap();
    let result: WorkerResponse = serde_json::from_slice(&result_bytes).unwrap();
    match result {
        WorkerResponse::DispatchResult {
            request_id,
            response,
        } => {
            assert_eq!(request_id, 42);
            assert_eq!(response.status, 200);
            assert_eq!(
                response.headers.get("content-type").map(|s| s.as_str()),
                Some("text/plain")
            );
            let body_bytes = {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(&response.body_base64)
                    .unwrap()
            };
            assert_eq!(body_bytes, b"stub response");
        }
        other => panic!("expected DispatchResult, got {:?}", other),
    }

    // Cleanup
    let _ = std::fs::remove_file(&socket_path);
}
