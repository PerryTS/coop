//! Per-deployment environment exported to the Perry-compiled application.
//!
//! `@coop/runtime`'s `kv` and `storage` modules read their configuration from
//! `process.env` at module-init time. The application library is `dlopen`'d
//! into this process (see `plugin_host::load_deployment`), and Perry lowers a
//! `process.env.X` member read to a live `std::env::var` call, so the worker's
//! own environment *is* the handler's environment. This module owns the exact
//! set of variables the worker exports, and the rules that make them safe.
//!
//! Two of those rules are load-bearing rather than cosmetic:
//!
//! 1. **The key prefix must be injective.** `kv` prepends `COOP_REDIS_PREFIX`
//!    to every key so two deployments cannot address each other's data. That
//!    only holds if no deployment's prefix is a prefix of another's *at a key
//!    boundary*: with a bare `"{name}:"` prefix, deployment `a` reading the
//!    literal key `b:session` lands on `a:b:session`, which is exactly where
//!    deployment `a:b` writes `session`. `:` is legal in a directory name, and
//!    the deployment name is a directory name, so this is reachable. We reject
//!    the separator in the name instead of hoping.
//!
//! 2. **Perry's `new Redis(...)` ignores its argument.** The pinned provider's
//!    `js_ioredis_new` (`.perry-main/crates/perry-stdlib/src/ioredis.rs`)
//!    discards the config pointer and builds its connection URL from
//!    `REDIS_HOST` / `REDIS_PORT` / `REDIS_PASSWORD` / `REDIS_TLS`. Passing a
//!    URL to the constructor in TypeScript therefore does nothing at all. The
//!    worker translates the operator's `COOP_REDIS_URL` into those four
//!    variables so that the ignored-argument constructor still reaches the
//!    configured server. If that translation is ever dropped, `kv` silently
//!    talks to `rediss://127.0.0.1:6379` — note the TLS default — instead of
//!    failing, which is why the translation is unit-tested rather than trusted.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// Key prefix `kv` prepends to every key. Read at module-init time by
/// `packages/coop-runtime/src/kv.ts`.
pub const REDIS_PREFIX_VAR: &str = "COOP_REDIS_PREFIX";
/// Presence of this variable is what `kv` treats as "Redis is configured".
/// The value is informational; the connection itself is built by the provider
/// from the `REDIS_*` variables below.
pub const REDIS_URL_VAR: &str = "COOP_REDIS_URL";
/// Per-deployment object-storage root. Read by
/// `packages/coop-runtime/src/storage.ts`.
pub const STORAGE_DIR_VAR: &str = "COOP_STORAGE_DIR";

/// The four variables Perry's `js_ioredis_new` actually reads.
pub const PROVIDER_REDIS_HOST_VAR: &str = "REDIS_HOST";
pub const PROVIDER_REDIS_PORT_VAR: &str = "REDIS_PORT";
pub const PROVIDER_REDIS_PASSWORD_VAR: &str = "REDIS_PASSWORD";
pub const PROVIDER_REDIS_TLS_VAR: &str = "REDIS_TLS";

/// Separator between the deployment prefix and the application's own key.
const KV_PREFIX_SEPARATOR: char = ':';

/// What a dedicated worker was told about the box's shared services.
#[derive(Debug, Default, Clone)]
pub struct DeploymentServices<'a> {
    /// `runtime.toml` `[redis] url`, forwarded by the daemon. `None` leaves
    /// `kv` unconfigured, and `kv` then throws rather than connecting to a
    /// default server.
    pub redis_url: Option<&'a str>,
    /// `runtime.toml` `[paths] storage_dir`, forwarded by the daemon. The
    /// deployment's own subdirectory is derived from it here.
    pub storage_root: Option<&'a Path>,
}

