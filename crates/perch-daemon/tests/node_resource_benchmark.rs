//! Opt-in startup and aggregate memory baseline for independent Node processes.
//!
//! One process represents one separately deployed application, matching the
//! usual process-isolated Node deployment shape. The handler is intentionally
//! as small as the Perry benchmark handler; framework memory is not included.

use futures::{stream, StreamExt};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[path = "support/benchmark_cgroup.rs"]
mod benchmark_cgroup;
#[path = "support/process_cpu.rs"]
mod process_cpu;
#[path = "support/process_memory.rs"]
mod process_memory;

use benchmark_cgroup::{command_for, BenchmarkCgroup};
use process_cpu::process_cpu_time;
use process_memory::median_group_memory_kib;

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const SCENARIOS: &[(usize, usize)] = &[(1, 1), (1, 100), (100, 1)];

#[derive(Debug)]
struct NodeProcess {
    child: Child,
    port: u16,
    app_count: usize,
}

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
#[ignore = "opt-in Node process startup and memory baseline"]
async fn measure_node_process_startup_and_rss() {
    let node = std::env::var_os("PERCH_BENCH_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node"));
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/node_small_app.mjs");
    let trials = std::env::var("PERCH_BENCH_TRIALS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3);
    let requests = std::env::var("PERCH_BENCH_REQUESTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let scenarios = std::env::var("PERCH_BENCH_NODE_SCENARIOS")
        .ok()
        .map(|value| parse_scenarios(&value))
        .unwrap_or_else(|| SCENARIOS.to_vec());
    let cgroup_root = std::env::var_os("PERCH_BENCH_CGROUP_ROOT").map(PathBuf::from);
    if cgroup_root.is_some() {
        assert!(cfg!(target_os = "linux"), "benchmark cgroups require Linux");
    }
    assert!(trials > 0, "PERCH_BENCH_TRIALS must be positive");

    let version = Command::new(&node)
        .arg("--version")
        .output()
        .expect("run Node")
        .stdout;
    eprintln!("node={}", String::from_utf8_lossy(&version).trim());
    eprintln!(
        "platform={}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    eprintln!("trials={trials}");
    eprintln!("requests={requests}");
    eprintln!("scenarios={scenarios:?}");
    eprintln!(
        "cgroup_root={}",
        cgroup_root
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "disabled".into())
    );

    for (process_count, apps_per_process) in scenarios {
        let logical_apps = process_count * apps_per_process;
        let mut samples = Vec::with_capacity(trials);
        for trial in 0..trials {
            let cgroup = cgroup_root.as_deref().map(|root| {
                BenchmarkCgroup::prepare(
                    root,
                    &format!("node-p{process_count}-a{apps_per_process}-t{}", trial + 1),
                )
                .expect("prepare Node benchmark cgroup")
            });
            let (mut processes, startup) = start_group(
                &node,
                &fixture,
                process_count,
                apps_per_process,
                cgroup.as_ref(),
            );
            let pids: Vec<_> = processes.iter().map(|process| process.child.id()).collect();
            let ready_rss_kib = median_group_rss_kib(&pids);
            let ready_memory = median_group_memory_kib(&pids);
            let ready_cgroup_kib = cgroup
                .as_ref()
                .and_then(BenchmarkCgroup::memory_current_kib);

            let warm_started = Instant::now();
            warm_every_process(&processes).await;
            let warm_all = warm_started.elapsed();
            let warm_rss_kib = median_group_rss_kib(&pids);
            let warm_memory = median_group_memory_kib(&pids);
            let warm_cgroup_kib = cgroup
                .as_ref()
                .and_then(BenchmarkCgroup::memory_current_kib);

            let cpu_before = process_cpu_time(&pids);
            let workload_started = Instant::now();
            if requests > 0 {
                run_workload(&processes, requests).await;
            }
            let workload = workload_started.elapsed();
            let workload_cpu = process_cpu_time(&pids).saturating_sub(cpu_before);
            let post_workload_rss_kib = median_group_rss_kib(&pids);
            let post_workload_memory = median_group_memory_kib(&pids);
            let post_workload_cgroup_kib = cgroup
                .as_ref()
                .and_then(BenchmarkCgroup::memory_current_kib);
            let cgroup_peak_kib = cgroup.as_ref().and_then(BenchmarkCgroup::memory_peak_kib);
            stop_group(&mut processes);

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
                "node_processes={process_count} logical_apps={logical_apps} trial={} startup_ms={:.3} ready_rss_mib={:.3} ready_pss_mib={:.3} ready_private_dirty_mib={:.3} ready_cgroup_mib={:.3} warm_all_ms={:.3} warm_rss_mib={:.3} warm_pss_mib={:.3} warm_private_dirty_mib={:.3} warm_cgroup_mib={:.3} requests={requests} workload_ms={:.3} server_cpu_ms={:.3} post_workload_rss_mib={:.3} post_workload_pss_mib={:.3} post_workload_private_dirty_mib={:.3} post_workload_cgroup_mib={:.3} cgroup_peak_mib={:.3}",
                trial + 1,
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

        samples.sort_by_key(|sample| sample.startup);
        let startup_median = samples[samples.len() / 2].startup;
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
            "NODE_RESULT processes={process_count} logical_apps={logical_apps} startup_median_ms={:.3} ready_rss_median_mib={:.3} ready_pss_median_mib={:.3} ready_private_dirty_median_mib={:.3} ready_cgroup_median_mib={:.3} warm_all_median_ms={:.3} warm_rss_median_mib={:.3} warm_pss_median_mib={:.3} warm_private_dirty_median_mib={:.3} warm_cgroup_median_mib={:.3} requests={requests} workload_median_ms={:.3} server_cpu_median_ms={:.3} requests_per_second={:.3} server_cpu_us_per_request={:.3} post_workload_rss_median_mib={:.3} post_workload_pss_median_mib={:.3} post_workload_private_dirty_median_mib={:.3} post_workload_cgroup_median_mib={:.3} cgroup_peak_median_mib={:.3}",
            millis(startup_median),
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
            rate(requests, workload_times[workload_times.len() / 2]),
            cpu_micros_per_request(requests, workload_cpu[workload_cpu.len() / 2]),
            mib(post_workload_rss_median),
            optional_mib(post_workload_pss_median),
            optional_mib(post_workload_private_dirty_median),
            optional_mib(post_workload_cgroup_median),
            optional_mib(cgroup_peak_median),
        );
    }
}

fn parse_scenarios(value: &str) -> Vec<(usize, usize)> {
    value
        .split(',')
        .map(|scenario| {
            let (processes, apps) = scenario.trim().split_once('x').unwrap_or_else(|| {
                panic!("invalid Node scenario {scenario:?}; expected PROCESSxAPP")
            });
            let processes = processes
                .parse::<usize>()
                .expect("parse Node process count");
            let apps = apps.parse::<usize>().expect("parse Node app count");
            assert!(
                processes > 0 && apps > 0,
                "Node scenario counts must be positive"
            );
            (processes, apps)
        })
        .collect()
}

#[test]
fn parses_configured_scenarios() {
    assert_eq!(
        parse_scenarios("1x1, 1x10,100x1"),
        vec![(1, 1), (1, 10), (100, 1)]
    );
}

fn start_group(
    node: &Path,
    fixture: &Path,
    count: usize,
    apps_per_process: usize,
    cgroup: Option<&BenchmarkCgroup>,
) -> (Vec<NodeProcess>, Duration) {
    let started = Instant::now();
    let mut pending = Vec::with_capacity(count);
    for _ in 0..count {
        let mut child = command_for(node, cgroup)
            .arg(fixture)
            .env("NODE_APP_COUNT", apps_per_process.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Node app");
        let stdout = child.stdout.take().expect("capture Node stdout");
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let line = BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .find(|line| line.starts_with("READY "));
            let _ = ready_tx.send(line);
        });
        pending.push((child, ready_rx));
    }

    let deadline = Instant::now() + READY_TIMEOUT;
    let mut processes = Vec::with_capacity(count);
    for (mut child, ready_rx) in pending {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = ready_rx
            .recv_timeout(remaining)
            .expect("Node app did not become ready")
            .unwrap_or_else(|| {
                let status = child.wait().expect("wait for failed Node app");
                panic!("Node app exited before ready: {status}");
            });
        let port = line
            .strip_prefix("READY ")
            .expect("Node ready line prefix")
            .parse::<u16>()
            .expect("parse Node port");
        processes.push(NodeProcess {
            child,
            port,
            app_count: apps_per_process,
        });
    }
    (processes, started.elapsed())
}

