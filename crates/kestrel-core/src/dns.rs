//! DNS resolver with fallback for systems without `/etc/resolv.conf` (e.g. Android Termux).

use std::net::SocketAddr;
use std::sync::Arc;

use hickory_resolver::config::{LookupIpStrategy, ResolverConfig};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::TokioResolver;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// DNS resolver that wraps hickory-resolver with a fallback to Google DNS.
///
/// On most systems hickory reads `/etc/resolv.conf` for nameserver config.
/// On Android Termux that file doesn't exist, so we fall back to
/// `ResolverConfig::default()` (Google public DNS: 8.8.8.8, 8.8.4.4).
pub struct DnsResolver {
    inner: Arc<TokioResolver>,
}

impl Default for DnsResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl DnsResolver {
    pub fn new() -> Self {
        let resolver = TokioResolver::builder_tokio()
            .map(|mut b| {
                b.options_mut().ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
                b.build()
            })
            .unwrap_or_else(|_| {
                let mut b = TokioResolver::builder_with_config(
                    ResolverConfig::default(),
                    TokioConnectionProvider::default(),
                );
                b.options_mut().ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
                b.build()
            });
        Self {
            inner: Arc::new(resolver),
        }
    }
}

impl Resolve for DnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let inner = self.inner.clone();
        Box::pin(async move {
            let lookup = inner.lookup_ip(name.as_str()).await?;
            let addrs: Addrs = Box::new(lookup.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

/// Build a shared DNS resolver suitable for injection into reqwest clients.
pub fn build_dns_resolver() -> Arc<DnsResolver> {
    Arc::new(DnsResolver::new())
}
