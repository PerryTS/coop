//! Prometheus metrics at `/_coop/metrics`.
//!
//! Exposes counters and gauges for the daemon's operational health:
//! - `coop_deployments_total` — number of loaded deployments
//! - `coop_requests_total` — HTTP requests by deployment, method, status
//! - `coop_request_duration_seconds` — request latency histogram
//!
//! The metrics crate + prometheus exporter handle the wire format; we
//! just describe the metrics and record them in the dispatch path.

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// Global Prometheus handle — initialized once at daemon startup.
static PROM_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
static REQUEST_METRICS: OnceLock<RwLock<RequestMetricCache>> = OnceLock::new();

type RequestMetricCache = HashMap<String, HashMap<(&'static str, u16), RequestMetricHandles>>;

struct RequestMetricHandles {
    requests: metrics::Counter,
    duration: metrics::Histogram,
}

/// Initialize the Prometheus exporter. Call once from `main()`.
pub fn init() {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder");
    PROM_HANDLE.set(handle).ok();
}

/// Build a sub-router that serves the Prometheus scrape endpoint.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/", get(scrape))
}

async fn scrape() -> Response {
    match PROM_HANDLE.get() {
        Some(handle) => {
            let body = handle.render();
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    "text/plain; version=0.0.4; charset=utf-8",
                )
                .body(Body::from(body))
                .unwrap()
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "metrics not initialized").into_response(),
    }
}

/// Record an HTTP request. Called from the listener's dispatch path.
pub fn record_request(deployment: &str, method: &str, status: u16, duration_secs: f64) {
    let method = http_method_label(method);
    let cache = REQUEST_METRICS.get_or_init(|| RwLock::new(HashMap::new()));
    {
        let cache = cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handles) = cache
            .get(deployment)
            .and_then(|deployment| deployment.get(&(method, status)))
        {
            handles.requests.increment(1);
            handles.duration.record(duration_secs);
            return;
        }
    }

    // Register each bounded deployment/method/status series once. Reusing the
    // handles avoids two registry lookups and three label allocations on every
    // request. Concurrent first observations are harmless: the recorder
    // returns handles for the same metric key and the write-side entry picks
    // one of them before recording.
    let labels = [
        ("deployment", deployment.to_string()),
        ("method", method.to_string()),
        ("status", status.to_string()),
    ];
    let handles = RequestMetricHandles {
        requests: metrics::counter!("coop_requests_total", &labels),
        duration: metrics::histogram!("coop_request_duration_seconds", &labels),
    };
    let mut cache = cache
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let handles = cache
        .entry(deployment.to_string())
        .or_default()
        .entry((method, status))
        .or_insert(handles);
    handles.requests.increment(1);
    handles.duration.record(duration_secs);
}

/// HTTP methods originate on the public network and therefore cannot be used
/// directly as Prometheus label values. Keep the standard finite vocabulary
/// and collapse extension methods into one series.
fn http_method_label(method: &str) -> &'static str {
    if method.eq_ignore_ascii_case("GET") {
        "GET"
    } else if method.eq_ignore_ascii_case("HEAD") {
        "HEAD"
    } else if method.eq_ignore_ascii_case("POST") {
        "POST"
    } else if method.eq_ignore_ascii_case("PUT") {
        "PUT"
    } else if method.eq_ignore_ascii_case("DELETE") {
        "DELETE"
    } else if method.eq_ignore_ascii_case("CONNECT") {
        "CONNECT"
    } else if method.eq_ignore_ascii_case("OPTIONS") {
        "OPTIONS"
    } else if method.eq_ignore_ascii_case("TRACE") {
        "TRACE"
    } else if method.eq_ignore_ascii_case("PATCH") {
        "PATCH"
    } else {
        "OTHER"
    }
}

/// Update the deployment gauge.
pub fn set_deployment_count(count: usize) {
    metrics::gauge!("coop_deployments_total").set(count as f64);
}

/// Export the effective failure-domain policy without a mutable class label,
/// which would leave stale time series after an in-place policy change.
pub fn set_deployment_isolation(deployment: &str, process_isolated: bool, inherited: bool) {
    metrics::gauge!(
        "coop_deployment_process_isolated",
        "deployment" => deployment.to_string()
    )
    .set(if process_isolated { 1.0 } else { 0.0 });
    metrics::gauge!(
        "coop_deployment_isolation_inherited",
        "deployment" => deployment.to_string()
    )
    .set(if inherited { 1.0 } else { 0.0 });
}

