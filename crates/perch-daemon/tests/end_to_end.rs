//! End-to-end smoke test for Checkpoint 2.
//!
//! Spins up a real perch-daemon with a mock deployment pointing at the
//! existing `hello.dylib` (Phase A.2 artifact), then curls the daemon
//! and verifies the response flows through daemon → worker → plugin →
//! response.
//!
//! Because the Phase A.2 plugin only registers a `"greet"` tool (not a
//! `"route"` tool), this test uses a `perch.toml` whose sole handler
//! points at `tool = "greet"` and expects the dispatch to reach it —
//! with the current wire protocol the worker will pass the JSON-encoded
//! DeploymentRequest as the tool's single string arg, and the plugin
//! will ignore the content and return the fixed `"hello from perry
//! plugin"` string. perch-worker's DeploymentHost will try to parse
//! that string as a DeploymentResponse JSON, fail, and return a 500
//! with the parse error in the body.
//!
//! That's the **expected** behavior for the smoke test: it proves the
//! FULL path (curl → axum → router → worker client → Unix socket →
//! perch-worker → plugin → reply → back through axum → HTTP response)
//! is alive and wired up. The plugin returning a non-JSON string is
//! not the point — the point is that EVERY hop in the chain executes.
//!
//! A proper end-to-end test with a real "route" tool lands once Perry
//! is unblocked and we can compile a plugin that matches the MVP wire
//! protocol.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Pick a free TCP port by binding 0 and reading back the assigned port.
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn workspace_root() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    PathBuf::from(manifest_dir)
        .parent() // crates/
        .and_then(|p| p.parent()) // workspace
        .unwrap()
        .to_path_buf()
}

fn deployment_dylib() -> PathBuf {
    // Prefer the v0.5 hello-handler dylib which has a proper `handle` export.
    let v5 = workspace_root().join("scripts/derisk/build/hello-handler.dylib");
    if v5.exists() {
        return v5;
    }
    workspace_root().join("scripts/derisk/build/hello.dylib")
}

fn deployment_name() -> &'static str {
    // Deployment name must match the dylib stem so module_name_from_path
    // derives the right Perry symbol name.
    "hello-handler"
}

fn using_v5() -> bool {
    deployment_dylib()
        .file_name()
        .map(|n| n.to_str().unwrap_or("").contains("hello-handler"))
        .unwrap_or(false)
}

fn perch_worker_binary() -> PathBuf {
    // Prefer the debug build since `cargo test` emits debug. If that's
    // missing, try release.
    let debug = workspace_root().join("target/debug/perch-worker");
    if debug.exists() {
        return debug;
    }
    workspace_root().join("target/release/perch-worker")
}

fn perch_daemon_binary() -> PathBuf {
    let debug = workspace_root().join("target/debug/perch");
    if debug.exists() {
        return debug;
    }
    workspace_root().join("target/release/perch")
}

