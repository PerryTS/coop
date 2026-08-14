//! Linux cgroup-v2 ownership for dedicated worker generations.
//!
//! The daemon prepares limits before spawn and passes the generation's
//! `cgroup.procs` path to the trusted worker. The worker moves itself before
//! loading Perry providers or application code, avoiding an unconstrained app
//! startup window. `auto` mode falls back to the RSS watchdog when the host has
//! not delegated a writable hierarchy; `required` fails activation closed.

use crate::config::{CgroupMode, DeploymentLimitsConfig, ShardExecutionConfig, WorkerCgroupConfig};
#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use tracing::info;
use tracing::warn;

#[cfg(target_os = "linux")]
const CPU_PERIOD_US: u64 = 100_000;

pub struct WorkerCgroup {
    path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WorkerCgroupStats {
    pub memory_current_bytes: Option<u64>,
    pub memory_peak_bytes: Option<u64>,
    pub pids_current: Option<u64>,
    pub cpu_usage_usec: Option<u64>,
    pub memory_oom_events: Option<u64>,
    pub memory_oom_kill_events: Option<u64>,
}

impl WorkerCgroup {
    pub fn prepare_shard(
        config: &WorkerCgroupConfig,
        shard_id: &str,
        generation: u64,
        shard: &ShardExecutionConfig,
    ) -> Result<Option<Self>> {
        let mut limits = DeploymentLimitsConfig::default();
        limits.max_worker_rss_mb = shard.max_rss_mb;
        limits.max_worker_cpu_percent = shard.max_cpu_percent;
        limits.max_worker_pids = shard.max_pids;
        Self::prepare(config, shard_id, generation, &limits)
    }