pub fn set_deployment_shard(deployment: &str, shard: Option<(usize, u64)>) {
    metrics::gauge!(
        "coop_deployment_sharded",
        "deployment" => deployment.to_string()
    )
    .set(if shard.is_some() { 1.0 } else { 0.0 });
    metrics::gauge!(
        "coop_deployment_shard_slot",
        "deployment" => deployment.to_string()
    )
    .set(shard.map_or(-1.0, |(slot, _)| slot as f64));
    metrics::gauge!(
        "coop_deployment_shard_generation",
        "deployment" => deployment.to_string()
    )
    .set(shard.map_or(0.0, |(_, generation)| generation as f64));
}

pub fn record_shard_start(slot: usize, outcome: &str) {
    metrics::counter!(
        "coop_worker_shard_starts_total",
        "slot" => slot.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

pub fn record_shard_failure(slot: usize, reason: &str) {
    metrics::counter!(
        "coop_worker_shard_failures_total",
        "slot" => slot.to_string(),
        "reason" => reason.to_string()
    )
    .increment(1);
}

pub fn set_shard_resident_deployments(slot: usize, count: usize) {
    metrics::gauge!(
        "coop_worker_shard_resident_deployments",
        "slot" => slot.to_string()
    )
    .set(count as f64);
}

pub fn record_worker_cgroup(deployment: &str, outcome: &str) {
    metrics::counter!(
        "coop_worker_cgroup_preparations_total",
        "deployment" => deployment.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

pub fn set_worker_cgroup_stats(deployment: &str, stats: &crate::cgroup::WorkerCgroupStats) {
    for (name, value) in [
        (
            "coop_worker_cgroup_memory_current_bytes",
            stats.memory_current_bytes,
        ),
        (
            "coop_worker_cgroup_memory_peak_bytes",
            stats.memory_peak_bytes,
        ),
        ("coop_worker_cgroup_pids_current", stats.pids_current),
        ("coop_worker_cgroup_cpu_usage_usec", stats.cpu_usage_usec),
        (
            "coop_worker_cgroup_memory_oom_events",
            stats.memory_oom_events,
        ),
        (
            "coop_worker_cgroup_memory_oom_kill_events",
            stats.memory_oom_kill_events,
        ),
    ] {
        if let Some(value) = value {
            metrics::gauge!(name, "deployment" => deployment.to_string()).set(value as f64);
        }
    }
}

pub fn record_worker_restart(deployment: &str, reason: &str, outcome: &str) {
    metrics::counter!(
        "coop_worker_restarts_total",
        "deployment" => deployment.to_string(),
        "reason" => reason.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

pub fn set_worker_restart_backoff(deployment: &str, seconds: f64) {
    metrics::gauge!(
        "coop_worker_restart_backoff_seconds",
        "deployment" => deployment.to_string()
    )
    .set(seconds);
}

pub fn set_worker_transport(deployment: &str, backlog: usize, in_flight: usize) {
    metrics::gauge!(
        "coop_worker_transport_backlog",
        "deployment" => deployment.to_string()
    )
    .set(backlog as f64);
    metrics::gauge!(
        "coop_worker_transport_in_flight",
        "deployment" => deployment.to_string()
    )
    .set(in_flight as f64);
}

pub fn record_worker_transport_queue_wait(
    deployment: &str,
    entrypoint: &str,
    outcome: &str,
    duration_secs: f64,
) {
    metrics::histogram!(
        "coop_worker_transport_queue_wait_seconds",
        "deployment" => deployment.to_string(),
        "entrypoint" => entrypoint.to_string(),
        "outcome" => outcome.to_string()
    )
    .record(duration_secs);
}

pub fn record_worker_transport_round_trip(
    deployment: &str,
    entrypoint: &str,
    outcome: &str,
    duration_secs: f64,
) {
    metrics::histogram!(
        "coop_worker_transport_round_trip_seconds",
        "deployment" => deployment.to_string(),
        "entrypoint" => entrypoint.to_string(),
        "outcome" => outcome.to_string()
    )
    .record(duration_secs);
}

/// Count complete length-prefixed protocol frames. The four-byte frame header
/// is included so this represents bytes written to or read from the socket,
/// not only the JSON payload size. A partial frame that fails mid-I/O is not
/// guessed; its failure is represented by the round-trip/poison metrics.
pub fn record_worker_transport_bytes(
    deployment: &str,
    entrypoint: &str,
    direction: &str,
    bytes: usize,
) {
    if bytes == 0 {
        return;
    }
    metrics::counter!(
        "coop_worker_transport_bytes_total",
        "deployment" => deployment.to_string(),
        "entrypoint" => entrypoint.to_string(),
        "direction" => direction.to_string()
    )
    .increment(bytes as u64);
}

pub fn record_worker_transport_cancellation(deployment: &str, entrypoint: &str, phase: &str) {
    metrics::counter!(
        "coop_worker_transport_cancellations_total",
        "deployment" => deployment.to_string(),
        "entrypoint" => entrypoint.to_string(),
        "phase" => phase.to_string(),
        "cause" => "future_dropped"
    )
    .increment(1);
}

pub fn record_worker_transport_poisoned(deployment: &str, cause: &str) {
    metrics::counter!(
        "coop_worker_transport_poisoned_total",
        "deployment" => deployment.to_string(),
        "cause" => cause.to_string()
    )
    .increment(1);
}

pub fn record_worker_transport_drain(deployment: &str, outcome: &str, duration_secs: f64) {
    metrics::histogram!(
        "coop_worker_transport_drain_duration_seconds",
        "deployment" => deployment.to_string(),
        "outcome" => outcome.to_string()
    )
    .record(duration_secs);
}

/// Record the current number of admitted invocations across HTTP, cron, and
/// queue work for one deployment.
pub fn record_invocation_rejected(deployment: &str, entrypoint: &str, reason: &str) {
    metrics::counter!(
        "coop_invocation_rejections_total",
        "deployment" => deployment.to_string(),
        "entrypoint" => entrypoint.to_string(),
        "reason" => reason.to_string()
    )
    .increment(1);
}

pub fn record_invocation_timeout(deployment: &str, entrypoint: &str) {
    metrics::counter!(
        "coop_invocation_timeouts_total",
        "deployment" => deployment.to_string(),
        "entrypoint" => entrypoint.to_string()
    )
    .increment(1);
}

pub fn set_executor_queue(deployment: &str, depth: usize, capacity: usize) {
    metrics::gauge!(
        "coop_executor_queue_depth",
        "deployment" => deployment.to_string()
    )
    .set(depth as f64);
    metrics::gauge!(
        "coop_executor_queue_capacity",
        "deployment" => deployment.to_string()
    )
    .set(capacity as f64);
}

pub fn set_application_arena(deployment: &str, live_bytes: u64, reserved_bytes: u64) {
    metrics::gauge!(
        "coop_application_arena_live_bytes",
        "deployment" => deployment.to_string()
    )
    .set(live_bytes as f64);
    metrics::gauge!(
        "coop_application_arena_reserved_bytes",
        "deployment" => deployment.to_string()
    )
    .set(reserved_bytes as f64);
}

pub fn set_artifact_inventory(deployment: &str, packages: usize, retained_bytes: u64) {
    metrics::gauge!(
        "coop_artifact_packages",
        "deployment" => deployment.to_string()
    )
    .set(packages as f64);
    metrics::gauge!(
        "coop_artifact_retained_bytes",
        "deployment" => deployment.to_string()
    )
    .set(retained_bytes as f64);
}

pub fn record_artifact_collection(deployment: &str, outcome: &str, removed: usize) {
    metrics::counter!(
        "coop_artifact_collections_total",
        "deployment" => deployment.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
    if removed > 0 {
        metrics::counter!(
            "coop_artifact_packages_collected_total",
            "deployment" => deployment.to_string()
        )
        .increment(removed as u64);
    }
}

pub fn record_artifact_reconciliation(outcome: &str, removed: usize) {
    metrics::counter!(
        "coop_artifact_reconciliations_total",
        "outcome" => outcome.to_string()
    )
    .increment(1);
    if removed > 0 {
        metrics::counter!("coop_artifact_entries_reconciled_total").increment(removed as u64);
    }
}

pub fn record_rollback(deployment: &str, outcome: &str) {
    metrics::counter!(
        "coop_deployment_rollbacks_total",
        "deployment" => deployment.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

pub fn record_activation(deployment: &str, outcome: &str, duration_secs: f64, requests: u16) {
    let labels = [
        ("deployment", deployment.to_string()),
        ("outcome", outcome.to_string()),
    ];
    metrics::counter!("coop_deployment_activations_total", &labels).increment(1);
    metrics::histogram!("coop_deployment_activation_duration_seconds", &labels)
        .record(duration_secs);
    metrics::histogram!("coop_deployment_activation_requests", &labels).record(requests as f64);
    metrics::gauge!(
        "coop_deployment_activation_healthy",
        "deployment" => deployment.to_string()
    )
    .set(if outcome == "failure" { 0.0 } else { 1.0 });
}

pub fn record_runtime_reuse(deployment: &str) {
    metrics::counter!(
        "coop_deployment_runtime_reuses_total",
        "deployment" => deployment.to_string()
    )
    .increment(1);
}

pub fn record_compile_queue_wait(deployment: &str, duration_secs: f64) {
    metrics::histogram!(
        "coop_compile_queue_wait_seconds",
        "deployment" => deployment.to_string()
    )
    .record(duration_secs);
}

pub fn record_compile(
    deployment: &str,
    outcome: &str,
    cache: &str,
    duration_secs: f64,
    peak_rss_bytes: u64,
) {
    let labels = [
        ("deployment", deployment.to_string()),
        ("outcome", outcome.to_string()),
        ("cache", cache.to_string()),
    ];
    metrics::counter!("coop_compiles_total", &labels).increment(1);
    metrics::histogram!("coop_compile_duration_seconds", &labels).record(duration_secs);
    if peak_rss_bytes > 0 {
        metrics::histogram!("coop_compile_peak_rss_bytes", &labels).record(peak_rss_bytes as f64);
    }
}

pub fn record_compile_phase(deployment: &str, phase: &str, outcome: &str, duration_secs: f64) {
    let labels = [
        ("deployment", deployment.to_string()),
        ("phase", phase.to_string()),
        ("outcome", outcome.to_string()),
    ];
    metrics::counter!("coop_compile_phases_total", &labels).increment(1);
    metrics::histogram!("coop_compile_phase_duration_seconds", &labels).record(duration_secs);
}

pub fn record_compile_cache_entries(deployment: &str, layer: &str, outcome: &str, entries: u64) {
    metrics::counter!(
        "coop_compile_cache_entries_total",
        "deployment" => deployment.to_string(),
        "layer" => layer.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(entries);
}

pub fn set_queue_stats(
    deployment: &str,
    queue: &str,
    total_depth: u64,
    visible_depth: u64,
    active_leases: u64,
    oldest_visible_age_seconds: f64,
    dead_letters: u64,
) {
    let labels = [
        ("deployment", deployment.to_string()),
        ("queue", queue.to_string()),
    ];
    metrics::gauge!("coop_queue_depth", &labels).set(total_depth as f64);
    metrics::gauge!("coop_queue_visible_depth", &labels).set(visible_depth as f64);
    metrics::gauge!("coop_queue_active_leases", &labels).set(active_leases as f64);
    metrics::gauge!("coop_queue_oldest_visible_age_seconds", &labels)
        .set(oldest_visible_age_seconds);
    metrics::gauge!("coop_queue_dead_letters", &labels).set(dead_letters as f64);
}

pub fn record_queue_claim(deployment: &str, queue: &str, recovered_expired_lease: bool) {
    metrics::counter!(
        "coop_queue_claims_total",
        "deployment" => deployment.to_string(),
        "queue" => queue.to_string(),
        "recovered_expired_lease" => if recovered_expired_lease { "true" } else { "false" }
    )
    .increment(1);
    if recovered_expired_lease {
        metrics::counter!(
            "coop_queue_expired_leases_total",
            "deployment" => deployment.to_string(),
            "queue" => queue.to_string()
        )
        .increment(1);
    }
}

pub fn record_cron_schedule(deployment: &str, schedule: &str, outcome: &str) {
    metrics::counter!(
        "coop_cron_schedule_events_total",
        "deployment" => deployment.to_string(),
        "schedule" => schedule.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

pub fn record_cron_lateness(deployment: &str, schedule: &str, seconds: f64) {
    metrics::histogram!(
        "coop_cron_lateness_seconds",
        "deployment" => deployment.to_string(),
        "schedule" => schedule.to_string()
    )
    .record(seconds);
}

pub fn record_cron_completion(deployment: &str, schedule: &str, outcome: &str, duration_secs: f64) {
    let labels = [
        ("deployment", deployment.to_string()),
        ("schedule", schedule.to_string()),
        ("outcome", outcome.to_string()),
    ];
    metrics::counter!("coop_cron_invocations_total", &labels).increment(1);
    metrics::histogram!("coop_cron_duration_seconds", &labels).record(duration_secs);
}

pub fn record_queue_delivery(deployment: &str, queue: &str, outcome: &str, duration_secs: f64) {
    let labels = [
        ("deployment", deployment.to_string()),
        ("queue", queue.to_string()),
        ("outcome", outcome.to_string()),
    ];
    metrics::counter!("coop_queue_deliveries_total", &labels).increment(1);
    metrics::histogram!("coop_queue_delivery_duration_seconds", &labels).record(duration_secs);
}

pub fn record_queue_retry(deployment: &str, queue: &str, outcome: &str) {
    metrics::counter!(
        "coop_queue_retries_total",
        "deployment" => deployment.to_string(),
        "queue" => queue.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

pub fn record_queue_deferred(deployment: &str, queue: &str, reason: &str) {
    metrics::counter!(
        "coop_queue_deferrals_total",
        "deployment" => deployment.to_string(),
        "queue" => queue.to_string(),
        "reason" => reason.to_string()
    )
    .increment(1);
}

pub fn record_queue_operator_action(deployment: &str, queue: &str, action: &str, outcome: &str) {
    metrics::counter!(
        "coop_queue_operator_actions_total",
        "deployment" => deployment.to_string(),
        "queue" => queue.to_string(),
        "action" => action.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

pub fn record_queue_store_error(operation: &str) {
    metrics::counter!(
        "coop_queue_store_errors_total",
        "operation" => operation.to_string()
    )
    .increment(1);
}

pub fn set_queue_pool_status(status: &coop_app_host::queue_store::QueuePoolStatus) {
    metrics::gauge!("coop_queue_pool_connections", "state" => "max").set(status.max_size as f64);
    metrics::gauge!("coop_queue_pool_connections", "state" => "open").set(status.size as f64);
    metrics::gauge!("coop_queue_pool_connections", "state" => "available")
        .set(status.available as f64);
    metrics::gauge!("coop_queue_pool_waiters").set(status.waiting as f64);

    let checked_out = status.size.saturating_sub(status.available);
    let utilization = if status.max_size == 0 {
        0.0
    } else {
        checked_out as f64 / status.max_size as f64
    };
    metrics::gauge!("coop_queue_pool_utilization_ratio").set(utilization);
}

pub fn record_queue_dlq_pruned(deployment: &str, queue: &str, removed: u64) {
    if removed > 0 {
        metrics::counter!(
            "coop_queue_dead_letters_pruned_total",
            "deployment" => deployment.to_string(),
            "queue" => queue.to_string()
        )
        .increment(removed);
    }
}

#[cfg(test)]
mod tests {
    use super::http_method_label;

    #[test]
    fn public_http_method_label_has_a_finite_vocabulary() {
        assert_eq!(http_method_label("GET"), "GET");
        assert_eq!(http_method_label("get"), "GET");
        assert_eq!(http_method_label("PROPFIND"), "OTHER");
        assert_eq!(http_method_label("attacker-controlled-method"), "OTHER");
    }

    /// Run alone in release mode so the process-global metrics recorder and
    /// optimizer match the production hot path:
    ///
    /// `cargo test --release -p coop-daemon --bin coop
    /// metrics::tests::request_metric_hot_path_cost -- --ignored --nocapture
    /// --test-threads=1`
    #[test]
    #[ignore = "release-only metric-cost evidence"]
    fn request_metric_hot_path_cost() {
        super::init();
        const WARMUP: usize = 10_000;
        const ITERATIONS: usize = 500_000;
        for _ in 0..WARMUP {
            super::record_request("metric-probe", "GET", 200, 0.001);
        }
        let started = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            super::record_request("metric-probe", "GET", 200, 0.001);
        }
        let elapsed = started.elapsed();
        let nanoseconds_per_request = elapsed.as_nanos() as f64 / ITERATIONS as f64;
        let max_nanoseconds = std::env::var("COOP_METRICS_MAX_NS_PER_REQUEST")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(1_000.0);
        println!(
            "{{\"iterations\":{ITERATIONS},\"elapsed_seconds\":{},\"nanoseconds_per_request\":{nanoseconds_per_request},\"maximum_nanoseconds_per_request\":{max_nanoseconds}}}",
            elapsed.as_secs_f64()
        );
        assert!(
            nanoseconds_per_request <= max_nanoseconds,
            "request metric cost {nanoseconds_per_request:.1} ns exceeded {max_nanoseconds:.1} ns"
        );
    }
}
