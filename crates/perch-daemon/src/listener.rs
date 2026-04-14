//! HTTP listener — accepts incoming requests and routes them through
//! `RouterState` to either a static file or a perch-worker dispatch.
//!
//! For Checkpoint 2 this is a single axum server on `listen_http`. The
//! full three-socket layout (`:80` / `:443` via ACME / `:8081` origin)
//! arrives in Checkpoints 3 and 4.

use crate::config::{RuntimeConfig, TlsMode};
use crate::deployments::DeploymentSupervisor;
use crate::router::{DeploymentName, RouteMatch, RouterState};
use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use perch_host_abi::DeploymentRequest;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

/// Per-request handler context passed as axum state.
#[derive(Clone)]
pub struct ListenerState {
    pub supervisor: Arc<DeploymentSupervisor>,
    pub runtime_cfg: Arc<RuntimeConfig>,
}

/// Spin up the HTTP listener(s). Blocks the current task until the
/// server exits (either via graceful shutdown or error).
///
/// In TLS mode "off": single listener on `listen_http`.
/// In TLS mode "acme": two listeners — HTTP on `listen_http` (for
///   redirects + ACME HTTP-01 challenges), HTTPS on `listen_https` via
///   `rustls-acme`.
/// In TLS mode "manual": two listeners — HTTP redirect + HTTPS with
///   static certs.
///
/// The origin listener (`:8081` for Bunny pull traffic) is added in
/// Checkpoint 4.
pub async fn serve(
    runtime_cfg: Arc<RuntimeConfig>,
    supervisor: Arc<DeploymentSupervisor>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = ListenerState {
        supervisor: supervisor.clone(),
        runtime_cfg: runtime_cfg.clone(),
    };

    let admin_state = crate::admin::AdminState {
        supervisor: supervisor.clone(),
        runtime_cfg: runtime_cfg.clone(),
    };

    let admin_path = runtime_cfg.admin.path.trim_end_matches('/');

    let app = Router::new()
        .nest(admin_path, crate::admin::router().with_state(admin_state))
        .nest("/_perch/metrics", crate::metrics::router())
        .fallback(any(dispatch))
        .with_state(state);

    match runtime_cfg.tls.mode {
        TlsMode::Off => {
            // Single HTTP listener — development mode or behind a CDN/proxy.
            let addr: SocketAddr = runtime_cfg
                .http
                .listen_http
                .parse()
                .with_context(|| format!("parsing listen_http {:?}", runtime_cfg.http.listen_http))?;

            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding {:?}", addr))?;

            info!(addr = %addr, "HTTP listener ready (TLS=off)");

            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
                .context("axum serve (HTTP)")?;
        }
        TlsMode::Acme => {
            // ACME mode: provision + renew Let's Encrypt certs via
            // TLS-ALPN-01 using `rustls-acme`.
            //
            // TODO: the rustls-acme AxumAcceptor doesn't plug directly
            // into axum::serve (which requires TcpListener). The fix is
            // either:
            //   (a) Use axum-server crate which has a generic Listener
            //   (b) Use a manual hyper accept loop with tokio-rustls
            //   (c) Wait for axum 0.8 which generalizes the listener
            //
            // For now, the config infrastructure (tls.rs validation,
            // domain collection, three-mode skeleton) is in place. The
            // actual ACME acceptor wiring lands when we test against
            // Let's Encrypt staging with a real domain + DNS.
            //
            // Until then, fall back to HTTP-only and log clearly.

            let acme_domains = crate::tls::collect_acme_domains(&runtime_cfg, &[]);
            let contact = runtime_cfg.tls.acme_contact.as_deref().unwrap_or("(none)");
            let acme_dir = runtime_cfg.tls.acme_directory.as_deref()
                .unwrap_or("https://acme-v02.api.letsencrypt.org/directory");

            info!(
                domains = ?acme_domains,
                contact = %contact,
                directory = %acme_dir,
                "ACME configured — acceptor wiring pending (serving HTTP-only for now)"
            );

            let addr: SocketAddr = runtime_cfg
                .http
                .listen_http
                .parse()
                .context("parsing listen_http")?;
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .context("binding HTTP")?;
            info!(addr = %addr, "HTTP listener ready (ACME acceptor wiring pending)");

            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
                .context("axum serve")?;
        }
        TlsMode::Manual => {
            // Placeholder — manual cert loading lands in a follow-up.
            // For now, fall back to HTTP-only and warn.
            warn!("tls.mode=manual is not yet implemented; falling back to HTTP-only");
            let addr: SocketAddr = runtime_cfg
                .http
                .listen_http
                .parse()
                .context("parsing listen_http")?;

            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .context("binding HTTP")?;

            info!(addr = %addr, "HTTP listener ready (TLS=manual not implemented, fallback)");

            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
                .context("axum serve")?;
        }
    }

    Ok(())
}

