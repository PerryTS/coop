//! Roundtrip for the real Next.js benchmark artifact.
//!
//! This test is deliberately unskippable. Its whole value is proving that a
//! Next App Route executes through a Coop-compiled application library on the
//! pinned Perry providers, so "the fixture is missing" and "the fixture is
//! stale" must never be silently green. Both are repaired by rebuilding the
//! fixture through Coop's own compile pipeline; anything that stops the
//! rebuild fails the test with the reason.

use coop_app_host::host::DeploymentHost;
use coop_app_host::{initialize_runtime_libraries, perry_provider_identity};
use coop_host_abi::{AppLibraryManifest, HttpDispatchRequest};
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("worker crate is inside the workspace")
        .to_path_buf()
}

fn library_extension() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

/// Why one published package cannot satisfy this host. Collected rather than
/// discarded so a failure names the exact identity that drifted.
fn manifest_rejection(manifest: &AppLibraryManifest) -> Option<String> {
    let host_target = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    let provider_compiler = &perry_provider_identity().compiler_sha256;
    if manifest.deployment != "next-bench" {
        return Some(format!("deployment is {:?}", manifest.deployment));
    }
    if manifest.perry_version != coop_app_host::PERRY_RUNTIME_VERSION {
        return Some(format!(
            "Perry version {} != host {}",
            manifest.perry_version,
            coop_app_host::PERRY_RUNTIME_VERSION
        ));
    }
    if manifest.perry_commit != coop_app_host::PERRY_RUNTIME_COMMIT {
        return Some(format!(
            "Perry commit {} != host {}",
            manifest.perry_commit,
            coop_app_host::PERRY_RUNTIME_COMMIT
        ));
    }
    if &manifest.compiler_sha256 != provider_compiler {
        return Some(format!(
            "compiler {} != provider {provider_compiler}",
            manifest.compiler_sha256
        ));
    }
    if manifest.target != host_target {
        return Some(format!("target {} != host {host_target}", manifest.target));
    }
    None
}

/// Newest published `next-bench` package that this host can load, plus a
/// description of every candidate that was rejected.
fn published_fixture(namespace: &Path) -> (Option<PathBuf>, Vec<String>) {
    let extension = library_extension();
    let mut rejected = Vec::new();
    let Ok(entries) = std::fs::read_dir(namespace) else {
        return (None, rejected);
    };
    let mut usable: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let library = entry.path().join(format!("app.{extension}"));
        let Ok(metadata) = std::fs::metadata(&library) else {
            continue;
        };
        match AppLibraryManifest::load(&library) {
            Ok(Some(manifest)) => match manifest_rejection(&manifest) {
                None => {
                    usable.push((
                        metadata.modified().expect("artifact modification time"),
                        library,
                    ));
                }
                Some(reason) => rejected.push(format!("{}: {reason}", library.display())),
            },
            Ok(None) => rejected.push(format!("{}: no ABI manifest", library.display())),
            Err(error) => rejected.push(format!(
                "{}: unreadable manifest: {error}",
                library.display()
            )),
        }
    }
    usable.sort_by_key(|(modified, _)| *modified);
    (usable.pop().map(|(_, library)| library), rejected)
}

#[tokio::test]
async fn next_fixture_uses_required_http_abi() {
    let workspace = workspace_root();
    let extension = library_extension();
    let runtime = workspace.join(format!("var/coop/lib/libperry_runtime.{extension}"));
    let stdlib = workspace.join(format!("var/coop/lib/libperry_stdlib.{extension}"));
    assert!(
        runtime.exists() && stdlib.exists(),
        "Perry providers are missing ({} / {}); run scripts/build-perry-libraries.sh",
        runtime.display(),
        stdlib.display()
    );
    initialize_runtime_libraries(&runtime, &stdlib).expect("load Perry provider pair");

    let namespace = workspace.join("target/next-benchmark/coop-run/compiled/next-bench");
    let (mut app, mut rejected) = published_fixture(&namespace);
    if app.is_none() {
        // Regenerate rather than skip. The daemon performs the compile, so the
        // artifact under test is a real published package, never a hand-built
        // library that outlived its Perry pin.
        let script = workspace.join("scripts/prepare-next-benchmark.sh");
        eprintln!(
            "rebuilding the Next fixture through Coop ({}); rejected candidates: {rejected:?}",
            script.display()
        );
        let output = Command::new(&script)
            .current_dir(&workspace)
            .output()
            .unwrap_or_else(|error| panic!("running {}: {error}", script.display()));
        assert!(
            output.status.success(),
            "{} failed ({})\nstdout: {}\nstderr: {}",
            script.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
        let rebuilt = published_fixture(&namespace);
        app = rebuilt.0;
        rejected = rebuilt.1;
    }
    let app = app.unwrap_or_else(|| {
        panic!(
            "no published next-bench package under {} matches the pinned Perry providers \
             (version {}, commit {}); rejected: {rejected:?}",
            namespace.display(),
            coop_app_host::PERRY_RUNTIME_VERSION,
            coop_app_host::PERRY_RUNTIME_COMMIT,
        )
    });
    // The loaded bytes must be the ones Coop published: `DeploymentHost::load`
    // re-verifies the manifest's SHA-256 and size before the library is mapped.
    let manifest = AppLibraryManifest::load(&app)
        .expect("read fixture manifest")
        .expect("published package carries an ABI manifest");
    eprintln!(
        "next fixture: {} ({} bytes, sha256 {}, entry {})",
        app.display(),
        manifest.library_size.expect("published library size"),
        manifest
            .library_sha256
            .as_deref()
            .expect("published library digest"),
        manifest.handle_symbol,
    );

    let host = DeploymentHost::load("next-bench", &app, None).expect("preload Next fixture");
    let response = host
        .dispatch_http(HttpDispatchRequest {
            method: "GET".into(),
            path: "/api/benchmark".into(),
            query: "iterations=100".into(),
            headers: vec![("host".into(), "benchmark.local".into())],
            remote_addr: "127.0.0.1".into(),
            scheme: "http".into(),
            host: "benchmark.local".into(),
            body: Vec::new(),
        })
        .await
        .expect("dispatch through HTTP application ABI");

    eprintln!(
        "next route response: status={} headers={:?} body={}",
        response.status,
        response.headers,
        String::from_utf8_lossy(&response.body)
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers,
        vec![("content-type".into(), "application/json".into())]
    );
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["runtime"], "next");
    assert_eq!(body["iterations"], 100);
    assert_eq!(body["checksum"], 3_726_872_593_u64);
    host.shutdown().await.expect("stop app executor");
}
