//! Admin UI — server-rendered HTML at `/_coop/admin` with htmx.
//!
//! Pages per the spec:
//! - Dashboard: list of deployments, status, key metrics
//!
//! No React, no build step, no JS framework. Server-rendered HTML with
//! htmx for partial refreshes. "Ugly is fine; present is the win."

use crate::config::RuntimeConfig;
use crate::deployments::DeploymentSupervisor;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use base64::Engine;
use coop_app_host::queue_store::{DeadLetterReplayOutcome, DeadLetterSummary};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Clone)]
pub struct AdminState {
    pub supervisor: Arc<DeploymentSupervisor>,
    pub runtime_cfg: Arc<RuntimeConfig>,
}

/// Build the admin sub-router. Mounted under `/_coop/admin`.
pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/deployments/:name", get(deployment_detail))
        .route("/deployments/:name/health", get(deployment_health))
        .route("/deployments/:name/memory", get(deployment_memory))
        .route("/deployments/:name/artifacts", get(deployment_artifacts))
        .route("/deployments/:name/reload", post(reload_deployment))
        .route(
            "/deployments/:name/rollback/:package",
            post(rollback_deployment),
        )
        .route(
            "/deployments/:name/queues/:queue/dead-letters",
            get(list_dead_letters),
        )
        .route(
            "/deployments/:name/queues/:queue/dead-letters/:id/replay",
            post(replay_dead_letter),
        )
        .route(
            "/deployments/:name/queues/:queue/dead-letters/:id",
            delete(purge_dead_letter),
        )
}

async fn deployment_health(State(state): State<AdminState>, Path(name): Path<String>) -> Response {
    match state.supervisor.activation_status(&name).await {
        Some(status) => Json(status).into_response(),
        None => (StatusCode::NOT_FOUND, "Deployment not found").into_response(),
    }
}

async fn deployment_memory(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin(&state.runtime_cfg, &headers) {
        return response;
    }
    match state.supervisor.memory_status(&name).await {
        Ok(Some(status)) => Json(status).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "Deployment not found").into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("reading deployment memory status: {error:#}"),
        )
            .into_response(),
    }
}

async fn reload_deployment(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin(&state.runtime_cfg, &headers) {
        return response;
    }
    if headers
        .get("x-coop-confirm")
        .and_then(|value| value.to_str().ok())
        != Some("reload")
    {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            "reload requires X-Coop-Confirm: reload",
        )
            .into_response();
    }
    match state.supervisor.load_deployment(&name).await {
        Ok(()) => Json(serde_json::json!({
            "deployment": name,
            "status": "activated"
        }))
        .into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            format!("deployment reload failed: {error:#}"),
        )
            .into_response(),
    }
}

async fn deployment_artifacts(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    match state.supervisor.artifact_status(&name).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reading deployment artifact state: {error:#}"),
        )
            .into_response(),
    }
}

