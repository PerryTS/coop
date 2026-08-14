//! Per-deployment router: turns an incoming HTTP request into a routing
//! decision (`RouteMatch`).
//!
//! The daemon owns a `RouterState` that's built from all currently-loaded
//! deployments. On reload it rebuilds atomically, swapping the new table
//! in via `arc-swap` semantics. Each request hits `route()` and gets back:
//!
//! - `StaticFile` — serve the file from disk directly (no worker hop)
//! - `WorkerDispatch` — forward to a named tool on a specific deployment
//! - `NotFound` — return 404
//!
//! Routing precedence within a matched host:
//!
//! 1. Exact-match handlers (method + path) win
//! 2. `[[static]]` blocks match by prefix; first-registered wins
//! 3. Otherwise 404
//!
//! Routing precedence across hosts: host must match exactly (case-
//! insensitive, with port stripping). If no host matches, fall back to
//! path-prefix routing under `/<deployment-name>/` for development
//! convenience.

use crate::config::{DeploymentConfig, HandlerConfig, StaticConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Identifier for a loaded deployment inside the router.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct DeploymentName(pub String);

/// Allocation bounds copied into the immutable route snapshot. Keeping these
/// beside the matching handler avoids a live-deployment lock before reading
/// the request body; dispatch validates against the exact active generation
/// again after the body has been materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpRouteLimits {
    pub max_body_bytes: usize,
    pub max_header_bytes: usize,
}

/// What the router decided to do with a request.
#[derive(Debug, Clone)]
pub enum RouteMatch {
    /// Serve a file directly from the deployment's static directory.
    /// The path returned here is the **on-disk** path under
    /// `<deployments_dir>/<name>/<directory>/<remainder>`; the axum
    /// handler turns this into a `tower-http::ServeDir` invocation.
    StaticFile {
        deployment: DeploymentName,
        root: PathBuf,
        /// The URL prefix the static block is mounted under (e.g. `/`).
        /// Used to strip the mount prefix before looking up on disk.
        mount_prefix: String,
    },
    /// Dispatch to the deployment selected by this route. ABI-v2 libraries
    /// have one required Buffer entry point, so handler/tool metadata stays in
    /// the immutable route table and is not cloned into every request match.
    WorkerDispatch {
        deployment: DeploymentName,
        request_limits: HttpRouteLimits,
    },
    /// No deployment / handler / static block matched.
    NotFound,
}

/// Per-deployment routing table (handlers + statics + hostnames).
#[derive(Debug, Clone)]
pub struct DeploymentRoutes {
    pub name: DeploymentName,
    pub deployment_dir: PathBuf,
    pub hostnames: Vec<String>,
    pub handlers: Vec<(HandlerConfig, String /* tool_name */)>,
    pub static_blocks: Vec<StaticConfig>,
    pub request_limits: HttpRouteLimits,
}

impl DeploymentRoutes {
    /// Build routes from a loaded deployment config and the path to the
    /// deployment's source directory.
    pub fn from_config(config: &DeploymentConfig, deployment_dir: PathBuf) -> Self {
        let hostnames = config
            .hosts
            .domains
            .iter()
            .map(|h| h.to_lowercase())
            .collect::<Vec<_>>();

        let handlers = config
            .handlers
            .iter()
            .map(|h| {
                let tool = h
                    .tool
                    .clone()
                    .unwrap_or_else(|| DeploymentConfig::default_tool_name(&h.file));
                (h.clone(), tool)
            })
            .collect::<Vec<_>>();

        Self {
            name: DeploymentName(config.name.clone()),
            deployment_dir,
            hostnames,
            handlers,
            static_blocks: config.static_blocks.clone(),
            request_limits: HttpRouteLimits {
                max_body_bytes: config.limits.max_request_body_bytes,
                max_header_bytes: config.limits.max_request_header_bytes,
            },
        }
    }
}

