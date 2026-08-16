//! Cron scheduler — fires handler functions on schedule.
//!
//! Reads `[[crons]]` blocks from the deployment's coop.toml and starts a
//! tokio task for each. Each task sleeps until the next fire time, then
//! invokes the handler. Uses the `cron` crate for expression parsing.
//!
//! The handler is invoked through the same `DeploymentHost::fire_cron()`
//! path as daemon-initiated cron dispatches, keeping the call model
//! uniform.

use anyhow::Result;
use cron::Schedule;
use coop_app_host::host::DeploymentHost;
use std::str::FromStr;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// A parsed cron entry from coop.toml.
#[derive(Debug, Clone)]
pub struct CronEntry {
    pub schedule: String,
    pub file: String,
}

/// Start background cron tasks for each configured cron expression.
/// Returns join handles so the caller can abort them on shutdown.
pub fn start_crons(entries: Vec<CronEntry>, host: Arc<DeploymentHost>) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();

    for entry in entries {
        // Parse the cron expression. The `cron` crate expects 6 fields
        // (sec min hour day month weekday) but standard cron has 5.
        // Prepend "0 " to convert 5-field to 6-field.
        let expr = if entry.schedule.split_whitespace().count() == 5 {
            format!("0 {}", entry.schedule)
        } else {
            entry.schedule.clone()
        };

        let schedule = match Schedule::from_str(&expr) {
            Ok(s) => s,
            Err(e) => {
                error!(
                    schedule = %entry.schedule,
                    file = %entry.file,
                    error = ?e,
                    "invalid cron expression, skipping"
                );
                continue;
            }
        };

        let file = entry.file.clone();
        let host = host.clone();

        let handle = tokio::spawn(async move {
            info!(schedule = %entry.schedule, file = %file, "cron task started");

            loop {
                // Find the next fire time.
                let now = chrono::Utc::now();
                let next = match schedule.upcoming(chrono::Utc).next() {
                    Some(t) => t,
                    None => {
                        warn!(file = %file, "cron schedule exhausted, stopping");
                        return;
                    }
                };

                let wait = (next - now)
                    .to_std()
                    .unwrap_or(std::time::Duration::from_secs(1));
                debug!(
                    file = %file,
                    next = %next,
                    wait_secs = wait.as_secs(),
                    "sleeping until next cron fire"
                );

                tokio::time::sleep(wait).await;

                let context = coop_host_abi::CronContext {
                    expression: entry.schedule.clone(),
                    scheduled_at_ms: next.timestamp_millis() as u64,
                    dispatched_at_ms: chrono::Utc::now().timestamp_millis() as u64,
                };

                if let Err(e) = host.fire_cron(context).await {
                    error!(
                        file = %file,
                        error = ?e,
                        "cron invocation failed"
                    );
                }
            }
        });

        handles.push(handle);
    }

    handles
}
