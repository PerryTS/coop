//! Admin UI — server-rendered HTML at `/_perch/admin` with htmx.
//!
//! Pages per the spec:
//! - Dashboard: list of deployments, status, key metrics
//! - Deployment detail: per-deployment CDN status, DNS instructions
//!
//! No React, no build step, no JS framework. Server-rendered HTML with
//! htmx for partial refreshes. "Ugly is fine; present is the win."

use crate::config::RuntimeConfig;
use crate::deployments::DeploymentSupervisor;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AdminState {
    pub supervisor: Arc<DeploymentSupervisor>,
    pub runtime_cfg: Arc<RuntimeConfig>,
}

/// Build the admin sub-router. Mounted under `/_perch/admin`.
pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/deployments/{name}", get(deployment_detail))
}

async fn dashboard(State(state): State<AdminState>) -> Response {
    let router = state.supervisor.current_router().await;
    let bunny_enabled = state.runtime_cfg.cdn.bunny.is_some();
    let tls_mode = format!("{:?}", state.runtime_cfg.tls.mode);

    let mut deployments = Vec::new();
    for d in router.all() {
        let cdn_status = if bunny_enabled {
            let zone_name = format!("perch-{}", d.name.0);
            format!("Bunny: {}.b-cdn.net", zone_name)
        } else {
            "No CDN".to_string()
        };

        deployments.push(DeploymentRow {
            name: d.name.0.clone(),
            hostnames: d.hostnames.join(", "),
            handler_count: d.handlers.len(),
            static_count: d.static_blocks.len(),
            cdn_status,
        });
    }

    let html = render_dashboard(&DashboardData {
        deployment_count: deployments.len(),
        tls_mode,
        bunny_enabled,
        deployments,
    });

    Html(html).into_response()
}

async fn deployment_detail(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    let router = state.supervisor.current_router().await;
    match router.get(&name) {
        Some(d) => {
            let bunny_enabled = state.runtime_cfg.cdn.bunny.is_some();
            let cdn_url = if bunny_enabled {
                Some(format!("perch-{}.b-cdn.net", d.name.0))
            } else {
                None
            };

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
                cdn_url,
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
    bunny_enabled: bool,
    deployments: Vec<DeploymentRow>,
}

struct DeploymentRow {
    name: String,
    hostnames: String,
    handler_count: usize,
    static_count: usize,
    cdn_status: String,
}

struct DeploymentDetailData {
    name: String,
    hostnames: Vec<String>,
    handlers: Vec<HandlerRow>,
    static_blocks: Vec<StaticRow>,
    cdn_url: Option<String>,
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
                <td><a href="/_perch/admin/deployments/{name}">{name}</a></td>
                <td>{hostnames}</td>
                <td>{handlers}</td>
                <td>{statics}</td>
                <td>{cdn}</td>
            </tr>"#,
            name = d.name,
            hostnames = d.hostnames,
            handlers = d.handler_count,
            statics = d.static_count,
            cdn = d.cdn_status,
        ));
    }

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Perch Admin</title>
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
    <h1>Perch Admin</h1>

    <div>
        <span class="stat"><strong>{count}</strong> deployments</span>
        <span class="stat"><strong>{tls}</strong> TLS</span>
        <span class="stat"><strong>{bunny}</strong> CDN</span>
    </div>

    <h2>Deployments</h2>
    <table>
        <thead>
            <tr>
                <th>Name</th>
                <th>Hostnames</th>
                <th>Handlers</th>
                <th>Static</th>
                <th>CDN</th>
            </tr>
        </thead>
        <tbody>
            {rows}
        </tbody>
    </table>

    <p style="margin-top: 2rem; color: #888; font-size: 0.85rem;">
        Perch v{version} &middot;
        <a href="/_perch/metrics">Prometheus metrics</a>
    </p>
</body>
</html>"#,
        count = data.deployment_count,
        tls = data.tls_mode,
        bunny = if data.bunny_enabled { "Bunny" } else { "None" },
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

    let cdn_html = match &data.cdn_url {
        Some(url) => format!(
            r#"<h3>CDN</h3>
            <p>Bunny Pull Zone: <code>{url}</code></p>
            <p>Point each hostname CNAME to <code>{url}</code> to activate edge caching.</p>"#
        ),
        None => "<h3>CDN</h3><p>Not configured</p>".to_string(),
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{name} — Perch Admin</title>
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
    <p><a href="/_perch/admin">&larr; Dashboard</a></p>
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

    {cdn_html}
</body>
</html>"#,
        name = data.name,
        hostnames = hostnames_html,
        handler_rows = handler_rows,
        static_rows = static_rows,
        cdn_html = cdn_html,
    )
}
