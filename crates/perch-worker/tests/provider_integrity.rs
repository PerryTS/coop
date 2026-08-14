use perch_app_host::initialize_runtime_libraries;
use serde_json::Value;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("worker crate is inside the workspace")
        .to_path_buf()
}

#[test]
fn provider_manifest_rejects_changed_runtime_size_before_loading() {
    let workspace = workspace_root();
    let source_manifest = workspace.join("var/perch/lib/perry-libraries.json");
    if !source_manifest.exists() {
        eprintln!("skip: Perry providers are not built");
        return;
    }

    let package = tempfile::tempdir().expect("temporary provider package");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&source_manifest).expect("read provider manifest"))
            .expect("parse provider manifest");
    let runtime_file = manifest["runtime_file"].as_str().unwrap().to_string();
    let stdlib_file = manifest["stdlib_file"].as_str().unwrap().to_string();

    // Both paths must exist for canonicalization, but no provider bytes should
    // be mapped before the package identity has been validated.
    std::fs::write(package.path().join(&runtime_file), b"changed").unwrap();
    std::fs::write(package.path().join(&stdlib_file), b"changed").unwrap();
    manifest["runtime_size"] = Value::from(1_u64);
    std::fs::write(
        package.path().join("perry-libraries.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let error = initialize_runtime_libraries(
        &package.path().join(runtime_file),
        &package.path().join(stdlib_file),
    )
    .expect_err("changed provider must fail closed");
    assert!(error.to_string().contains("runtime size mismatch"));
}
