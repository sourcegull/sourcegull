//! The SSRF guard of webhook deliveries.
//!
//! Webhook URLs are entered by site admins, but a delivery must still not become a way to reach
//! the deployment's internal network (the database, the repo nodes, cloud metadata endpoints)
//! from the worker. Every address a delivery connects to is checked:
//!
//! - Host names are resolved by [`GuardedResolver`], the HTTP client's only resolver, and the
//!   connection fails if **any** address is not public ([`is_public`]); since the client
//!   connects to exactly the addresses checked, DNS rebinding between check and connect is not
//!   possible.
//! - IP literals in the URL, and in every redirect target, are checked by [`check_url`] (the
//!   client follows at most [`MAX_REDIRECTS`] redirects, each target checked again).
//! - Only `http` and `https` URLs are sent, and environment proxies are ignored (a proxy would
//!   resolve the host itself).
//!
//! Site config `webhooks.outbound.allowedHosts` ([`Allowlist`]) exempts host names, IP
//! addresses and CIDR ranges, e.g. a receiver inside the cluster.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use ipnet::IpNet;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

/// Redirects followed per delivery.
pub const MAX_REDIRECTS: usize = 3;

/// Why a delivery target was refused.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("URL scheme {0:?} is not allowed: use http or https")]
    Scheme(String),
    #[error("the URL has no host")]
    NoHost,
    #[error(
        "{host} resolves to {ip}, which is not a public address; add the host or network to \
         the site config's webhooks.outbound.allowedHosts to allow it"
    )]
    Denied { host: String, ip: IpAddr },
    #[error("resolving {host}: {source}")]
    Resolve { host: String, source: io::Error },
    #[error("{0} did not resolve to any address")]
    NoAddresses(String),
}

/// Networks that are not public: loopback, private, link-local (with the cloud metadata
/// address), shared (CGNAT), reserved, documentation, benchmarking and multicast ranges, and the
/// IPv6 equivalents (ULA, link-local, site-local, discard, NAT64 local use, Teredo).
static NON_PUBLIC: LazyLock<Vec<IpNet>> = LazyLock::new(|| {
    [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.88.99.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/4",
        "240.0.0.0/4",
        // Azure's wire server and metadata endpoint, a public address.
        "168.63.129.16/32",
        "::/128",
        "::1/128",
        "::/96",
        "64:ff9b:1::/48",
        "100::/64",
        "2001::/32",
        "2001:db8::/32",
        "fc00::/7",
        "fe80::/10",
        "fec0::/10",
        "ff00::/8",
    ]
    .iter()
    .map(|n| n.parse().expect("valid network"))
    .collect()
});

/// The IPv4 address embedded in an IPv6 address that forwards to it: IPv4-mapped
/// (`::ffff:a.b.c.d`), NAT64 (`64:ff9b::a.b.c.d`) and 6to4 (`2002:aabb:ccdd::/48`).
fn embedded_v4(ip: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = ip.segments();
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return Some(std::net::Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    if s[0] == 0x2002 {
        return Some(std::net::Ipv4Addr::new(
            (s[1] >> 8) as u8,
            s[1] as u8,
            (s[2] >> 8) as u8,
            s[2] as u8,
        ));
    }
    None
}

/// Whether `ip` is a public unicast address (IPv6 addresses embedding an IPv4 address are
/// judged by it).
pub fn is_public(ip: IpAddr) -> bool {
    if let IpAddr::V6(v6) = ip
        && let Some(v4) = embedded_v4(v6)
    {
        return is_public(IpAddr::V4(v4));
    }
    !NON_PUBLIC.iter().any(|n| n.contains(&ip))
}

/// Hosts and networks exempt from the guard (`webhooks.outbound.allowedHosts`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    hosts: Vec<String>,
    /// `.example.com` for `*.example.com`.
    suffixes: Vec<String>,
    nets: Vec<IpNet>,
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

impl Allowlist {
    /// Parses entries: host names, `*.domain` wildcards (subdomains only), IP addresses and
    /// CIDR ranges. Empty entries are ignored.
    pub fn new<S: AsRef<str>>(entries: &[S]) -> Self {
        let mut list = Self::default();
        for entry in entries {
            let entry = normalize_host(entry.as_ref());
            if entry.is_empty() {
                continue;
            }
            if let Ok(net) = entry.parse::<IpNet>() {
                list.nets.push(net);
            } else if let Ok(ip) = entry.parse::<IpAddr>() {
                list.nets.push(IpNet::from(ip));
            } else if let Some(domain) = entry.strip_prefix("*.") {
                list.suffixes.push(format!(".{domain}"));
            } else {
                list.hosts.push(entry);
            }
        }
        list
    }

    /// Whether the host name is exempt.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = normalize_host(host);
        self.hosts.contains(&host) || self.suffixes.iter().any(|s| host.ends_with(s.as_str()))
    }

    /// Whether the address is exempt.
    pub fn allows_ip(&self, ip: IpAddr) -> bool {
        let canonical = ip.to_canonical();
        self.nets
            .iter()
            .any(|n| n.contains(&ip) || n.contains(&canonical))
    }

    /// Fails unless `ip` (an address of `host`) is public or exempt.
    pub fn check(&self, host: &str, ip: IpAddr) -> Result<(), GuardError> {
        if is_public(ip) || self.allows_host(host) || self.allows_ip(ip) {
            Ok(())
        } else {
            Err(GuardError::Denied {
                host: normalize_host(host),
                ip,
            })
        }
    }
}

