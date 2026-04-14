//! Filesystem watcher for `deployments/`.
//!
//! Uses `notify-debouncer-mini` to batch rapid-fire filesystem events
//! (editor saves often produce a burst of inode changes) into single
//! reload requests. When any file under `<deployments_dir>/<name>/`
//! changes, we call `supervisor.load_deployment(name)` which handles the
//! drain-and-replace lifecycle.
//!
//! Deployment add: a new directory appears → load it.
//! Deployment change: any file inside changes → reload it.
//! Deployment remove: the directory disappears → tear it down.

use crate::deployments::DeploymentSupervisor;
use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEvent};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Start watching `deployments_dir` in a background tokio task. The
/// watcher lives for the duration of the program; a single shutdown
/// signal (graceful_shutdown) stops the task cleanly.
pub fn start(
    deployments_dir: PathBuf,
    supervisor: Arc<DeploymentSupervisor>,
) -> Result<tokio::task::JoinHandle<()>> {
    if !deployments_dir.exists() {
        std::fs::create_dir_all(&deployments_dir)
            .with_context(|| format!("creating deployments dir {:?}", deployments_dir))?;
    }

    // Use an unbounded tokio channel to bridge the blocking notify
    // debouncer (which runs on its own thread) to the async supervisor.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<DebouncedEvent>>();

    // Spawn the debouncer on a blocking thread. The closure runs on the
    // notify thread and forwards events into the tokio channel.
    let watch_path = deployments_dir.clone();
    let _debouncer_handle = std::thread::Builder::new()
        .name("perch-notify-watcher".to_string())
        .spawn(move || {
            let mut debouncer = match new_debouncer(
                Duration::from_millis(500),
                move |res: Result<Vec<DebouncedEvent>, notify::Error>| match res {
                    Ok(events) => {
                        let _ = tx.send(events);
                    }
                    Err(e) => {
                        error!(error = ?e, "notify debouncer error");
                    }
                },
            ) {
                Ok(d) => d,
                Err(e) => {
                    error!(error = ?e, "failed to create notify debouncer");
                    return;
                }
            };

            if let Err(e) = debouncer
                .watcher()
                .watch(&watch_path, RecursiveMode::Recursive)
            {
                error!(error = ?e, path = %watch_path.display(), "failed to watch path");
                return;
            }

            info!(path = %watch_path.display(), "deployment watcher active");
            // Park this thread forever — the debouncer drops its internal
            // watcher handles when `debouncer` is dropped, which happens
            // when the process exits.
            loop {
                std::thread::park();
            }
        })
        .context("spawning watcher thread")?;

    let handle = tokio::spawn(async move {
        while let Some(events) = rx.recv().await {
            // Collect unique deployment names from the events.
            let names = deployments_affected(&events, &deployments_dir);
            if names.is_empty() {
                continue;
            }
            for name in names {
                debug!(deployment = %name, "filesystem change detected");
                let deployment_dir = deployments_dir.join(&name);
                if deployment_dir.exists() {
                    if let Err(e) = supervisor.load_deployment(&name).await {
                        warn!(
                            deployment = %name,
                            error = ?e,
                            "reload failed"
                        );
                    }
                } else {
                    info!(deployment = %name, "deployment directory removed, tearing down");
                    supervisor.remove_deployment(&name).await;
                }
            }
        }
    });

    Ok(handle)
}

/// Figure out which deployment directories a burst of events touched.
/// Returns unique top-level names.
fn deployments_affected(
    events: &[DebouncedEvent],
    deployments_dir: &Path,
) -> HashSet<String> {
    let mut names = HashSet::new();
    for event in events {
        let rel = match event.path.strip_prefix(deployments_dir) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Some(first) = rel.components().next() {
            if let std::path::Component::Normal(s) = first {
                if let Some(s) = s.to_str() {
                    names.insert(s.to_string());
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify_debouncer_mini::DebouncedEventKind;

    #[test]
    fn deployments_affected_extracts_top_level_name() {
        let base = PathBuf::from("/var/perch/deployments");
        let events = vec![
            DebouncedEvent {
                path: base.join("landing/static/index.html"),
                kind: DebouncedEventKind::Any,
            },
            DebouncedEvent {
                path: base.join("landing/handlers/contact.ts"),
                kind: DebouncedEventKind::Any,
            },
            DebouncedEvent {
                path: base.join("chirp/perch.toml"),
                kind: DebouncedEventKind::Any,
            },
        ];
        let names = deployments_affected(&events, &base);
        assert_eq!(names.len(), 2);
        assert!(names.contains("landing"));
        assert!(names.contains("chirp"));
    }

    #[test]
    fn deployments_affected_ignores_events_outside_dir() {
        let base = PathBuf::from("/var/perch/deployments");
        let events = vec![DebouncedEvent {
            path: PathBuf::from("/etc/motd"),
            kind: DebouncedEventKind::Any,
        }];
        let names = deployments_affected(&events, &base);
        assert!(names.is_empty());
    }
}
