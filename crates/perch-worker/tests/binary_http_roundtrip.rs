//! Optional process-isolated roundtrip for the real Next benchmark artifact.

use perch_app_host::host::DeploymentHost;
use perch_app_host::initialize_runtime_libraries;
use perch_host_abi::HttpDispatchRequest;
use std::path::PathBuf;

#[tokio::test]
async fn next_fixture_uses_required_http_abi() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("worker crate is inside the workspace")
        .to_path_buf();
    let runtime = workspace.join("var/perch/lib/libperry_runtime.dylib");
    let stdlib = workspace.join("var/perch/lib/libperry_stdlib.dylib");
    let app = workspace.join("target/next-benchmark/perch-run/compiled/next-bench.dylib");
    if !(runtime.exists() && stdlib.exists() && app.exists()) {
        eprintln!("skip: optimized Next benchmark fixture is not built");
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let host = DeploymentHost::load("next-bench", &app, None).expect("preload Next fixture");
    let response = host
        .dispatch_http(HttpDispatchRequest {
            method: "GET".into(),
            path: "/api/benchmark".into(),
            query: "iterations=100".into(),
            headers: vec![("host".into(), "benchmark.local".into())],
            remote_addr: "127.0.0.1".into(),
            scheme: "http".into(),
            host: "benchmark.local".into(),
            body: Vec::new(),
        })
        .await
        .expect("dispatch through HTTP application ABI");

    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers,
        vec![("content-type".into(), "application/json".into())]
    );
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["runtime"], "next");
    assert_eq!(body["iterations"], 100);
    assert_eq!(body["checksum"], 3_726_872_593_u64);
    host.shutdown().await.expect("stop app executor");
}