/// The full router state — everything a request dispatch needs to
/// decide where the request goes. Built atomically by `RouterState::build`
/// from a list of loaded deployments.
#[derive(Debug, Default)]
pub struct RouterState {
    /// Lookup by lowercase hostname (no port). Multiple hostnames can
    /// point at the same deployment.
    by_host: HashMap<String, Arc<DeploymentRoutes>>,
    /// Lookup by deployment name (for path-prefix fallback and for
    /// admin/management).
    by_name: HashMap<String, Arc<DeploymentRoutes>>,
}

impl RouterState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the router state from a fresh list of loaded deployments.
    /// Call this whenever a deployment is added, changed, or removed.
    pub fn build(deployments: Vec<DeploymentRoutes>) -> Self {
        let mut by_host = HashMap::new();
        let mut by_name = HashMap::new();
        for d in deployments {
            let arc = Arc::new(d);
            for host in &arc.hostnames {
                by_host.insert(host.clone(), arc.clone());
            }
            by_name.insert(arc.name.0.clone(), arc.clone());
        }
        Self { by_host, by_name }
    }

    /// Resolve a request to a `RouteMatch`. `host_header` is the raw
    /// value of the `Host:` header (may include port); `method` is
    /// uppercased; `path` is the URL path with no query string.
    ///
    /// Returns a tuple of `(effective_path, match)`. The `effective_path`
    /// is the path **as seen by the deployment** — for host-based
    /// routing it's the same as the input `path`, but for the
    /// development path-prefix fallback (`/<deployment>/rest`) it has
    /// the `/<deployment>` prefix stripped so handler lookups and static
    /// file lookups work against the deployment's "native" URL space.
    /// The listener uses `effective_path` both when calling `ServeDir`
    /// and when populating the `DeploymentRequest.path` field.
    pub fn route(
        &self,
        host_header: Option<&str>,
        method: &str,
        path: &str,
    ) -> (String, RouteMatch) {
        // Step 1: identify the deployment by host header, or fall back
        // to path-prefix routing for development.
        let (deployment, remaining_path) = match self.match_deployment(host_header, path) {
            Some(pair) => pair,
            None => return (path.to_string(), RouteMatch::NotFound),
        };

        let effective = remaining_path.to_string();

        // Step 2: check exact-match handlers (method + path).
        if let Some(m) = self.match_handler(&deployment, method, remaining_path) {
            return (effective, m);
        }

        // Step 3: check static blocks by prefix.
        if let Some(m) = self.match_static(&deployment, remaining_path) {
            return (effective, m);
        }

        (effective, RouteMatch::NotFound)
    }

    /// Look up the deployment for a given request. Returns the routes
    /// plus the path that's "effective within" the deployment (with any
    /// path-prefix fallback stripped).
    fn match_deployment<'a>(
        &self,
        host_header: Option<&str>,
        path: &'a str,
    ) -> Option<(Arc<DeploymentRoutes>, &'a str)> {
        // 1. Try host-based routing.
        if let Some(host) = host_header {
            let host_key = normalize_host(host);
            if let Some(d) = self.by_host.get(&host_key) {
                return Some((d.clone(), path));
            }
        }

        // 2. Fall back to path-prefix routing: `/<deployment>/rest`.
        let stripped = path.trim_start_matches('/');
        let mut parts = stripped.splitn(2, '/');
        let first = parts.next()?;
        let rest = parts.next().unwrap_or("");
        if let Some(d) = self.by_name.get(first) {
            // Reconstruct the effective path with a leading slash.
            // `/chirp` → `/`, `/chirp/foo` → `/foo`.
            let effective = if rest.is_empty() {
                "/"
            } else {
                // SAFETY: we want a lifetime-extension-free slice; the
                // "path" str is sliced to just after the deployment name
                // plus its trailing slash.
                let offset = first.len() + 1; // leading "/" + name
                let offset = offset + 1; // trailing slash
                if path.len() > offset {
                    &path[offset - 1..] // keep the slash
                } else {
                    "/"
                }
            };
            return Some((d.clone(), effective));
        }

        None
    }

    fn match_handler(
        &self,
        deployment: &Arc<DeploymentRoutes>,
        method: &str,
        path: &str,
    ) -> Option<RouteMatch> {
        for (handler, _) in &deployment.handlers {
            if handler.path != path {
                continue;
            }
            match handler.method.as_deref() {
                None | Some("") => {
                    return Some(RouteMatch::WorkerDispatch {
                        deployment: deployment.name.clone(),
                        request_limits: deployment.request_limits,
                    });
                }
                Some(m) if m.eq_ignore_ascii_case(method) => {
                    return Some(RouteMatch::WorkerDispatch {
                        deployment: deployment.name.clone(),
                        request_limits: deployment.request_limits,
                    });
                }
                _ => continue,
            }
        }
        None
    }

    fn match_static(&self, deployment: &Arc<DeploymentRoutes>, path: &str) -> Option<RouteMatch> {
        for block in &deployment.static_blocks {
            if path_matches_prefix(path, &block.path) {
                let mut root = deployment.deployment_dir.clone();
                // `directory` is relative to the deployment dir. Strip
                // any leading `./` that might be in the TOML.
                let dir = block
                    .directory
                    .strip_prefix("./")
                    .unwrap_or(&block.directory);
                root.push(dir);
                return Some(RouteMatch::StaticFile {
                    deployment: deployment.name.clone(),
                    root,
                    mount_prefix: block.path.clone(),
                });
            }
        }
        None
    }

    /// Number of loaded deployments.
    pub fn deployment_count(&self) -> usize {
        self.by_name.len()
    }

    /// Look up a deployment by name.
    pub fn get(&self, name: &str) -> Option<Arc<DeploymentRoutes>> {
        self.by_name.get(name).cloned()
    }

    /// All deployments, for iteration in admin UI / metrics.
    pub fn all(&self) -> impl Iterator<Item = &Arc<DeploymentRoutes>> {
        self.by_name.values()
    }
}