    pub fn prepare(
        config: &WorkerCgroupConfig,
        deployment: &str,
        generation: u64,
        limits: &DeploymentLimitsConfig,
    ) -> Result<Option<Self>> {
        if config.mode == CgroupMode::Disabled {
            return Ok(None);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (generation, limits);
            let error = anyhow!("worker cgroups require Linux cgroup v2");
            return match config.mode {
                CgroupMode::Required => Err(error),
                CgroupMode::Auto => {
                    warn!(
                        deployment,
                        ?error,
                        "worker cgroup unavailable; using RSS watchdog"
                    );
                    crate::metrics::record_worker_cgroup(deployment, "fallback");
                    Ok(None)
                }
                CgroupMode::Disabled => Ok(None),
            };
        }
        #[cfg(target_os = "linux")]
        {
            match Self::prepare_linux(config, deployment, generation, limits) {
                Ok(cgroup) => {
                    crate::metrics::record_worker_cgroup(deployment, "enforced");
                    Ok(Some(cgroup))
                }
                Err(error) if config.mode == CgroupMode::Auto => {
                    warn!(
                        deployment,
                        ?error,
                        root = %config.root.display(),
                        "worker cgroup unavailable; using RSS watchdog"
                    );
                    crate::metrics::record_worker_cgroup(deployment, "fallback");
                    Ok(None)
                }
                Err(error) => {
                    crate::metrics::record_worker_cgroup(deployment, "failure");
                    Err(error)
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn prepare_linux(
        config: &WorkerCgroupConfig,
        deployment: &str,
        generation: u64,
        limits: &DeploymentLimitsConfig,
    ) -> Result<Self> {
        let parent = config
            .root
            .parent()
            .ok_or_else(|| anyhow!("cgroup root {} has no parent", config.root.display()))?;
        require_cgroup_v2(parent)?;
        enable_available_controllers(parent)?;
        reject_symlink(&config.root)?;
        std::fs::create_dir_all(&config.root)
            .with_context(|| format!("creating worker cgroup root {}", config.root.display()))?;
        enable_available_controllers(&config.root)?;

        let path = config.root.join(format!(
            "{}-{}-{}",
            deployment,
            std::process::id(),
            generation
        ));
        reject_symlink(&path)?;
        std::fs::create_dir(&path)
            .with_context(|| format!("creating worker cgroup {}", path.display()))?;

        let result = (|| {
            write_control(
                &path,
                "memory.max",
                u64::from(limits.max_worker_rss_mb)
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| anyhow!("worker memory limit overflow"))?,
            )?;
            write_control(&path, "memory.swap.max", 0)?;
            write_control(&path, "memory.oom.group", 1)?;
            let quota = u64::from(limits.max_worker_cpu_percent)
                .checked_mul(CPU_PERIOD_US)
                .ok_or_else(|| anyhow!("worker CPU quota overflow"))?
                / 100;
            write_text(&path.join("cpu.max"), &format!("{quota} {CPU_PERIOD_US}"))?;
            write_control(&path, "pids.max", u64::from(limits.max_worker_pids))?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_dir(&path);
            return Err(error);
        }

        info!(
            deployment,
            cgroup = %path.display(),
            memory_max_mb = limits.max_worker_rss_mb,
            cpu_percent = limits.max_worker_cpu_percent,
            pids_max = limits.max_worker_pids,
            "prepared worker failure-domain cgroup"
        );
        Ok(Self { path })
    }

    pub fn procs_path(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    pub fn stats(&self) -> WorkerCgroupStats {
        let memory_events = read_key_values(&self.path.join("memory.events"));
        let cpu_stat = read_key_values(&self.path.join("cpu.stat"));
        WorkerCgroupStats {
            memory_current_bytes: read_u64(&self.path.join("memory.current")),
            memory_peak_bytes: read_u64(&self.path.join("memory.peak")),
            pids_current: read_u64(&self.path.join("pids.current")),
            cpu_usage_usec: cpu_stat
                .as_ref()
                .and_then(|values| lookup_value(values, "usage_usec")),
            memory_oom_events: memory_events
                .as_ref()
                .and_then(|values| lookup_value(values, "oom")),
            memory_oom_kill_events: memory_events
                .as_ref()
                .and_then(|values| lookup_value(values, "oom_kill")),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkerCgroup {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    cgroup = %self.path.display(),
                    ?error,
                    "worker cgroup cleanup deferred"
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn require_cgroup_v2(parent: &Path) -> Result<()> {
    let controllers = parent.join("cgroup.controllers");
    if !controllers.is_file() {
        return Err(anyhow!(
            "{} is not inside a cgroup-v2 hierarchy (missing {})",
            parent.display(),
            controllers.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn enable_available_controllers(path: &Path) -> Result<()> {
    let controllers_path = path.join("cgroup.controllers");
    if !controllers_path.exists() {
        // A normal-filesystem test double does not synthesize child control
        // files. The actual hierarchy was already verified at its parent.
        return Ok(());
    }
    let controllers = std::fs::read_to_string(&controllers_path)
        .with_context(|| format!("reading {}", controllers_path.display()))?;
    let required = ["memory", "cpu", "pids"];
    for controller in required {
        if !controllers
            .split_whitespace()
            .any(|value| value == controller)
        {
            return Err(anyhow!(
                "cgroup controller {controller:?} is unavailable under {}",
                path.display()
            ));
        }
    }
    let subtree = path.join("cgroup.subtree_control");
    write_text(&subtree, "+memory +cpu +pids")
        .with_context(|| format!("enabling controllers below {}", path.display()))
}

#[cfg(target_os = "linux")]
fn reject_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "cgroup path {} must not be a symlink",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

#[cfg(target_os = "linux")]
fn write_control(path: &Path, name: &str, value: u64) -> Result<()> {
    write_text(&path.join(name), &value.to_string())
}

#[cfg(target_os = "linux")]
fn write_text(path: &Path, value: &str) -> Result<()> {
    std::fs::write(path, value).with_context(|| format!("writing {}", path.display()))
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_key_values(path: &Path) -> Option<Vec<(String, u64)>> {
    Some(
        std::fs::read_to_string(path)
            .ok()?
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once(' ')?;
                Some((key.to_string(), value.parse().ok()?))
            })
            .collect(),
    )
}

fn lookup_value(values: &[(String, u64)], key: &str) -> Option<u64> {
    values
        .iter()
        .find_map(|(candidate, value)| (candidate == key).then_some(*value))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::config::{CgroupMode, DeploymentLimitsConfig, WorkerCgroupConfig};

    #[test]
    fn prepares_limits_and_reads_generation_stats() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("cgroup.controllers"), "memory cpu pids\n").unwrap();
        std::fs::write(temp.path().join("cgroup.subtree_control"), "").unwrap();
        let root = temp.path().join("perch");
        let config = WorkerCgroupConfig {
            mode: CgroupMode::Required,
            root: root.clone(),
        };
        let mut limits = DeploymentLimitsConfig::default();
        limits.max_worker_rss_mb = 64;
        limits.max_worker_cpu_percent = 150;
        limits.max_worker_pids = 12;

        let cgroup = WorkerCgroup::prepare(&config, "fixture", 7, &limits)
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(cgroup.path.join("memory.max")).unwrap(),
            (64_u64 * 1024 * 1024).to_string()
        );
        assert_eq!(
            std::fs::read_to_string(cgroup.path.join("cpu.max")).unwrap(),
            "150000 100000"
        );
        assert_eq!(
            std::fs::read_to_string(cgroup.path.join("pids.max")).unwrap(),
            "12"
        );

        std::fs::write(cgroup.path.join("memory.current"), "4096\n").unwrap();
        std::fs::write(cgroup.path.join("memory.peak"), "8192\n").unwrap();
        std::fs::write(cgroup.path.join("pids.current"), "3\n").unwrap();
        std::fs::write(cgroup.path.join("cpu.stat"), "usage_usec 42\n").unwrap();
        std::fs::write(
            cgroup.path.join("memory.events"),
            "low 0\nhigh 0\nmax 1\noom 2\noom_kill 1\n",
        )
        .unwrap();
        let stats = cgroup.stats();
        assert_eq!(stats.memory_current_bytes, Some(4096));
        assert_eq!(stats.memory_peak_bytes, Some(8192));
        assert_eq!(stats.pids_current, Some(3));
        assert_eq!(stats.cpu_usage_usec, Some(42));
        assert_eq!(stats.memory_oom_events, Some(2));
        assert_eq!(stats.memory_oom_kill_events, Some(1));
    }
}
