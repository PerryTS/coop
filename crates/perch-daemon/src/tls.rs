//! TLS integration — ACME (Let's Encrypt), manual certs, or off.
//!
//! When `tls.mode = "acme"` in runtime.toml, the daemon uses `rustls-acme`
//! to automatically provision and renew Let's Encrypt certificates for all
//! non-Bunny-fronted hostnames. TLS-ALPN-01 challenges are handled inline
//! on the `:443` listener (no separate port-80 challenge path needed,
//! though we keep `:80` for HTTP→HTTPS redirects).
//!
//! The ACME state (account key + issued certificates) is cached in
//! `acme_cache_dir` so restarts don't re-issue. The first request to a
//! new hostname triggers a background cert request; the ALPN challenge
//! completes within ~5 seconds for Let's Encrypt production and ~1 second
//! for staging.
//!
//! When `tls.mode = "manual"`, static PEM files from runtime.toml are
//! loaded. When `tls.mode = "off"`, TLS is disabled (development mode or
//! behind a CDN/proxy that terminates TLS).
//!
//! Checkpoint 3 ships the ACME integration. Checkpoint 4 adds the Bunny
//! CDN awareness that lets the daemon skip ACME for Bunny-fronted
//! hostnames (Bunny handles TLS at the edge for those).

use crate::config::{RuntimeConfig, TlsMode};
use anyhow::{Context, Result};
use std::sync::Arc;

/// Check whether TLS is enabled and valid. Returns a descriptive string
/// for logging.
pub fn describe_tls_config(config: &RuntimeConfig) -> String {
    match config.tls.mode {
        TlsMode::Off => "TLS disabled (mode=off)".to_string(),
        TlsMode::Acme => {
            let contact = config.tls.acme_contact.as_deref().unwrap_or("(none)");
            let dir = config
                .tls
                .acme_directory
                .as_deref()
                .unwrap_or("https://acme-v02.api.letsencrypt.org/directory");
            format!("ACME (contact={}, directory={})", contact, dir)
        }
        TlsMode::Manual => {
            let cert = config
                .tls
                .tls_cert
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(missing)".to_string());
            format!("Manual (cert={})", cert)
        }
    }
}

/// Validate the TLS configuration. Errors early if required fields are
/// missing for the selected mode.
pub fn validate_tls_config(config: &RuntimeConfig) -> Result<()> {
    match config.tls.mode {
        TlsMode::Off => Ok(()),
        TlsMode::Acme => {
            if config.tls.acme_contact.is_none() {
                anyhow::bail!(
                    "tls.mode = \"acme\" requires tls.acme_contact \
                     (e.g. \"mailto:admin@example.com\")"
                );
            }
            std::fs::create_dir_all(&config.paths.acme_cache_dir).with_context(|| {
                format!("creating ACME cache dir {:?}", config.paths.acme_cache_dir)
            })?;
            Ok(())
        }
        TlsMode::Manual => {
            let cert =
                config.tls.tls_cert.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("tls.mode = \"manual\" requires tls.tls_cert")
                })?;
            let key = config
                .tls
                .tls_key
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("tls.mode = \"manual\" requires tls.tls_key"))?;
            if !cert.exists() {
                anyhow::bail!("tls_cert {:?} does not exist", cert);
            }
            if !key.exists() {
                anyhow::bail!("tls_key {:?} does not exist", key);
            }
            Ok(())
        }
    }
}

// ── ACME listener integration (actual rustls-acme wiring) ──
//
// rustls-acme provides an `AcmeConfig` builder that wraps a rustls
// `ServerConfig`. We build the config here; the listener module uses it
// to construct the HTTPS acceptor.
//
// The full integration flow:
//
// 1. At daemon startup, build `AcmeConfig` with the ACME directory URL,
//    contact email, and cache directory.
// 2. For each loaded deployment, collect the non-Bunny hostnames.
// 3. Feed them as `domains` into `AcmeConfig`.
// 4. Build an `AcmeAcceptor` that wraps a `TcpListener` on `:443`.
// 5. The acceptor handles TLS-ALPN-01 challenges inline; all other TLS
//    connections complete normally and feed into axum.
//
// Placeholder — the actual `AcmeAcceptor` integration is wired in
// listener.rs in a follow-up within this checkpoint. The function below
// just creates the config; integrating it into axum's listener requires
// swapping from `axum::serve(TcpListener)` to a manual accept loop that
// feeds connections through the ACME acceptor.

/// Collect all non-Bunny hostnames from the current deployment set. These
/// are the domains perch's own ACME manages certs for.
pub fn collect_acme_domains(
    config: &RuntimeConfig,
    deployments: &[crate::config::DeploymentConfig],
) -> Vec<String> {
    let bunny_enabled = config.cdn.bunny.is_some();
    let mut domains = Vec::new();
    for dep in deployments {
        let dep_bunny = bunny_enabled && dep.cdn.enabled;
        if dep_bunny {
            // Bunny handles TLS at the edge for this deployment's domains.
            continue;
        }
        for d in &dep.hosts.domains {
            if !d.is_empty() {
                domains.push(d.to_lowercase());
            }
        }
    }
    domains.sort();
    domains.dedup();
    domains
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

    fn minimal_config() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    #[test]
    fn describe_off() {
        let cfg = minimal_config();
        assert!(describe_tls_config(&cfg).contains("disabled"));
    }

    #[test]
    fn validate_acme_requires_contact() {
        let mut cfg = minimal_config();
        cfg.tls.mode = TlsMode::Acme;
        let err = validate_tls_config(&cfg).unwrap_err();
        assert!(err.to_string().contains("acme_contact"));
    }

    #[test]
    fn collect_acme_domains_skips_bunny() {
        let mut cfg = minimal_config();
        cfg.cdn.bunny = Some(BunnyConfig {
            api_key: "test".to_string(),
            default_cache_duration_secs: 86400,
        });

        let deps = vec![
            DeploymentConfig {
                name: "landing".to_string(),
                hosts: HostsConfig {
                    domains: vec!["landing.com".to_string()],
                },
                cdn: DeploymentCdnConfig { enabled: true },
                ..Default::default()
            },
            DeploymentConfig {
                name: "api".to_string(),
                hosts: HostsConfig {
                    domains: vec!["api.example.com".to_string()],
                },
                cdn: DeploymentCdnConfig { enabled: false },
                ..Default::default()
            },
        ];

        let domains = collect_acme_domains(&cfg, &deps);
        // landing.com is Bunny-fronted (opted in) → skipped
        // api.example.com opted out of CDN → managed by ACME
        assert_eq!(domains, vec!["api.example.com".to_string()]);
    }
}