async fn rollback_deployment(
    State(state): State<AdminState>,
    Path((name, package)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin(&state.runtime_cfg, &headers) {
        return response;
    }
    if headers
        .get("x-coop-confirm")
        .and_then(|value| value.to_str().ok())
        != Some("rollback")
    {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            "rollback requires X-Coop-Confirm: rollback",
        )
            .into_response();
    }

    match state.supervisor.rollback(&name, &package).await {
        Ok(()) => Json(serde_json::json!({
            "deployment": name,
            "active_package": package,
            "status": "activated"
        }))
        .into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            format!("rollback activation failed: {error:#}"),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct DeadLetterQuery {
    #[serde(default = "default_dead_letter_limit")]
    limit: u32,
    before_failed_at_ms: Option<i64>,
    before_id: Option<String>,
}

fn default_dead_letter_limit() -> u32 {
    50
}

#[derive(Debug, Serialize)]
struct DeadLetterPage {
    deployment: String,
    queue: String,
    entries: Vec<DeadLetterEntry>,
    next_before_failed_at_ms: Option<i64>,
    next_before_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeadLetterEntry {
    id: String,
    payload_bytes: u64,
    attempts: u32,
    max_attempts: u32,
    created_at_ms: i64,
    failed_at_ms: i64,
    final_error: String,
}

impl From<DeadLetterSummary> for DeadLetterEntry {
    fn from(value: DeadLetterSummary) -> Self {
        Self {
            id: value.id,
            payload_bytes: value.payload_bytes,
            attempts: value.attempts,
            max_attempts: value.max_attempts,
            created_at_ms: value.created_at_ms,
            failed_at_ms: value.failed_at_ms,
            final_error: value.final_error,
        }
    }
}

async fn list_dead_letters(
    State(state): State<AdminState>,
    Path((deployment, queue)): Path<(String, String)>,
    Query(query): Query<DeadLetterQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin(&state.runtime_cfg, &headers) {
        return response;
    }
    if !(1..=200).contains(&query.limit) {
        return (
            StatusCode::BAD_REQUEST,
            "dead-letter limit must be between 1 and 200",
        )
            .into_response();
    }
    let cursor = match (query.before_failed_at_ms, query.before_id.as_deref()) {
        (None, None) => None,
        (Some(failed_at_ms), Some(id)) => Some((failed_at_ms, id)),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "dead-letter cursor requires before_failed_at_ms and before_id",
            )
                .into_response();
        }
    };
    let Some(store) = state.supervisor.durable_queue_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable queue service is not configured",
        )
            .into_response();
    };
    match store
        .list_dead_letters(&deployment, &queue, query.limit, cursor)
        .await
    {
        Ok(dead_letters) => {
            let next = dead_letters
                .last()
                .map(|entry| (entry.failed_at_ms, entry.id.clone()));
            let entries = dead_letters
                .into_iter()
                .map(DeadLetterEntry::from)
                .collect();
            crate::metrics::record_queue_operator_action(
                &deployment,
                &queue,
                "inspect_dlq",
                "success",
            );
            info!(%deployment, %queue, "durable queue DLQ inspected by administrator");
            Json(DeadLetterPage {
                deployment,
                queue,
                entries,
                next_before_failed_at_ms: next.as_ref().map(|value| value.0),
                next_before_id: next.map(|value| value.1),
            })
            .into_response()
        }
        Err(error) => {
            crate::metrics::record_queue_operator_action(
                &deployment,
                &queue,
                "inspect_dlq",
                "error",
            );
            warn!(%deployment, %queue, ?error, "durable queue DLQ inspection failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("listing dead letters failed: {error:#}"),
            )
                .into_response()
        }
    }
}

async fn replay_dead_letter(
    State(state): State<AdminState>,
    Path((deployment, queue, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin(&state.runtime_cfg, &headers) {
        return response;
    }
    if headers
        .get("x-coop-confirm")
        .and_then(|value| value.to_str().ok())
        != Some("replay-dead-letter")
    {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            "replay requires X-Coop-Confirm: replay-dead-letter",
        )
            .into_response();
    }
    let Some(store) = state.supervisor.durable_queue_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable queue service is not configured",
        )
            .into_response();
    };
    match store.replay_dead_letter(&deployment, &queue, &id).await {
        Ok(DeadLetterReplayOutcome::Replayed) => {
            record_dlq_mutation(&deployment, &queue, &id, "replay", "success");
            Json(serde_json::json!({
                "deployment": deployment,
                "queue": queue,
                "id": id,
                "status": "replayed"
            }))
            .into_response()
        }
        Ok(DeadLetterReplayOutcome::NotFound) => {
            record_dlq_mutation(&deployment, &queue, &id, "replay", "not_found");
            (StatusCode::NOT_FOUND, "dead letter not found").into_response()
        }
        Ok(DeadLetterReplayOutcome::AlreadyLive) => {
            record_dlq_mutation(&deployment, &queue, &id, "replay", "already_live");
            (
                StatusCode::CONFLICT,
                "message ID is already present in the live queue",
            )
                .into_response()
        }
        Err(error) => {
            record_dlq_mutation(&deployment, &queue, &id, "replay", "error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("dead-letter replay failed: {error:#}"),
            )
                .into_response()
        }
    }
}

