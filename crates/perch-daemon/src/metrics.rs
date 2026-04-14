//! Prometheus metrics at `/_perch/metrics`.
//!
//! Exposes counters and gauges for the daemon's operational health:
//! - `perch_deployments_total` — number of loaded deployments
//! - `perch_requests_total` — HTTP requests by deployment, method, status
//! - `perch_request_duration_seconds` — request latency histogram
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
use std::sync::OnceLock;

/// Global Prometheus handle — initialized once at daemon startup.
static PROM_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

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
                .header(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "metrics not initialized").into_response(),
    }
}

/// Record an HTTP request. Called from the listener's dispatch path.
pub fn record_request(deployment: &str, method: &str, status: u16, duration_secs: f64) {
    let labels = [
        ("deployment", deployment.to_string()),
        ("method", method.to_string()),
        ("status", status.to_string()),
    ];
    metrics::counter!("perch_requests_total", &labels).increment(1);
    metrics::histogram!("perch_request_duration_seconds", &labels).record(duration_secs);
}

/// Update the deployment gauge.
pub fn set_deployment_count(count: usize) {
    metrics::gauge!("perch_deployments_total").set(count as f64);
}
