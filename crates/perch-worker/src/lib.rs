//! In-process host for Perry-compiled Perch application libraries.
//!
//! Both the standalone compatibility worker and the main Perch daemon use
//! this crate. Keeping the loader and executor in one library guarantees that
//! in-process and process-isolated deployments use the same ABI checks and the
//! same thread-affinity rules.

pub mod host;
pub mod plugin_host;
pub mod queue_store;
pub mod runtime_libraries;

/// Exact Perry runtime build this host accepts from the provider manifest.
pub const PERRY_RUNTIME_VERSION: &str = env!("PERCH_PERRY_RUNTIME_VERSION");

/// Exact Perry main revision accepted by this host.
pub const PERRY_RUNTIME_COMMIT: &str = env!("PERCH_PERRY_RUNTIME_COMMIT");

pub use runtime_libraries::{
    initialize_runtime_libraries, initialize_runtime_libraries_with_verification,
    perry_provider_identity, register_queue_enqueue_callback, runtime_libraries_loaded,
    PerchQueueEnqueueCallback, PerryProviderIdentity, ProviderVerification,
};

/// Current resident-set size in KiB. Used only at startup and exponentially
/// sampled request counts, so the macOS `ps` fallback is never on the hot
/// request path.
pub fn process_rss_kib() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
        return line.split_whitespace().nth(1)?.parse().ok();
    }

    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p"])
            .arg(std::process::id().to_string())
            .output()
            .ok()?;
        return String::from_utf8_lossy(&output.stdout).trim().parse().ok();
    }

    #[allow(unreachable_code)]
    None
}
