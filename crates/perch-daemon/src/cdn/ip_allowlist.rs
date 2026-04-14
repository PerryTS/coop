//! Bunny edge IP allowlist for trusted-proxy header enforcement.
//!
//! Perch's origin listener (`:8081`) serves Bunny's pull traffic. To
//! prevent `X-Forwarded-For` spoofing, we only trust those headers when
//! the source IP is a known Bunny edge server.
//!
//! The allowlist is fetched from Bunny's public API on daemon startup and
//! refreshed every 24 hours. The list is typically ~100-300 IPs. We store
//! them as a `HashSet<IpAddr>` behind an `ArcSwap` for lock-free reads on
//! the hot path.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

/// URL that returns a newline-separated list of Bunny edge IPs.
const BUNNY_EDGE_IPS_URL: &str = "https://bunnycdn.com/api/system/edgeserverlist";

/// How often to refresh the IP list.
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Shared allowlist state. The listener checks `contains()` on every
/// request that arrives on the origin port.
#[derive(Clone)]
pub struct EdgeIpAllowlist {
    rx: watch::Receiver<Arc<HashSet<IpAddr>>>,
}

impl EdgeIpAllowlist {
    /// Create a new allowlist and start the background refresh task.
    /// The first fetch happens synchronously (blocking the caller) so
    /// the daemon starts with a populated list. Subsequent refreshes
    /// happen in the background.
    pub async fn start() -> Result<Self> {
        let initial = fetch_edge_ips().await.unwrap_or_else(|e| {
            warn!(error = ?e, "initial Bunny edge IP fetch failed, starting with empty allowlist");
            HashSet::new()
        });

        info!(count = initial.len(), "Bunny edge IP allowlist loaded");

        let (tx, rx) = watch::channel(Arc::new(initial));

        // Background refresh task.
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(REFRESH_INTERVAL);
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                match fetch_edge_ips().await {
                    Ok(ips) => {
                        info!(count = ips.len(), "refreshed Bunny edge IP allowlist");
                        let _ = tx.send(Arc::new(ips));
                    }
                    Err(e) => {
                        error!(error = ?e, "failed to refresh Bunny edge IPs, keeping previous list");
                    }
                }
            }
        });

        Ok(Self { rx })
    }

    /// Check whether an IP is a known Bunny edge server.
    pub fn contains(&self, ip: &IpAddr) -> bool {
        self.rx.borrow().contains(ip)
    }

    /// Current number of IPs in the allowlist.
    pub fn len(&self) -> usize {
        self.rx.borrow().len()
    }

    /// Create an allowlist from a static set (for testing).
    #[cfg(test)]
    pub fn from_static(ips: HashSet<IpAddr>) -> Self {
        let (_, rx) = watch::channel(Arc::new(ips));
        Self { rx }
    }
}

/// Fetch the current edge IP list from Bunny's public API.
async fn fetch_edge_ips() -> Result<HashSet<IpAddr>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("building HTTP client for edge IP fetch")?;

    let resp = client
        .get(BUNNY_EDGE_IPS_URL)
        .send()
        .await
        .context("fetching Bunny edge IP list")?;

    if !resp.status().is_success() {
        anyhow::bail!(
            "Bunny edge IP list: HTTP {}",
            resp.status()
        );
    }

    let body = resp.text().await.context("reading edge IP body")?;

    let mut ips = HashSet::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed.parse::<IpAddr>() {
            Ok(ip) => {
                ips.insert(ip);
            }
            Err(_) => {
                // Some entries might be CIDR ranges (e.g. "1.2.3.0/24").
                // For the MVP we only match exact IPs; CIDR matching can
                // be added later if Bunny's list includes ranges.
                debug!(entry = trimmed, "skipping non-IP entry in edge list");
            }
        }
    }

    Ok(ips)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ip_list() {
        // Simulate the format Bunny returns.
        let body = "1.2.3.4\n5.6.7.8\n \n9.10.11.12\n";
        let mut ips = HashSet::new();
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(ip) = trimmed.parse::<IpAddr>() {
                ips.insert(ip);
            }
        }
        assert_eq!(ips.len(), 3);
        assert!(ips.contains(&"1.2.3.4".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn static_allowlist_works() {
        let mut set = HashSet::new();
        set.insert("10.0.0.1".parse::<IpAddr>().unwrap());
        let allowlist = EdgeIpAllowlist::from_static(set);
        assert!(allowlist.contains(&"10.0.0.1".parse().unwrap()));
        assert!(!allowlist.contains(&"10.0.0.2".parse().unwrap()));
    }
}
