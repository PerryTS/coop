//! Bunny.net REST API client.
//!
//! Covers the subset of Bunny's API that Perch needs:
//!
//! - Pull Zone: create, get by name, delete
//! - Custom Hostname: add to a Pull Zone (Bunny auto-provisions TLS)
//! - Cache: purge all for a Pull Zone
//!
//! API docs: https://docs.bunny.net/reference/pullzonepublic_index
//!
//! Authentication: `AccessKey: <api_key>` header on every request.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

const BUNNY_API_BASE: &str = "https://api.bunny.net";

/// A Bunny Pull Zone, as returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PullZone {
    pub id: i64,
    pub name: String,
    pub origin_url: Option<String>,
    #[serde(default)]
    pub hostnames: Vec<Hostname>,
    pub enabled: Option<bool>,
}

/// A custom hostname attached to a Pull Zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Hostname {
    pub id: Option<i64>,
    pub value: String,
    pub force_s_s_l: Option<bool>,
    pub is_system_hostname: Option<bool>,
    pub has_certificate: Option<bool>,
}

/// Request body for creating a Pull Zone.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct CreatePullZoneRequest {
    name: String,
    origin_url: String,
    /// 0 = Standard, 1 = Volume
    #[serde(rename = "Type")]
    zone_type: u8,
}

/// Request body for adding a custom hostname.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct AddHostnameRequest {
    hostname: String,
}

pub struct BunnyClient {
    api_key: String,
    http: reqwest::Client,
}

impl BunnyClient {
    pub fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }

    /// List all Pull Zones (paginated, but Perch will have few enough
    /// that a single page is sufficient).
    pub async fn list_pull_zones(&self) -> Result<Vec<PullZone>> {
        let url = format!("{}/pullzone", BUNNY_API_BASE);
        let resp = self
            .http
            .get(&url)
            .header("AccessKey", &self.api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .context("listing pull zones")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list pull zones: HTTP {} — {}",
                status,
                body
            ));
        }

        resp.json::<Vec<PullZone>>()
            .await
            .context("parsing pull zone list")
    }

    /// Find a Pull Zone by name. Returns `None` if no zone with that name
    /// exists. Scans the full list (small for Perch — one zone per
    /// deployment, ~10-50 max).
    pub async fn get_pull_zone_by_name(&self, name: &str) -> Result<Option<PullZone>> {
        let zones = self.list_pull_zones().await?;
        Ok(zones.into_iter().find(|z| z.name == name))
    }

    /// Create a new Pull Zone pointing at the given origin.
    pub async fn create_pull_zone(
        &self,
        name: &str,
        origin_host: &str,
        origin_port: u16,
    ) -> Result<PullZone> {
        let origin_url = format!("http://{}:{}", origin_host, origin_port);
        let body = CreatePullZoneRequest {
            name: name.to_string(),
            origin_url,
            zone_type: 0, // Standard
        };

        debug!(
            name = name,
            origin = %body.origin_url,
            "creating Pull Zone"
        );

        let resp = self
            .http
            .post(&format!("{}/pullzone", BUNNY_API_BASE))
            .header("AccessKey", &self.api_key)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .context("creating pull zone")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "create pull zone '{}': HTTP {} — {}",
                name,
                status,
                body
            ));
        }

        resp.json::<PullZone>()
            .await
            .context("parsing created pull zone")
    }

    /// Delete a Pull Zone by ID.
    pub async fn delete_pull_zone(&self, zone_id: i64) -> Result<()> {
        let resp = self
            .http
            .delete(&format!("{}/pullzone/{}", BUNNY_API_BASE, zone_id))
            .header("AccessKey", &self.api_key)
            .send()
            .await
            .context("deleting pull zone")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "delete pull zone {}: HTTP {} — {}",
                zone_id,
                status,
                body
            ));
        }
        Ok(())
    }

    /// Add a custom hostname to a Pull Zone. Bunny will auto-provision a
    /// Let's Encrypt certificate for the hostname once DNS is pointed at
    /// the Pull Zone's CDN URL.
    pub async fn add_hostname(&self, zone_id: i64, hostname: &str) -> Result<()> {
        let body = AddHostnameRequest {
            hostname: hostname.to_string(),
        };

        let resp = self
            .http
            .post(&format!(
                "{}/pullzone/{}/addHostname",
                BUNNY_API_BASE, zone_id
            ))
            .header("AccessKey", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("adding hostname")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "add hostname '{}' to zone {}: HTTP {} — {}",
                hostname,
                zone_id,
                status,
                body
            ));
        }
        Ok(())
    }

    /// Remove a custom hostname from a Pull Zone.
    pub async fn remove_hostname(&self, zone_id: i64, hostname: &str) -> Result<()> {
        #[derive(Serialize)]
        #[serde(rename_all = "PascalCase")]
        struct Body {
            hostname: String,
        }

        let resp = self
            .http
            .delete(&format!(
                "{}/pullzone/{}/removeHostname",
                BUNNY_API_BASE, zone_id
            ))
            .header("AccessKey", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&Body {
                hostname: hostname.to_string(),
            })
            .send()
            .await
            .context("removing hostname")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "remove hostname '{}' from zone {}: HTTP {} — {}",
                hostname,
                zone_id,
                status,
                body
            ));
        }
        Ok(())
    }

    /// Purge the entire cache for a Pull Zone. Called after every deploy
    /// so updated static files are served immediately.
    pub async fn purge_cache(&self, zone_id: i64) -> Result<()> {
        let resp = self
            .http
            .post(&format!(
                "{}/pullzone/{}/purgeCache",
                BUNNY_API_BASE, zone_id
            ))
            .header("AccessKey", &self.api_key)
            .send()
            .await
            .context("purging cache")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "purge cache for zone {}: HTTP {} — {}",
                zone_id,
                status,
                body
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_zone_deserializes() {
        let json = r#"{
            "Id": 123,
            "Name": "perch-landing",
            "OriginUrl": "http://1.2.3.4:8081",
            "Enabled": true,
            "Hostnames": [
                {
                    "Id": 456,
                    "Value": "perch-landing.b-cdn.net",
                    "ForceSSL": false,
                    "IsSystemHostname": true,
                    "HasCertificate": true
                }
            ]
        }"#;
        let zone: PullZone = serde_json::from_str(json).unwrap();
        assert_eq!(zone.id, 123);
        assert_eq!(zone.name, "perch-landing");
        assert_eq!(zone.hostnames.len(), 1);
        assert_eq!(zone.hostnames[0].value, "perch-landing.b-cdn.net");
    }
}