async fn purge_dead_letter(
    State(state): State<AdminState>,
    Path((deployment, queue, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin(&state.runtime_cfg, &headers) {
        return response;
    }
    if headers
        .get("x-coop-confirm")
        .and_then(|value| value.to_str().ok())
        != Some("purge-dead-letter")
    {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            "purge requires X-Coop-Confirm: purge-dead-letter",
        )
            .into_response();
    }
    let Some(store) = state.supervisor.durable_queue_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable queue service is not configured",
        )
            .into_response();
    };
    match store.purge_dead_letter(&deployment, &queue, &id).await {
        Ok(true) => {
            record_dlq_mutation(&deployment, &queue, &id, "purge", "success");
            Json(serde_json::json!({
                "deployment": deployment,
                "queue": queue,
                "id": id,
                "status": "purged"
            }))
            .into_response()
        }
        Ok(false) => {
            record_dlq_mutation(&deployment, &queue, &id, "purge", "not_found");
            (StatusCode::NOT_FOUND, "dead letter not found").into_response()
        }
        Err(error) => {
            record_dlq_mutation(&deployment, &queue, &id, "purge", "error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("dead-letter purge failed: {error:#}"),
            )
                .into_response()
        }
    }
}

fn record_dlq_mutation(deployment: &str, queue: &str, id: &str, action: &str, outcome: &str) {
    crate::metrics::record_queue_operator_action(deployment, queue, action, outcome);
    info!(
        deployment,
        queue,
        message_id = id,
        action,
        outcome,
        "durable queue DLQ operator action"
    );
}

fn authorize_admin(config: &RuntimeConfig, headers: &HeaderMap) -> Result<(), Response> {
    let Some(expected_hash) = config.admin.password_hash.as_deref() else {
        return Err((
            StatusCode::FORBIDDEN,
            "admin mutations are disabled until admin.password_hash is configured",
        )
            .into_response());
    };
    let password = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .and_then(|decoded| String::from_utf8(decoded).ok())
        .and_then(|credentials| {
            let (username, password) = credentials.split_once(':')?;
            (username == "coop").then(|| password.to_string())
        });
    if password.is_some_and(|password| bcrypt::verify(password, expected_hash).unwrap_or(false)) {
        Ok(())
    } else {
        Err(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Basic realm=\"Coop admin\"")
            .body(axum::body::Body::from("invalid admin credentials"))
            .unwrap())
    }
}

async fn dashboard(State(state): State<AdminState>) -> Response {
    let router = state.supervisor.current_router();
    let tls_mode = format!("{:?}", state.runtime_cfg.tls.mode);

    let mut deployments = Vec::new();
    for d in router.all() {
        deployments.push(DeploymentRow {
            name: d.name.0.clone(),
            hostnames: d.hostnames.join(", "),
            handler_count: d.handlers.len(),
            static_count: d.static_blocks.len(),
        });
    }

    let html = render_dashboard(&DashboardData {
        deployment_count: deployments.len(),
        tls_mode,
        deployments,
    });

    Html(html).into_response()
}

