//! Linux cgroup-v2 support shared by the opt-in resource benchmarks.
//!
//! A benchmark trial gets one cgroup containing its complete server topology.
//! Starting the executable through a tiny shell trampoline moves the process
//! into that cgroup before `exec`, so runtime initialization is accounted for
//! as well as ready and warm memory.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub(crate) struct BenchmarkCgroup {
    path: PathBuf,
}

impl BenchmarkCgroup {
    pub(crate) fn prepare(root: &Path, label: &str) -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            prepare_linux(root, label)
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (root, label);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "benchmark cgroups require Linux",
            ))
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn procs_path(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    pub(crate) fn memory_current_kib(&self) -> Option<u64> {
        Some(read_bytes(&self.path.join("memory.current")) / 1024)
    }

    pub(crate) fn memory_peak_kib(&self) -> Option<u64> {
        Some(read_bytes(&self.path.join("memory.peak")) / 1024)
    }
}

/// Construct a command that joins `cgroup` before executing `program`.
///
/// Arguments appended to the returned command are forwarded to `program`.
pub(crate) fn command_for(program: &Path, cgroup: Option<&BenchmarkCgroup>) -> Command {
    let Some(cgroup) = cgroup else {
        return Command::new(program);
    };

    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("set -e; printf '%s\\n' \"$$\" > \"$1\"; shift; exec \"$@\"")
            .arg("perch-cgroup-exec")
            .arg(cgroup.procs_path())
            .arg(program);
        command
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cgroup;
        unreachable!("BenchmarkCgroup cannot be prepared outside Linux")
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn command_fails_closed_when_cgroup_join_fails() {
        let cgroup = BenchmarkCgroup {
            path: PathBuf::from(format!(
                "/definitely-missing-perch-benchmark-cgroup-{}",
                std::process::id()
            )),
        };
        let status = command_for(Path::new("/bin/true"), Some(&cgroup))
            .status()
            .expect("run cgroup trampoline");
        std::mem::forget(cgroup);
        assert!(!status.success(), "failed cgroup join must prevent exec");
    }
}

#[cfg(target_os = "linux")]
fn prepare_linux(root: &Path, label: &str) -> std::io::Result<BenchmarkCgroup> {
    use std::io::{Error, ErrorKind};

    if !root.is_absolute() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "benchmark cgroup root must be absolute",
        ));
    }
    if label.is_empty()
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "benchmark cgroup label must contain only ASCII letters, digits, '-' or '_'",
        ));
    }

    if root.exists() {
        let metadata = std::fs::symlink_metadata(root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "benchmark cgroup root must be a real directory",
            ));
        }
    } else {
        std::fs::create_dir(root)?;
    }

    let controllers = std::fs::read_to_string(root.join("cgroup.controllers"))?;
    for required in ["memory", "cpu", "pids"] {
        if !controllers.split_whitespace().any(|item| item == required) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("benchmark cgroup root does not expose {required}"),
            ));
        }
    }
    std::fs::write(root.join("cgroup.subtree_control"), "+memory +cpu +pids")?;

    let path = root.join(format!("run-{}-{label}", std::process::id()));
    std::fs::create_dir(&path)?;

    let configure = || -> std::io::Result<()> {
        std::fs::write(path.join("memory.max"), "max")?;
        std::fs::write(path.join("memory.swap.max"), "max")?;
        std::fs::write(path.join("memory.oom.group"), "1")?;
        std::fs::write(path.join("cpu.max"), "max 100000")?;
        std::fs::write(path.join("pids.max"), "max")?;
        // Require peak accounting rather than silently degrading to RSS.
        std::fs::read_to_string(path.join("memory.peak"))?;
        Ok(())
    };
    if let Err(error) = configure() {
        let _ = std::fs::remove_dir(&path);
        return Err(error);
    }

    Ok(BenchmarkCgroup { path })
}

fn read_bytes(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

impl Drop for BenchmarkCgroup {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Err(error) = std::fs::remove_dir(&self.path) {
            eprintln!(
                "warning: failed to remove benchmark cgroup {}: {error}",
                self.path.display()
            );
        }
    }
}