async fn warm_every_process(processes: &[NodeProcess]) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build benchmark HTTP client");
    for process in processes {
        for app_index in 0..process.app_count {
            let response = client
                .get(format!("http://127.0.0.1:{}/", process.port))
                .header("host", format!("bench-{app_index:03}.bench"))
                .send()
                .await
                .expect("dispatch Node warm request");
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.bytes().await.expect("read Node response").as_ref(),
                b"ok"
            );
        }
    }
}

async fn run_workload(processes: &[NodeProcess], requests: usize) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(100)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build Node workload HTTP client");
    let endpoints: Vec<_> = processes
        .iter()
        .flat_map(|process| {
            (0..process.app_count)
                .map(|app_index| (process.port, format!("bench-{app_index:03}.bench")))
        })
        .collect();
    stream::iter(0..requests)
        .map(|index| {
            let client = client.clone();
            let (port, host) = endpoints[index % endpoints.len()].clone();
            async move {
                let response = client
                    .get(format!("http://127.0.0.1:{port}/"))
                    .header("host", host)
                    .send()
                    .await
                    .expect("dispatch Node workload request");
                assert_eq!(response.status(), 200);
                assert_eq!(
                    response
                        .bytes()
                        .await
                        .expect("read Node workload response")
                        .as_ref(),
                    b"ok"
                );
            }
        })
        .buffer_unordered(50)
        .collect::<Vec<_>>()
        .await;
}

fn median_group_rss_kib(pids: &[u32]) -> u64 {
    let mut readings = Vec::with_capacity(7);
    for _ in 0..7 {
        let pid_list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid_list])
            .output()
            .expect("sample aggregate Node RSS");
        assert!(output.status.success(), "ps failed while sampling Node RSS");
        readings.push(
            String::from_utf8(output.stdout)
                .expect("ps output is UTF-8")
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| line.parse::<u64>().expect("parse Node RSS in KiB"))
                .sum(),
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    median_u64(readings)
}

fn stop_group(processes: &mut [NodeProcess]) {
    for process in processes.iter_mut() {
        let _ = process.child.kill();
    }
    for process in processes {
        let _ = process.child.wait();
    }
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
