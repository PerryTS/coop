//! Optional roundtrip against the real shared-runtime smoke artifact.
//!
//! The daemon integration tests create this artifact from the pinned Perry
//! compiler. Keeping this test conditional lets `cargo test` work in a clean
//! checkout while still exercising the worker-side loader when the generated
//! provider pair and app library are present.

use perch_app_host::host::DeploymentHost;
use perch_app_host::initialize_runtime_libraries;
use perch_host_abi::{AppLibraryManifest, HttpDispatchRequest};
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("worker crate is inside the workspace")
        .to_path_buf()
}

fn shared_library_extension() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

fn provider_paths(workspace: &std::path::Path) -> (PathBuf, PathBuf) {
    let extension = shared_library_extension();
    (
        std::env::var_os("PERCH_TEST_RUNTIME_LIBRARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                workspace.join(format!("var/perch/lib/libperry_runtime.{extension}"))
            }),
        std::env::var_os("PERCH_TEST_STDLIB_LIBRARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                workspace.join(format!("var/perch/lib/libperry_stdlib.{extension}"))
            }),
    )
}

fn source_app_path(workspace: &std::path::Path) -> PathBuf {
    if let Some(path) = std::env::var_os("PERCH_TEST_APP_LIBRARY") {
        return PathBuf::from(path);
    }
    let legacy = workspace.join(format!(
        "target/dynamic-smoke/compiled/test1.{}",
        shared_library_extension()
    ));
    let namespace = workspace.join("target/dynamic-smoke/compiled/test1");
    let mut candidates: Vec<_> = std::fs::read_dir(&namespace)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            entry
                .path()
                .join(format!("app.{}", shared_library_extension()))
        })
        .filter(|path| path.is_file())
        .collect();
    candidates.sort();
    candidates.pop().unwrap_or(legacy)
}

#[tokio::test]
async fn shared_runtime_app_roundtrip() {
    let workspace = workspace_root();
    let (runtime, stdlib) = provider_paths(&workspace);
    let app = source_app_path(&workspace);

    if !(runtime.exists() && stdlib.exists() && app.exists()) {
        eprintln!(
            "skip: build the Perry providers and dynamic-smoke app to run the shared-runtime roundtrip"
        );
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let host = DeploymentHost::load("test1", &app, None).expect("preload app library");
    let response = host
        .dispatch_http(HttpDispatchRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: String::new(),
            headers: Vec::new(),
            remote_addr: "127.0.0.1".to_string(),
            scheme: "http".to_string(),
            host: "test1.local".to_string(),
            body: Vec::new(),
        })
        .await
        .expect("dispatch through preloaded app");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"ok");
    let memory = host.memory_stats().await.expect("query app arena");
    assert!(memory.arena_live_bytes > 0);
    assert!(memory.arena_reserved_bytes >= memory.arena_live_bytes);
    host.shutdown().await.expect("stop app executor");
}