/// Reject a deployment name that cannot be used as both a filesystem
/// component and an unambiguous Redis key prefix.
///
/// The daemon validates the name too (`config.rs`), but a worker can be run
/// by hand, and the consequence of a bad name here is one deployment reading
/// another's data — so this is checked where it is used.
fn check_deployment_name(deployment: &str) -> Result<()> {
    if deployment.is_empty() {
        return Err(anyhow!("deployment name must not be empty"));
    }
    if deployment == "." || deployment == ".." {
        return Err(anyhow!(
            "deployment name {deployment:?} is a relative path component"
        ));
    }
    if let Some(bad) = deployment
        .chars()
        .find(|c| matches!(c, '/' | '\\' | '\0') || c.is_control())
    {
        return Err(anyhow!(
            "deployment name {deployment:?} contains {bad:?}, which cannot appear \
             in a storage path component"
        ));
    }
    if deployment.contains(KV_PREFIX_SEPARATOR) {
        return Err(anyhow!(
            "deployment name {deployment:?} contains {KV_PREFIX_SEPARATOR:?}, which \
             would make its kv key prefix ambiguous with another deployment's keys"
        ));
    }
    Ok(())
}

/// The kv key prefix for a deployment. Every `kv` key is written under this.
pub fn redis_key_prefix(deployment: &str) -> String {
    format!("coop{KV_PREFIX_SEPARATOR}{deployment}{KV_PREFIX_SEPARATOR}")
}

/// The object-storage root for a deployment.
pub fn storage_dir(storage_root: &Path, deployment: &str) -> PathBuf {
    storage_root.join(deployment)
}

/// Translate an operator-supplied `redis://` / `rediss://` URL into the four
/// variables the pinned Perry provider reads.
///
/// This deliberately fails on URL features the provider cannot express rather
/// than dropping them: a database selector and a username both change which
/// data you reach, and silently ignoring either would put a deployment on the
/// wrong keyspace or the wrong account with no diagnostic.
fn provider_redis_env(url: &str) -> Result<Vec<(String, String)>> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        anyhow!("redis URL {url:?} has no scheme; expected redis:// or rediss://")
    })?;
    let tls = match scheme {
        "redis" => false,
        "rediss" => true,
        other => {
            return Err(anyhow!(
                "redis URL scheme {other:?} is not supported; expected redis or rediss"
            ))
        }
    };

    // Split the authority from the path before looking for the credential
    // separator, so a '@' inside a path cannot be read as one.
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    if !path.is_empty() && path != "0" {
        return Err(anyhow!(
            "redis URL {url:?} selects database {path:?}; the pinned Perry ioredis \
             provider cannot select a database and would silently use 0"
        ));
    }

    // rsplit: a password may itself contain '@'.
    let (credentials, host_port) = match authority.rsplit_once('@') {
        Some((credentials, host_port)) => (Some(credentials), host_port),
        None => (None, authority),
    };

    let password = match credentials {
        None => None,
        Some(credentials) => match credentials.split_once(':') {
            // `redis://:secret@host` — the only credential form the provider
            // can express.
            Some(("", password)) => Some(password.to_string()),
            Some((user, _)) => {
                return Err(anyhow!(
                    "redis URL {url:?} carries username {user:?}; the pinned Perry \
                     ioredis provider sends no username and would authenticate as default"
                ))
            }
            // `redis://secret@host` is a username in RFC terms, not a password.
            None => {
                return Err(anyhow!(
                    "redis URL {url:?} carries a bare userinfo component; write the \
                     password as redis://:PASSWORD@host so it is not sent as a username"
                ))
            }
        },
    };

    let (host, port) = match host_port.rsplit_once(':') {
        // Guard against an IPv6 literal, whose colons are not a port separator.
        Some((host, port)) if !host.contains(':') && !port.is_empty() => (host, port),
        _ => (host_port, "6379"),
    };
    if host.is_empty() {
        return Err(anyhow!("redis URL {url:?} has no host"));
    }
    if port.parse::<u16>().is_err() {
        return Err(anyhow!("redis URL {url:?} has a non-numeric port {port:?}"));
    }

    let mut vars = vec![
        (PROVIDER_REDIS_HOST_VAR.to_string(), host.to_string()),
        (PROVIDER_REDIS_PORT_VAR.to_string(), port.to_string()),
        (
            PROVIDER_REDIS_TLS_VAR.to_string(),
            // The provider treats any value other than the literal "false" as
            // TLS-on, so a plaintext URL must set it explicitly.
            if tls { "true" } else { "false" }.to_string(),
        ),
    ];
    if let Some(password) = password {
        vars.push((PROVIDER_REDIS_PASSWORD_VAR.to_string(), password));
    }
    Ok(vars)
}

