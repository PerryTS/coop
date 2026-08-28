//! Opt-in process-level startup and memory benchmark for the in-process host.
//!
//! This deliberately uses precompiled app libraries so compilation is not
//! included in daemon startup. Each scenario starts a real Coop daemon,
//! waits for the HTTP listener (which is published only after every app has
//! initialized), samples RSS, invokes every app once, then samples RSS again.

use coop_host_abi::AppLibraryManifest;
use futures::{stream, StreamExt};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PACKAGE_DIGEST_V2_DOMAIN: &[u8] = b"coop-application-package-v2\0";

#[path = "support/benchmark_cgroup.rs"]
mod benchmark_cgroup;
#[path = "support/process_cpu.rs"]
mod process_cpu;
#[path = "support/process_memory.rs"]
mod process_memory;

use benchmark_cgroup::{command_for, BenchmarkCgroup};
use process_cpu::process_cpu_time;
use process_memory::median_group_memory_kib;

const READY_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_SCENARIOS: &[usize] = &[0, 1, 100];

#[derive(Debug)]
struct Sample {
    startup: Duration,
    ready_rss_kib: u64,
    ready_pss_kib: Option<u64>,
    ready_private_dirty_kib: Option<u64>,
    warm_rss_kib: u64,
    warm_pss_kib: Option<u64>,
    warm_private_dirty_kib: Option<u64>,
    warm_all: Duration,
    workload: Duration,
    workload_cpu: Duration,
    post_workload_rss_kib: u64,
    post_workload_pss_kib: Option<u64>,
    post_workload_private_dirty_kib: Option<u64>,
    ready_cgroup_kib: Option<u64>,
    warm_cgroup_kib: Option<u64>,
    post_workload_cgroup_kib: Option<u64>,
    cgroup_peak_kib: Option<u64>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in process startup and memory benchmark"]
async fn measure_in_process_startup_and_rss() {
    let workspace = workspace_root();
    let daemon = std::env::var_os("COOP_BENCH_DAEMON")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target/release/coop"));
    let library_extension = shared_library_extension();
    let source_app = locate_prepared_app(&workspace, library_extension);
    let source_manifest = AppLibraryManifest::load(&source_app)
        .expect("read source app manifest")
        .expect("source app has an ABI manifest");
    let runtime = workspace.join(format!("var/coop/lib/libperry_runtime.{library_extension}"));
    let stdlib = workspace.join(format!("var/coop/lib/libperry_stdlib.{library_extension}"));

    for required in [&daemon, &source_app, &runtime, &stdlib] {
        assert!(
            required.exists(),
            "required benchmark input is missing: {required:?}"
        );
    }

    let trials = std::env::var("COOP_BENCH_TRIALS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3);
    let requests = std::env::var("COOP_BENCH_REQUESTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let preload_concurrency = std::env::var("COOP_BENCH_PRELOAD_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4);
    let cgroup_root = std::env::var_os("COOP_BENCH_CGROUP_ROOT").map(PathBuf::from);
    if cgroup_root.is_some() {
        assert!(cfg!(target_os = "linux"), "benchmark cgroups require Linux");
    }
    assert!(trials > 0, "COOP_BENCH_TRIALS must be positive");
    let scenarios = std::env::var("COOP_BENCH_APP_COUNTS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|count| count.trim().parse::<usize>().expect("parse app count"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| DEFAULT_SCENARIOS.to_vec());

    eprintln!("daemon={}", daemon.display());
    eprintln!(
        "platform={}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    eprintln!("trials={trials}");
    eprintln!("requests={requests}");
    eprintln!("preload_concurrency={preload_concurrency}");
    eprintln!(
        "cgroup_root={}",
        cgroup_root
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "disabled".into())
    );
    let fixture_parent = workspace.join("target/coop-resource-benchmark");
    std::fs::create_dir_all(&fixture_parent).expect("create benchmark fixture parent");
    eprintln!("fixture_parent={}", fixture_parent.display());
    for app_count in scenarios {
        let effective_requests = if app_count > 0 { requests } else { 0 };
        let mut samples = Vec::with_capacity(trials);
        // Keep the exact app bytes across process trials. Trial 1 measures
        // first activation of newly linked/signed images; later trials measure
        // a daemon restart over deployed artifacts. Recreating each Mach-O for
        // every trial makes macOS repeat its one-time code validation and
        // is not comparable to Node restarting over the same build output.
        let fixture = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(&fixture_parent)
            .expect("create benchmark fixture");
        let config = prepare_fixture(
            fixture.path(),
            app_count,
            &workspace,
            &source_app,
            &source_manifest,
            preload_concurrency,
        );
        let port = port_from_config(&config);
        for trial in 0..trials {
            let cgroup = cgroup_root.as_deref().map(|root| {
                BenchmarkCgroup::prepare(root, &format!("perry-a{app_count}-t{}", trial + 1))
                    .expect("prepare benchmark cgroup")
            });
            let (mut child, startup) = start_and_wait(&daemon, &config, cgroup.as_ref());
            let pid = child.id();
            let ready_rss_kib = median_rss_kib(pid);
            let ready_memory = median_group_memory_kib(&[pid]);
            let ready_cgroup_kib = cgroup
                .as_ref()
                .and_then(BenchmarkCgroup::memory_current_kib);

            let warm_started = Instant::now();
            if app_count > 0 {
                warm_every_app(port, app_count).await;
            }
            let warm_all = warm_started.elapsed();
            let warm_rss_kib = median_rss_kib(pid);
            let warm_memory = median_group_memory_kib(&[pid]);
            let warm_cgroup_kib = cgroup
                .as_ref()
                .and_then(BenchmarkCgroup::memory_current_kib);

            let cpu_before = process_cpu_time(&[pid]);
            let workload_started = Instant::now();
            if effective_requests > 0 {
                run_workload(port, app_count, effective_requests).await;
            }
            let workload = workload_started.elapsed();
            let workload_cpu = process_cpu_time(&[pid]).saturating_sub(cpu_before);
            let post_workload_rss_kib = median_rss_kib(pid);
            let post_workload_memory = median_group_memory_kib(&[pid]);
            let post_workload_cgroup_kib = cgroup
                .as_ref()
                .and_then(BenchmarkCgroup::memory_current_kib);
            let cgroup_peak_kib = cgroup.as_ref().and_then(BenchmarkCgroup::memory_peak_kib);

            stop(&mut child);
            let sample = Sample {
                startup,
                ready_rss_kib,
                ready_pss_kib: ready_memory.pss,
                ready_private_dirty_kib: ready_memory.private_dirty,
                warm_rss_kib,
                warm_pss_kib: warm_memory.pss,
                warm_private_dirty_kib: warm_memory.private_dirty,
                warm_all,
                workload,
                workload_cpu,
                post_workload_rss_kib,
                post_workload_pss_kib: post_workload_memory.pss,
                post_workload_private_dirty_kib: post_workload_memory.private_dirty,
                ready_cgroup_kib,
                warm_cgroup_kib,
                post_workload_cgroup_kib,
                cgroup_peak_kib,
            };
            eprintln!(
                "apps={app_count} trial={} artifact_state={} startup_ms={:.3} ready_rss_mib={:.3} ready_pss_mib={:.3} ready_private_dirty_mib={:.3} ready_cgroup_mib={:.3} warm_all_ms={:.3} warm_rss_mib={:.3} warm_pss_mib={:.3} warm_private_dirty_mib={:.3} warm_cgroup_mib={:.3} requests={effective_requests} workload_ms={:.3} server_cpu_ms={:.3} post_workload_rss_mib={:.3} post_workload_pss_mib={:.3} post_workload_private_dirty_mib={:.3} post_workload_cgroup_mib={:.3} cgroup_peak_mib={:.3}",
                trial + 1,
                if trial == 0 { "fresh" } else { "restart" },
                millis(sample.startup),
                mib(sample.ready_rss_kib),
                optional_mib(sample.ready_pss_kib),
                optional_mib(sample.ready_private_dirty_kib),
                optional_mib(sample.ready_cgroup_kib),
                millis(sample.warm_all),
                mib(sample.warm_rss_kib),
                optional_mib(sample.warm_pss_kib),
                optional_mib(sample.warm_private_dirty_kib),
                optional_mib(sample.warm_cgroup_kib),
                millis(sample.workload),
                millis(sample.workload_cpu),
                mib(sample.post_workload_rss_kib),
                optional_mib(sample.post_workload_pss_kib),
                optional_mib(sample.post_workload_private_dirty_kib),
                optional_mib(sample.post_workload_cgroup_kib),
                optional_mib(sample.cgroup_peak_kib),
            );
            samples.push(sample);
        }

        let fresh_artifact_startup = samples[0].startup;
        let mut restart_startups: Vec<_> = samples
            .iter()
            .skip(1)
            .map(|sample| sample.startup)
            .collect();
        restart_startups.sort();
        let restart_startup_median = median_duration_millis(&restart_startups);
        let ready_rss_median =
            median_u64(samples.iter().map(|sample| sample.ready_rss_kib).collect());
        let ready_pss_median =
            median_optional_u64(samples.iter().map(|sample| sample.ready_pss_kib).collect());
        let ready_private_dirty_median = median_optional_u64(
            samples
                .iter()
                .map(|sample| sample.ready_private_dirty_kib)
                .collect(),
        );
        let warm_rss_median =
            median_u64(samples.iter().map(|sample| sample.warm_rss_kib).collect());
        let warm_pss_median =
            median_optional_u64(samples.iter().map(|sample| sample.warm_pss_kib).collect());
        let warm_private_dirty_median = median_optional_u64(
            samples
                .iter()
                .map(|sample| sample.warm_private_dirty_kib)
                .collect(),
        );
        let mut warm_times: Vec<_> = samples.iter().map(|sample| sample.warm_all).collect();
        warm_times.sort();
        let mut workload_times: Vec<_> = samples.iter().map(|sample| sample.workload).collect();
        workload_times.sort();
        let mut workload_cpu: Vec<_> = samples.iter().map(|sample| sample.workload_cpu).collect();
        workload_cpu.sort();
        let post_workload_rss_median = median_u64(
            samples
                .iter()
                .map(|sample| sample.post_workload_rss_kib)
                .collect(),
        );
        let post_workload_pss_median = median_optional_u64(
            samples
                .iter()
                .map(|sample| sample.post_workload_pss_kib)
                .collect(),
        );
        let post_workload_private_dirty_median = median_optional_u64(
            samples
                .iter()
                .map(|sample| sample.post_workload_private_dirty_kib)
                .collect(),
        );
        let ready_cgroup_median = median_optional_u64(
            samples
                .iter()
                .map(|sample| sample.ready_cgroup_kib)
                .collect(),
        );
        let warm_cgroup_median = median_optional_u64(
            samples
                .iter()
                .map(|sample| sample.warm_cgroup_kib)
                .collect(),
        );
        let post_workload_cgroup_median = median_optional_u64(
            samples
                .iter()
                .map(|sample| sample.post_workload_cgroup_kib)
                .collect(),
        );
        let cgroup_peak_median = median_optional_u64(
            samples
                .iter()
                .map(|sample| sample.cgroup_peak_kib)
                .collect(),
        );
        eprintln!(
            "RESULT apps={app_count} fresh_artifact_startup_ms={:.3} restart_startup_median_ms={:.3} ready_rss_median_mib={:.3} ready_pss_median_mib={:.3} ready_private_dirty_median_mib={:.3} ready_cgroup_median_mib={:.3} warm_all_median_ms={:.3} warm_rss_median_mib={:.3} warm_pss_median_mib={:.3} warm_private_dirty_median_mib={:.3} warm_cgroup_median_mib={:.3} requests={effective_requests} workload_median_ms={:.3} server_cpu_median_ms={:.3} requests_per_second={:.3} server_cpu_us_per_request={:.3} post_workload_rss_median_mib={:.3} post_workload_pss_median_mib={:.3} post_workload_private_dirty_median_mib={:.3} post_workload_cgroup_median_mib={:.3} cgroup_peak_median_mib={:.3}",
            millis(fresh_artifact_startup),
            restart_startup_median,
            mib(ready_rss_median),
            optional_mib(ready_pss_median),
            optional_mib(ready_private_dirty_median),
            optional_mib(ready_cgroup_median),
            millis(warm_times[warm_times.len() / 2]),
            mib(warm_rss_median),
            optional_mib(warm_pss_median),
            optional_mib(warm_private_dirty_median),
            optional_mib(warm_cgroup_median),
            millis(workload_times[workload_times.len() / 2]),
            millis(workload_cpu[workload_cpu.len() / 2]),
            rate(effective_requests, workload_times[workload_times.len() / 2]),
            cpu_micros_per_request(
                effective_requests,
                workload_cpu[workload_cpu.len() / 2]
            ),
            mib(post_workload_rss_median),
            optional_mib(post_workload_pss_median),
            optional_mib(post_workload_private_dirty_median),
            optional_mib(post_workload_cgroup_median),
            optional_mib(cgroup_peak_median),
        );
    }
}

/// Which compiled application this benchmark replicates.
///
/// Defaults to the tiny dependency-free app, which is what Linux CI has always
/// measured. `COOP_BENCH_APP_LIBRARY` points it at any other published
/// package -- in practice the Next.js fixture, produced by
/// `scripts/prepare-next-benchmark.sh`:
///
/// ```text
/// scripts/prepare-next-benchmark.sh
/// COOP_BENCH_APP_LIBRARY=target/next-benchmark/coop-run/compiled/next-bench/<pkg>/app.so \
///   cargo test --release -p coop-daemon --test resource_benchmark \
///   measure_in_process_startup_and_rss -- --ignored --nocapture
/// ```
///
/// Replicating the Next app is meaningful because Perry compiles the Next
/// route INTO the application dylib: at load time it needs the shared
/// providers and nothing else, no `node_modules`, so N copies measure the real
/// marginal cost of an Nth Next deployment on one box.
fn locate_prepared_app(workspace: &Path, extension: &str) -> PathBuf {
    if let Some(explicit) = std::env::var_os("COOP_BENCH_APP_LIBRARY") {
        let path = PathBuf::from(explicit);
        assert!(
            path.is_file(),
            "COOP_BENCH_APP_LIBRARY does not name a file: {path:?}"
        );
        return path;
    }
    let compiled = workspace.join("target/dynamic-smoke/compiled");
    let legacy = compiled.join(format!("test1.{extension}"));
    let namespace = compiled.join("test1");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&namespace)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join(format!("app.{extension}")))
        .filter(|path| path.is_file())
        .collect();
    candidates.sort();
    candidates.pop().unwrap_or(legacy)
}

/// Request path the benchmark drives, and the substring its response must
/// contain.
///
/// The tiny fixture answers `/` with exactly `ok`, and the harness asserted
/// both literally. That is a FIXTURE CONTRACT, not a property of the
/// benchmark: point `COOP_BENCH_APP_LIBRARY` at the Next.js fixture and it
/// serves `/api/benchmark` with JSON, so the run died on `left: 404` before
/// measuring anything.
///
/// A substring rather than an exact match, because a real application's body
/// carries incidental detail (checksums, timing) that would make an equality
/// assertion brittle without making it stronger. The point is to prove the
/// request was actually served, not to re-verify the payload -- correctness
/// belongs to `binary_http_roundtrip`.
/// Concurrency and per-request timeout for the workload phase.
///
/// Both were hardcoded (50 concurrent, 10 s) around the tiny fixture, whose
/// handler answers in microseconds. A real Next.js route is orders of
/// magnitude heavier per dispatch, and 50 concurrent requests queued on one
/// app's executor exceed a 10 s client timeout — on a QUIET host, so this is
/// the workload's shape rather than contention. Measured: 50 requests pass in
/// 3.4 s, 500 time out.
/// Execution mode under measurement.
///
/// `in_process` runs every app in the daemon address space — maximum density.
/// Until perry#8546 (class registries made per application image, #8893) it
/// was limited to one working JS heap: every image but the last-initialised
/// dispatched into the wrong code. `worker` gives each deployment its own
/// process, which never had that problem.
///
/// The comparison decides whether the in-process model is worth a large change
/// to Perry: a `.dylib`'s text pages are already shared across processes by the
/// kernel, so `worker` may capture most of the memory win without it.
fn bench_execution_mode() -> String {
    std::env::var("COOP_BENCH_EXECUTION_MODE").unwrap_or_else(|_| "in_process".to_string())
}

fn bench_workload_concurrency() -> usize {
    std::env::var("COOP_BENCH_WORKLOAD_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
}

fn bench_request_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("COOP_BENCH_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
    )
}

fn bench_request_path() -> String {
    std::env::var("COOP_BENCH_REQUEST_PATH").unwrap_or_else(|_| "/".to_string())
}

fn bench_expect_body() -> String {
    std::env::var("COOP_BENCH_EXPECT_BODY").unwrap_or_else(|_| "ok".to_string())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("daemon crate is inside workspace")
        .to_path_buf()
}

fn prepare_fixture(
    root: &Path,
    app_count: usize,
    workspace: &Path,
    source_app: &Path,
    source_manifest: &AppLibraryManifest,
    preload_concurrency: usize,
) -> PathBuf {
    let deployments = root.join("deployments");
    let compiled = root.join("compiled");
    let sockets = root.join("sockets");
    for directory in [
        &deployments,
        &compiled,
        &sockets,
        &root.join("storage"),
        &root.join("logs"),
        &root.join("acme"),
    ] {
        std::fs::create_dir_all(directory).expect("create fixture directory");
    }

    let source_package = source_app
        .parent()
        .expect("prepared app belongs to an immutable package");
    let source_config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(source_package.join("deployment.coop.json"))
            .expect("prepared app has packaged deployment config"),
    )
    .expect("parse prepared deployment config");
    let static_manifest_bytes = std::fs::read(source_package.join("static.coop-manifest.json"))
        .expect("prepared app has static snapshot manifest");

