//! HTTP listener — accepts incoming requests and routes them through
//! `RouterState` to either a static file or a coop-worker dispatch.
//!
//! For Checkpoint 2 this is a single axum server on `listen_http`. The
//! full three-socket layout (`:80` / `:443` via ACME / `:8081` origin)
//! arrives in Checkpoints 3 and 4.

use crate::config::{RuntimeConfig, TlsMode};
use crate::deployments::{DeploymentSupervisor, InvocationError};
use crate::router::{DeploymentName, HttpRouteLimits, RouteMatch, RouterState};
use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use coop_host_abi::{HttpDispatchRequest, HttpDispatchResponse};
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
/// The origin listener (`:8081`) is added in
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
        .nest("/_coop/metrics", crate::metrics::router())
        .fallback(any(dispatch))
        .with_state(state);

    match runtime_cfg.tls.mode {
        TlsMode::Off => {
            // Single HTTP listener — development mode or behind a CDN/proxy.
            let addr: SocketAddr = runtime_cfg.http.listen_http.parse().with_context(|| {
                format!("parsing listen_http {:?}", runtime_cfg.http.listen_http)
            })?;

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
            let acme_dir = runtime_cfg
                .tls
                .acme_directory
                .as_deref()
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
    let router = state.supervisor.current_router();
    let (effective_path, match_) = router.route(
        req.headers()
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok()),
        req.method().as_str(),
        req.uri().path(),
    );

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
            request_limits,
        } => {
            dispatch_to_deployment(
                state.supervisor,
                deployment,
                request_limits,
                req,
                effective_path,
            )
            .await
        }
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
    parts.path_and_query = Some(stripped.parse().unwrap_or_else(|_| "/".parse().unwrap()));
    let new_uri = Uri::from_parts(parts).context("rebuilding URI")?;
    *req.uri_mut() = new_uri;

    // tower::Service dispatch. ServeDir implements Service<Request<Body>>.
    use tower::ServiceExt;
    let service = ServeDir::new(root).precompressed_gzip();
    let response = service.oneshot(req).await.context("ServeDir call failed")?;

    // tower-http ServeDir returns `Response<ServeFileSystemResponseBody>`;
    // axum wants `Response<Body>`. The IntoResponse impl does the conversion.
    Ok(response.into_response())
}

/// Dispatch to the deployment's already-warm runtime.
async fn dispatch_to_deployment(
    supervisor: Arc<DeploymentSupervisor>,
    deployment: DeploymentName,
    limits: HttpRouteLimits,
    req: Request,
    effective_path: String,
) -> Response {
    let started = std::time::Instant::now();
    // Build the DeploymentRequest from the axum request.
    let method = req.method().to_string();
    let scheme = req.uri().scheme_str().unwrap_or("http").to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let raw_header_bytes = req.headers().iter().fold(0usize, |total, (name, value)| {
        total
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len())
    });
    if raw_header_bytes > limits.max_header_bytes {
        crate::metrics::record_invocation_rejected(&deployment.0, "http", "request_headers");
        return record_deployment_response(
            &deployment.0,
            &method,
            started,
            limit_response(
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "request_headers_too_large",
                "request headers exceed the deployment limit",
            ),
        );
    }

    let mut headers = Vec::with_capacity(req.headers().len());
    for (k, v) in req.headers() {
        if let Ok(vs) = v.to_str() {
            headers.push((k.as_str().to_ascii_lowercase(), vs.to_string()));
        }
    }

    let remote_addr = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_default();

    // Enforce the deployment limit while consuming the body so a small
    // configured limit also bounds listener-side allocation.
    let body = match axum::body::to_bytes(req.into_body(), limits.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = ?e, "reading request body failed");
            crate::metrics::record_invocation_rejected(&deployment.0, "http", "request_body");
            return record_deployment_response(
                &deployment.0,
                &method,
                started,
                limit_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_body_too_large",
                    "request body exceeds the deployment limit or could not be read",
                ),
            );
        }
    };
    let deployment_req = HttpDispatchRequest {
        method: method.clone(),
        path: effective_path,
        query,
        headers,
        remote_addr,
        scheme,
        host,
        body: body.to_vec(),
    };

    let response = match supervisor.dispatch(&deployment.0, deployment_req).await {
        Ok(dep_resp) => build_response(dep_resp),
        Err(e) => {
            error!(
                deployment = %deployment.0,
                error = ?e,
                "deployment dispatch failed"
            );
            invocation_error_response(&e)
        }
    };
    record_deployment_response(&deployment.0, &method, started, response)
}

fn record_deployment_response(
    deployment: &str,
    method: &str,
    started: std::time::Instant,
    response: Response,
) -> Response {
    crate::metrics::record_request(
        deployment,
        method,
        response.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    response
}

fn invocation_error_response(error: &InvocationError) -> Response {
    match error {
        InvocationError::Unavailable(_) => limit_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "deployment_unavailable",
            "deployment is not currently available",
        ),
        InvocationError::Overloaded { .. } => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )
            .header(axum::http::header::RETRY_AFTER, "1")
            .header("x-coop-error", "overloaded")
            .body(Body::from("coop: deployment invocation limit is full\n"))
            .unwrap(),
        InvocationError::DeadlineExceeded { .. } => limit_response(
            StatusCode::GATEWAY_TIMEOUT,
            "deadline_exceeded",
            "deployment invocation exceeded its wall-clock limit",
        ),
        InvocationError::RequestTooLarge { field, .. } => limit_response(
            if *field == "headers" {
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            },
            "request_too_large",
            "request exceeds the deployment limit",
        ),
        InvocationError::ResponseTooLarge { .. } => limit_response(
            StatusCode::BAD_GATEWAY,
            "response_too_large",
            "deployment response exceeds the configured limit",
        ),
        InvocationError::Runtime(_) => internal_error_response("deployment dispatch failed"),
    }
}

fn limit_response(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    Response::builder()
        .status(status)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .header("x-coop-error", code)
        .body(Body::from(format!("coop: {message}\n")))
        .unwrap()
}

fn build_response(dep_resp: HttpDispatchResponse) -> Response {
    let status = StatusCode::from_u16(dep_resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut header_map = HeaderMap::new();
    for (k, v) in dep_resp.headers {
        if let (Ok(name), Ok(val)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
            header_map.append(name, val);
        }
    }

    let mut response = Response::builder()
        .status(status)
        .body(Body::from(dep_resp.body))
        .unwrap();
    *response.headers_mut() = header_map;
    response
}

fn not_found_response() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(Body::from("coop: 404 Not Found\n"))
        .unwrap()
}

fn internal_error_response(msg: &str) -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(Body::from(format!("coop: 500 {}\n", msg)))
        .unwrap()
}

#[allow(dead_code)]
pub fn router_count_for_test(router: &RouterState) -> usize {
    router.deployment_count()
}
