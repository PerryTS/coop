//! Queue poller — processes messages from a Postgres-backed job queue.
//!
//! Each `[[queues]]` block in perch.toml starts a background tokio task
//! that polls Postgres using `SELECT ... FOR UPDATE SKIP LOCKED`. When a
//! message is claimed, it's dispatched to the handler. On success the
//! message is deleted; on failure it's retried with exponential backoff
//! up to `max_retries`, then moved to the dead-letter queue.
//!
//! The queue table schema (`_perch_queue`) lives in the deployment's
//! Postgres schema and is created by the daemon's migration runner.
//!
//! For v0 this is the scaffolding + the poll loop structure. Actual
//! Postgres polling requires Phase B (pg parameterized queries in Perry)
//! and a running Postgres instance.

use crate::host::DeploymentHost;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct QueueConfig {
    pub name: String,
    pub concurrency: u32,
    pub max_retries: u32,
}

/// Start background queue poller tasks. Returns join handles.
pub fn start_queue_pollers(
    queues: Vec<QueueConfig>,
    host: Arc<DeploymentHost>,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();

    for q in queues {
        for worker_idx in 0..q.concurrency {
            let q = q.clone();
            let host = host.clone();

            let handle = tokio::spawn(async move {
                info!(
                    queue = %q.name,
                    worker = worker_idx,
                    max_retries = q.max_retries,
                    "queue poller started (Postgres polling is scaffolded — \
                     connect Postgres in runtime.toml to enable)"
                );

                // Poll loop: in production this would be:
                //   1. BEGIN
                //   2. SELECT id, payload, attempts FROM _perch_queue
                //      WHERE queue_name = $1 AND visible_at <= now()
                //      ORDER BY created_at
                //      FOR UPDATE SKIP LOCKED
                //      LIMIT 1
                //   3. If no row: COMMIT, sleep 1s, retry
                //   4. If row: deliver to handler
                //   5. On success: DELETE FROM _perch_queue WHERE id = $1; COMMIT
                //   6. On failure (attempts < max_retries):
                //      UPDATE _perch_queue SET attempts = attempts + 1,
                //        visible_at = now() + interval '...'
                //      WHERE id = $1; COMMIT
                //   7. On failure (attempts >= max_retries):
                //      Move to _perch_queue_dlq; DELETE from _perch_queue; COMMIT

                loop {
                    // Scaffold: just sleep. Real Postgres polling lands with Phase B.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    debug!(
                        queue = %q.name,
                        worker = worker_idx,
                        "queue poll tick (no-op without Postgres)"
                    );
                }
            });

            handles.push(handle);
        }
    }

    handles
}
