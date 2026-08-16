//! Real compiler + provider callback + daemon consumer + PostgreSQL proof.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn provider(name: &str) -> PathBuf {
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    root()
        .join("var/coop/lib")
        .join(format!("libperry_{name}.{extension}"))
}

fn spawn(config: &Path) -> tokio::process::Child {
    tokio::process::Command::new(root().join("target/debug/coop"))
        .args(["--config"])
        .arg(config)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn send_when_ready(
    daemon: &mut tokio::process::Child,
    client: &reqwest::Client,
    base_url: &str,
) -> reqwest::Response {
    for _ in 0..180 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(status) = daemon.try_wait().unwrap() {
            panic!("queue daemon exited before readiness: {status}");
        }
        if let Ok(candidate) = client
            .get(format!("{base_url}/send"))
            .header("host", "queue.test")
            .send()
            .await
        {
            if candidate.status().as_u16() == 202 {
                return candidate;
            }
        }
    }
    panic!("queue app never became ready");
}

async fn wait_for_listener(
    daemon: &mut tokio::process::Child,
    client: &reqwest::Client,
    base_url: &str,
) {
    for _ in 0..180 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(status) = daemon.try_wait().unwrap() {
            panic!("queue daemon exited before readiness: {status}");
        }
        if client
            .get(format!(
                "{base_url}/_coop/admin/deployments/queue-app/artifacts"
            ))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
    }
    panic!("queue daemon listener never became ready");
}

async fn wait_for_queue_empty(pg: &tokio_postgres::Client) {
    for _ in 0..250 {
        let pending: i64 = pg
            .query_one(
                "SELECT count(*)::bigint FROM coop_queue_messages WHERE deployment = 'queue-app'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if pending == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("host-enqueued message was not acknowledged");
}

async fn connect_postgres_when_ready(url: &str) -> tokio_postgres::Client {
    let mut last_error = None;
    for _ in 0..120 {
        match tokio_postgres::connect(url, tokio_postgres::NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                return client;
            }
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "PostgreSQL did not recover after container restart: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no connection attempt completed".to_string())
    );
}

struct PostgresContainerOutage {
    name: String,
    stopped: bool,
}

impl PostgresContainerOutage {
    fn stop(name: String) -> Self {
        assert!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
            "COOP_TEST_POSTGRES_CONTAINER contains unsafe characters"
        );
        docker_container_action("stop", &name);
        Self {
            name,
            stopped: true,
        }
    }

    fn restart(&mut self) {
        if self.stopped {
            docker_container_action("start", &self.name);
            self.stopped = false;
        }
    }
}

impl Drop for PostgresContainerOutage {
    fn drop(&mut self) {
        if self.stopped {
            let _ = Command::new("docker").args(["start", &self.name]).status();
        }
    }
}