/// Checks a URL before it is requested (and every redirect target): `http`/`https` with a
/// host, and an IP literal host must pass [`Allowlist::check`]. Host names are checked when
/// they are resolved ([`GuardedResolver`]).
pub fn check_url(url: &Url, allow: &Allowlist) -> Result<(), GuardError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(GuardError::Scheme(url.scheme().to_owned()));
    }
    match url.host() {
        None => Err(GuardError::NoHost),
        Some(Host::Ipv4(ip)) => allow.check(&ip.to_string(), IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => allow.check(&ip.to_string(), IpAddr::V6(ip)),
        Some(Host::Domain("")) => Err(GuardError::NoHost),
        Some(Host::Domain(_)) => Ok(()),
    }
}

/// Checks a webhook URL entered by a site admin: it parses and passes [`check_url`] with
/// `webhooks.outbound.allowedHosts` (shared by the API and the resources file).
pub fn check_webhook_url(config: &sourcegull_config::SiteConfig, raw: &str) -> Result<(), String> {
    let url = Url::parse(raw).map_err(|e| format!("invalid URL: {e}"))?;
    let allow = Allowlist::new(&config.webhooks_outbound_allowed_hosts);
    check_url(&url, &allow).map_err(|e| e.to_string())
}

/// A future of resolved addresses.
pub type LookupFuture = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;

/// Resolves host names (the system resolver in production, a table in tests).
pub trait Lookup: Send + Sync + 'static {
    fn lookup(&self, host: &str) -> LookupFuture;
}

/// The system resolver (`getaddrinfo` through tokio).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(&self, host: &str) -> LookupFuture {
        let host = host.to_owned();
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .map(|a| a.ip())
                .collect())
        })
    }
}

/// Resolves `host` and checks every address; IP literals are checked as they are.
pub async fn resolve_checked(
    lookup: &dyn Lookup,
    allow: &Allowlist,
    host: &str,
) -> Result<Vec<IpAddr>, GuardError> {
    let bare = normalize_host(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        allow.check(&bare, ip)?;
        return Ok(vec![ip]);
    }
    let ips = lookup
        .lookup(&bare)
        .await
        .map_err(|source| GuardError::Resolve {
            host: bare.clone(),
            source,
        })?;
    if ips.is_empty() {
        return Err(GuardError::NoAddresses(bare));
    }
    for ip in &ips {
        allow.check(&bare, *ip)?;
    }
    Ok(ips)
}

/// The HTTP client's resolver: [`resolve_checked`] over a [`Lookup`].
pub struct GuardedResolver {
    allow: Arc<Allowlist>,
    lookup: Arc<dyn Lookup>,
}