#[tokio::test]
async fn full_stack_smoke_test() {
    // Skip the test if prerequisites aren't present.
    if !deployment_dylib().exists() {
        eprintln!(
            "SKIP: {} not found — compile hello-handler.ts with perry first",
            deployment_dylib().display()
        );
        return;
    }
    if !perch_worker_binary().exists() {
        eprintln!(
            "SKIP: {} not found — run `cargo build -p perch-worker`",
            perch_worker_binary().display()
        );
        return;
    }
    if !perch_daemon_binary().exists() {
        eprintln!(
            "SKIP: {} not found — run `cargo build -p perch-daemon`",
            perch_daemon_binary().display()
        );
        return;
    }

    // Create a scratch workspace for this test.
    let tmp = tempfile::tempdir().unwrap();
    let var_dir = tmp.path();
    let deployments_dir = var_dir.join("deployments");
    let compiled_dir = var_dir.join("compiled");
    let sockets_dir = var_dir.join("sockets");
    let storage_dir = var_dir.join("storage");
    let logs_dir = var_dir.join("logs");
    let acme_dir = var_dir.join("acme");

    for d in [
        &deployments_dir,
        &compiled_dir,
        &sockets_dir,
        &storage_dir,
        &logs_dir,
        &acme_dir,
    ] {
        std::fs::create_dir_all(d).unwrap();
    }

    // Lay out a mock deployment. The deployment name MUST match the dylib
    // stem so that module_name_from_path derives the correct Perry symbol.
    let dep_name = deployment_name();
    let deployment_dir = deployments_dir.join(dep_name);
    std::fs::create_dir_all(&deployment_dir).unwrap();
    std::fs::create_dir_all(deployment_dir.join("static")).unwrap();
    std::fs::write(
        deployment_dir.join("static/index.html"),
        "<h1>hello from static</h1>\n",
    )
    .unwrap();

    std::fs::write(
        deployment_dir.join("perch.toml"),
        format!(
            r#"
name = "{dep_name}"

[hosts]
domains = ["hello.test"]

[[handlers]]
file = "hello-handler.ts"
path = "/api"
method = "GET"

[[static]]
directory = "./static"
path = "/"
"#
        ),
    )
    .unwrap();

    // Copy the deployment dylib into compiled_dir. Name matches deployment.
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let compiled_dylib = compiled_dir.join(format!("{}.{}", dep_name, ext));
    std::fs::copy(deployment_dylib(), &compiled_dylib).unwrap();
    let is_v5 = using_v5();

    let port = pick_free_port();

    // Write runtime.toml
    let runtime_toml = var_dir.join("runtime.toml");
    std::fs::write(
        &runtime_toml,
        format!(
            r#"
[http]
listen_http = "127.0.0.1:{port}"

[paths]
deployments_dir = "{}"
compiled_dir = "{}"
sockets_dir = "{}"
storage_dir = "{}"
logs_dir = "{}"
acme_cache_dir = "{}"
state_db = "{}"
perch_worker_binary = "{}"
perry_binary = "perry"

[tls]
mode = "off"
"#,
            deployments_dir.display(),
            compiled_dir.display(),
            sockets_dir.display(),
            storage_dir.display(),
            logs_dir.display(),
            acme_dir.display(),
            var_dir.join("state.sqlite").display(),
            perch_worker_binary().display(),
        ),
    )
    .unwrap();

    // Start perch-daemon
    let mut daemon = tokio::process::Command::new(perch_daemon_binary())
        .arg("--config")
        .arg(&runtime_toml)
        .env("RUST_LOG", "info,perch_daemon=debug,perch_worker=debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn perch daemon");

    // Forward daemon stdout/stderr to the test runner so failures are
    // debuggable.
    if let Some(stdout) = daemon.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[daemon] {}", line);
            }
        });
    }
    if let Some(stderr) = daemon.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[daemon:err] {}", line);
            }
        });
    }

    // Wait for the listener to come up.
    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let mut ready = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(resp) = client
            .get(format!("{}/index.html", base_url))
            .header("host", "hello.test")
            .send()
            .await
        {
            if resp.status().is_success() || resp.status() == 404 {
                ready = true;
                break;
            }
        }
    }

    if !ready {
        let _ = daemon.kill().await;
        panic!("daemon didn't come up within 15 seconds");
    }

    // TEST 1: static serving via host-based routing.
    let static_resp = client
        .get(format!("{}/index.html", base_url))
        .header("host", "hello.test")
        .send()
        .await
        .expect("static GET");
    assert_eq!(static_resp.status(), 200, "expected 200 for /index.html");
    let static_body = static_resp.text().await.unwrap();
    assert!(
        static_body.contains("hello from static"),
        "static body did not contain expected content: {:?}",
        static_body
    );

    // TEST 2: static serving for root path.
    let root_resp = client
        .get(format!("{}/", base_url))
        .header("host", "hello.test")
        .send()
        .await
        .expect("static GET /");
    // tower-http serves index.html by default on a directory request when
    // fallback is set, but by default ServeDir returns 404 for directories
    // without index handling. Either outcome is acceptable here — we only
    // assert the request didn't error out through the daemon.
    assert!(
        root_resp.status() == 200 || root_resp.status() == 404,
        "unexpected status for /: {}",
        root_resp.status()
    );

    // TEST 3: unknown host falls through to 404.
    let unknown_resp = client
        .get(format!("{}/", base_url))
        .header("host", "unknown.test")
        .send()
        .await
        .expect("unknown host GET");
    assert_eq!(
        unknown_resp.status(),
        404,
        "expected 404 for unknown host"
    );

    // TEST 4: path-prefix fallback with no host header.
    let pp_resp = client
        .get(format!("{}/{}/index.html", base_url, dep_name))
        .send()
        .await
        .expect("path-prefix GET");
    assert_eq!(
        pp_resp.status(),
        200,
        "expected 200 for path-prefix /{}/index.html",
        dep_name
    );

    // TEST 5: handler dispatch — GET /api should reach the worker.
    let api_resp = client
        .get(format!("{}/api", base_url))
        .header("host", "hello.test")
        .send()
        .await
        .expect("api GET");
    let api_status = api_resp.status();
    let api_body = api_resp.text().await.unwrap();
    eprintln!("api status: {}", api_status);
    eprintln!("api body: {}", api_body);

    if is_v5 {
        // v0.5 echo-v5.dylib has a proper `handle` function that returns
        // a valid DeploymentResponse JSON → 200 with the echoed body.
        assert_eq!(
            api_status, 200,
            "expected 200 from v0.5 echo handler, got {}. Body: {}",
            api_status, api_body
        );
        assert!(
            api_body.contains("echo from perch"),
            "expected echo body, got: {}",
            api_body
        );
    } else {
        // v0.4 hello.dylib has a "greet" tool (not "handle") → 500 because
        // perch-worker can't find the handle function.
        assert_eq!(
            api_status, 500,
            "expected 500 from v0.4 dispatch (no handle export), got {}",
            api_status
        );
    }

    // TEST 6: admin UI.
    let admin_resp = client
        .get(format!("{}/_perch/admin", base_url))
        .send()
        .await
        .expect("admin GET");
    assert_eq!(admin_resp.status(), 200, "expected 200 for admin UI");
    let admin_body = admin_resp.text().await.unwrap();
    assert!(
        admin_body.contains("Perch Admin"),
        "admin page should contain 'Perch Admin'"
    );
    assert!(
        admin_body.contains(dep_name),
        "admin page should list the deployment"
    );

    // TEST 7: Prometheus metrics endpoint responds.
    let metrics_resp = client
        .get(format!("{}/_perch/metrics", base_url))
        .send()
        .await
        .expect("metrics GET");
    assert_eq!(metrics_resp.status(), 200, "expected 200 for metrics");

    // Cleanup: stop the daemon.
    let _ = daemon.kill().await;
}
