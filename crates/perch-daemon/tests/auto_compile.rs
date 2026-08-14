//! End-to-end test: automatic Perry compilation.
//!
//! This is the real Perch experience: drop TypeScript into a deployment
//! directory, and the daemon auto-compiles it via Perry and serves it.
//! No pre-compiled dylibs. No manual steps. Just TypeScript → native.
//!
//! Prerequisites:
//! - Perry binary at ~/projects/perry/perry/target/release/perry
//! - Perch daemon + worker built

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

fn pick_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn workspace_root() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn perry_binary() -> PathBuf {
    let p = workspace_root().join(".perry-main/target/perry-dev/perry");
    if p.exists() {
        return p;
    }
    // Fallback to PATH
    PathBuf::from("perry")
}

fn perry_libraries() -> (PathBuf, PathBuf) {
    let dir = workspace_root().join("var/perch/lib");
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

fn perch_worker_binary() -> PathBuf {
    let d = workspace_root().join("target/debug/perch-worker");
    if d.exists() {
        d
    } else {
        workspace_root().join("target/release/perch-worker")
    }
}

fn perch_daemon_binary() -> PathBuf {
    let d = workspace_root().join("target/debug/perch");
    if d.exists() {
        d
    } else {
        workspace_root().join("target/release/perch")
    }
}

fn spawn_daemon(config: &std::path::Path) -> tokio::process::Child {
    let mut daemon = tokio::process::Command::new(perch_daemon_binary())
        .arg("--config")
        .arg(config)
        .env("RUST_LOG", "info,perch_daemon=debug,perch_worker=debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn daemon");

    if let Some(stderr) = daemon.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let r = tokio::io::BufReader::new(stderr);
            let mut lines = r.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[daemon] {}", line);
            }
        });
    }
    if let Some(stdout) = daemon.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let r = tokio::io::BufReader::new(stdout);
            let mut lines = r.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[daemon:out] {}", line);
            }
        });
    }
    daemon
}

fn prometheus_counter(metrics: &str, name: &str, deployment: &str, outcome: &str) -> f64 {
    metrics
        .lines()
        .filter(|line| {
            line.starts_with(name)
                && line.contains(&format!("deployment=\"{deployment}\""))
                && line.contains(&format!("outcome=\"{outcome}\""))
        })
        .filter_map(|line| line.split_whitespace().last()?.parse::<f64>().ok())
        .sum()
}

