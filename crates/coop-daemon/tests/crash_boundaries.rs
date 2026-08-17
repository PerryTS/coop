#![cfg(unix)]

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

fn workspace_root() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn perry_binary() -> PathBuf {
    let binary = workspace_root().join(".perry-main/target/perry-dev/perry");
    binary.exists().then_some(binary).unwrap_or_default()
}

fn perry_libraries() -> (PathBuf, PathBuf) {
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let root = workspace_root().join("var/coop/lib");
    (
        root.join(format!("libperry_runtime.{extension}")),
        root.join(format!("libperry_stdlib.{extension}")),
    )
}

fn pick_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn handler_source(body: &str) -> String {
    format!(
        r#"
export function handle(_frame: Buffer): Buffer {{
  const body = Buffer.from({});
  const name = Buffer.from("content-type");
  const value = Buffer.from("text/plain");
  const output = Buffer.alloc(5 + 2 + 4 + 4 + name.length + 4 + value.length + 4 + body.length);
  output[0] = 0x43; output[1] = 0x4f; output[2] = 0x4f; output[3] = 0x50; output[4] = 2;
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
}}
"#,
        serde_json::to_string(body).unwrap()
    )
}

fn spawn_daemon(config: &Path, crash_point: Option<(&str, &Path, &Path)>) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_coop"));
    command
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if std::env::var_os("COOP_TEST_VERBOSE_CRASH").is_some() {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    if let Some((point, marker, arm)) = crash_point {
        command
            .env("COOP_TEST_PROCESS_CRASH_POINT", point)
            .env("COOP_TEST_PROCESS_CRASH_DEPLOYMENT", "crash-app")
            .env("COOP_TEST_PROCESS_CRASH_MARKER", marker)
            .env("COOP_TEST_PROCESS_CRASH_ARM", arm);
    }
    command.spawn().expect("spawn Coop daemon")
}

async fn wait_for_body(client: &reqwest::Client, base: &str, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = "no response".to_string();
    loop {
        if let Ok(response) = client
            .get(format!("{base}/run"))
            .header("host", "crash.test")
            .send()
            .await
        {
            let status = response.status();
            if response.status().is_success() {
                if let Ok(body) = response.text().await {
                    if body == expected {
                        return;
                    }
                    last = format!("status={status}, body={body:?}");
                }
            } else {
                last = format!("status={status}");
            }
        }
        assert!(
            Instant::now() < deadline,
            "daemon never served expected body {expected:?}; last observation: {last}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_marker(path: &Path) -> HashMap<String, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            return contents
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
        }
        assert!(
            Instant::now() < deadline,
            "crash marker was not written: {path:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn process_is_running(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("inspect process state");
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .trim()
            .chars()
            .next()
            .is_some_and(|state| state != 'Z')
}

async fn wait_until_stopped(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_is_running(pid) {
        assert!(Instant::now() < deadline, "process {pid} survived SIGKILL");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn sigkill(daemon: &mut tokio::process::Child) {
    let pid = daemon.id().expect("daemon PID") as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let status = daemon.wait().await.expect("reap killed daemon");
    assert!(!status.success());
}

fn staging_entries(compiled: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(compiled.join(".staging"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect()
}

#[tokio::test]
async fn sigkill_matrix_preserves_old_generation_across_compile_stage_and_activation() {
    let perry = perry_binary();
    let (runtime, stdlib) = perry_libraries();
    if !perry.exists() || !runtime.exists() || !stdlib.exists() {
        eprintln!("SKIP: Perry compiler/shared libraries not built");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let deployments = root.join("deployments");
    let compiled = root.join("compiled");
    let deployment = deployments.join("crash-app");
    for directory in [
        deployment.join("handlers"),
        compiled.clone(),
        root.join("sockets"),
        root.join("storage"),
        root.join("logs"),
        root.join("acme"),
    ] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(
        deployment.join("coop.toml"),
        r#"
name = "crash-app"

[hosts]
domains = ["crash.test"]

[[handlers]]
file = "handlers/run.ts"
path = "/run"
method = "GET"

[activation]
path = "/run"
method = "GET"
requests = 2
expected_status = 200
"#,
    )
    .unwrap();
    let source = deployment.join("handlers/run.ts");
    std::fs::write(&source, handler_source("generation-0")).unwrap();

    let port = pick_free_port();
    let config = root.join("runtime.toml");
    let password_hash = bcrypt::hash("test-secret", 4).unwrap();
    std::fs::write(
        &config,
        format!(
            r#"
[http]
listen_http = "127.0.0.1:{port}"

[execution]
watch_deployments = false
compile_timeout_seconds = 30
staging_reconcile_age_seconds = 86400

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

[admin]
path = "/_coop/admin"
password_hash = "{}"
"#,
            deployments.display(),
            compiled.display(),
            root.join("sockets").display(),
            root.join("storage").display(),
            root.join("logs").display(),
            root.join("acme").display(),
            root.join("state.sqlite").display(),
            workspace_root().join("target/debug/coop-worker").display(),
            perry.display(),
            runtime.display(),
            stdlib.display(),
            password_hash,
        ),
    )
    .unwrap();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");

    let mut baseline = spawn_daemon(&config, None);
    wait_for_body(&client, &base, "generation-0").await;
    sigkill(&mut baseline).await;

    let points = ["compiler_started", "staging_validated", "activation_probed"];
    let mut active_body = "generation-0".to_string();
    for (index, point) in points.into_iter().enumerate() {
        eprintln!("crash-boundary: entering {point} with active body {active_body:?}");
        let marker = root.join(format!("{point}.marker"));
        let arm = root.join(format!("{point}.arm"));
        let mut daemon = spawn_daemon(&config, Some((point, &marker, &arm)));
        wait_for_body(&client, &base, &active_body).await;
        std::fs::write(&arm, b"armed\n").unwrap();

        let candidate_body = format!("generation-{}", index + 1);
        std::fs::write(&source, handler_source(&candidate_body)).unwrap();
        let reload_client = client.clone();
        let reload_url = format!("{base}/_coop/admin/deployments/crash-app/reload");
        let reload = tokio::spawn(async move {
            reload_client
                .post(reload_url)
                .basic_auth("coop", Some("test-secret"))
                .header("x-coop-confirm", "reload")
                .send()
                .await
        });
        let marker_values = wait_for_marker(&marker).await;
        assert_eq!(marker_values.get("point").map(String::as_str), Some(point));
        wait_for_body(&client, &base, &active_body).await;
        if point != "activation_probed" {
            assert!(
                !staging_entries(&compiled).is_empty(),
                "{point} did not leave a real staged package before SIGKILL"
            );
        }

        sigkill(&mut daemon).await;
        let _ = reload.await;
        if point == "compiler_started" {
            let guard_pid = marker_values["child_pid"].parse::<i32>().unwrap();
            wait_until_stopped(guard_pid).await;
        }

        let mut recovered = spawn_daemon(&config, None);
        wait_for_body(&client, &base, &active_body).await;
        eprintln!("crash-boundary: {point} restart retained {active_body:?}");
        assert!(
            staging_entries(&compiled).is_empty(),
            "startup did not reconcile dead-owner staging after {point}"
        );
        let response = client
            .post(format!("{base}/_coop/admin/deployments/crash-app/reload"))
            .basic_auth("coop", Some("test-secret"))
            .header("x-coop-confirm", "reload")
            .send()
            .await
            .expect("reload candidate after recovery");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        wait_for_body(&client, &base, &candidate_body).await;
        eprintln!("crash-boundary: {point} promoted {candidate_body:?}");
        active_body = candidate_body;
        sigkill(&mut recovered).await;
    }
}