#[tokio::test]
#[ignore = "pinned Perry retains one old-generation request/response Buffer pair per invocation"]
async fn host_buffer_churn_is_reclaimed_by_perry() {
    let workspace = workspace_root();
    let (runtime, stdlib) = provider_paths(&workspace);
    let app = source_app_path(&workspace);
    if !(runtime.exists() && stdlib.exists() && app.exists()) {
        eprintln!(
            "skip: build the Perry providers and dynamic-smoke app to run automatic-GC proof"
        );
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let host = DeploymentHost::load_with_options(
        "automatic-gc",
        &app,
        None,
        perch_app_host::host::DeploymentHostOptions {
            gc_reclaim_check_interval: 256,
            gc_reclaim_growth_bytes: 256 * 1024,
            ..Default::default()
        },
    )
    .expect("preload automatic-GC app library");
    let mut peak_live = 0_u64;
    for request in 1..=50_000 {
        let response = host
            .dispatch_http(smoke_request())
            .await
            .expect("dispatch automatic-GC request");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
        if request % 5_000 == 0 {
            let memory = host.memory_stats().await.expect("query automatic-GC arena");
            peak_live = peak_live.max(memory.arena_live_bytes);
            eprintln!("automatic GC checkpoint request={request}: {memory:?}");
        }
    }
    let memory = host.memory_stats().await.expect("query automatic-GC arena");
    assert!(
        peak_live < 1024 * 1024,
        "automatic collector did not bound temporary request objects: peak={peak_live}, final={memory:?}"
    );
    host.shutdown()
        .await
        .expect("stop automatic-GC app executor");
}

#[tokio::test]
async fn host_boundary_full_gc_reclaims_buffers_and_preserves_next_response() {
    let workspace = workspace_root();
    let (runtime, stdlib) = provider_paths(&workspace);
    let app = source_app_path(&workspace);
    if !(runtime.exists() && stdlib.exists() && app.exists()) {
        eprintln!("skip: shared-runtime fixture is not built");
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let host = DeploymentHost::load("host-boundary-gc", &app, None)
        .expect("preload host-boundary-GC app library");
    for cycle in 1..=2 {
        for _ in 0..5_000 {
            let response = host
                .dispatch_http(smoke_request())
                .await
                .expect("dispatch before host-boundary GC");
            assert_eq!(response.status, 200);
            assert_eq!(response.body, b"ok");
        }
        let before = host.memory_stats().await.expect("query pre-GC arena");
        assert!(
            before.arena_live_bytes >= 750_000,
            "fixture did not create the expected Buffer churn: {before:?}"
        );
        assert_eq!(
            host.memory_pressure(2).await.expect("request full GC"),
            2,
            "executor boundary should be a precise collection point"
        );
        let after = host.memory_stats().await.expect("query post-GC arena");
        assert!(
            after.arena_live_bytes < 64 * 1024,
            "full GC did not reclaim host-ABI Buffer churn: cycle={cycle} before={before:?} after={after:?}"
        );

        let response = host
            .dispatch_http(smoke_request())
            .await
            .expect("dispatch after host-boundary GC");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }
    host.shutdown()
        .await
        .expect("stop host-boundary-GC app executor");
}

#[tokio::test]
#[ignore = "opt-in repeated native image/executor lifecycle smoke"]
async fn repeated_load_dispatch_shutdown_reclaims_executor_threads() {
    let workspace = workspace_root();
    let (runtime, stdlib) = provider_paths(&workspace);
    let app = source_app_path(&workspace);
    if !(runtime.exists() && stdlib.exists() && app.exists()) {
        eprintln!("skip: shared-runtime fixture is not built");
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let cycles = std::env::var("PERCH_LIFECYCLE_CYCLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(250);

    // Stabilize provider/runtime one-time allocations before measuring
    // retained generation resources.
    for iteration in 0..5 {
        run_lifecycle_iteration(&app, format!("warmup-{iteration}")).await;
    }
    let baseline_threads = process_thread_count().expect("sample baseline thread count");
    let baseline_fds = process_file_descriptor_count().expect("sample baseline descriptor count");
    let baseline_rss = perch_app_host::process_rss_kib();
    let mut peak_threads = baseline_threads;
    let mut peak_fds = baseline_fds;
    let mut rss_samples = Vec::new();
    if let Some(rss) = baseline_rss {
        rss_samples.push((0usize, rss));
    }
    for iteration in 0..cycles {
        run_lifecycle_iteration(&app, format!("reload-{iteration}")).await;
        if (iteration + 1) % 10 == 0 || iteration + 1 == cycles {
            peak_threads = peak_threads
                .max(process_thread_count().expect("sample lifecycle thread checkpoint"));
            peak_fds = peak_fds.max(
                process_file_descriptor_count().expect("sample lifecycle descriptor checkpoint"),
            );
            if let Some(rss) = perch_app_host::process_rss_kib() {
                rss_samples.push((iteration + 1, rss));
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let final_threads = process_thread_count().expect("sample final thread count");
    let final_fds = process_file_descriptor_count().expect("sample final descriptor count");
    let final_rss = perch_app_host::process_rss_kib();
    eprintln!(
        "repeated reload lifecycle: cycles={cycles} threads={baseline_threads}->{final_threads} peak={peak_threads} fds={baseline_fds}->{final_fds} peak={peak_fds} rss_kib={baseline_rss:?}->{final_rss:?} checkpoints={rss_samples:?}"
    );
    assert!(
        peak_threads <= baseline_threads + 2 && final_threads <= baseline_threads + 1,
        "executor threads leaked across reloads: baseline={baseline_threads} peak={peak_threads} final={final_threads}"
    );
    assert!(
        peak_fds <= baseline_fds + 4 && final_fds <= baseline_fds + 2,
        "file descriptors leaked across reloads: baseline={baseline_fds} peak={peak_fds} final={final_fds}"
    );
    if let (Some(before), Some(after)) = (baseline_rss, final_rss) {
        assert!(
            after <= before.saturating_add(64 * 1024),
            "{cycles} reloads retained more than 64 MiB after warmup: {before} KiB -> {after} KiB"
        );
    }
}

async fn run_lifecycle_iteration(app: &std::path::Path, deployment: String) {
    let host = DeploymentHost::load(&deployment, app, None).expect("load repeated app executor");
    let response = host
        .dispatch_http(HttpDispatchRequest {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            headers: Vec::new(),
            remote_addr: "127.0.0.1".into(),
            scheme: "http".into(),
            host: "reload.test".into(),
            body: Vec::new(),
        })
        .await
        .expect("dispatch repeated app");
    assert_eq!(response.status, 200);
    host.shutdown()
        .await
        .expect("shutdown repeated app executor");
}

fn process_file_descriptor_count() -> Option<usize> {
    let path = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(path).ok().map(Iterator::count)
}

fn process_thread_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        return std::fs::read_dir("/proc/self/task")
            .ok()
            .map(Iterator::count);
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        return Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .count()
                .saturating_sub(1),
        );
    }
    #[allow(unreachable_code)]
    None
}

#[test]
fn strict_host_rejects_manifestless_app() {
    let workspace = workspace_root();
    let (runtime, stdlib) = provider_paths(&workspace);
    let source_app = source_app_path(&workspace);
    if !(runtime.exists() && stdlib.exists() && source_app.exists()) {
        eprintln!("skip: Perry providers or dynamic-smoke app are not built");
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let app_dir = tempfile::tempdir().expect("manifestless app directory");
    let app = app_dir
        .path()
        .join(format!("manifestless.{}", shared_library_extension()));
    std::fs::copy(source_app, &app).expect("copy app without sidecar");
    let error = DeploymentHost::load("manifestless", &app, None)
        .err()
        .expect("strict in-process host must reject a manifestless app");
    assert!(error.to_string().contains("has no ABI manifest"));
}

#[test]
fn strict_host_rejects_abi_v1_app() {
    let workspace = workspace_root();
    let (runtime, stdlib) = provider_paths(&workspace);
    let source_app = source_app_path(&workspace);
    let Some(mut manifest) = AppLibraryManifest::load(&source_app).expect("read source manifest")
    else {
        eprintln!("skip: dynamic-smoke app is not built");
        return;
    };
    if !(runtime.exists() && stdlib.exists() && source_app.exists()) {
        eprintln!("skip: Perry providers or dynamic-smoke app are not built");
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let app_dir = tempfile::tempdir().expect("ABI-v1 app directory");
    let app = app_dir
        .path()
        .join(format!("abi-v1.{}", shared_library_extension()));
    std::fs::copy(source_app, &app).expect("copy app");
    manifest.abi_version = 1;
    manifest.write(&app).expect("write ABI-v1 manifest");
    let error = DeploymentHost::load("abi-v1", &app, None)
        .err()
        .expect("ABI-v1 app must be rejected");
    assert!(error.to_string().contains("app-library ABI mismatch"));
}

#[test]
fn strict_host_rejects_modified_verified_app() {
    use std::io::{Read, Seek, SeekFrom, Write};

    let workspace = workspace_root();
    let (runtime, stdlib) = provider_paths(&workspace);
    let source_app = source_app_path(&workspace);
    let Some(manifest) = AppLibraryManifest::load(&source_app).expect("read source manifest")
    else {
        eprintln!("skip: dynamic-smoke app is not built");
        return;
    };
    if !(runtime.exists() && stdlib.exists() && source_app.exists() && manifest.boundary_verified) {
        eprintln!("skip: verified dynamic-smoke app or providers are not built");
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let app_dir = tempfile::tempdir().expect("modified app directory");
    let app = app_dir
        .path()
        .join(format!("modified.{}", shared_library_extension()));
    std::fs::copy(&source_app, &app).expect("copy verified app");
    manifest.write(&app).expect("copy app manifest");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&app)
        .expect("open copied app");
    file.seek(SeekFrom::Start(64)).unwrap();
    let mut original = [0_u8];
    file.read_exact(&mut original).unwrap();
    file.seek(SeekFrom::Start(64)).unwrap();
    file.write_all(&[original[0] ^ 0xff]).unwrap();
    file.flush().unwrap();

    let error = DeploymentHost::load("modified", &app, None)
        .err()
        .expect("modified verified app must be rejected");
    assert!(error.to_string().contains("SHA-256 integrity check"));
}

/// Capacity proof for the intended one-process/many-app deployment shape.
/// Kept ignored because it creates 100 shared-library images and OS threads.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "opt-in 100-app capacity smoke"]
async fn hundred_preloaded_apps_dispatch() {
    let workspace = workspace_root();
    let (runtime, stdlib) = provider_paths(&workspace);
    let source_app = source_app_path(&workspace);
    let Some(source_manifest) =
        AppLibraryManifest::load(&source_app).expect("read source app manifest")
    else {
        eprintln!("skip: dynamic-smoke app is not built");
        return;
    };
    if !(runtime.exists() && stdlib.exists() && source_app.exists()) {
        eprintln!("skip: Perry providers or dynamic-smoke app are not built");
        return;
    }

    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");
    let app_dir = tempfile::tempdir().expect("capacity app directory");
    let mut load_tasks = Vec::with_capacity(100);
    for index in 0..100 {
        let name = format!("capacity-{index:03}");
        let app = app_dir
            .path()
            .join(format!("{name}.{}", shared_library_extension()));
        std::fs::copy(&source_app, &app).expect("copy app image");
        #[cfg(target_os = "macos")]
        {
            let status = std::process::Command::new("install_name_tool")
                .arg("-id")
                .arg(&app)
                .arg(&app)
                .status()
                .expect("run install_name_tool");
            assert!(status.success(), "assign unique app install name");
        }
        let mut manifest = source_manifest.clone();
        manifest.deployment = name.clone();
        if manifest.boundary_verified {
            manifest.library_size = Some(std::fs::metadata(&app).unwrap().len());
            manifest.library_sha256 = Some(
                perch_app_host::plugin_host::library_sha256(&app).expect("hash copied app image"),
            );
        }
        manifest.write(&app).expect("write copied app manifest");
        load_tasks.push(tokio::task::spawn_blocking(move || {
            DeploymentHost::load(&name, &app, None)
        }));
    }

    let mut hosts = Vec::with_capacity(100);
    for task in load_tasks {
        hosts.push(std::sync::Arc::new(
            task.await.expect("join app preload").expect("preload app"),
        ));
    }

    let mut ready_live_bytes = 0_u64;
    let mut ready_reserved_bytes = 0_u64;
    for host in &hosts {
        let memory = host.memory_stats().await.expect("query ready app arena");
        ready_live_bytes += memory.arena_live_bytes;
        ready_reserved_bytes += memory.arena_reserved_bytes;
    }
    eprintln!(
        "100 ready app images: rss={:?} KiB, Perry arenas: live={} bytes, reserved={} bytes, reserved/app={} bytes",
        perch_app_host::process_rss_kib(),
        ready_live_bytes,
        ready_reserved_bytes,
        ready_reserved_bytes / hosts.len() as u64,
    );

    let started = std::time::Instant::now();
    let mut dispatches = Vec::with_capacity(hosts.len());
    for host in &hosts {
        let host = host.clone();
        dispatches.push(tokio::spawn(async move {
            host.dispatch_http(smoke_request()).await
        }));
    }
    for dispatch in dispatches {
        let response = dispatch.await.expect("join dispatch").expect("dispatch");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }
    let mut live_bytes = 0_u64;
    let mut reserved_bytes = 0_u64;
    for host in &hosts {
        let memory = host.memory_stats().await.expect("query app arena");
        live_bytes += memory.arena_live_bytes;
        reserved_bytes += memory.arena_reserved_bytes;
    }
    eprintln!(
        "100 warm app images dispatched in {:?}; rss={:?} KiB, Perry arenas: live={} bytes, reserved={} bytes, reserved/app={} bytes",
        started.elapsed(),
        perch_app_host::process_rss_kib(),
        live_bytes,
        reserved_bytes,
        reserved_bytes / hosts.len() as u64,
    );

    for host in hosts {
        host.shutdown().await.expect("stop app executor");
    }
}

fn smoke_request() -> HttpDispatchRequest {
    HttpDispatchRequest {
        method: "GET".to_string(),
        path: "/".to_string(),
        query: String::new(),
        headers: Vec::new(),
        remote_addr: "127.0.0.1".to_string(),
        scheme: "http".to_string(),
        host: "test1.local".to_string(),
        body: Vec::new(),
    }
}