fn docker_container_action(action: &str, name: &str) {
    let mut command = Command::new("docker");
    command.arg(action);
    if action == "stop" {
        command.args(["--time", "5"]);
    }
    let output = command
        .arg(name)
        .output()
        .unwrap_or_else(|error| panic!("running docker {action} for {name}: {error}"));
    assert!(
        output.status.success(),
        "docker {action} failed for {name}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
}

async fn wait_for_exact_dead_letter_payload(pg: &tokio_postgres::Client, payload: &[u8]) {
    for _ in 0..250 {
        let found: bool = pg
            .query_one(
                r#"
SELECT EXISTS (
    SELECT 1 FROM coop_queue_dead_letters
    WHERE deployment = 'queue-app' AND queue_name = 'jobs' AND payload = $1
)
"#,
                &[&payload],
            )
            .await
            .unwrap()
            .get(0);
        if found {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("raw queue payload was not retained byte-for-byte in the DLQ");
}

async fn insert_queue_message(
    pg: &tokio_postgres::Client,
    id: &str,
    payload: &[u8],
    max_attempts: i32,
) {
    pg.execute(
        r#"
INSERT INTO coop_queue_messages (
    id, deployment, queue_name, payload, max_attempts
) VALUES ($1, 'queue-app', 'jobs', $2, $3)
"#,
        &[&id, &payload, &max_attempts],
    )
    .await
    .unwrap();
}

async fn wait_for_dead_letter(pg: &tokio_postgres::Client, id: &str) -> (i32, i32, String) {
    for _ in 0..250 {
        if let Some(row) = pg
            .query_opt(
                r#"
SELECT attempts, max_attempts, final_error
FROM coop_queue_dead_letters
WHERE id = $1 AND deployment = 'queue-app' AND queue_name = 'jobs'
"#,
                &[&id],
            )
            .await
            .unwrap()
        {
            return (row.get(0), row.get(1), row.get(2));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("message {id:?} was not moved to the dead-letter queue");
}

async fn wait_for_active_lease(pg: &tokio_postgres::Client, id: &str) {
    for _ in 0..250 {
        let leased = pg
            .query_opt(
                r#"
SELECT attempts
FROM coop_queue_messages
WHERE id = $1 AND lease_token IS NOT NULL AND lease_expires_at > clock_timestamp()
"#,
                &[&id],
            )
            .await
            .unwrap();
        if leased.is_some_and(|row| row.get::<_, i32>(0) == 1) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("message {id:?} was not actively leased on its first delivery");
}

async fn active_package(client: &reqwest::Client, base_url: &str) -> String {
    client
        .get(format!(
            "{base_url}/_coop/admin/deployments/queue-app/artifacts"
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["active"]
        .as_str()
        .unwrap()
        .to_string()
}

fn assert_stable_application_exports(compiled: &Path) {
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let library = std::fs::read_dir(compiled.join("queue-app"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path().join(format!("app.{extension}")))
        .find(|path| path.is_file())
        .expect("compiled queue app library");
    let mut nm = std::process::Command::new("nm");
    if cfg!(target_os = "macos") {
        nm.arg("-gU");
    } else {
        nm.args(["-D", "--defined-only"]);
    }
    let output = nm.arg(&library).output().unwrap();
    assert!(output.status.success());
    let symbols = String::from_utf8_lossy(&output.stdout);
    assert!(symbols.contains("coop_app_http_v2"));
    assert!(symbols.contains("coop_app_queue_0_v2"));
    assert!(!symbols.contains("__perry_wrap_perry_fn_"));
    let manifest = coop_host_abi::AppLibraryManifest::load(&library)
        .unwrap()
        .unwrap();
    assert_eq!(manifest.handle_symbol, "coop_app_http_v2");
    assert_eq!(manifest.queue_entries[0].symbol, "coop_app_queue_0_v2");
}

async fn stop_daemon(daemon: &mut tokio::process::Child) {
    let pid = daemon.id().expect("running daemon process");
    #[cfg(unix)]
    unsafe {
        assert_eq!(libc::kill(pid as i32, libc::SIGTERM), 0);
    }
    #[cfg(not(unix))]
    daemon.start_kill().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(15), daemon.wait())
        .await
        .expect("daemon did not stop within grace")
        .unwrap();
    assert!(status.success(), "daemon stopped unsuccessfully: {status}");
}

async fn kill_daemon_during_delivery(daemon: &mut tokio::process::Child) {
    daemon.start_kill().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), daemon.wait())
        .await
        .expect("killed daemon did not exit")
        .unwrap();
    assert!(
        !status.success(),
        "abruptly killed daemon exited successfully"
    );
}

#[tokio::test]
#[ignore = "requires COOP_TEST_POSTGRES_URL and built Perry providers"]
async fn runtime_send_is_tenant_bound_durable_and_consumed() {
    let postgres_url = std::env::var("COOP_TEST_POSTGRES_URL")
        .expect("COOP_TEST_POSTGRES_URL must point to a disposable database");
    let perry = root().join(".perry-main/target/perry-dev/perry");
    let runtime = provider("runtime");
    let stdlib = provider("stdlib");
    assert!(perry.exists() && runtime.exists() && stdlib.exists());

    let temp = tempfile::tempdir().unwrap();
    let deployments = temp.path().join("deployments");
    let deployment = deployments.join("queue-app");
    std::fs::create_dir_all(deployment.join("handlers")).unwrap();
    std::fs::create_dir_all(deployment.join("node_modules/@coop")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        root().join("packages/coop-runtime"),
        deployment.join("node_modules/@coop/runtime"),
    )
    .unwrap();
    std::fs::write(
        deployment.join("package.json"),
        r#"{
  "name": "queue-app",
  "private": true,
  "perry": { "allow": { "nativeLibrary": ["@coop/runtime/src/queue"] } }
}"#,
    )
    .unwrap();
    std::fs::write(
        deployment.join("coop.toml"),
        r#"
name = "queue-app"
[hosts]
domains = ["queue.test"]
[[handlers]]
file = "handlers/send.ts"
path = "/send"
method = "GET"
[[queues]]
file = "handlers/consume.ts"
name = "jobs"
concurrency = 1
max_retries = 2
poll_interval_ms = 10
visibility_timeout_ms = 1500
retry_base_delay_ms = 10
retry_max_delay_ms = 100
max_enqueue_delay_ms = 1000
dlq_retention_days = 0
[limits]
max_wall_clock_ms = 1000
"#,
    )
    .unwrap();
    std::fs::write(
        deployment.join("handlers/consume.ts"),
        r#"
export async function handle(frame: Buffer): Promise<Buffer> {
  if (frame.length < 6 || frame[4] !== 5) throw new Error("invalid queue frame");
  let offset = 5;
  const queueLength = frame.readUInt32BE(offset); offset += 4 + queueLength;
  const idLength = frame.readUInt32BE(offset); offset += 4 + idLength;
  const attempt = frame.readUInt32BE(offset);
  const marker = frame[frame.length - 1];
  if (marker === 0xfc && attempt === 1) {
    await new Promise<void>((resolve) => setTimeout(resolve, 10000));
  }
  const disposition = marker === 0xfd ? 2 : marker === 0xfe ? 1 : 0;
  const output = Buffer.alloc(6);
  output[0] = 0x50; output[1] = 0x43; output[2] = 0x48; output[3] = 0x32; output[4] = 6; output[5] = disposition;
  return output;
}
"#,
    )
    .unwrap();
    std::fs::write(
        deployment.join("handlers/send.ts"),
        r#"
import { queue } from "@coop/runtime/src/queue";

export async function handle(_frame: Buffer): Promise<Buffer> {
  await queue.send("jobs", { marker: "provider-owned", bytes: [0, 255] });
  // 0xfd makes the fixture handler retain this raw payload in the DLQ.
  const raw = Buffer.alloc(3);
  raw[0] = 0; raw[1] = 255; raw[2] = 253;
  await queue.sendRaw("jobs", raw);
  const body = Buffer.from("queued");
  const output = Buffer.alloc(5 + 2 + 4 + 4 + body.length);
  output[0] = 0x50; output[1] = 0x43; output[2] = 0x48; output[3] = 0x32; output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(202, offset); offset += 2;
  output.writeUInt32BE(0, offset); offset += 4;
  output.writeUInt32BE(body.length, offset); offset += 4;
  body.copy(output, offset);
  return output;
}
"#,
    )
    .unwrap();

    let listen = port();
    let runtime_toml = temp.path().join("runtime.toml");
    let compiled = temp.path().join("compiled");
    let sockets = temp.path().join("sockets");
    let worker = root().join("target/debug/coop-worker");
    assert!(
        worker.exists(),
        "build coop-worker before this ignored test"
    );
    for directory in [&compiled, &sockets, &temp.path().join("storage")] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let admin_password_hash = bcrypt::hash("test-secret", 4).unwrap();
    let runtime_config = |mode: &str, listen: u16| {
        format!(
            r#"
[http]
listen_http = "127.0.0.1:{listen}"
[execution]
mode = "{mode}"
[paths]
deployments_dir = "{}"
compiled_dir = "{}"
sockets_dir = "{}"
storage_dir = "{}"
logs_dir = "{}"
state_db = "{}"
acme_cache_dir = "{}"
perry_binary = "{}"
coop_worker_binary = "{}"
perry_runtime_library = "{}"
perry_stdlib_library = "{}"
[postgres]
url = {:?}
max_connections = 8
[admin]
password_hash = {:?}
[tls]
mode = "off"
"#,
            deployments.display(),
            compiled.display(),
            sockets.display(),
            temp.path().join("storage").display(),
            temp.path().join("logs").display(),
            temp.path().join("state.sqlite").display(),
            temp.path().join("acme").display(),
            perry.display(),
            worker.display(),
            runtime.display(),
            stdlib.display(),
            postgres_url,
            admin_password_hash,
        )
    };
    std::fs::write(&runtime_toml, runtime_config("in_process", listen)).unwrap();

    let (pg, connection) = tokio_postgres::connect(&postgres_url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut daemon = spawn(&runtime_toml);
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{listen}");
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let explicit_dlq_id = format!("explicit-dlq-{run_id}");
    let exhausted_dlq_id = format!("exhausted-dlq-{run_id}");
    let killed_delivery_id = format!("killed-delivery-{run_id}");
    let operator_replay_id = format!("operator-replay-{run_id}");
    let response = send_when_ready(&mut daemon, &client, &url).await;
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"queued");
    wait_for_exact_dead_letter_payload(&pg, &[0, 255, 253]).await;
    wait_for_queue_empty(&pg).await;
    assert_stable_application_exports(&compiled);

    let metrics = client
        .get(format!("{url}/_coop/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("coop_queue_claims_total"));
    assert!(metrics.contains("coop_queue_deliveries_total"));
    assert!(metrics.contains("coop_queue_pool_connections"));
    assert!(metrics.contains("coop_queue_pool_waiters"));
    assert!(metrics.contains("coop_queue_pool_utilization_ratio"));
    assert!(metrics.contains("outcome=\"ack\""));

    // An explicit handler DLQ disposition must atomically remove the durable
    // message and preserve its first-attempt diagnostics in the dead-letter
    // table through the real in-process application ABI.
    insert_queue_message(&pg, &explicit_dlq_id, &[0xfd], 3).await;
    let (attempts, max_attempts, final_error) = wait_for_dead_letter(&pg, &explicit_dlq_id).await;
    assert_eq!((attempts, max_attempts), (1, 3));
    assert_eq!(final_error, "handler returned dead-letter");
    let metrics = client
        .get(format!("{url}/_coop/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("outcome=\"dlq\""));

    // Restart over the exact retained package in worker mode. This proves
    // both the worker's provider callback (HTTP enqueue) and the daemon ->
    // worker raw queue dispatch/ack path without recompiling the app.
    stop_daemon(&mut daemon).await;
    let worker_listen = port();
    std::fs::write(&runtime_toml, runtime_config("worker", worker_listen)).unwrap();
    let worker_url = format!("http://127.0.0.1:{worker_listen}");
    let mut daemon = spawn(&runtime_toml);
    let response = send_when_ready(&mut daemon, &client, &worker_url).await;
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"queued");
    wait_for_queue_empty(&pg).await;

    // A nack crosses the daemon/worker boundary twice: the first delivery is
    // retried with the configured backoff and the second exhausts the durable
    // attempt budget into the DLQ.
    insert_queue_message(&pg, &exhausted_dlq_id, &[0xfe], 2).await;
    let (attempts, max_attempts, final_error) = wait_for_dead_letter(&pg, &exhausted_dlq_id).await;
    assert_eq!((attempts, max_attempts), (2, 2));
    assert_eq!(final_error, "handler returned nack");
    let metrics = client
        .get(format!("{worker_url}/_coop/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("coop_queue_retries_total"));
    assert!(metrics.contains("outcome=\"scheduled\""));
    assert!(metrics.contains("outcome=\"exhausted_dlq\""));

    let dlq_url =
        format!("{worker_url}/_coop/admin/deployments/queue-app/queues/jobs/dead-letters");
    assert_eq!(
        client.get(&dlq_url).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "DLQ inspection must be authenticated"
    );
    let page = client
        .get(&dlq_url)
        .basic_auth("coop", Some("test-secret"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let listed_ids = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(listed_ids.contains(&explicit_dlq_id.as_str()));
    assert!(listed_ids.contains(&exhausted_dlq_id.as_str()));

    // Seed a poison-message diagnostic whose raw payload is safe on replay.
    // The authenticated operator transition must atomically restore the same
    // delivery ID and let the active worker generation acknowledge it.
    pg.execute(
        r#"
INSERT INTO coop_queue_dead_letters (
    id, deployment, queue_name, payload, attempts, max_attempts,
    created_at, final_error
) VALUES ($1, 'queue-app', 'jobs', $2, 2, 3, clock_timestamp(), 'operator test')
"#,
        &[&operator_replay_id, &vec![0_u8]],
    )
    .await
    .unwrap();
    let replay = client
        .post(format!("{dlq_url}/{operator_replay_id}/replay"))
        .basic_auth("coop", Some("test-secret"))
        .header("x-coop-confirm", "replay-dead-letter")
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), reqwest::StatusCode::OK);
    wait_for_queue_empty(&pg).await;
    let replay_source: i64 = pg
        .query_one(
            "SELECT count(*)::bigint FROM coop_queue_dead_letters WHERE id = $1",
            &[&operator_replay_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(replay_source, 0);

    assert_eq!(
        client
            .delete(format!("{dlq_url}/{exhausted_dlq_id}"))
            .basic_auth("coop", Some("test-secret"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::PRECONDITION_REQUIRED
    );
    let purge = client
        .delete(format!("{dlq_url}/{exhausted_dlq_id}"))
        .basic_auth("coop", Some("test-secret"))
        .header("x-coop-confirm", "purge-dead-letter")
        .send()
        .await
        .unwrap();
    assert_eq!(purge.status(), reqwest::StatusCode::OK);

    // A source replacement must publish and activate a new package before
    // its queue consumer begins claiming. Once the active identity changes,
    // prove the replacement generation can enqueue and acknowledge work.
    let old_package = active_package(&client, &worker_url).await;
    std::fs::write(
        deployment.join("handlers/consume.ts"),
        r#"
// replacement generation
export async function handle(frame: Buffer): Promise<Buffer> {
  if (frame.length < 6 || frame[4] !== 5) throw new Error("invalid queue frame");
  let offset = 5;
  const queueLength = frame.readUInt32BE(offset); offset += 4 + queueLength;
  const idLength = frame.readUInt32BE(offset); offset += 4 + idLength;
  const attempt = frame.readUInt32BE(offset);
  const marker = frame[frame.length - 1];
  if (marker === 0xfc && attempt === 1) {
    await new Promise<void>((resolve) => setTimeout(resolve, 10000));
  }
  const disposition = marker === 0xfd ? 2 : marker === 0xfe ? 1 : 0;
  const output = Buffer.alloc(6);
  output[0] = 0x50; output[1] = 0x43; output[2] = 0x48; output[3] = 0x32; output[4] = 6; output[5] = disposition;
  return output;
}
"#,
    )
    .unwrap();
    client
        .post(format!(
            "{worker_url}/_coop/admin/deployments/queue-app/reload"
        ))
        .basic_auth("coop", Some("test-secret"))
        .header("x-coop-confirm", "reload")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let replacement_package = active_package(&client, &worker_url).await;
    assert_ne!(replacement_package, old_package);
    let response = send_when_ready(&mut daemon, &client, &worker_url).await;
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"queued");
    wait_for_queue_empty(&pg).await;

    // Rollback is another generation transition: the replacement consumer
    // must stop before the retained original package begins claiming.
    client
        .post(format!(
            "{worker_url}/_coop/admin/deployments/queue-app/rollback/{old_package}"
        ))
        .basic_auth("coop", Some("test-secret"))
        .header("x-coop-confirm", "rollback")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(active_package(&client, &worker_url).await, old_package);
    let response = send_when_ready(&mut daemon, &client, &worker_url).await;
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"queued");
    wait_for_queue_empty(&pg).await;
    stop_daemon(&mut daemon).await;

    // Restart the retained package in the real multi-application shard mode.
    // This exercises both directions of the shard protocol: the provider
    // callback commits queue.send()/queue.sendRaw() through the per-runtime
    // context, and the daemon dispatches claimed raw bytes back to that exact
    // runtime ID without treating the shard as a dedicated-worker alias.
    let shard_listen = port();
    std::fs::write(&runtime_toml, runtime_config("shard", shard_listen)).unwrap();
    let shard_url = format!("http://127.0.0.1:{shard_listen}");
    let mut daemon = spawn(&runtime_toml);
    let response = send_when_ready(&mut daemon, &client, &shard_url).await;
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"queued");
    wait_for_queue_empty(&pg).await;
    let memory = client
        .get(format!(
            "{shard_url}/_coop/admin/deployments/queue-app/memory"
        ))
        .basic_auth("coop", Some("test-secret"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(memory["execution_mode"], "shard");
    assert!(memory["worker_pid"].as_u64().is_some());
    assert!(memory["shard_slot"].as_u64().is_some());
    let shard_delivery_id = format!("shard-delivery-{run_id}");
    insert_queue_message(&pg, &shard_delivery_id, &[0], 3).await;
    wait_for_queue_empty(&pg).await;
    let metrics = client
        .get(format!("{shard_url}/_coop/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("coop_queue_deliveries_total"));
    assert!(metrics.contains("outcome=\"ack\""));
    assert!(metrics.contains("coop_worker_shard_resident_deployments"));
    stop_daemon(&mut daemon).await;

    // Kill an in-process daemon only after the database proves that the real
    // handler owns an active first-attempt lease. The replacement process must
    // wait for visibility expiry, recover the lease, and ack attempt two.
    insert_queue_message(&pg, &killed_delivery_id, &[0xfc], 3).await;
    let crash_listen = port();
    std::fs::write(&runtime_toml, runtime_config("in_process", crash_listen)).unwrap();
    let crash_url = format!("http://127.0.0.1:{crash_listen}");
    let mut daemon = spawn(&runtime_toml);
    wait_for_listener(&mut daemon, &client, &crash_url).await;
    wait_for_active_lease(&pg, &killed_delivery_id).await;
    kill_daemon_during_delivery(&mut daemon).await;

    let recovery_listen = port();
    std::fs::write(&runtime_toml, runtime_config("in_process", recovery_listen)).unwrap();
    let recovery_url = format!("http://127.0.0.1:{recovery_listen}");
    let mut daemon = spawn(&runtime_toml);
    wait_for_listener(&mut daemon, &client, &recovery_url).await;
    wait_for_queue_empty(&pg).await;
    let metrics = client
        .get(format!("{recovery_url}/_coop/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("coop_queue_expired_leases_total"));
    assert!(metrics.contains("recovered_expired_lease=\"true\""));

    // Sever every daemon-side PostgreSQL session while keeping this control
    // connection alive. The pool must establish fresh sessions and the
    // provider enqueue -> commit -> delivery path must recover in place.
    let daemon_backends = pg
        .query(
            r#"
SELECT pid
FROM pg_stat_activity
WHERE datname = current_database() AND pid <> pg_backend_pid()
"#,
            &[],
        )
        .await
        .unwrap();
    let mut terminated = 0_u64;
    for row in daemon_backends {
        let pid: i32 = row.get(0);
        let killed: bool = pg
            .query_one("SELECT pg_terminate_backend($1)", &[&pid])
            .await
            .unwrap()
            .get(0);
        terminated += u64::from(killed);
    }
    assert!(
        terminated > 0,
        "no daemon PostgreSQL sessions were terminated"
    );
    let response = send_when_ready(&mut daemon, &client, &recovery_url).await;
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"queued");
    wait_for_queue_empty(&pg).await;

    // An optional disposable-container gate takes the database completely
    // offline, rather than merely terminating the pool's current sessions.
    // The guard restarts the exact validated container even if a later
    // assertion panics. The daemon must remain alive, surface an unavailable
    // enqueue while storage is absent, establish a fresh pool after restart,
    // and resume the complete enqueue -> claim -> ack path in place.
    let pg = if let Ok(container) = std::env::var("COOP_TEST_POSTGRES_CONTAINER") {
        let mut outage = PostgresContainerOutage::stop(container);
        let unavailable = tokio::time::timeout(
            Duration::from_secs(3),
            client
                .get(format!("{recovery_url}/send"))
                .header("host", "queue.test")
                .send(),
        )
        .await;
        assert!(
            !matches!(unavailable, Ok(Ok(response)) if response.status().as_u16() == 202),
            "queue enqueue was acknowledged while PostgreSQL was stopped"
        );
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "daemon exited during PostgreSQL outage"
        );
        outage.restart();
        let recovered_pg = connect_postgres_when_ready(&postgres_url).await;
        let response = send_when_ready(&mut daemon, &client, &recovery_url).await;
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"queued");
        wait_for_queue_empty(&recovered_pg).await;
        recovered_pg
    } else {
        pg
    };

    let dead_letters: i64 = pg
        .query_one(
            r#"
SELECT count(*)::bigint
FROM coop_queue_dead_letters
WHERE id = $1 OR id = $2
"#,
            &[&explicit_dlq_id, &exhausted_dlq_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(dead_letters, 1);
    stop_daemon(&mut daemon).await;
}