/// Build the complete environment a dedicated worker exports for its
/// deployment, in the order it should be applied.
///
/// Returns an error rather than a partial environment: a deployment that
/// starts with half its capabilities configured is harder to diagnose than one
/// that refuses to start.
pub fn deployment_environment(
    deployment: &str,
    services: &DeploymentServices<'_>,
) -> Result<Vec<(String, String)>> {
    check_deployment_name(deployment)?;

    let mut vars = vec![(REDIS_PREFIX_VAR.to_string(), redis_key_prefix(deployment))];

    if let Some(url) = services.redis_url {
        if url.is_empty() {
            return Err(anyhow!("redis URL must not be empty when configured"));
        }
        vars.extend(provider_redis_env(url)?);
        vars.push((REDIS_URL_VAR.to_string(), url.to_string()));
    }

    if let Some(root) = services.storage_root {
        vars.push((
            STORAGE_DIR_VAR.to_string(),
            storage_dir(root, deployment)
                .to_str()
                .ok_or_else(|| anyhow!("storage directory for {deployment:?} is not UTF-8"))?
                .to_string(),
        ));
    }

    Ok(vars)
}

/// Export the deployment environment into this process.
///
/// Must run before `DeploymentHost::load*`: `kv.ts` and `storage.ts` capture
/// their configuration in module-level `const`s, which Perry evaluates inside
/// `perry_module_init()` while the host is loading.
///
/// The storage directory is created here rather than in the application,
/// because the host owns the path and the application only owns keys under it.
pub fn export_deployment_environment(
    deployment: &str,
    services: &DeploymentServices<'_>,
) -> Result<Vec<(String, String)>> {
    let vars = deployment_environment(deployment, services)?;

    if let Some(root) = services.storage_root {
        let dir = storage_dir(root, deployment);
        std::fs::create_dir_all(&dir).map_err(|error| {
            anyhow!(
                "creating object-storage directory {}: {error}",
                dir.display()
            )
        })?;
    }

    for (key, value) in &vars {
        std::env::set_var(key, value);
    }
    Ok(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn services<'a>(redis: Option<&'a str>, storage: Option<&'a Path>) -> DeploymentServices<'a> {
        DeploymentServices {
            redis_url: redis,
            storage_root: storage,
        }
    }

    fn lookup<'a>(vars: &'a [(String, String)], key: &str) -> Option<&'a str> {
        vars.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn prefix_is_always_exported_even_without_a_configured_redis() {
        // kv.ts reads COOP_REDIS_PREFIX at module init unconditionally. If it
        // were only set alongside a URL, an unconfigured deployment would
        // build unprefixed keys the moment Redis was later switched on.
        let vars = deployment_environment("chirp", &services(None, None)).unwrap();
        assert_eq!(lookup(&vars, REDIS_PREFIX_VAR), Some("coop:chirp:"));
        assert_eq!(lookup(&vars, REDIS_URL_VAR), None);
        assert_eq!(lookup(&vars, PROVIDER_REDIS_HOST_VAR), None);
    }

    #[test]
    fn two_deployments_cannot_reach_each_others_keys() {
        // The whole point of the prefix. `a` writing the literal key
        // "b:session" must not land where `a:b` writes "session" — which is
        // why a name containing the separator is refused outright.
        let a = redis_key_prefix("a");
        assert_eq!(a.clone() + "b:session", "coop:a:b:session");
        assert!(
            deployment_environment("a:b", &services(None, None)).is_err(),
            "a deployment name containing the kv separator must be refused"
        );
    }

    #[test]
    fn a_traversing_or_empty_deployment_name_is_refused() {
        for name in ["", ".", "..", "../escape", "a/b", "a\\b", "a\0b"] {
            assert!(
                deployment_environment(name, &services(None, None)).is_err(),
                "deployment name {name:?} must be refused"
            );
        }
    }

    #[test]
    fn plaintext_url_turns_the_providers_tls_default_off() {
        // js_ioredis_new defaults REDIS_TLS to ON. A redis:// URL that did not
        // write "false" would produce a rediss:// connection to a plaintext
        // server and fail at first use, far from the configuration mistake.
        let vars = deployment_environment("chirp", &services(Some("redis://127.0.0.1:6379"), None))
            .unwrap();
        assert_eq!(lookup(&vars, PROVIDER_REDIS_HOST_VAR), Some("127.0.0.1"));
        assert_eq!(lookup(&vars, PROVIDER_REDIS_PORT_VAR), Some("6379"));
        assert_eq!(lookup(&vars, PROVIDER_REDIS_TLS_VAR), Some("false"));
        assert_eq!(lookup(&vars, PROVIDER_REDIS_PASSWORD_VAR), None);
        assert_eq!(
            lookup(&vars, REDIS_URL_VAR),
            Some("redis://127.0.0.1:6379"),
            "kv.ts uses the URL's presence as its configured/unconfigured test"
        );
    }

    #[test]
    fn tls_url_password_and_default_port_are_translated() {
        let vars = deployment_environment(
            "chirp",
            &services(Some("rediss://:s3cr3t@redis.internal"), None),
        )
        .unwrap();
        assert_eq!(
            lookup(&vars, PROVIDER_REDIS_HOST_VAR),
            Some("redis.internal")
        );
        assert_eq!(lookup(&vars, PROVIDER_REDIS_PORT_VAR), Some("6379"));
        assert_eq!(lookup(&vars, PROVIDER_REDIS_TLS_VAR), Some("true"));
        assert_eq!(lookup(&vars, PROVIDER_REDIS_PASSWORD_VAR), Some("s3cr3t"));
    }

    #[test]
    fn a_password_containing_an_at_sign_is_not_split_early() {
        let vars = deployment_environment(
            "chirp",
            &services(Some("redis://:p@ss@10.0.0.5:6380"), None),
        )
        .unwrap();
        assert_eq!(lookup(&vars, PROVIDER_REDIS_HOST_VAR), Some("10.0.0.5"));
        assert_eq!(lookup(&vars, PROVIDER_REDIS_PORT_VAR), Some("6380"));
        assert_eq!(lookup(&vars, PROVIDER_REDIS_PASSWORD_VAR), Some("p@ss"));
    }

    #[test]
    fn url_features_the_provider_cannot_express_are_refused_not_dropped() {
        // Each of these changes which data you reach. The provider builds
        // "scheme://[:pw@]host:port" and has no database or username field, so
        // accepting them would put the deployment on a different keyspace or a
        // different account with no diagnostic at all.
        for url in [
            "redis://127.0.0.1:6379/1",       // database selector
            "redis://alice:secret@127.0.0.1", // username
            "redis://secret@127.0.0.1",       // bare userinfo
            "http://127.0.0.1:6379",          // wrong scheme
            "127.0.0.1:6379",                 // no scheme
            "redis://",                       // no host
            "redis://127.0.0.1:not-a-port",   // unparseable port
        ] {
            assert!(
                deployment_environment("chirp", &services(Some(url), None)).is_err(),
                "redis URL {url:?} must be refused rather than silently reinterpreted"
            );
        }
    }

    #[test]
    fn database_zero_is_the_one_selector_the_provider_does_express() {
        let vars =
            deployment_environment("chirp", &services(Some("redis://127.0.0.1:6379/0"), None))
                .unwrap();
        assert_eq!(lookup(&vars, PROVIDER_REDIS_HOST_VAR), Some("127.0.0.1"));
    }

    #[test]
    fn storage_dir_is_scoped_to_the_deployment_and_created_on_export() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("storage");
        let vars = export_deployment_environment("chirp", &services(None, Some(&root))).unwrap();

        let expected = root.join("chirp");
        assert_eq!(
            lookup(&vars, STORAGE_DIR_VAR),
            Some(expected.to_str().unwrap())
        );
        assert!(
            expected.is_dir(),
            "the host owns the storage root; the application only owns keys under it"
        );
        assert_eq!(
            std::env::var(STORAGE_DIR_VAR).unwrap(),
            expected.display().to_string()
        );
        assert_eq!(std::env::var(REDIS_PREFIX_VAR).unwrap(), "coop:chirp:");
    }

    #[test]
    fn a_bad_redis_url_refuses_the_whole_environment() {
        // Half-configured is worse than refused: a deployment that came up
        // with storage but no kv would fail later, in application code.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("storage");
        assert!(deployment_environment(
            "chirp",
            &services(Some("redis://127.0.0.1:6379/3"), Some(&root))
        )
        .is_err());
    }
}