async fn deployment_detail(State(state): State<AdminState>, Path(name): Path<String>) -> Response {
    let router = state.supervisor.current_router();
    match router.get(&name) {
        Some(d) => {
            let html = render_deployment_detail(&DeploymentDetailData {
                name: d.name.0.clone(),
                hostnames: d.hostnames.clone(),
                handlers: d
                    .handlers
                    .iter()
                    .map(|(h, tool)| HandlerRow {
                        method: h.method.clone().unwrap_or_else(|| "ANY".to_string()),
                        path: h.path.clone(),
                        file: h.file.display().to_string(),
                        tool: tool.clone(),
                    })
                    .collect(),
                static_blocks: d
                    .static_blocks
                    .iter()
                    .map(|s| StaticRow {
                        path: s.path.clone(),
                        directory: s.directory.display().to_string(),
                    })
                    .collect(),
            });
            Html(html).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Deployment not found").into_response(),
    }
}

// ── Template data structures ──

struct DashboardData {
    deployment_count: usize,
    tls_mode: String,
    deployments: Vec<DeploymentRow>,
}

struct DeploymentRow {
    name: String,
    hostnames: String,
    handler_count: usize,
    static_count: usize,
}

struct DeploymentDetailData {
    name: String,
    hostnames: Vec<String>,
    handlers: Vec<HandlerRow>,
    static_blocks: Vec<StaticRow>,
}

struct HandlerRow {
    method: String,
    path: String,
    file: String,
    tool: String,
}

struct StaticRow {
    path: String,
    directory: String,
}

// ── Inline HTML templates (no external files needed) ──
//
// Using format! strings instead of askama templates for the MVP — keeps
// everything in one file, no template directory to manage. askama is in
// Cargo.toml for when we want to split into proper .html files later.

fn render_dashboard(data: &DashboardData) -> String {
    let mut rows = String::new();
    for d in &data.deployments {
        rows.push_str(&format!(
            r#"<tr>
                <td><a href="/_coop/admin/deployments/{name}">{name}</a></td>
                <td>{hostnames}</td>
                <td>{handlers}</td>
                <td>{statics}</td>
            </tr>"#,
            name = d.name,
            hostnames = d.hostnames,
            handlers = d.handler_count,
            statics = d.static_count,
        ));
    }

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Coop Admin</title>
    <script src="https://unpkg.com/htmx.org@2.0.4"></script>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; margin: 2rem; color: #333; }}
        h1 {{ color: #1a1a2e; }}
        table {{ border-collapse: collapse; width: 100%; margin-top: 1rem; }}
        th, td {{ border: 1px solid #ddd; padding: 0.5rem 1rem; text-align: left; }}
        th {{ background: #f5f5f5; }}
        tr:hover {{ background: #fafafa; }}
        a {{ color: #2563eb; text-decoration: none; }}
        a:hover {{ text-decoration: underline; }}
        .stat {{ display: inline-block; background: #f0f0f0; padding: 0.5rem 1rem; border-radius: 4px; margin-right: 1rem; margin-bottom: 0.5rem; }}
        .stat strong {{ display: block; font-size: 1.5rem; }}
    </style>
</head>
<body>
    <h1>Coop Admin</h1>

    <div>
        <span class="stat"><strong>{count}</strong> deployments</span>
        <span class="stat"><strong>{tls}</strong> TLS</span>
    </div>

    <h2>Deployments</h2>
    <table>
        <thead>
            <tr>
                <th>Name</th>
                <th>Hostnames</th>
                <th>Handlers</th>
                <th>Static</th>
            </tr>
        </thead>
        <tbody>
            {rows}
        </tbody>
    </table>

    <p style="margin-top: 2rem; color: #888; font-size: 0.85rem;">
        Coop v{version} &middot;
        <a href="/_coop/metrics">Prometheus metrics</a>
    </p>
</body>
</html>"#,
        count = data.deployment_count,
        tls = data.tls_mode,
        rows = rows,
        version = env!("CARGO_PKG_VERSION"),
    )
}

fn render_deployment_detail(data: &DeploymentDetailData) -> String {
    let hostnames_html = data
        .hostnames
        .iter()
        .map(|h| format!("<li><code>{}</code></li>", h))
        .collect::<Vec<_>>()
        .join("\n");

    let mut handler_rows = String::new();
    for h in &data.handlers {
        handler_rows.push_str(&format!(
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td><code>{}</code></td></tr>",
            h.method, h.path, h.file, h.tool
        ));
    }

    let mut static_rows = String::new();
    for s in &data.static_blocks {
        static_rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td></tr>",
            s.path, s.directory
        ));
    }

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{name} — Coop Admin</title>
    <script src="https://unpkg.com/htmx.org@2.0.4"></script>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; margin: 2rem; color: #333; }}
        h1 {{ color: #1a1a2e; }}
        table {{ border-collapse: collapse; width: 100%; margin-top: 0.5rem; }}
        th, td {{ border: 1px solid #ddd; padding: 0.5rem 1rem; text-align: left; }}
        th {{ background: #f5f5f5; }}
        a {{ color: #2563eb; text-decoration: none; }}
        a:hover {{ text-decoration: underline; }}
        code {{ background: #f5f5f5; padding: 2px 6px; border-radius: 3px; font-size: 0.9em; }}
    </style>
</head>
<body>
    <p><a href="/_coop/admin">&larr; Dashboard</a></p>
    <h1>{name}</h1>

    <h3>Hostnames</h3>
    <ul>{hostnames}</ul>

    <h3>Handlers</h3>
    <table>
        <thead><tr><th>Method</th><th>Path</th><th>File</th><th>Tool</th></tr></thead>
        <tbody>{handler_rows}</tbody>
    </table>

    <h3>Static</h3>
    <table>
        <thead><tr><th>Path</th><th>Directory</th></tr></thead>
        <tbody>{static_rows}</tbody>
    </table>

</body>
</html>"#,
        name = data.name,
        hostnames = hostnames_html,
        handler_rows = handler_rows,
        static_rows = static_rows,
    )
}