    for index in 0..app_count {
        let name = app_name(index);
        let deployment = deployments.join(&name);
        std::fs::create_dir_all(&deployment).expect("create deployment directory");
        std::fs::write(
            deployment.join("coop.toml"),
            format!(
                r#"name = "{name}"

[hosts]
domains = ["{name}.bench"]

[[handlers]]
file = "handlers/main.ts"
path = "/"
method = "GET"
"#,
            ),
        )
        .expect("write deployment config");

        let mut manifest = source_manifest.clone();
        manifest.deployment = name.clone();
        if manifest.boundary_verified {
            manifest.library_size = Some(std::fs::metadata(source_app).unwrap().len());
            manifest.library_sha256 = Some(
                coop_app_host::plugin_host::library_sha256(source_app)
                    .expect("hash prepared app image"),
            );
        }
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).expect("serialize app manifest");
        let mut packaged_config = source_config.clone();
        packaged_config["name"] = serde_json::Value::String(name.clone());
        packaged_config["hosts"]["domains"] = serde_json::json!([format!("{name}.bench")]);
        let config_bytes =
            serde_json::to_vec_pretty(&packaged_config).expect("serialize packaged config");
        let package = package_digest(&manifest_bytes, &config_bytes, &static_manifest_bytes);
        let package_dir = compiled.join(&name).join(&package);
        std::fs::create_dir_all(&package_dir).expect("create immutable package directory");
        let extension = shared_library_extension();
        let app = package_dir.join(format!("app.{extension}"));
        std::fs::copy(source_app, &app).expect("copy app image");
        std::fs::write(AppLibraryManifest::adjacent_path(&app), manifest_bytes)
            .expect("write app ABI manifest");
        std::fs::write(package_dir.join("deployment.coop.json"), config_bytes)
            .expect("write packaged deployment config");
        std::fs::write(
            package_dir.join("static.coop-manifest.json"),
            &static_manifest_bytes,
        )
        .expect("write static snapshot manifest");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_millis() as u64;
        let state = serde_json::json!({
            "version": 1,
            "deployment": name.clone(),
            "active": {
                "package_sha256": package,
                "activated_at_ms": now_ms,
            },
            "previous": [],
            "updated_at_ms": now_ms,
        });
        std::fs::write(
            compiled.join(&name).join(".coop-deployment-state.json"),
            serde_json::to_vec_pretty(&state).expect("serialize deployment state"),
        )
        .expect("write active package state");
    }

    let port = pick_free_port();
    let execution_mode = bench_execution_mode();
    let config = root.join("runtime.toml");
    std::fs::write(
        &config,
        format!(
            r#"[http]
listen_http = "127.0.0.1:{port}"

[execution]
mode = "{execution_mode}"
preload_concurrency = {preload_concurrency}

[paths]
deployments_dir = "{}"
compiled_dir = "{}"
sockets_dir = "{}"
storage_dir = "{}"
logs_dir = "{}"
acme_cache_dir = "{}"
state_db = "{}"
perry_binary = "{}"
perry_runtime_library = "{}"
perry_stdlib_library = "{}"

[tls]
mode = "off"
"#,
            deployments.display(),
            compiled.display(),
            sockets.display(),
            root.join("storage").display(),
            root.join("logs").display(),
            root.join("acme").display(),
            root.join("state.sqlite").display(),
            workspace
                .join(".perry-main/target/perry-dev/perry")
                .display(),
            workspace
                .join(format!(
                    "var/coop/lib/libperry_runtime.{}",
                    shared_library_extension()
                ))
                .display(),
            workspace
                .join(format!(
                    "var/coop/lib/libperry_stdlib.{}",
                    shared_library_extension()
                ))
                .display(),
        ),
    )
    .expect("write runtime config");
    config
}