#[tokio::test]
async fn auto_compile_from_raw_typescript() {
    let (perry_runtime, perry_stdlib) = perry_libraries();
    if !perry_binary().exists() || !perry_runtime.exists() || !perry_stdlib.exists() {
        eprintln!("SKIP: Perry compiler/shared libraries not built");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let v = tmp.path();

    let deployments = v.join("deployments");
    let compiled = v.join("compiled");
    let sockets = v.join("sockets");
    for d in [
        &deployments,
        &compiled,
        &sockets,
        &v.join("storage"),
        &v.join("logs"),
        &v.join("acme"),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }

    // Create a minimal deployment with raw TypeScript — NO pre-compiled dylib.
    let dep_dir = deployments.join("greeter");
    std::fs::create_dir_all(dep_dir.join("handlers")).unwrap();
    std::fs::create_dir_all(dep_dir.join("static")).unwrap();

    std::fs::write(
        dep_dir.join("perch.toml"),
        r#"
name = "greeter"

[hosts]
domains = ["greeter.test"]

[[handlers]]
file = "handlers/greet.ts"
path = "/greet"
method = "GET"

[[crons]]
file = "handlers/cron.ts"
schedule = "*/1 * * * * *"

[[queues]]
file = "handlers/queue.ts"
name = "mail"

[[static]]
directory = "./static"
path = "/"

[activation]
path = "/greet"
method = "GET"
requests = 2
expected_status = 200

[limits]
max_request_body_bytes = 3
max_request_header_bytes = 65536
max_response_body_bytes = 1024
max_response_header_bytes = 65536
max_queue_payload_bytes = 1024
"#,
    )
    .unwrap();

    std::fs::write(
        dep_dir.join("handlers/cron.ts"),
        r#"
export function handle(frame: Buffer): Buffer {
  if (frame.length < 5 || frame[4] !== 3) throw new Error("invalid cron frame");
  const output = Buffer.alloc(5);
  output[0] = 0x50; output[1] = 0x43; output[2] = 0x48; output[3] = 0x32; output[4] = 4;
  return output;
}
"#,
    )
    .unwrap();

    std::fs::write(
        dep_dir.join("handlers/queue.ts"),
        r#"
export function handle(frame: Buffer): Buffer {
  if (frame.length < 5 || frame[4] !== 5) throw new Error("invalid queue frame");
  const output = Buffer.alloc(6);
  output[0] = 0x50; output[1] = 0x43; output[2] = 0x48; output[3] = 0x32; output[4] = 6; output[5] = 0;
  return output;
}
"#,
    )
    .unwrap();

    let greet_source = dep_dir.join("handlers/greet.ts");
    let greet_v1 = r#"
export function handle(_frame: Buffer): Buffer {
  const body = Buffer.from("Hello, World! Binary ABI");
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
"#;
    std::fs::write(&greet_source, greet_v1).unwrap();

    let static_v1 = "<h1>Greeter</h1><a href=\"/greet\">Say hello</a>\n";
    std::fs::write(dep_dir.join("static/index.html"), static_v1).unwrap();

    let port = pick_free_port();
    let rt = v.join("runtime.toml");
    let admin_password_hash = bcrypt::hash("test-secret", 4).unwrap();
    let cgroup_config = std::env::var("PERCH_TEST_CGROUP_ROOT")
        .ok()
        .map(|root| {
            format!(
                "\n[execution.cgroup]\nmode = \"required\"\nroot = {}\n",
                serde_json::to_string(&root).unwrap()
            )
        })
        .unwrap_or_default();
    std::fs::write(
        &rt,
        format!(
            r#"
[http]
listen_http = "127.0.0.1:{port}"
[execution.shards]
count = 1
max_apps = 4
max_rss_mb = 1024
max_cpu_percent = 400
max_pids = 128
[paths]
deployments_dir = "{}"
compiled_dir = "{}"
sockets_dir = "{}"
storage_dir = "{}"
logs_dir = "{}"
acme_cache_dir = "{}"
state_db = "{}"
perch_worker_binary = "{}"
perry_binary = "{}"
perry_runtime_library = "{}"
perry_stdlib_library = "{}"
[queue_service]
allow_handlers_without_store = true
[tls]
mode = "off"
[admin]
path = "/_perch/admin"
password_hash = "{}"
{}
"#,
            deployments.display(),
            compiled.display(),
            sockets.display(),
            v.join("storage").display(),
            v.join("logs").display(),
            v.join("acme").display(),
            v.join("state.sqlite").display(),
            perch_worker_binary().display(),
            perry_binary().display(),
            perry_runtime.display(),
            perry_stdlib.display(),
            admin_password_hash,
            cgroup_config,
        ),
    )
    .unwrap();

    // Verify NO dylib exists yet.
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let legacy_dylib = compiled.join(format!("greeter.{}", ext));
    let artifact_namespace = compiled.join("greeter");
    assert!(
        !legacy_dylib.exists() && !artifact_namespace.exists(),
        "compiled artifact should not exist before daemon starts"
    );

    // Explicit deploy-pipeline prebuild publishes an immutable package but
    // does not activate it or write deployment state.
    let prebuild = tokio::process::Command::new(perch_daemon_binary())
        .arg("--config")
        .arg(&rt)
        .arg("build")
        .arg("greeter")
        .output()
        .await
        .expect("run explicit prebuild");
    assert!(
        prebuild.status.success(),
        "prebuild failed: {}",
        String::from_utf8_lossy(&prebuild.stderr)
    );
    assert!(artifact_namespace.exists());
    assert!(
        !artifact_namespace
            .join(".perch-deployment-state.json")
            .exists(),
        "prebuild must not activate or mutate deployment state"
    );

    // A static-only package change must not invoke Perry or create different
    // application bytes. It publishes a new immutable config/static package
    // around the already verified compiled image.
    let first_prebuild_dir = std::fs::read_dir(&artifact_namespace)
        .unwrap()
        .find_map(|entry| {
            let entry = entry.ok()?;
            entry.file_type().ok()?.is_dir().then_some(entry.path())
        })
        .expect("first prebuild package");
    let first_prebuild_library = first_prebuild_dir.join(format!("app.{ext}"));
    let first_prebuild_manifest = perch_host_abi::AppLibraryManifest::load(&first_prebuild_library)
        .unwrap()
        .unwrap();
    std::fs::write(
        dep_dir.join("static/index.html"),
        "<h1>static-only change</h1>\n",
    )
    .unwrap();
    let static_prebuild = tokio::process::Command::new(perch_daemon_binary())
        .arg("--config")
        .arg(&rt)
        .arg("build")
        .arg("greeter")
        .output()
        .await
        .expect("run static-only prebuild");
    assert!(
        static_prebuild.status.success(),
        "static-only prebuild failed: {}",
        String::from_utf8_lossy(&static_prebuild.stderr)
    );
    let static_package_dir = std::fs::read_dir(&artifact_namespace)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir() && path != &first_prebuild_dir)
        .expect("static-only package");
    let static_library = static_package_dir.join(format!("app.{ext}"));
    let static_manifest = perch_host_abi::AppLibraryManifest::load(&static_library)
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(&first_prebuild_library).unwrap(),
        std::fs::read(&static_library).unwrap(),
        "static-only package must reuse exact application image bytes"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&first_prebuild_library).unwrap().ino(),
            std::fs::metadata(&static_library).unwrap().ino(),
            "same-filesystem packages should hard-link verified app code"
        );
    }
    assert_eq!(
        first_prebuild_manifest.compile_source_sha256,
        static_manifest.compile_source_sha256
    );
    assert_ne!(
        first_prebuild_manifest.source_sha256,
        static_manifest.source_sha256
    );
    let static_prebuild_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&static_prebuild.stdout),
        String::from_utf8_lossy(&static_prebuild.stderr)
    );
    assert!(
        static_prebuild_log.contains("reused verified compiled application image"),
        "static-only prebuild did not report compiled-code reuse:\n{static_prebuild_log}"
    );
    std::fs::write(dep_dir.join("static/index.html"), static_v1).unwrap();
    std::fs::remove_dir_all(static_package_dir).unwrap();

    // Start daemon.
    let mut daemon = spawn_daemon(&rt);

    let base = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    // Wait for daemon to compile and come up. Perry compile takes a few
    // seconds (auto-optimize rebuild), so we wait longer.
    let mut ready = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(resp) = client
            .get(format!("{}/index.html", base))
            .header("host", "greeter.test")
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
        // Check if the dylib was created (compile may have succeeded even
        // if the worker didn't start).
        eprintln!("artifact namespace exists: {}", artifact_namespace.exists());
        panic!("daemon didn't come up within 60 seconds — check [daemon] logs above");
    }

    let initial_health: serde_json::Value = client
        .get(format!("{}/_perch/admin/deployments/greeter/health", base))
        .send()
        .await
        .expect("activation health response")
        .json()
        .await
        .expect("activation health JSON");
    assert_eq!(initial_health["outcome"], "success");
    assert_eq!(initial_health["configured"], true);
    assert_eq!(initial_health["requests"], 2);
    assert_eq!(initial_health["last_status"], 200);

    let memory_without_auth = client
        .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
        .send()
        .await
        .expect("unauthenticated memory response");
    assert_eq!(
        memory_without_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let initial_memory: serde_json::Value = client
        .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
        .basic_auth("perch", Some("test-secret"))
        .send()
        .await
        .expect("deployment memory response")
        .json()
        .await
        .expect("deployment memory JSON");
    assert_eq!(initial_memory["execution_mode"], "in_process");
    assert_eq!(initial_memory["requested_isolation_class"], "inherit");
    assert_eq!(initial_memory["effective_isolation_class"], "trusted");
    assert!(initial_memory["arena_live_bytes"].as_u64().unwrap() > 0);
    assert!(
        initial_memory["arena_reserved_bytes"].as_u64().unwrap()
            >= initial_memory["arena_live_bytes"].as_u64().unwrap()
    );

    // An exact reload is a true no-op: it neither starts another executor nor
    // reruns a probe for the already healthy immutable package.
    let no_op_reload = client
        .post(format!("{}/_perch/admin/deployments/greeter/reload", base))
        .basic_auth("perch", Some("test-secret"))
        .header("x-perch-confirm", "reload")
        .send()
        .await
        .expect("exact no-op reload");
    assert_eq!(no_op_reload.status(), 200);
    let health_after_no_op: serde_json::Value = client
        .get(format!("{}/_perch/admin/deployments/greeter/health", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        health_after_no_op["completed_at_ms"], initial_health["completed_at_ms"],
        "an exact reload must retain the current healthy generation"
    );

    // The complete library+manifest package is published by one directory
    // rename under a content-addressed deployment namespace.
    let artifact_dirs = std::fs::read_dir(&artifact_namespace)
        .expect("content-addressed deployment namespace")
        .filter(|entry| {
            entry
                .as_ref()
                .ok()
                .and_then(|entry| entry.file_type().ok())
                .is_some_and(|kind| kind.is_dir())
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("read content-addressed deployment namespace");
    assert_eq!(artifact_dirs.len(), 1, "one immutable artifact expected");
    let dylib = artifact_dirs[0].path().join(format!("app.{ext}"));
    assert!(
        dylib.exists(),
        "perry should have compiled the dylib at {:?}",
        dylib
    );
    assert!(
        !legacy_dylib.exists(),
        "auto-compile must not overwrite the legacy mutable path"
    );
    assert!(
        !dep_dir.join("perch_entry.ts").exists(),
        "generated multi-entry source must be removed after compile"
    );
    let app_manifest = dylib.with_extension("perch-lib.json");
    assert!(
        app_manifest.exists(),
        "Perch should have written an ABI manifest at {:?}",
        app_manifest
    );
    assert!(
        dylib
            .parent()
            .unwrap()
            .join("deployment.perch.json")
            .exists(),
        "rollbackable package should carry its exact deployment config"
    );
    assert!(
        artifact_namespace
            .join(".perch-deployment-state.json")
            .exists(),
        "activation should atomically record the active package"
    );
    let manifest = perch_host_abi::AppLibraryManifest::load(&dylib)
        .unwrap()
        .expect("app manifest");
    assert!(manifest.boundary_verified);
    assert!(manifest.library_sha256.is_some());
    assert!(manifest.source_sha256.is_some());
    assert_eq!(
        manifest.library_size,
        Some(std::fs::metadata(&dylib).unwrap().len())
    );
    assert_eq!(manifest.handler_abi, perch_host_abi::HandlerAbi::Wrapped);
    assert_eq!(manifest.cron_entries.len(), 1);
    assert_eq!(manifest.queue_entries.len(), 1);

    let mut nm = std::process::Command::new("nm");
    #[cfg(target_os = "macos")]
    nm.arg("-gU");
    #[cfg(not(target_os = "macos"))]
    nm.args(["-D", "--defined-only"]);
    let exports = nm.arg(&dylib).output().expect("inspect app exports");
    assert!(exports.status.success());
    assert_eq!(
        String::from_utf8_lossy(&exports.stdout).lines().count(),
        4,
        "app dylib should only export init and configured Buffer entries"
    );

    // TEST: GET /greet → handler responds.
    let resp = client
        .get(format!("{}/greet", base))
        .header("host", "greeter.test")
        .send()
        .await
        .expect("GET /greet");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    eprintln!("GET /greet → {} body={}", status, body);
    assert_eq!(status, 200, "expected 200 from auto-compiled handler");
    assert_eq!(body, "Hello, World! Binary ABI");

    let oversized = client
        .request(reqwest::Method::GET, format!("{}/greet", base))
        .header("host", "greeter.test")
        .body("four")
        .send()
        .await
        .expect("oversized GET body");
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        oversized
            .headers()
            .get("x-perch-error")
            .and_then(|value| value.to_str().ok()),
        Some("request_body_too_large")
    );

    let first_package = artifact_dirs[0].file_name().to_string_lossy().to_string();
    let greet_v2 = greet_v1.replace(
        "Hello, World! Binary ABI",
        "Hello from immutable package v2",
    );
    std::fs::write(&greet_source, greet_v2).unwrap();
    let config_v2 = std::fs::read_to_string(dep_dir.join("perch.toml"))
        .unwrap()
        .replace("max_request_body_bytes = 3", "max_request_body_bytes = 5");
    std::fs::write(dep_dir.join("perch.toml"), &config_v2).unwrap();
    std::fs::write(dep_dir.join("static/index.html"), "<h1>Greeter v2</h1>\n").unwrap();
    let denied_reload = client
        .post(format!("{}/_perch/admin/deployments/greeter/reload", base))
        .header("x-perch-confirm", "reload")
        .send()
        .await
        .expect("unauthenticated reload rejection");
    assert_eq!(denied_reload.status(), reqwest::StatusCode::UNAUTHORIZED);
    let reload = client
        .post(format!("{}/_perch/admin/deployments/greeter/reload", base))
        .basic_auth("perch", Some("test-secret"))
        .header("x-perch-confirm", "reload")
        .send()
        .await
        .expect("explicit authenticated reload");
    assert_eq!(
        reload.status(),
        200,
        "reload failed: {:?}",
        reload.text().await
    );
    // Allow a duplicate watcher event to serialize behind the explicit reload
    // before testing rollback. Its exact cache hit must not create a package.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let v2 = client
        .get(format!("{}/greet", base))
        .header("host", "greeter.test")
        .send()
        .await
        .unwrap();
    assert_eq!(v2.status(), 200);
    assert_eq!(v2.text().await.unwrap(), "Hello from immutable package v2");
    let v2_larger_body = client
        .request(reqwest::Method::GET, format!("{}/greet", base))
        .header("host", "greeter.test")
        .body("four")
        .send()
        .await
        .unwrap();
    assert_eq!(v2_larger_body.status(), 200, "v2 config raises body limit");
    let v2_static = client
        .get(format!("{}/index.html", base))
        .header("host", "greeter.test")
        .send()
        .await
        .unwrap();
    assert_eq!(v2_static.text().await.unwrap(), "<h1>Greeter v2</h1>\n");

    // A compiled and initialized generation that fails its app-defined health
    // contract never changes routing or active artifact state.
    let failing_health_config = config_v2.replace("expected_status = 200", "expected_status = 201");
    std::fs::write(dep_dir.join("perch.toml"), &failing_health_config).unwrap();
    let failed_health_reload = client
        .post(format!("{}/_perch/admin/deployments/greeter/reload", base))
        .basic_auth("perch", Some("test-secret"))
        .header("x-perch-confirm", "reload")
        .send()
        .await
        .expect("health-gated reload response");
    assert_eq!(failed_health_reload.status(), reqwest::StatusCode::CONFLICT);
    let retained_v2 = client
        .get(format!("{}/greet", base))
        .header("host", "greeter.test")
        .send()
        .await
        .unwrap();
    assert_eq!(
        retained_v2.text().await.unwrap(),
        "Hello from immutable package v2"
    );
    let retained_health: serde_json::Value = client
        .get(format!("{}/_perch/admin/deployments/greeter/health", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(retained_health["outcome"], "success");
    assert_eq!(retained_health["last_status"], 200);
    std::fs::write(dep_dir.join("perch.toml"), &config_v2).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    let artifact_status: serde_json::Value = client
        .get(format!(
            "{}/_perch/admin/deployments/greeter/artifacts",
            base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(artifact_status["packages"].as_array().unwrap().len(), 3);
    assert!(artifact_status["retained_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        artifact_status["previous"][0].as_str(),
        Some(first_package.as_str())
    );

    let rollback = client
        .post(format!(
            "{}/_perch/admin/deployments/greeter/rollback/{}",
            base, first_package
        ))
        .basic_auth("perch", Some("test-secret"))
        .header("x-perch-confirm", "rollback")
        .send()
        .await
        .expect("authenticated rollback");
    assert_eq!(
        rollback.status(),
        200,
        "rollback failed: {:?}",
        rollback.text().await
    );
    let rolled_back = client
        .get(format!("{}/greet", base))
        .header("host", "greeter.test")
        .send()
        .await
        .unwrap();
    assert_eq!(rolled_back.status(), 200);
    assert_eq!(
        rolled_back.text().await.unwrap(),
        "Hello, World! Binary ABI"
    );
    let restored_limit = client
        .request(reqwest::Method::GET, format!("{}/greet", base))
        .header("host", "greeter.test")
        .body("four")
        .send()
        .await
        .unwrap();
    assert_eq!(
        restored_limit.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "rollback must restore the package's deployment limits"
    );

    let metrics = client
        .get(format!("{}/_perch/metrics", base))
        .send()
        .await
        .expect("metrics response")
        .text()
        .await
        .expect("metrics body");
    assert!(metrics.contains("perch_requests_total"));
    assert!(metrics.contains("deployment=\"greeter\""));
    assert!(metrics.contains("perch_deployments_total 1"));
    assert!(metrics.contains("perch_invocation_rejections_total"));
    assert!(metrics.contains("reason=\"request_body\""));
    assert!(metrics.contains("perch_deployment_rollbacks_total"));
    assert!(metrics.contains("perch_deployment_activations_total"));
    assert!(metrics.contains("outcome=\"failure\""));
    assert!(metrics.contains("perch_artifact_retained_bytes"));
    assert!(metrics.contains("perch_application_arena_live_bytes"));
    assert!(metrics.contains("perch_application_arena_reserved_bytes"));
    assert!(metrics.contains("perch_deployment_process_isolated"));
    assert!(metrics.contains("perch_deployment_isolation_inherited"));

    // TEST: static serving also works.
    let static_resp = client
        .get(format!("{}/index.html", base))
        .header("host", "greeter.test")
        .send()
        .await
        .unwrap();
    assert_eq!(static_resp.status(), 200);
    assert_eq!(
        static_resp.text().await.unwrap(),
        "<h1>Greeter</h1><a href=\"/greet\">Say hello</a>\n",
        "rollback must restore immutable static assets"
    );

    // Rollback is durable deployment state, not only an in-memory route swap.
    // The mutable source still contains v2, so a restart that recompiles or
    // selects sources instead of the persisted package would serve v2 here.
    daemon.kill().await.expect("stop daemon before restart");
    daemon.wait().await.expect("reap daemon before restart");
    daemon = spawn_daemon(&rt);
    let mut restart_body = None;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(response) = client
            .get(format!("{}/greet", base))
            .header("host", "greeter.test")
            .send()
            .await
        {
            if response.status().is_success() {
                restart_body = response.text().await.ok();
                break;
            }
        }
    }
    assert_eq!(
        restart_body.as_deref(),
        Some("Hello, World! Binary ABI"),
        "restart must restore the exact package selected by rollback"
    );
    assert_eq!(
        std::fs::read_dir(&artifact_namespace)
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .ok()
                .and_then(|entry| entry.file_type().ok())
                .is_some_and(|kind| kind.is_dir()))
            .count(),
        2,
        "restart must not publish or recompile another package"
    );

    // A broken replacement compiles only in staging. It must neither mutate
    // the active immutable package nor remove the already-routable runtime.
    std::fs::write(
        &greet_source,
        "export function handle(frame: Buffer): Buffer { return Buffer.from(\"unterminated); }",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    let retained = client
        .get(format!("{}/greet", base))
        .header("host", "greeter.test")
        .send()
        .await
        .expect("old deployment remains routable after failed replacement");
    assert_eq!(retained.status(), 200);
    assert_eq!(
        retained.text().await.unwrap(),
        "Hello, World! Binary ABI",
        "failed replacement must leave previous runtime healthy"
    );
    assert_eq!(
        std::fs::read_dir(&artifact_namespace)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .ok()
                    .and_then(|entry| entry.file_type().ok())
                    .is_some_and(|kind| kind.is_dir())
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .len(),
        2,
        "failed replacement must not publish another artifact"
    );
    let staging_root = compiled.join(".staging");
    assert!(
        !staging_root.exists() || std::fs::read_dir(staging_root).unwrap().next().is_none(),
        "failed replacement staging package must be cleaned"
    );

    // Isolation is deployment-owned. Exercise both explicit overrides under
    // the same daemon whose box-wide default is in-process. Changing only the
    // policy must start the requested runtime rather than reusing the current
    // byte-identical runtime across a failure-domain boundary.
    let mut idle_shard_pid = None;
    if perch_worker_binary().exists() {
        let restored_v2 = greet_v1
            .replace(
                "Hello, World! Binary ABI",
                "Hello from immutable package v2",
            )
            .replace(
                "export function handle(_frame: Buffer): Buffer {",
                r#"export function handle(_frame: Buffer): Buffer {
  let requestOffset = 5;
  const methodLength = _frame.readUInt32BE(requestOffset); requestOffset += 4 + methodLength;
  const pathLength = _frame.readUInt32BE(requestOffset); requestOffset += 4;
  const requestPath = _frame.subarray(requestOffset, requestOffset + pathLength).toString();
  if (requestPath === "/hang") { while (true) {} }"#,
            );
        std::fs::write(&greet_source, restored_v2).unwrap();
        let dedicated_config = format!(
            "{}\n\n[[handlers]]\nfile = \"handlers/greet.ts\"\npath = \"/hang\"\nmethod = \"GET\"\n\n[isolation]\nclass = \"dedicated\"\n",
            config_v2.replace(
                "max_queue_payload_bytes = 1024",
                "max_queue_payload_bytes = 1024\nmax_wall_clock_ms = 250"
            )
        );
        std::fs::write(dep_dir.join("perch.toml"), &dedicated_config).unwrap();
        let dedicated_reload = client
            .post(format!("{}/_perch/admin/deployments/greeter/reload", base))
            // A debug Perry worker may spend several seconds loading the
            // provider libraries after a cold compile. Keep the HTTP
            // assertion comfortably above that activation cost; request
            // deadline behavior is tested separately through `/hang`.
            .timeout(Duration::from_secs(30))
            .basic_auth("perch", Some("test-secret"))
            .header("x-perch-confirm", "reload")
            .send()
            .await
            .expect("dedicated isolation reload");
        assert_eq!(
            dedicated_reload.status(),
            200,
            "dedicated reload failed: {:?}",
            dedicated_reload.text().await
        );
        let mut dedicated_memory: serde_json::Value = client
            .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
            .basic_auth("perch", Some("test-secret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(dedicated_memory["requested_isolation_class"], "dedicated");
        assert_eq!(dedicated_memory["effective_isolation_class"], "dedicated");
        assert_eq!(dedicated_memory["execution_mode"], "worker");
        assert!(dedicated_memory["worker_pid"].as_u64().is_some());
        if std::env::var_os("PERCH_TEST_CGROUP_ROOT").is_some() {
            assert!(dedicated_memory["cgroup"]["memory_current_bytes"]
                .as_u64()
                .is_some());
            assert!(dedicated_memory["cgroup"]["memory_peak_bytes"]
                .as_u64()
                .is_some());
        } else {
            assert!(dedicated_memory["cgroup"].is_null());
        }
        let dedicated_response = client
            .get(format!("{}/greet", base))
            .header("host", "greeter.test")
            .send()
            .await
            .unwrap();
        assert_eq!(dedicated_response.status(), 200);
        assert_eq!(
            dedicated_response.text().await.unwrap(),
            "Hello from immutable package v2"
        );

        // A native call cannot be interrupted safely inside a process. The
        // deadline must still return promptly, poison the ordered transport,
        // replace the entire dedicated worker generation, and restore healthy
        // traffic from the immutable package.
        let timed_out_pid = dedicated_memory["worker_pid"].as_u64().unwrap();
        let timed_out = client
            .get(format!("{}/hang", base))
            .header("host", "greeter.test")
            .send()
            .await
            .expect("non-returning worker request receives its deadline");
        assert_eq!(timed_out.status(), reqwest::StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            timed_out
                .headers()
                .get("x-perch-error")
                .and_then(|value| value.to_str().ok()),
            Some("deadline_exceeded")
        );
        let mut timeout_replacement = None;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let Ok(response) = client
                .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
                .basic_auth("perch", Some("test-secret"))
                .send()
                .await
            else {
                continue;
            };
            let Ok(memory) = response.json::<serde_json::Value>().await else {
                continue;
            };
            if memory["worker_pid"]
                .as_u64()
                .is_some_and(|pid| pid != timed_out_pid)
            {
                timeout_replacement = Some(memory);
                break;
            }
        }
        dedicated_memory = timeout_replacement
            .expect("timed-out dedicated worker generation was not automatically replaced");
        let after_timeout = client
            .get(format!("{}/greet", base))
            .header("host", "greeter.test")
            .send()
            .await
            .unwrap();
        assert_eq!(after_timeout.status(), 200);
        assert_eq!(
            after_timeout.text().await.unwrap(),
            "Hello from immutable package v2"
        );

        let killed_pid = dedicated_memory["worker_pid"].as_u64().unwrap() as libc::pid_t;
        assert_eq!(
            unsafe { libc::kill(killed_pid, libc::SIGKILL) },
            0,
            "kill dedicated worker generation"
        );
        let mut replacement_pid = None;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let Ok(response) = client
                .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
                .basic_auth("perch", Some("test-secret"))
                .send()
                .await
            else {
                continue;
            };
            let Ok(memory) = response.json::<serde_json::Value>().await else {
                continue;
            };
            replacement_pid = memory["worker_pid"]
                .as_u64()
                .filter(|pid| *pid != killed_pid as u64);
            if replacement_pid.is_some() {
                break;
            }
        }
        assert!(
            replacement_pid.is_some(),
            "idle killed dedicated worker was not automatically replaced"
        );
        let after_worker_crash = client
            .get(format!("{}/greet", base))
            .header("host", "greeter.test")
            .send()
            .await
            .unwrap();
        assert_eq!(after_worker_crash.status(), 200);
        assert_eq!(
            after_worker_crash.text().await.unwrap(),
            "Hello from immutable package v2"
        );
        let restart_metrics = client
            .get(format!("{}/_perch/metrics", base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(restart_metrics.contains("perch_worker_restarts_total"));
        assert!(restart_metrics.contains("reason=\"exit\""));
        assert!(restart_metrics.contains("perch_worker_transport_backlog"));
        assert!(restart_metrics.contains("perch_worker_transport_in_flight"));
        assert!(restart_metrics.contains("perch_worker_transport_queue_wait_seconds"));
        assert!(restart_metrics.contains("perch_worker_transport_round_trip_seconds"));
        assert!(restart_metrics.contains("perch_worker_transport_bytes_total"));
        assert!(restart_metrics.contains("perch_worker_transport_poisoned_total"));
        assert!(restart_metrics.contains("cause=\"deadline\""));

        // A real shard must contain multiple independently compiled
        // deployments, not merely rename the dedicated-worker mode. Force one
        // hash slot, load a second deployment, and prove both routes share the
        // same supervised process while retaining distinct generations.
        let sidekick_dir = deployments.join("sidekick");
        std::fs::create_dir_all(sidekick_dir.join("handlers")).unwrap();
        std::fs::create_dir_all(sidekick_dir.join("static")).unwrap();
        for file in ["greet.ts", "cron.ts", "queue.ts"] {
            std::fs::copy(
                dep_dir.join("handlers").join(file),
                sidekick_dir.join("handlers").join(file),
            )
            .unwrap();
        }
        std::fs::copy(
            dep_dir.join("static/index.html"),
            sidekick_dir.join("static/index.html"),
        )
        .unwrap();
        let sidekick_config = config_v2
            .replacen("name = \"greeter\"", "name = \"sidekick\"", 1)
            .replacen("greeter.test", "sidekick.test", 1)
            + "\n\n[isolation]\nclass = \"sharded\"\n";
        std::fs::write(sidekick_dir.join("perch.toml"), &sidekick_config).unwrap();

        let sharded_config =
            dedicated_config.replace("class = \"dedicated\"", "class = \"sharded\"");
        std::fs::write(dep_dir.join("perch.toml"), &sharded_config).unwrap();
        let sharded_reload = client
            .post(format!("{}/_perch/admin/deployments/greeter/reload", base))
            .timeout(Duration::from_secs(30))
            .basic_auth("perch", Some("test-secret"))
            .header("x-perch-confirm", "reload")
            .send()
            .await
            .expect("sharded greeter reload");
        assert_eq!(
            sharded_reload.status(),
            200,
            "sharded greeter reload failed: {:?}",
            sharded_reload.text().await
        );
        let sidekick_reload = client
            .post(format!("{}/_perch/admin/deployments/sidekick/reload", base))
            .timeout(Duration::from_secs(30))
            .basic_auth("perch", Some("test-secret"))
            .header("x-perch-confirm", "reload")
            .send()
            .await
            .expect("sharded sidekick reload");
        assert_eq!(
            sidekick_reload.status(),
            200,
            "sharded sidekick reload failed: {:?}",
            sidekick_reload.text().await
        );

        // Both deployments now run in the shard. Capture the cumulative
        // greeter counter only after publication, then require a subsequent
        // scheduled fire to cross the runtime-ID-aware shard cron protocol.
        let cron_before = client
            .get(format!("{}/_perch/metrics", base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let cron_before = prometheus_counter(
            &cron_before,
            "perch_cron_invocations_total",
            "greeter",
            "success",
        );
        let mut shard_cron_observed = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let metrics = client
                .get(format!("{}/_perch/metrics", base))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            if prometheus_counter(
                &metrics,
                "perch_cron_invocations_total",
                "greeter",
                "success",
            ) > cron_before
            {
                shard_cron_observed = true;
                break;
            }
        }
        assert!(
            shard_cron_observed,
            "scheduled cron did not complete through the shard runtime protocol"
        );

        let greeter_shard: serde_json::Value = client
            .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
            .basic_auth("perch", Some("test-secret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let sidekick_shard: serde_json::Value = client
            .get(format!("{}/_perch/admin/deployments/sidekick/memory", base))
            .basic_auth("perch", Some("test-secret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(greeter_shard["requested_isolation_class"], "sharded");
        assert_eq!(greeter_shard["effective_isolation_class"], "sharded");
        assert_eq!(greeter_shard["execution_mode"], "shard");
        assert_eq!(greeter_shard["shard_slot"], 0);
        assert_eq!(sidekick_shard["execution_mode"], "shard");
        assert_eq!(sidekick_shard["shard_slot"], 0);
        let mut shared_pid = greeter_shard["worker_pid"].as_u64().unwrap();
        assert_eq!(sidekick_shard["worker_pid"].as_u64(), Some(shared_pid));
        assert_eq!(
            sidekick_shard["shard_generation"],
            greeter_shard["shard_generation"]
        );
        let sidekick_response = client
            .get(format!("{}/greet", base))
            .header("host", "sidekick.test")
            .send()
            .await
            .unwrap();
        assert_eq!(sidekick_response.status(), 200);
        assert_eq!(
            sidekick_response.text().await.unwrap(),
            "Hello from immutable package v2"
        );

        // A hard deadline in one resident application invalidates the shared
        // native failure domain. The sibling is disrupted by design, but both
        // deployments must be restored together in one new shard generation.
        let shard_timeout = client
            .get(format!("{}/hang", base))
            .header("host", "greeter.test")
            .send()
            .await
            .expect("non-returning shard request receives its deadline");
        assert_eq!(shard_timeout.status(), reqwest::StatusCode::GATEWAY_TIMEOUT);
        let mut timeout_shard_pid = None;
        for _ in 0..180 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let greeter = client
                .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
                .basic_auth("perch", Some("test-secret"))
                .send()
                .await
                .ok()
                .and_then(|response| response.error_for_status().ok());
            let sidekick = client
                .get(format!("{}/_perch/admin/deployments/sidekick/memory", base))
                .basic_auth("perch", Some("test-secret"))
                .send()
                .await
                .ok()
                .and_then(|response| response.error_for_status().ok());
            let (Some(greeter), Some(sidekick)) = (greeter, sidekick) else {
                continue;
            };
            let Ok(greeter) = greeter.json::<serde_json::Value>().await else {
                continue;
            };
            let Ok(sidekick) = sidekick.json::<serde_json::Value>().await else {
                continue;
            };
            let greeter_pid = greeter["worker_pid"].as_u64();
            let sidekick_pid = sidekick["worker_pid"].as_u64();
            if greeter_pid == sidekick_pid && greeter_pid.is_some_and(|pid| pid != shared_pid) {
                timeout_shard_pid = greeter_pid;
                break;
            }
        }
        shared_pid =
            timeout_shard_pid.expect("timed-out shard did not restore every resident deployment");
        for host in ["greeter.test", "sidekick.test"] {
            let response = client
                .get(format!("{}/greet", base))
                .header("host", host)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                200,
                "route did not recover after hard shard timeout for {host}"
            );
        }

        assert_eq!(
            unsafe { libc::kill(shared_pid as libc::pid_t, libc::SIGKILL) },
            0,
            "kill shared worker shard"
        );
        let mut replacement_shard_pid = None;
        for _ in 0..180 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let greeter = client
                .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
                .basic_auth("perch", Some("test-secret"))
                .send()
                .await
                .ok()
                .and_then(|response| response.error_for_status().ok());
            let sidekick = client
                .get(format!("{}/_perch/admin/deployments/sidekick/memory", base))
                .basic_auth("perch", Some("test-secret"))
                .send()
                .await
                .ok()
                .and_then(|response| response.error_for_status().ok());
            let (Some(greeter), Some(sidekick)) = (greeter, sidekick) else {
                continue;
            };
            let Ok(greeter) = greeter.json::<serde_json::Value>().await else {
                continue;
            };
            let Ok(sidekick) = sidekick.json::<serde_json::Value>().await else {
                continue;
            };
            let greeter_pid = greeter["worker_pid"].as_u64();
            let sidekick_pid = sidekick["worker_pid"].as_u64();
            if greeter_pid == sidekick_pid && greeter_pid.is_some_and(|pid| pid != shared_pid) {
                replacement_shard_pid = greeter_pid;
                break;
            }
        }
        assert!(
            replacement_shard_pid.is_some(),
            "all deployments in a killed shard must be restored into one new generation"
        );
        for host in ["greeter.test", "sidekick.test"] {
            let response = client
                .get(format!("{}/greet", base))
                .header("host", host)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "restored shard route for {host}");
            assert_eq!(
                response.text().await.unwrap(),
                "Hello from immutable package v2"
            );
        }

        let trusted_config =
            dedicated_config.replace("class = \"dedicated\"", "class = \"trusted\"");
        std::fs::write(dep_dir.join("perch.toml"), trusted_config).unwrap();
        let trusted_reload = client
            .post(format!("{}/_perch/admin/deployments/greeter/reload", base))
            .timeout(Duration::from_secs(30))
            .basic_auth("perch", Some("test-secret"))
            .header("x-perch-confirm", "reload")
            .send()
            .await
            .expect("trusted isolation reload");
        assert_eq!(
            trusted_reload.status(),
            200,
            "trusted reload failed: {:?}",
            trusted_reload.text().await
        );
        let trusted_memory: serde_json::Value = client
            .get(format!("{}/_perch/admin/deployments/greeter/memory", base))
            .basic_auth("perch", Some("test-secret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(trusted_memory["requested_isolation_class"], "trusted");
        assert_eq!(trusted_memory["effective_isolation_class"], "trusted");
        assert_eq!(trusted_memory["execution_mode"], "in_process");
        assert!(trusted_memory["worker_pid"].is_null());

        // Unloading greeter's generation must not stop its sidekick sibling.
        let sidekick_after_greeter_unload = client
            .get(format!("{}/greet", base))
            .header("host", "sidekick.test")
            .send()
            .await
            .unwrap();
        assert_eq!(sidekick_after_greeter_unload.status(), 200);
        assert_eq!(
            sidekick_after_greeter_unload.text().await.unwrap(),
            "Hello from immutable package v2"
        );

        // Move the last resident deployment out of the shard. The lazily
        // retained provider process is now idle; killing the daemon must still
        // terminate it rather than leave an orphan worker behind.
        let sidekick_trusted_config =
            sidekick_config.replace("class = \"sharded\"", "class = \"trusted\"");
        std::fs::write(sidekick_dir.join("perch.toml"), sidekick_trusted_config).unwrap();
        let sidekick_trusted_reload = client
            .post(format!("{}/_perch/admin/deployments/sidekick/reload", base))
            .timeout(Duration::from_secs(30))
            .basic_auth("perch", Some("test-secret"))
            .header("x-perch-confirm", "reload")
            .send()
            .await
            .expect("trusted sidekick reload");
        assert_eq!(
            sidekick_trusted_reload.status(),
            200,
            "trusted sidekick reload failed: {:?}",
            sidekick_trusted_reload.text().await
        );
        let sidekick_trusted_memory: serde_json::Value = client
            .get(format!("{}/_perch/admin/deployments/sidekick/memory", base))
            .basic_auth("perch", Some("test-secret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sidekick_trusted_memory["execution_mode"], "in_process");
        let pid = replacement_shard_pid.unwrap() as libc::pid_t;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "idle shard exited before its owning daemon"
        );
        idle_shard_pid = Some(pid);
    } else {
        eprintln!("SKIP: perch-worker binary missing; isolation override roundtrip not run");
    }

    daemon
        .kill()
        .await
        .expect("kill daemon for parent-death proof");
    if let Some(pid) = idle_shard_pid {
        let mut stopped = false;
        for _ in 0..50 {
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            stopped,
            "idle worker shard {pid} survived its owning daemon's SIGKILL"
        );
    }

    // Exercise the two non-HTTP binary application entries directly. The
    // daemon process has stopped, so this test process owns a fresh provider
    // and application lifecycle.
    perch_app_host::initialize_runtime_libraries(&perry_runtime, &perry_stdlib)
        .expect("load providers for entry roundtrip");
    let host = perch_app_host::host::DeploymentHost::load("greeter", &dylib, None)
        .expect("load compiled multi-entry app");
    host.fire_cron(perch_host_abi::CronContext {
        expression: "*/1 * * * * *".into(),
        scheduled_at_ms: 42,
        dispatched_at_ms: 47,
    })
    .await
    .expect("binary cron roundtrip");
    let disposition = host
        .deliver_queue(perch_host_abi::QueueDispatchMessage {
            queue_name: "mail".into(),
            message_id: "m-1".into(),
            attempt: 1,
            max_retries: 5,
            payload: vec![0, 1, 0xff],
        })
        .await
        .expect("binary queue roundtrip");
    assert_eq!(disposition, perch_host_abi::QueueDisposition::Ack);
    host.shutdown().await.expect("shutdown multi-entry app");
    eprintln!("AUTO-COMPILE TEST PASSED");
}
