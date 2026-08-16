//! Trusted-proxy header extraction.
//!
//! When a request arrives via the origin port (`:8081`) from a validated
//! Bunny edge IP, we trust `X-Forwarded-For`, `X-Forwarded-Proto`, and
//! `X-Forwarded-Host` headers. These let handlers see real client IPs and
//! the original scheme/host, even though the Bunny→coop hop is plain HTTP.
//!
//! When a request arrives via the public ports (`:80`/`:443`) directly
//! from a client, X-Forwarded headers are IGNORED to prevent spoofing.
//!
//! This module extracts the forwarded values and returns them as a
//! `ProxyInfo` struct that the listener uses when building the
//! `DeploymentRequest`.

use std::net::IpAddr;

/// Extracted proxy information from trusted headers.
#[derive(Debug, Clone)]
pub struct ProxyInfo {
    /// The real client IP (from X-Forwarded-For), or the direct peer IP
    /// if no trusted proxy is in the path.
    pub client_ip: IpAddr,
    /// The original scheme (http or https) from X-Forwarded-Proto, or
    /// the connection's actual scheme.
    pub scheme: String,
    /// The original Host header from X-Forwarded-Host, or the direct
    /// Host header.
    pub host: String,
}

/// Extract proxy info from headers when the source is a trusted proxy.
/// `peer_ip` is the direct TCP connection's IP. `trusted` indicates
/// whether we should trust X-Forwarded-* headers from this peer.
pub fn extract(headers: &axum::http::HeaderMap, peer_ip: IpAddr, trusted: bool) -> ProxyInfo {
    if !trusted {
        return ProxyInfo {
            client_ip: peer_ip,
            scheme: "http".to_string(),
            host: headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string(),
        };
    }

    // X-Forwarded-For: client, proxy1, proxy2
    // The leftmost entry is the real client; each successive one is a
    // proxy that forwarded the request. We take the first.
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer_ip);

    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http")
        .to_string();

    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
        })
        .unwrap_or("")
        .to_string();

    ProxyInfo {
        client_ip,
        scheme,
        host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn untrusted_ignores_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("host", "example.com".parse().unwrap());

        let info = extract(&headers, "10.0.0.1".parse().unwrap(), false);
        assert_eq!(info.client_ip, "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(info.scheme, "http");
        assert_eq!(info.host, "example.com");
    }

    #[test]
    fn trusted_reads_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 10.0.0.1".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("x-forwarded-host", "chirp.io".parse().unwrap());
        headers.insert("host", "origin.box.internal".parse().unwrap());

        let info = extract(&headers, "10.0.0.1".parse().unwrap(), true);
        assert_eq!(info.client_ip, "1.2.3.4".parse::<IpAddr>().unwrap());
        assert_eq!(info.scheme, "https");
        assert_eq!(info.host, "chirp.io");
    }

    #[test]
    fn trusted_falls_back_when_headers_missing() {
        let headers = HeaderMap::new();
        let info = extract(&headers, "10.0.0.1".parse().unwrap(), true);
        assert_eq!(info.client_ip, "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(info.scheme, "http");
        assert_eq!(info.host, "");
    }
}
