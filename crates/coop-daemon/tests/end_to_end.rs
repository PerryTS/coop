//! End-to-end smoke test for the current immutable application contract.
//!
//! The test starts from raw TypeScript, lets Coop compile and publish an
//! app-only content-addressed package, eagerly activates it in a dedicated
//! worker, then verifies static and HTTP routing through the complete stack.
//! It deliberately does not create the removed mutable
//! `compiled/<deployment>.dylib` legacy layout.

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

fn deployment_name() -> &'static str {
    "hello-handler"
}

fn perry_binary() -> PathBuf {
    workspace_root().join(".perry-main/target/perry-dev/perry")
}

fn coop_worker_binary() -> PathBuf {
    // Prefer the debug build since `cargo test` emits debug. If that's
    // missing, try release.
    let debug = workspace_root().join("target/debug/coop-worker");
    if debug.exists() {
        return debug;
    }
    workspace_root().join("target/release/coop-worker")
}

fn coop_daemon_binary() -> PathBuf {
    let debug = workspace_root().join("target/debug/coop");
    if debug.exists() {
        return debug;
    }
    workspace_root().join("target/release/coop")
}

fn perry_libraries() -> (PathBuf, PathBuf) {
    let dir = workspace_root().join("var/coop/lib");
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    (
        dir.join(format!("libperry_runtime.{extension}")),
        dir.join(format!("libperry_stdlib.{extension}")),
    )
}

#[tokio::test]
async fn full_stack_smoke_test() {
    let (perry_runtime, perry_stdlib) = perry_libraries();
    // Skip the test if prerequisites aren't present.
    if !perry_binary().exists() {
        eprintln!(
            "SKIP: {} not found — prepare the pinned Perry compiler first",
            perry_binary().display()
        );
        return;
    }
    if !coop_worker_binary().exists() {
        eprintln!(
            "SKIP: {} not found — run `cargo build -p coop-worker`",
            coop_worker_binary().display()
        );
        return;
    }
    if !coop_daemon_binary().exists() {
        eprintln!(
            "SKIP: {} not found — run `cargo build -p coop-daemon`",
            coop_daemon_binary().display()
        );
        return;
    }
    if !perry_runtime.exists() || !perry_stdlib.exists() {
        eprintln!("SKIP: Perry shared libraries not built");
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

    // Lay out a raw-source deployment. Compilation must create the immutable
    // package; a mutable top-level compiled dylib is intentionally absent.
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
        deployment_dir.join("coop.toml"),
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
    std::fs::write(
        deployment_dir.join("hello-handler.ts"),
        r#"
export function handle(_frame: Buffer): Buffer {
  const body = Buffer.from("echo from coop");
  const name = Buffer.from("content-type");
  const value = Buffer.from("text/plain");
  const output = Buffer.alloc(5 + 2 + 4 + 4 + name.length + 4 + value.length + 4 + body.length);
  output[0] = 0x50; output[1] = 0x43; output[2] = 0x48; output[3] = 0x32; output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(200, offset); offset += 2;
  output.writeUInt32BE(1, offset); offset += 4;
  output.writeUInt32BE(name.length, offset); offset += 4;
  name.copy(output, offset); offset += name.length;
  output.writeUInt32BE(value.length, offset); offset += 4;
  value.copy(output, offset); offset += value.length;
  output.writeUInt32BE(body.length, offset); offset += 4;
  body.copy(output, offset);
  return output;
}
"#,
    )
    .unwrap();

    let port = pick_free_port();

    // Write runtime.toml
    let runtime_toml = var_dir.join("runtime.toml");
    std::fs::write(
        &runtime_toml,
        format!(
            r#"
[http]
listen_http = "127.0.0.1:{port}"

[execution]
mode = "worker"

[paths]
deployments_dir = "{}"
compiled_dir = "{}"
sockets_dir = "{}"
storage_dir = "{}"
logs_dir = "{}"
acme_cache_dir = "{}"
state_db = "{}"
coop_worker_binary = "{}"
perry_binary = "{}"
perry_runtime_library = "{}"
perry_stdlib_library = "{}"

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
            coop_worker_binary().display(),
            perry_binary().display(),
            perry_runtime.display(),
            perry_stdlib.display(),
        ),
    )
    .unwrap();

    // Start coop-daemon
    let mut daemon = tokio::process::Command::new(coop_daemon_binary())
        .arg("--config")
        .arg(&runtime_toml)
        .env("RUST_LOG", "info,coop_daemon=debug,coop_worker=debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn coop daemon");

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
            if resp.status().is_success() {
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
    assert_eq!(unknown_resp.status(), 404, "expected 404 for unknown host");

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

    assert_eq!(
        api_status, 200,
        "expected 200 from strict binary handler, got {}. Body: {}",
        api_status, api_body
    );
    assert_eq!(api_body, "echo from coop");

    // TEST 6: admin UI.
    let admin_resp = client
        .get(format!("{}/_coop/admin", base_url))
        .send()
        .await
        .expect("admin GET");
    assert_eq!(admin_resp.status(), 200, "expected 200 for admin UI");
    let admin_body = admin_resp.text().await.unwrap();
    assert!(
        admin_body.contains("Coop Admin"),
        "admin page should contain 'Coop Admin'"
    );
    assert!(
        admin_body.contains(dep_name),
        "admin page should list the deployment"
    );

    // TEST 7: Prometheus metrics endpoint responds.
    let metrics_resp = client
        .get(format!("{}/_coop/metrics", base_url))
        .send()
        .await
        .expect("metrics GET");
    assert_eq!(metrics_resp.status(), 200, "expected 200 for metrics");

    // Cleanup: stop the daemon.
    let _ = daemon.kill().await;
}
