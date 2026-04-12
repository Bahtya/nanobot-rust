//! SSRF protection — block requests to private/internal IP ranges.

use anyhow::{Context, Result};
use ipnet::IpNet;
use std::net::IpAddr;
use tracing::debug;
use url::Url;

/// Networks that are always blocked for SSRF protection.
const BLOCKED_NETWORKS: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "192.0.2.0/24",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
    "::1/128",
    "fc00::/7",
    "fe80::/10",
    "ff00::/8",
    "169.254.169.254/32",
];

/// SSRF protection checker.
#[derive(Debug, Clone)]
pub struct SsrfGuard {
    blocked_nets: Vec<IpNet>,
    whitelist_nets: Vec<IpNet>,
}

impl SsrfGuard {
    /// Create a new SsrfGuard with default blocked networks.
    pub fn new() -> Self {
        let blocked: Vec<IpNet> = BLOCKED_NETWORKS
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        Self {
            blocked_nets: blocked,
            whitelist_nets: Vec::new(),
        }
    }

    /// Add a network to the whitelist.
    pub fn add_whitelist(&mut self, cidr: &str) -> Result<()> {
        let net: IpNet = cidr
            .parse()
            .with_context(|| format!("Invalid CIDR: {}", cidr))?;
        self.whitelist_nets.push(net);
        debug!("Added SSRF whitelist: {}", cidr);
        Ok(())
    }

    /// Add multiple networks to the whitelist.
    pub fn add_whitelists(&mut self, cidrs: &[String]) -> Result<()> {
        for cidr in cidrs {
            self.add_whitelist(cidr)?;
        }
        Ok(())
    }

    /// Check if an IP address is allowed.
    pub fn is_ip_allowed(&self, ip: &IpAddr) -> bool {
        if self.whitelist_nets.iter().any(|net| net.contains(ip)) {
            return true;
        }
        if self.blocked_nets.iter().any(|net| net.contains(ip)) {
            debug!("Blocked IP: {}", ip);
            return false;
        }
        true
    }

    /// Validate a URL for SSRF safety.
    pub fn validate_url(&self, url_str: &str) -> Result<()> {
        let url = Url::parse(url_str).with_context(|| format!("Invalid URL: {}", url_str))?;

        let host = url.host_str().context("URL has no host")?;

        if let Ok(ip) = host.parse::<IpAddr>() {
            if !self.is_ip_allowed(&ip) {
                anyhow::bail!("SSRF blocked: URL resolves to blocked IP {}", ip);
            }
            return Ok(());
        }

        if is_internal_hostname(host) {
            anyhow::bail!("SSRF blocked: hostname '{}' appears to be internal", host);
        }

        Ok(())
    }

    /// Check if a string contains internal/private URLs.
    pub fn contains_internal_urls(&self, text: &str) -> bool {
        // Simple check for common internal URL patterns
        let patterns = [
            "http://localhost",
            "http://127.0.0.",
            "http://10.",
            "http://192.168.",
        ];
        for pattern in patterns {
            if text.contains(pattern) {
                return true;
            }
        }
        false
    }
}

impl Default for SsrfGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if a hostname looks like an internal address.
fn is_internal_hostname(host: &str) -> bool {
    let lower = host.to_lowercase();
    let suffixes = [".local", ".internal", ".localhost", ".intranet"];
    lower == "localhost" || suffixes.iter().any(|s| lower.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_private_ipv4() {
        let guard = SsrfGuard::new();
        assert!(!guard.is_ip_allowed(&"127.0.0.1".parse().unwrap()));
        assert!(!guard.is_ip_allowed(&"10.0.0.1".parse().unwrap()));
        assert!(!guard.is_ip_allowed(&"172.16.0.1".parse().unwrap()));
        assert!(!guard.is_ip_allowed(&"192.168.1.1".parse().unwrap()));
        assert!(!guard.is_ip_allowed(&"169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn test_allow_public_ipv4() {
        let guard = SsrfGuard::new();
        assert!(guard.is_ip_allowed(&"8.8.8.8".parse().unwrap()));
        assert!(guard.is_ip_allowed(&"1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn test_whitelist_overrides() {
        let mut guard = SsrfGuard::new();
        guard.add_whitelist("100.64.0.0/10").unwrap();
        let tailscale_ip: IpAddr = "100.100.100.100".parse().unwrap();
        assert!(guard.is_ip_allowed(&tailscale_ip));
        assert!(!guard.is_ip_allowed(&"10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_validate_url_public() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("https://example.com").is_ok());
    }

    #[test]
    fn test_validate_url_private() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("http://127.0.0.1:8080").is_err());
        assert!(guard.validate_url("http://localhost:3000").is_err());
    }

    #[test]
    fn test_internal_hostnames() {
        assert!(is_internal_hostname("localhost"));
        assert!(is_internal_hostname("my.server.local"));
        assert!(!is_internal_hostname("example.com"));
    }

    #[test]
    fn test_block_ipv6_loopback() {
        let guard = SsrfGuard::new();
        let loopback: IpAddr = "::1".parse().unwrap();
        assert!(!guard.is_ip_allowed(&loopback));
    }

    #[test]
    fn test_block_ipv6_link_local() {
        let guard = SsrfGuard::new();
        let link_local: IpAddr = "fe80::1".parse().unwrap();
        assert!(!guard.is_ip_allowed(&link_local));
    }

    #[test]
    fn test_allow_public_ipv6() {
        let guard = SsrfGuard::new();
        // Google's public DNS over IPv6
        let public: IpAddr = "2001:4860:4860::8888".parse().unwrap();
        assert!(guard.is_ip_allowed(&public));
    }

    #[test]
    fn test_contains_internal_urls() {
        let guard = SsrfGuard::new();
        assert!(guard.contains_internal_urls("check http://localhost:3000"));
        assert!(guard.contains_internal_urls("connect to http://127.0.0.1/api"));
        assert!(guard.contains_internal_urls("http://10.0.0.1/secret"));
        assert!(guard.contains_internal_urls("http://192.168.1.1/router"));
    }

    #[test]
    fn test_not_internal_urls() {
        let guard = SsrfGuard::new();
        assert!(!guard.contains_internal_urls("https://example.com/page"));
        assert!(!guard.contains_internal_urls("visit https://google.com"));
        assert!(!guard.contains_internal_urls("just some text"));
    }

    #[test]
    fn test_validate_url_invalid() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("not-a-url").is_err());
        assert!(guard.validate_url("://missing-scheme").is_err());
    }

    #[test]
    fn test_ssrf_guard_default() {
        let guard = SsrfGuard::default();
        // Should behave the same as SsrfGuard::new()
        assert!(!guard.is_ip_allowed(&"127.0.0.1".parse().unwrap()));
        assert!(guard.is_ip_allowed(&"8.8.8.8".parse().unwrap()));
    }
}
