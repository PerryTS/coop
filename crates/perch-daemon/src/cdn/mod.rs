//! Bunny CDN integration — automatic Pull Zone management, cache purging,
//! and trusted-proxy IP allowlist.
//!
//! When `runtime.toml` has `[cdn.bunny] api_key = "..."`, the daemon
//! automatically creates/updates Bunny Pull Zones for each deployment that
//! hasn't opted out (`[cdn] enabled = false` in `perch.toml`). The user
//! never touches Bunny configuration — Ralph drops his API key once and
//! every deployment gets CDN-fronted static serving automatically.
//!
//! The module is split into three sub-files:
//!
//! - `bunny.rs` — REST API client (Pull Zone CRUD, custom hostname, purge)
//! - `ip_allowlist.rs` — edge IP list auto-fetch with 24h refresh
//! - Module-level `reconcile()` ties them together for deployment lifecycle

pub mod bunny;
pub mod ip_allowlist;

use crate::config::{BunnyConfig, DeploymentConfig, RuntimeConfig};
use anyhow::Result;
use tracing::{debug, error, info, warn};

/// Check whether Bunny CDN is configured at the box level.
pub fn is_bunny_enabled(config: &RuntimeConfig) -> bool {
    config.cdn.bunny.is_some()
}

/// Check whether a specific deployment should be Bunny-fronted.
pub fn should_use_bunny(runtime_cfg: &RuntimeConfig, deployment: &DeploymentConfig) -> bool {
    is_bunny_enabled(runtime_cfg) && deployment.cdn.enabled
}

/// Reconcile a deployment's Bunny Pull Zone after it's been loaded or
/// reloaded. Creates the Pull Zone if it doesn't exist, updates custom
/// hostnames, and logs DNS instructions for any hostname not yet pointed
/// at the CDN URL.
///
/// Called by the deployment supervisor after a successful deploy.
pub async fn reconcile_deployment(
    bunny_cfg: &BunnyConfig,
    deployment: &DeploymentConfig,
    origin_host: &str,
    origin_port: u16,
) -> Result<()> {
    let client = bunny::BunnyClient::new(&bunny_cfg.api_key);
    let zone_name = format!("perch-{}", deployment.name);

    // Step 1: Ensure the Pull Zone exists.
    let zone = match client.get_pull_zone_by_name(&zone_name).await? {
        Some(z) => {
            debug!(
                deployment = %deployment.name,
                zone_id = z.id,
                "Pull Zone already exists"
            );
            z
        }
        None => {
            info!(
                deployment = %deployment.name,
                zone_name = %zone_name,
                "creating Bunny Pull Zone"
            );
            client
                .create_pull_zone(&zone_name, origin_host, origin_port)
                .await?
        }
    };

    // Step 2: Ensure custom hostnames are registered.
    for domain in &deployment.hosts.domains {
        if domain.is_empty() {
            continue;
        }
        let already = zone
            .hostnames
            .iter()
            .any(|h| h.value.eq_ignore_ascii_case(domain));
        if already {
            debug!(
                deployment = %deployment.name,
                domain = %domain,
                "hostname already on Pull Zone"
            );
        } else {
            info!(
                deployment = %deployment.name,
                domain = %domain,
                zone_id = zone.id,
                "adding custom hostname to Pull Zone"
            );
            if let Err(e) = client.add_hostname(zone.id, domain).await {
                warn!(
                    deployment = %deployment.name,
                    domain = %domain,
                    error = ?e,
                    "failed to add hostname (may need DNS verification)"
                );
            }
        }

        // Log DNS instruction if the hostname is not yet pointed at Bunny.
        let cdn_url = format!("{}.b-cdn.net", zone_name);
        info!(
            deployment = %deployment.name,
            domain = %domain,
            cdn_url = %cdn_url,
            "→ To activate CDN: point {} CNAME to {}",
            domain,
            cdn_url
        );
    }

    Ok(())
}

/// Purge the CDN cache for a deployment after a redeploy. Clears all
/// cached content so updated static files are served immediately.
pub async fn purge_deployment(bunny_cfg: &BunnyConfig, deployment_name: &str) -> Result<()> {
    let client = bunny::BunnyClient::new(&bunny_cfg.api_key);
    let zone_name = format!("perch-{}", deployment_name);

    match client.get_pull_zone_by_name(&zone_name).await? {
        Some(zone) => {
            info!(
                deployment = deployment_name,
                zone_id = zone.id,
                "purging CDN cache"
            );
            client.purge_cache(zone.id).await?;
        }
        None => {
            debug!(
                deployment = deployment_name,
                "no Pull Zone found, nothing to purge"
            );
        }
    }
    Ok(())
}

/// Tear down the Pull Zone for a removed deployment.
pub async fn teardown_deployment(bunny_cfg: &BunnyConfig, deployment_name: &str) -> Result<()> {
    let client = bunny::BunnyClient::new(&bunny_cfg.api_key);
    let zone_name = format!("perch-{}", deployment_name);

    match client.get_pull_zone_by_name(&zone_name).await? {
        Some(zone) => {
            info!(
                deployment = deployment_name,
                zone_id = zone.id,
                "deleting Bunny Pull Zone"
            );
            client.delete_pull_zone(zone.id).await?;
        }
        None => {
            debug!(
                deployment = deployment_name,
                "no Pull Zone found, nothing to tear down"
            );
        }
    }
    Ok(())
}
