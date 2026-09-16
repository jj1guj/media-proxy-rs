//! SSRF policy shared by the fail-fast pre-check (`check_url`) and the
//! connect-time enforcement resolver (`ValidatingResolver`).
//!
//! Background (finding #2, DNS rebinding / TOCTOU): validating a hostname's
//! addresses *before* fetching is not sufficient, because the HTTP client
//! resolves the same hostname *again* when connecting. An attacker running
//! the authoritative nameserver (TTL 0) can answer the first query with a
//! public IP and the second with `127.0.0.1`, bypassing even the RFC1918
//! block. `ValidatingResolver` closes this by validating the very addresses
//! the connection is opened to: hyper-util calls the configured
//! `reqwest::dns::Resolve` implementation for every hostname connect (IP
//! literals skip DNS entirely, so they cannot be rebound), which makes the
//! pre-check vs connect resolutions a non-issue.

use std::sync::Arc;

use crate::{DnsCache, NetworkPolicy};

/// `reqwest::dns::Resolve` implementation that validates the addresses a
/// connection is actually opened to (finding #2, DNS rebinding / TOCTOU).
#[derive(Clone)]
pub(crate) struct ValidatingResolver {
    cache: Arc<DnsCache>,
    policy: Arc<NetworkPolicy>,
    /// Hostname of the configured egress proxy, if any. With an HTTP proxy the
    /// target host is never resolved locally; only the proxy host goes through
    /// this resolver, and it must not be subjected to the SSRF policy.
    proxy_host: Option<String>,
}

impl ValidatingResolver {
    pub(crate) fn new(
        cache: Arc<DnsCache>,
        policy: Arc<NetworkPolicy>,
        proxy: Option<&str>,
    ) -> Self {
        let proxy_host = proxy
            .and_then(|url| reqwest::Url::parse(url).ok())
            .and_then(|url| url.host_str().map(NetworkPolicy::normalize_host));
        Self {
            cache,
            policy,
            proxy_host,
        }
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl reqwest::dns::Resolve for ValidatingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let cache = self.cache.clone();
        let policy = self.policy.clone();
        let proxy_host = self.proxy_host.clone();
        Box::pin(async move {
            let (ips, _hit) = cache
                .resolve(&host, 0)
                .await
                .map_err(|error| -> BoxError { error.into() })?;
            let is_proxy = proxy_host
                .as_ref()
                .is_some_and(|proxy_host| NetworkPolicy::normalize_host(&host) == *proxy_host);
            if !is_proxy {
                if policy.is_host_blocked(&host) || ips.is_empty() {
                    let error: BoxError = "Blocked address".into();
                    return Err(error);
                }
                for ip in &ips {
                    policy
                        .check_ip(*ip)
                        .map_err(|error| -> BoxError { error.into() })?;
                }
            }
            let addrs: reqwest::dns::Addrs =
                Box::new(ips.into_iter().map(|ip| std::net::SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::dns::Resolve;
    use std::time::Duration;

    fn test_policy() -> NetworkPolicy {
        NetworkPolicy {
            ipv4_blocked_default: NetworkPolicy::parse_ranges(
                &[
                    "10.0.0.0/8".to_owned(),
                    "172.16.0.0/12".to_owned(),
                    "192.168.0.0/16".to_owned(),
                    "127.0.0.0/8".to_owned(),
                    "169.254.0.0/16".to_owned(),
                    "100.64.0.0/10".to_owned(),
                    "0.0.0.0/8".to_owned(),
                ],
                "test",
            )
            .unwrap(),
            allowed_networks: None,
            blocked_networks: None,
            blocked_hosts: Default::default(),
        }
    }

    #[test]
    fn resolver_blocks_rebinding_target() {
        // localhost must fail at resolve time even though "resolving" succeeds.
        let cache = Arc::new(DnsCache::new(
            Duration::from_secs(300),
            Duration::from_secs(10),
            Duration::from_secs(4),
            16,
        ));
        let policy = Arc::new(test_policy());
        let r = ValidatingResolver::new(cache, policy, None);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let name: reqwest::dns::Name = "localhost".parse().unwrap();
            assert!(r.resolve(name).await.is_err());
        });
    }
}
