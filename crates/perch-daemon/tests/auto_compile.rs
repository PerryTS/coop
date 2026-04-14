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
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn workspace_root() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent().unwrap().parent().unwrap().to_path_buf()
}

fn perry_binary() -> PathBuf {
    // Try the standard location
    let p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/Users/amlug".to_string()))
        .join("projects/perry/perry/target/release/perry");
    if p.exists() { return p; }
    // Fallback to PATH
    PathBuf::from("perry")
}

fn perch_worker_binary() -> PathBuf {
    let d = workspace_root().join("target/debug/perch-worker");
    if d.exists() { d } else { workspace_root().join("target/release/perch-worker") }
}

fn perch_daemon_binary() -> PathBuf {
    let d = workspace_root().join("target/debug/perch");
    if d.exists() { d } else { workspace_root().join("target/release/perch") }
}

#[tokio::test]
async fn auto_compile_from_raw_typescript() {
    if !perry_binary().exists() || perry_binary() == PathBuf::from("perry") {
        eprintln!("SKIP: perry binary not found at expected location");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let v = tmp.path();

    let deployments = v.join("deployments");
    let compiled = v.join("compiled");
    let sockets = v.join("sockets");
    for d in [&deployments, &compiled, &sockets,
              &v.join("storage"), &v.join("logs"), &v.join("acme")] {
        std::fs::create_dir_all(d).unwrap();
    }

    // Create a minimal deployment with raw TypeScript — NO pre-compiled dylib.
    let dep_dir = deployments.join("greeter");
    std::fs::create_dir_all(dep_dir.join("handlers")).unwrap();
    std::fs::create_dir_all(dep_dir.join("static")).unwrap();

    std::fs::write(dep_dir.join("perch.toml"), r#"
name = "greeter"

[hosts]
domains = ["greeter.test"]

[[handlers]]
file = "handlers/greet.ts"
path = "/greet"
method = "GET"

[[static]]
directory = "./static"
path = "/"
"#).unwrap();

    std::fs::write(dep_dir.join("handlers/greet.ts"), r#"
export function handle(reqJson: string): string {
  const req = JSON.parse(reqJson);
  const name = "World";
  const body = "Hello, " + name + "! Path: " + req.path;
  const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "text/plain" },
    body_base64: bodyB64,
  });
}
"#).unwrap();

    std::fs::write(dep_dir.join("static/index.html"),
        "<h1>Greeter</h1><a href=\"/greet\">Say hello</a>\n"
    ).unwrap();

    let port = pick_free_port();
    let rt = v.join("runtime.toml");
    std::fs::write(&rt, format!(r#"
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
perry_binary = "{}"
[tls]
mode = "off"
"#,
        deployments.display(), compiled.display(), sockets.display(),
        v.join("storage").display(), v.join("logs").display(),
        v.join("acme").display(), v.join("state.sqlite").display(),
        perch_worker_binary().display(), perry_binary().display(),
    )).unwrap();

    // Verify NO dylib exists yet.
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let dylib = compiled.join(format!("greeter.{}", ext));
    assert!(!dylib.exists(), "dylib should not exist before daemon starts");

    // Start daemon.
    let mut daemon = tokio::process::Command::new(perch_daemon_binary())
        .arg("--config").arg(&rt)
        .env("RUST_LOG", "info,perch_daemon=debug,perch_worker=debug")
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn().expect("spawn daemon");

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

    let base = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build().unwrap();

    // Wait for daemon to compile and come up. Perry compile takes a few
    // seconds (auto-optimize rebuild), so we wait longer.
    let mut ready = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(resp) = client.get(format!("{}/index.html", base))
            .header("host", "greeter.test").send().await {
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
        eprintln!("dylib exists: {}", dylib.exists());
        panic!("daemon didn't come up within 60 seconds — check [daemon] logs above");
    }

    // The dylib should now exist (created by perry compile).
    assert!(dylib.exists(), "perry should have compiled the dylib at {:?}", dylib);

    // TEST: GET /greet → handler responds.
    let resp = client.get(format!("{}/greet", base))
        .header("host", "greeter.test")
        .send().await.expect("GET /greet");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    eprintln!("GET /greet → {} body={}", status, body);
    assert_eq!(status, 200, "expected 200 from auto-compiled handler");
    assert!(body.contains("Hello, World!"), "body should contain greeting: {}", body);

    // TEST: static serving also works.
    let static_resp = client.get(format!("{}/index.html", base))
        .header("host", "greeter.test")
        .send().await.unwrap();
    assert_eq!(static_resp.status(), 200);

    let _ = daemon.kill().await;
    eprintln!("AUTO-COMPILE TEST PASSED");
}