impl GuardedResolver {
    pub fn new(allow: Arc<Allowlist>, lookup: Arc<dyn Lookup>) -> Self {
        Self { allow, lookup }
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let (allow, lookup) = (self.allow.clone(), self.lookup.clone());
        Box::pin(async move {
            let ips = resolve_checked(&*lookup, &allow, name.as_str()).await?;
            let addrs: Addrs = Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

/// An HTTP client that only reaches what the guard allows: the [`GuardedResolver`], checked
/// redirects, no proxies, and `timeout` for the whole request.
pub fn client(
    allow: Arc<Allowlist>,
    lookup: Arc<dyn Lookup>,
    timeout: Duration,
) -> reqwest::Result<reqwest::Client> {
    let redirects = allow.clone();
    reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout)
        .connect_timeout(timeout)
        .user_agent(concat!("Sourcegull-Webhooks/", env!("CARGO_PKG_VERSION")))
        .dns_resolver(Arc::new(GuardedResolver::new(allow, lookup)))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                return attempt.error(format!("more than {MAX_REDIRECTS} redirects"));
            }
            match check_url(attempt.url(), &redirects) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_and_non_public_addresses() {
        for denied in [
            "0.0.0.0",
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "100.100.100.200",
            "168.63.129.16",
            "192.0.0.192",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::1",
            "2002:c0a8:101::1",
            "::127.0.0.1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001:0:4136:e378::1",
            "100::1",
        ] {
            assert!(!is_public(ip(denied)), "{denied} should be denied");
        }
        for allowed in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "172.15.255.255",
            "100.128.0.1",
            "169.255.0.1",
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(is_public(ip(allowed)), "{allowed} should be allowed");
        }
    }

    #[test]
    fn allowlist_entries() {
        let allow = Allowlist::new(&[
            "hooks.internal",
            "*.corp.example.com",
            "10.1.0.0/16",
            "192.168.1.5",
            "fd00::/8",
            "  ",
            "Upper.Example.",
        ]);
        assert!(allow.allows_host("hooks.internal"));
        assert!(allow.allows_host("HOOKS.internal."));
        assert!(allow.allows_host("upper.example"));
        assert!(!allow.allows_host("evil-hooks.internal"));
        assert!(allow.allows_host("a.corp.example.com"));
        assert!(allow.allows_host("a.b.corp.example.com"));
        assert!(!allow.allows_host("corp.example.com"));
        assert!(!allow.allows_host("xcorp.example.com"));
        assert!(allow.allows_ip(ip("10.1.2.3")));
        assert!(!allow.allows_ip(ip("10.2.0.1")));
        assert!(allow.allows_ip(ip("192.168.1.5")));
        assert!(!allow.allows_ip(ip("192.168.1.6")));
        assert!(allow.allows_ip(ip("::ffff:10.1.0.1")));
        assert!(allow.allows_ip(ip("fd12::1")));
        assert!(!allow.allows_ip(ip("fe80::1")));

        assert!(allow.check("x", ip("10.1.9.9")).is_ok());
        assert!(allow.check("hooks.internal", ip("127.0.0.1")).is_ok());
        assert!(allow.check("x", ip("8.8.8.8")).is_ok());
        let err = allow.check("other", ip("127.0.0.1")).unwrap_err();
        assert!(
            err.to_string().contains("webhooks.outbound.allowedHosts"),
            "{err}"
        );
    }

    #[test]
    fn urls() {
        let none = Allowlist::default();
        let check = |u: &str, a: &Allowlist| check_url(&Url::parse(u).unwrap(), a);
        assert!(check("https://hooks.example.com/x", &none).is_ok());
        assert!(check("http://8.8.8.8/x", &none).is_ok());
        assert!(check("http://[2606:4700::1]/", &none).is_ok());
        assert!(matches!(
            check("ftp://example.com/", &none),
            Err(GuardError::Scheme(_))
        ));
        assert!(matches!(
            check("file:///etc/passwd", &none),
            Err(GuardError::Scheme(_))
        ));
        for bad in [
            "http://127.0.0.1:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[::ffff:7f00:1]/",
            "http://0x7f000001/",
            "http://2130706433/",
            "http://10.0.0.1/",
        ] {
            assert!(
                matches!(check(bad, &none), Err(GuardError::Denied { .. })),
                "{bad}"
            );
        }
        let allow = Allowlist::new(&["127.0.0.0/8", "::1"]);
        assert!(check("http://127.0.0.1:8080/", &allow).is_ok());
        assert!(check("http://[::1]/", &allow).is_ok());
    }

    struct Table(HashMap<&'static str, Vec<IpAddr>>);

    impl Lookup for Table {
        fn lookup(&self, host: &str) -> LookupFuture {
            let r = self
                .0
                .get(host)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such host"));
            Box::pin(async move { r })
        }
    }

    #[tokio::test]
    async fn dns_answers_are_checked() {
        let table = Table(HashMap::from([
            ("public.example", vec![ip("93.184.216.34")]),
            ("internal.example", vec![ip("10.0.0.5")]),
            // One private answer among public ones is enough to refuse.
            ("mixed.example", vec![ip("93.184.216.34"), ip("127.0.0.1")]),
            ("v6.example", vec![ip("2606:4700::1"), ip("fd00::1")]),
            ("mapped.example", vec![ip("::ffff:169.254.169.254")]),
            ("empty.example", vec![]),
        ]));
        let none = Allowlist::default();
        assert_eq!(
            resolve_checked(&table, &none, "public.example")
                .await
                .unwrap(),
            vec![ip("93.184.216.34")]
        );
        for host in [
            "internal.example",
            "mixed.example",
            "v6.example",
            "mapped.example",
        ] {
            let err = resolve_checked(&table, &none, host).await.unwrap_err();
            assert!(matches!(err, GuardError::Denied { .. }), "{host}: {err}");
        }
        assert!(matches!(
            resolve_checked(&table, &none, "empty.example").await,
            Err(GuardError::NoAddresses(_))
        ));
        assert!(matches!(
            resolve_checked(&table, &none, "missing.example").await,
            Err(GuardError::Resolve { .. })
        ));
        assert!(matches!(
            resolve_checked(&table, &none, "127.0.0.1").await,
            Err(GuardError::Denied { .. })
        ));

        // Allowed by host name (every address) or by network (only the addresses in it).
        let by_host = Allowlist::new(&["internal.example", "mixed.example"]);
        assert!(
            resolve_checked(&table, &by_host, "internal.example")
                .await
                .is_ok()
        );
        assert!(
            resolve_checked(&table, &by_host, "mixed.example")
                .await
                .is_ok()
        );
        let by_net = Allowlist::new(&["10.0.0.0/8", "fd00::/8"]);
        assert!(
            resolve_checked(&table, &by_net, "internal.example")
                .await
                .is_ok()
        );
        assert!(resolve_checked(&table, &by_net, "v6.example").await.is_ok());
        assert!(
            resolve_checked(&table, &by_net, "mixed.example")
                .await
                .is_err()
        );
    }
}