fn package_digest(manifest: &[u8], config: &[u8], static_manifest: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(PACKAGE_DIGEST_V2_DOMAIN);
    for bytes in [manifest, config, static_manifest] {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    format!("{:x}", digest.finalize())
}

fn start_and_wait(
    daemon: &Path,
    config: &Path,
    cgroup: Option<&BenchmarkCgroup>,
) -> (Child, Duration) {
    let started = Instant::now();
    let trace_startup = std::env::var_os("COOP_BENCH_TRACE_STARTUP").is_some();
    let mut command = command_for(daemon, cgroup);
    command.arg("--config").arg(config);
    if let Some(cgroup) = cgroup {
        command.arg("--self-cgroup-procs").arg(cgroup.procs_path());
    }
    // stderr was `Stdio::null()`, so a daemon that failed to start reported
    // only "exit status: 1" and threw its reason away. Every diagnosis of a
    // failed preload then required reproducing the run by hand. Capture it and
    // print it on the failure paths instead: the daemon already says exactly
    // what went wrong, we were just discarding it.
    let mut child = command
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn benchmark daemon");
    let stdout = child.stdout.take().expect("capture daemon stdout");
    let stderr = child.stderr.take().expect("capture daemon stderr");
    let captured_errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let error_sink = Arc::clone(&captured_errors);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut sink = error_sink.lock().expect("daemon stderr sink");
            // Bounded: a failing daemon can be chatty, and the tail is the
            // part that explains the exit.
            if sink.len() == 400 {
                sink.remove(0);
            }
            sink.push(line);
        }
    });
    let report_errors = || {
        let sink = captured_errors.lock().expect("daemon stderr sink");
        if sink.is_empty() {
            "  (daemon produced no stderr)".to_string()
        } else {
            sink.iter()
                .map(|line| format!("  daemon: {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let (line_tx, line_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let errors = report_errors();
            stop(&mut child);
            panic!("daemon did not become ready within {READY_TIMEOUT:?}\n{errors}");
        }
        match line_rx.recv_timeout(remaining.min(Duration::from_secs(1))) {
            Ok(line) => {
                if trace_startup {
                    eprintln!("daemon: {line}");
                }
                if line.contains("HTTP listener ready") {
                    return (child, started.elapsed());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().expect("query daemon status") {
                    panic!("daemon exited before ready: {status}\n{}", report_errors());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = child.wait().expect("wait for failed daemon");
                panic!(
                    "daemon output closed before ready: {status}\n{}",
                    report_errors()
                );
            }
        }
    }
}

async fn warm_every_app(port: u16, app_count: usize) {
    let client = reqwest::Client::builder()
        .timeout(bench_request_timeout())
        .build()
        .expect("build benchmark HTTP client");
    for index in 0..app_count {
        let name = app_name(index);
        let response = client
            .get(format!("http://127.0.0.1:{port}{}", bench_request_path()))
            .header("host", format!("{name}.bench"))
            .send()
            .await
            .expect("dispatch warm request");
        assert_eq!(response.status(), 200, "warm request for {name}");
        let body = response.bytes().await.expect("read warm response");
        let expected = bench_expect_body();
        assert!(
            String::from_utf8_lossy(body.as_ref()).contains(&expected),
            "warm response for {name} did not contain {expected:?}: {}",
            String::from_utf8_lossy(body.as_ref())
                .chars()
                .take(200)
                .collect::<String>()
        );
    }
}

async fn run_workload(port: u16, app_count: usize, requests: usize) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(100)
        .timeout(bench_request_timeout())
        .build()
        .expect("build workload HTTP client");
    stream::iter(0..requests)
        .map(|index| {
            let client = client.clone();
            let name = app_name(index % app_count);
            async move {
                let response = client
                    .get(format!("http://127.0.0.1:{port}{}", bench_request_path()))
                    .header("host", format!("{name}.bench"))
                    .send()
                    .await
                    .expect("dispatch workload request");
                assert_eq!(response.status(), 200);
                let body = response.bytes().await.expect("read workload response");
                let expected = bench_expect_body();
                assert!(
                    String::from_utf8_lossy(body.as_ref()).contains(&expected),
                    "workload response did not contain {expected:?}"
                );
            }
        })
        .buffer_unordered(bench_workload_concurrency())
        .collect::<Vec<_>>()
        .await;
}

/// Resident memory of the daemon AND every process it spawned.
///
/// This sampled only the daemon's own pid. In `in_process` mode that is the
/// whole system, so the figures were right. In `worker` mode each deployment
/// runs in its OWN process, none of which the sampler looked at — so ten apps
/// reported ~18 MiB, indistinguishable from one, and worker mode appeared
/// dramatically denser than in-process. That is not density, it is an empty
/// daemon: an instrument that cannot see what it claims to measure.
///
/// Summing the tree keeps `in_process` unchanged (it has no children holding
/// apps) while making `worker` mean what the column header says.
fn descendant_pids(root: u32) -> Vec<u32> {
    // One `ps` snapshot, then walk the ppid graph. Repeated `pgrep -P` calls
    // race against worker restarts and can miss a generation.
    let output = Command::new("ps")
        .args(["-eo", "pid=,ppid="])
        .output()
        .expect("list processes");
    assert!(output.status.success(), "ps failed while listing processes");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(pid), Some(ppid)) = (parts.next(), parts.next()) {
            if let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut out = vec![root];
    let mut queue = vec![root];
    while let Some(next) = queue.pop() {
        if let Some(kids) = children.get(&next) {
            for kid in kids {
                if !out.contains(kid) {
                    out.push(*kid);
                    queue.push(*kid);
                }
            }
        }
    }
    out
}

fn tree_rss_kib(root: u32) -> u64 {
    descendant_pids(root)
        .iter()
        .filter_map(|pid| {
            let output = Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .ok()?;
            // A worker can exit between the snapshot and this sample; skip it
            // rather than failing the whole measurement.
            String::from_utf8(output.stdout)
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .sum()
}

fn median_rss_kib(pid: u32) -> u64 {
    let mut readings = Vec::with_capacity(7);
    for _ in 0..7 {
        readings.push(tree_rss_kib(pid));
        std::thread::sleep(Duration::from_millis(25));
    }
    median_u64(readings)
}

fn median_u64(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

fn median_optional_u64(values: Vec<Option<u64>>) -> Option<u64> {
    values
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .map(median_u64)
}

fn median_duration_millis(sorted_values: &[Duration]) -> f64 {
    if sorted_values.is_empty() {
        return f64::NAN;
    }
    let middle = sorted_values.len() / 2;
    if sorted_values.len() % 2 == 0 {
        (millis(sorted_values[middle - 1]) + millis(sorted_values[middle])) / 2.0
    } else {
        millis(sorted_values[middle])
    }
}

fn shared_library_extension() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn pick_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("read ephemeral port")
        .port()
}

fn port_from_config(config: &Path) -> u16 {
    let contents = std::fs::read_to_string(config).expect("read runtime config");
    contents
        .lines()
        .find_map(|line| {
            line.strip_prefix("listen_http = \"127.0.0.1:")
                .and_then(|value| value.strip_suffix('\"'))
                .and_then(|value| value.parse().ok())
        })
        .expect("runtime config has HTTP port")
}

fn app_name(index: usize) -> String {
    format!("bench-{index:03}")
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn mib(kib: u64) -> f64 {
    kib as f64 / 1_024.0
}

fn optional_mib(kib: Option<u64>) -> f64 {
    kib.map(mib).unwrap_or(f64::NAN)
}

fn rate(requests: usize, elapsed: Duration) -> f64 {
    if requests == 0 || elapsed.is_zero() {
        return 0.0;
    }
    requests as f64 / elapsed.as_secs_f64()
}

fn cpu_micros_per_request(requests: usize, cpu: Duration) -> f64 {
    if requests == 0 {
        return 0.0;
    }
    cpu.as_secs_f64() * 1_000_000.0 / requests as f64
}
