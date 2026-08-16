//! Embed the Perry ABI version without linking Perry into the host.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let perry_workspace = manifest_dir.join("../../.perry-main/Cargo.toml");
    let perry_lock = manifest_dir.join("../../perry-main.lock");
    println!("cargo:rerun-if-changed={}", perry_workspace.display());
    println!("cargo:rerun-if-changed={}", perry_lock.display());

    let contents = fs::read_to_string(&perry_workspace)
        .unwrap_or_else(|error| panic!("reading {}: {error}", perry_workspace.display()));
    let workspace_package = contents
        .split_once("[workspace.package]")
        .map(|(_, tail)| tail)
        .expect("Perry workspace has [workspace.package]");
    let version = workspace_package
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
        })
        .expect("Perry workspace package has a version");

    let lock = fs::read_to_string(&perry_lock)
        .unwrap_or_else(|error| panic!("reading {}: {error}", perry_lock.display()));
    let locked_version = lock_value(&lock, "version").expect("Perry lock has a version");
    let commit = lock_value(&lock, "commit").expect("Perry lock has a commit");
    assert_eq!(
        version, locked_version,
        "Perry worktree does not match perry-main.lock"
    );
    assert_eq!(commit.len(), 40, "Perry lock commit must be a full SHA-1");

    println!("cargo:rustc-env=COOP_PERRY_RUNTIME_VERSION={version}");
    println!("cargo:rustc-env=COOP_PERRY_RUNTIME_COMMIT={commit}");
}

fn lock_value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        line.trim()
            .strip_prefix(&format!("{key} = \""))
            .and_then(|value| value.strip_suffix('"'))
    })
}