/// Dispatch a single incoming request via the `RouterState`.
async fn dispatch(State(state): State<ListenerState>, req: Request) -> Response {
    let method = req.method().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let host_header = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let router = state.supervisor.current_router().await;

    let (effective_path, match_) = router.route(host_header.as_deref(), &method, &path);

    match match_ {
        RouteMatch::NotFound => not_found_response(),
        RouteMatch::StaticFile {
            deployment,
            root,
            mount_prefix,
        } => serve_static(root, mount_prefix, effective_path, req)
            .await
            .unwrap_or_else(|e| {
                warn!(
                    deployment = %deployment.0,
                    error = ?e,
                    "static serve failed"
                );
                not_found_response()
            }),
        RouteMatch::WorkerDispatch {
            deployment,
            handler: _,
            tool_name: _,
        } => dispatch_to_worker(state.supervisor, deployment, req, effective_path, query).await,
    }
}

/// Serve a single static file via tower-http's ServeDir.
///
/// Rewrites the request URI to `effective_path` (what the deployment
/// sees — path-prefix fallback already stripped) and then to the path
/// relative to the mount prefix, so ServeDir resolves files correctly
/// against `root`.
async fn serve_static(
    root: std::path::PathBuf,
    mount_prefix: String,
    effective_path: String,
    mut req: Request,
) -> Result<Response> {
    // Strip the mount prefix from the effective path so ServeDir sees
    // the path relative to the root directory it was constructed with.
    let stripped = if mount_prefix == "/" {
        effective_path.clone()
    } else if let Some(rest) = effective_path.strip_prefix(&mount_prefix) {
        if rest.is_empty() {
            "/".to_string()
        } else if rest.starts_with('/') {
            rest.to_string()
        } else {
            format!("/{}", rest)
        }
    } else {
        effective_path.clone()
    };

    let uri = req.uri().clone();
    let mut parts = uri.into_parts();
    parts.path_and_query = Some(
        stripped
            .parse()
            .unwrap_or_else(|_| "/".parse().unwrap()),
    );
    let new_uri = Uri::from_parts(parts).context("rebuilding URI")?;
    *req.uri_mut() = new_uri;

    // tower::Service dispatch. ServeDir implements Service<Request<Body>>.
    use tower::ServiceExt;
    let service = ServeDir::new(root).precompressed_gzip();
    let response = service
        .oneshot(req)
        .await
        .context("ServeDir call failed")?;

    // tower-http ServeDir returns `Response<ServeFileSystemResponseBody>`;
    // axum wants `Response<Body>`. The IntoResponse impl does the conversion.
    Ok(response.into_response())
}

/// Forward a request to the deployment's perch-worker.
async fn dispatch_to_worker(
    supervisor: Arc<DeploymentSupervisor>,
    deployment: DeploymentName,
    req: Request,
    effective_path: String,
    query: String,
) -> Response {
    let client = match supervisor.client_for(&deployment.0).await {
        Some(c) => c,
        None => {
            error!(
                deployment = %deployment.0,
                "router matched a deployment with no live worker client"
            );
            return internal_error_response("deployment has no live worker");
        }
    };

    // Build the DeploymentRequest from the axum request.
    let method = req.method().to_string();
    let scheme = req.uri().scheme_str().unwrap_or("http").to_string();
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    for (k, v) in req.headers() {
        if let Ok(vs) = v.to_str() {
            headers.insert(k.as_str().to_ascii_lowercase(), vs.to_string());
        }
    }

    let remote_addr = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_default();

    // Consume the body up to a sane limit (the worker enforces
    // MAX_FRAME_SIZE on its side; we use the same bound here).
    let body = match axum::body::to_bytes(req.into_body(), perch_host_abi::MAX_FRAME_SIZE).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = ?e, "reading request body failed");
            return internal_error_response("request body too large or read error");
        }
    };
    let body_base64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&body)
    };

    let deployment_req = DeploymentRequest {
        method,
        path: effective_path,
        query,
        headers,
        remote_addr,
        scheme,
        host,
        body_base64,
    };

    match client.dispatch(deployment_req).await {
        Ok(dep_resp) => build_response(dep_resp),
        Err(e) => {
            error!(
                deployment = %deployment.0,
                error = ?e,
                "worker dispatch failed"
            );
            internal_error_response(&format!("worker dispatch failed: {}", e))
        }
    }
}

fn build_response(dep_resp: perch_host_abi::DeploymentResponse) -> Response {
    let status = StatusCode::from_u16(dep_resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let body_bytes = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&dep_resp.body_base64)
            .unwrap_or_default()
    };

    let mut header_map = HeaderMap::new();
    for (k, v) in dep_resp.headers {
        if let (Ok(name), Ok(val)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
            header_map.insert(name, val);
        }
    }

    let mut response = Response::builder()
        .status(status)
        .body(Body::from(body_bytes))
        .unwrap();
    *response.headers_mut() = header_map;
    response
}

fn not_found_response() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("perch: 404 Not Found\n"))
        .unwrap()
}

fn internal_error_response(msg: &str) -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("perch: 500 {}\n", msg)))
        .unwrap()
}

#[allow(dead_code)]
pub fn router_count_for_test(router: &RouterState) -> usize {
    router.deployment_count()
}
