//! End-to-end test: async handler with setTimeout.
//!
//! Verifies the async-aware invoke path in perch-worker's plugin_host.
//! The handler awaits a setTimeout promise; perch-worker's await_promise
//! drives Perry's event loop until the promise resolves, then extracts
//! the resolved string. Without async support, the handler would return
//! a Promise pointer that we'd fail to decode as a string.

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
    let p = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("projects/perry/perry/target/release/perry");
    if p.exists() { p } else { PathBuf::from("perry") }
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
async fn async_handler_awaits_settimeout_promise() {
    if !perry_binary().exists() {
        eprintln!("SKIP: perry binary not found");
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

    let dep_dir = deployments.join("asyncdep");
    std::fs::create_dir_all(dep_dir.join("handlers")).unwrap();

    std::fs::write(dep_dir.join("perch.toml"), r#"
name = "asyncdep"
[hosts]
domains = ["asyncdep.test"]
[[handlers]]
file = "handlers/main.ts"
path = "/"
method = "GET"
"#).unwrap();

    // Async handler — awaits setTimeout, returns a string.
    std::fs::write(dep_dir.join("handlers/main.ts"), r#"
export async function handle(reqJson: string): Promise<string> {
  // Force an actual async hop via setTimeout(0).
  await new Promise<void>((resolve) => setTimeout(resolve, 1));
  const body = "async resolved at " + Date.now();
  const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "text/plain" },
    body_base64: bodyB64,
  });
}
"#).unwrap();

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

    let mut ready = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(resp) = client.get(format!("{}/", base))
            .header("host", "asyncdep.test").send().await {
            if resp.status() == 200 {
                ready = true;
                break;
            }
        }
    }
    if !ready {
        let _ = daemon.kill().await;
        panic!("daemon didn't come up");
    }

    // Hit the async handler — verify the string contains "async resolved at"
    let resp = client.get(format!("{}/", base))
        .header("host", "asyncdep.test")
        .send().await.unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    eprintln!("GET / → {} body={}", status, body);

    assert_eq!(status, 200, "expected 200 from async handler");
    assert!(
        body.contains("async resolved at"),
        "expected 'async resolved at' in body, got: {}",
        body
    );

    let _ = daemon.kill().await;
}