/// Strip port and lowercase a Host header value for lookup.
pub fn normalize_host(host: &str) -> String {
    let without_port = host.split(':').next().unwrap_or(host);
    without_port.trim().to_ascii_lowercase()
}

/// Does `path` fall within `prefix`? Treats `/` as matching everything;
/// otherwise requires that `path == prefix` or `path` starts with
/// `prefix + "/"`.
fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    if path == prefix {
        return true;
    }
    let prefix_with_slash = format!("{}/", prefix);
    path.starts_with(&prefix_with_slash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HandlerConfig, StaticConfig};

    fn deployment(name: &str, hosts: &[&str], deployment_dir: &str) -> DeploymentRoutes {
        DeploymentRoutes {
            name: DeploymentName(name.to_string()),
            deployment_dir: PathBuf::from(deployment_dir),
            hostnames: hosts.iter().map(|s| s.to_lowercase()).collect(),
            handlers: Vec::new(),
            static_blocks: Vec::new(),
            request_limits: HttpRouteLimits {
                max_body_bytes: 1024 * 1024,
                max_header_bytes: 64 * 1024,
            },
        }
    }

    fn handler(method: Option<&str>, path: &str, file: &str) -> HandlerConfig {
        HandlerConfig {
            file: PathBuf::from(file),
            path: path.to_string(),
            method: method.map(|s| s.to_string()),
            tool: None,
        }
    }

    fn static_block(prefix: &str, dir: &str) -> StaticConfig {
        StaticConfig {
            directory: PathBuf::from(dir),
            path: prefix.to_string(),
        }
    }

    #[test]
    fn host_routing_dispatches_by_hostname() {
        let mut chirp = deployment("chirp", &["chirp.io", "www.chirp.io"], "/var/d/chirp");
        chirp.handlers.push((
            handler(Some("POST"), "/ingest", "handlers/ingest.ts"),
            "handlers_ingest".into(),
        ));

        let state = RouterState::build(vec![chirp]);
        match state.route(Some("chirp.io"), "POST", "/ingest").1 {
            RouteMatch::WorkerDispatch { deployment, .. } => {
                assert_eq!(deployment.0, "chirp");
            }
            other => panic!("expected WorkerDispatch, got {:?}", other),
        }
    }

    #[test]
    fn host_routing_is_case_insensitive_and_port_stripping() {
        let mut chirp = deployment("chirp", &["chirp.io"], "/var/d/chirp");
        chirp.handlers.push((
            handler(None, "/", "handlers/index.ts"),
            "handlers_index".into(),
        ));

        let state = RouterState::build(vec![chirp]);
        match state.route(Some("CHIRP.IO:8080"), "GET", "/").1 {
            RouteMatch::WorkerDispatch { deployment, .. } => {
                assert_eq!(deployment.0, "chirp");
            }
            other => panic!("expected WorkerDispatch, got {:?}", other),
        }
    }

    #[test]
    fn exact_handler_wins_over_static_catchall() {
        let mut landing = deployment("landing", &["landing.test"], "/var/d/landing");
        landing.handlers.push((
            handler(Some("POST"), "/contact", "handlers/contact.ts"),
            "handlers_contact".into(),
        ));
        landing.static_blocks.push(static_block("/", "static"));

        let state = RouterState::build(vec![landing]);

        // POST /contact → handler
        match state.route(Some("landing.test"), "POST", "/contact").1 {
            RouteMatch::WorkerDispatch { deployment, .. } => {
                assert_eq!(deployment.0, "landing");
            }
            other => panic!("expected WorkerDispatch for POST /contact, got {:?}", other),
        }

        // GET /contact → static fallback (no method match for the handler)
        match state.route(Some("landing.test"), "GET", "/contact").1 {
            RouteMatch::StaticFile { deployment, .. } => {
                assert_eq!(deployment.0, "landing");
            }
            other => panic!("expected StaticFile for GET /contact, got {:?}", other),
        }

        // GET /index.html → static
        match state.route(Some("landing.test"), "GET", "/index.html").1 {
            RouteMatch::StaticFile { deployment, .. } => {
                assert_eq!(deployment.0, "landing");
            }
            other => panic!("expected StaticFile, got {:?}", other),
        }
    }

    #[test]
    fn no_match_falls_back_to_not_found() {
        let landing = deployment("landing", &["landing.test"], "/var/d/landing");
        let state = RouterState::build(vec![landing]);
        match state.route(Some("unknown.com"), "GET", "/").1 {
            RouteMatch::NotFound => {}
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn path_prefix_fallback_works_without_host() {
        let mut chirp = deployment("chirp", &["chirp.io"], "/var/d/chirp");
        chirp.handlers.push((
            handler(Some("GET"), "/", "handlers/index.ts"),
            "handlers_index".into(),
        ));

        let state = RouterState::build(vec![chirp]);
        // No host header, use path-prefix: /chirp/ → chirp deployment, effective path /
        let (eff, m) = state.route(None, "GET", "/chirp/");
        assert_eq!(eff, "/");
        match m {
            RouteMatch::WorkerDispatch { deployment, .. } => {
                assert_eq!(deployment.0, "chirp");
            }
            other => panic!("expected WorkerDispatch via path prefix, got {:?}", other),
        }
    }

    #[test]
    fn path_prefix_fallback_strips_deployment_name_for_static() {
        let mut landing = deployment("landing", &["landing.test"], "/var/d/landing");
        landing.static_blocks.push(static_block("/", "static"));

        let state = RouterState::build(vec![landing]);
        let (eff, m) = state.route(None, "GET", "/landing/index.html");
        assert_eq!(eff, "/index.html");
        match m {
            RouteMatch::StaticFile { deployment, .. } => {
                assert_eq!(deployment.0, "landing");
            }
            other => panic!("expected StaticFile, got {:?}", other),
        }
    }
}
