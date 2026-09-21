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
//!
//! Caveat: when `config.proxy` is set, the proxy resolves and connects to the
//! target. This resolver sees only the proxy host, so only the independent URL
//! pre-check protects the target from SSRF. The proxy must enforce an equivalent
//! policy to close the DNS-rebinding TOCTOU in that configuration.

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

fn parse_proxy_url(url: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(url)
        .ok()
        .or_else(|| reqwest::Url::parse(&format!("http://{}", url)).ok())
}

impl ValidatingResolver {
    pub(crate) fn new(
        cache: Arc<DnsCache>,
        policy: Arc<NetworkPolicy>,
        proxy: Option<&str>,
    ) -> Self {
        let proxy_host = proxy
            .and_then(parse_proxy_url)
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
            // The proxy itself is not a fetch target. This bypass also means the
            // target is outside connect-time validation when proxying is enabled.
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
        let (ipv4_blocked_default, _) = NetworkPolicy::parse_ranges(
            &[
                "10.0.0.0/8".to_owned(),
                "172.16.0.0/12".to_owned(),
                "192.168.0.0/16".to_owned(),
                "127.0.0.0/8".to_owned(),
                "169.254.0.0/16".to_owned(),
                "100.64.0.0/10".to_owned(),
                "0.0.0.0/8".to_owned(),
                "192.0.0.0/24".to_owned(),
                "192.0.2.0/24".to_owned(),
                "198.18.0.0/15".to_owned(),
                "198.51.100.0/24".to_owned(),
                "203.0.113.0/24".to_owned(),
                "224.0.0.0/4".to_owned(),
                "240.0.0.0/4".to_owned(),
            ],
            "test",
        )
        .unwrap();
        NetworkPolicy {
            ipv4_blocked_default,
            allowed_networks: None,
            blocked_networks: None,
            allowed_networks_v6: None,
            blocked_networks_v6: None,
            blocked_hosts: Default::default(),
        }
    }

    #[test]
    fn ipv6_transition_ranges_are_checked_against_ipv4_policy() {
        let policy = test_policy();
        for ip in [
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2002:a9fe:a9fe::",
            "2001::1",
            "::ffff:0:7f00:1",
			"2001:db8::5efe:7f00:1",
			"2001:db8::200:5efe:a9fe:a9fe",
        ] {
            assert!(policy.check_ip(ip.parse().unwrap()).is_err());
        }
        assert!(policy.check_ip("2002:808:808::".parse().unwrap()).is_ok());
		assert!(policy
			.check_ip("2001:db8::5efe:808:808".parse().unwrap())
			.is_ok());
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

    #[test]
    fn proxy_url_accepts_schemeless_value() {
        let url = parse_proxy_url("10.0.0.5:3128").expect("schemeless proxy should parse");
        assert_eq!(url.host_str(), Some("10.0.0.5"));
        let url = parse_proxy_url("http://10.0.0.5:3128").expect("proxy URL should parse");
        assert_eq!(url.host_str(), Some("10.0.0.5"));
    }

    #[test]
    fn normalize_host_strips_ipv6_brackets() {
        assert_eq!(NetworkPolicy::normalize_host("[::1]"), "::1");
        assert_eq!(
            NetworkPolicy::normalize_host("[2001:db8::1]"),
            "2001:db8::1"
        );
        assert_eq!(NetworkPolicy::normalize_host("EXAMPLE.COM."), "example.com");
    }

    #[test]
    fn resolver_treats_schemeless_proxy_host_as_proxy() {
        let cache = Arc::new(DnsCache::new(
            Duration::from_secs(300),
            Duration::from_secs(10),
            Duration::from_secs(4),
            16,
        ));
        let policy = Arc::new(test_policy());
        let resolver = ValidatingResolver::new(cache, policy, Some("127.0.0.1:1"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let name: reqwest::dns::Name = "127.0.0.1".parse().unwrap();
            assert!(resolver.resolve(name).await.is_ok());
        });
    }
}
