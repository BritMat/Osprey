//! Privacy and anonymity for every request BM Osprey makes.
//!
//! Three independent layers, which `anonymous()` switches on together:
//!
//! - **Route**: direct, through a proxy or VPN proxy, or through Tor.
//!   Proxies always use remote DNS (`socks5h`), so the names of the sites
//!   you visit are never looked up on your own connection.
//! - **DNS**: when going direct, names can be resolved over encrypted
//!   DNS-over-HTTPS (Cloudflare 1.1.1.1, Quad9 or Google) instead of your
//!   ISP's resolver. The DoH servers are reached by IP address, so even
//!   that first lookup never touches the system resolver.
//! - **Identity**: a generic browser identity (the same user agent and
//!   language headers Tor Browser sends) instead of "BM-Osprey/0.1".
//!
//! Tor mode fails closed. If Tor is not running, nothing is sent at all.
//! There is no silent fallback to a direct connection.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hickory_resolver::config::{ResolverConfig, CLOUDFLARE, GOOGLE, QUAD9};
use hickory_resolver::TokioResolver;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The user agent of Tor Browser 15 on Windows. Shared by a huge number
/// of Tor users, so it says very little about you.
pub const GENERIC_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; rv:140.0) Gecko/20100101 Firefox/140.0";
pub const GENERIC_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.5";

/// Tor's usual local SOCKS ports: the Tor service, then Tor Browser.
pub const TOR_PORTS: &[&str] = &["127.0.0.1:9050", "127.0.0.1:9150"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    #[default]
    Direct,
    /// A proxy URL such as `socks5://127.0.0.1:1080` or `http://proxy:8080`.
    Proxy(String),
    /// Tor's SOCKS address. `None` means find it automatically.
    Tor(Option<String>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DnsMode {
    /// Whatever the computer is set to use, usually the ISP.
    #[default]
    System,
    /// Cloudflare 1.1.1.1 over HTTPS.
    Cloudflare,
    /// Quad9 9.9.9.9 over HTTPS. Also blocks known malware domains.
    Quad9,
    /// Google 8.8.8.8 over HTTPS.
    Google,
}

impl FromStr for DnsMode {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "system" | "default" | "off" => Self::System,
            "cloudflare" | "1.1.1.1" | "1.0.0.1" => Self::Cloudflare,
            "quad9" | "9.9.9.9" => Self::Quad9,
            "google" | "8.8.8.8" => Self::Google,
            other => {
                return Err(Error::Unsupported(format!(
                    "unknown DNS '{other}'. Use cloudflare (1.1.1.1), quad9, google or system"
                )))
            }
        })
    }
}

impl DnsMode {
    pub fn describe(self) -> &'static str {
        match self {
            Self::System => "system DNS (usually your ISP, not encrypted)",
            Self::Cloudflare => "Cloudflare 1.1.1.1 over HTTPS (encrypted)",
            Self::Quad9 => "Quad9 9.9.9.9 over HTTPS (encrypted)",
            Self::Google => "Google 8.8.8.8 over HTTPS (encrypted)",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Privacy {
    pub route: Route,
    pub dns: DnsMode,
    /// Send the generic browser identity instead of BM Osprey's own.
    pub generic_identity: bool,
    /// Tor only: how many separate circuits one download may use at
    /// once. More is faster, up to a point. 0 or 1 means one circuit.
    pub circuits: u8,
}

impl Privacy {
    /// Everything on: Tor, generic identity. DNS goes through Tor too.
    pub fn anonymous() -> Self {
        Self {
            route: Route::Tor(None),
            dns: DnsMode::System,
            generic_identity: true,
            circuits: 3,
        }
    }

    pub fn is_tor(&self) -> bool {
        matches!(self.route, Route::Tor(_))
    }

    /// Checks that the chosen route works before anything is sent, and
    /// finds Tor's port. Call once before making requests.
    pub async fn prepare(&mut self) -> Result<()> {
        match &self.route {
            Route::Tor(addr) => {
                let found = find_tor(addr.as_deref()).await?;
                self.route = Route::Tor(Some(found));
            }
            Route::Proxy(url) => {
                self.route = Route::Proxy(normalise_proxy(url)?);
            }
            Route::Direct => {}
        }
        Ok(())
    }

    /// The proxy URL requests should use, if any.
    pub fn proxy_url(&self) -> Result<Option<String>> {
        match &self.route {
            Route::Direct => Ok(None),
            Route::Proxy(url) => normalise_proxy(url).map(Some),
            Route::Tor(addr) => Ok(Some(format!(
                "socks5h://{}",
                addr.as_deref().unwrap_or(TOR_PORTS[0])
            ))),
        }
    }

    /// One line for the screen, like "Tor via 127.0.0.1:9150, generic identity".
    pub fn describe(&self) -> String {
        let route = match &self.route {
            Route::Direct => "direct connection".to_string(),
            Route::Proxy(p) => format!("proxy {}", redact_proxy(p)),
            Route::Tor(Some(a)) => format!("Tor via {a}"),
            Route::Tor(None) => "Tor".to_string(),
        };
        let mut parts = vec![route];
        if self.route == Route::Direct {
            parts.push(self.dns.describe().to_string());
        } else {
            parts.push("DNS resolved by the proxy".to_string());
        }
        if self.generic_identity {
            parts.push("generic browser identity".to_string());
        }
        parts.join(", ")
    }

    /// The DoH resolver to install, when one is wanted.
    pub(crate) fn resolver(&self) -> Option<Arc<DohResolver>> {
        doh_resolver(self.dns)
    }
}

/// Makes SOCKS proxies use remote DNS, so names never leak to the local
/// resolver. `socks5://` becomes `socks5h://`.
pub fn normalise_proxy(url: &str) -> Result<String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    let fixed = if let Some(rest) = lower.strip_prefix("socks5://") {
        format!("socks5h://{}", &url[url.len() - rest.len()..])
    } else if lower.starts_with("socks4://") || lower.starts_with("socks4a://") {
        return Err(Error::Unsupported(
            "SOCKS4 is not supported. Use a socks5:// proxy".into(),
        ));
    } else if lower.starts_with("socks5h://")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
    {
        url.to_string()
    } else if !url.contains("://") && url.contains(':') {
        // A bare host:port is taken as SOCKS5, the common case for VPN apps.
        format!("socks5h://{url}")
    } else {
        return Err(Error::Unsupported(format!(
            "'{url}' is not a proxy address. Examples: socks5://127.0.0.1:1080 or http://proxy.example:8080"
        )));
    };
    reqwest::Proxy::all(&fixed)
        .map_err(|e| Error::Unsupported(format!("bad proxy '{url}': {e}")))?;
    Ok(fixed)
}

fn redact_proxy(url: &str) -> String {
    // Never print proxy passwords.
    match url::Url::parse(url) {
        Ok(mut u) if u.password().is_some() => {
            let _ = u.set_password(Some("****"));
            u.to_string()
        }
        _ => url.to_string(),
    }
}

/// Finds a running Tor. With `custom` only that address is tried.
pub async fn find_tor(custom: Option<&str>) -> Result<String> {
    let candidates: Vec<String> = match custom {
        Some(c) => vec![c
            .trim()
            .trim_start_matches("socks5h://")
            .trim_start_matches("socks5://")
            .to_string()],
        None => TOR_PORTS.iter().map(|s| s.to_string()).collect(),
    };
    for addr in &candidates {
        let Ok(sock) = addr.parse::<SocketAddr>() else {
            continue;
        };
        let attempt =
            tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(sock))
                .await;
        if let Ok(Ok(_)) = attempt {
            return Ok(addr.clone());
        }
    }
    Err(Error::Unsupported(format!(
        "Tor is not running (tried {}). Nothing was sent. Start Tor Browser and leave it open, \
         or install the Tor service, then try again",
        candidates.join(" and ")
    )))
}

/// DNS-over-HTTPS resolver plugged into reqwest.
pub(crate) struct DohResolver(TokioResolver);

impl Resolve for DohResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.0.clone();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(name.as_str()).await?;
            let addrs: Vec<SocketAddr> = lookup.iter().map(|ip| SocketAddr::new(ip, 0)).collect();
            if addrs.is_empty() {
                return Err(format!("encrypted DNS found no address for {}", name.as_str()).into());
            }
            let addrs: Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

fn doh_resolver(mode: DnsMode) -> Option<Arc<DohResolver>> {
    static CF: OnceLock<Option<Arc<DohResolver>>> = OnceLock::new();
    static Q9: OnceLock<Option<Arc<DohResolver>>> = OnceLock::new();
    static GG: OnceLock<Option<Arc<DohResolver>>> = OnceLock::new();
    let (cell, group) = match mode {
        DnsMode::System => return None,
        DnsMode::Cloudflare => (&CF, CLOUDFLARE),
        DnsMode::Quad9 => (&Q9, QUAD9),
        DnsMode::Google => (&GG, GOOGLE),
    };
    cell.get_or_init(|| {
        let builder = TokioResolver::builder_with_config(
            ResolverConfig::https(&group),
            hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
        );
        builder.build().ok().map(|r| Arc::new(DohResolver(r)))
    })
    .clone()
}

/// Looks up `host` through the encrypted DNS server directly. Used by
/// the privacy check to prove DoH works on this computer.
pub async fn doh_lookup(mode: DnsMode, host: &str) -> Result<Vec<std::net::IpAddr>> {
    let resolver = doh_resolver(mode)
        .ok_or_else(|| Error::Unsupported("encrypted DNS is not switched on".into()))?;
    let lookup = tokio::time::timeout(Duration::from_secs(10), resolver.0.lookup_ip(host))
        .await
        .map_err(|_| {
            Error::Unsupported("encrypted DNS server did not answer in 10 seconds".into())
        })?
        .map_err(|e| Error::Unsupported(format!("encrypted DNS lookup failed: {e}")))?;
    Ok(lookup.iter().collect())
}

/// Proxy settings inherited from environment variables. They take over
/// direct connections (and their DNS) unless a route is chosen.
pub fn env_proxy() -> Option<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .iter()
    .find(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
    .map(|k| k.to_string())
}

/// What the outside world sees, from `check_exposure`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Exposure {
    pub setup: String,
    /// Result of a test lookup through encrypted DNS, when it is on.
    pub dns_test: Option<String>,
    pub public_ip: Option<String>,
    pub through_tor: Option<bool>,
    pub country: Option<String>,
    pub cloudflare_warp: Option<String>,
    pub problem: Option<String>,
}

/// Asks an outside service which IP address it sees. Uses the Tor
/// Project's checker in Tor mode and Cloudflare's trace page otherwise.
pub async fn check_exposure(opts: &crate::client::RequestOptions) -> Exposure {
    let mut out = Exposure {
        setup: opts.privacy.describe(),
        ..Default::default()
    };
    if opts.privacy.route == Route::Direct && opts.privacy.dns != DnsMode::System {
        out.dns_test = Some(match doh_lookup(opts.privacy.dns, "example.com").await {
            Ok(ips) => format!(
                "example.com resolved to {} through {}",
                ips.first().map(|i| i.to_string()).unwrap_or_default(),
                opts.privacy.dns.describe()
            ),
            Err(e) => format!("FAILED: {e}"),
        });
    }
    let client = match crate::client::build_client(opts) {
        Ok(c) => c,
        Err(e) => {
            out.problem = Some(e.to_string());
            return out;
        }
    };

    if opts.privacy.is_tor() {
        #[derive(Deserialize)]
        struct TorApi {
            #[serde(rename = "IsTor")]
            is_tor: bool,
            #[serde(rename = "IP")]
            ip: String,
        }
        match client
            .get("https://check.torproject.org/api/ip")
            .send()
            .await
        {
            Ok(r) => match r
                .text()
                .await
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<TorApi>(&t).map_err(|e| e.to_string()))
            {
                Ok(t) => {
                    out.through_tor = Some(t.is_tor);
                    out.public_ip = Some(t.ip);
                }
                Err(e) => {
                    out.problem = Some(format!("unexpected answer from check.torproject.org: {e}"))
                }
            },
            Err(e) => out.problem = Some(format!("could not reach check.torproject.org: {e}")),
        }
    }

    match client
        .get("https://www.cloudflare.com/cdn-cgi/trace")
        .send()
        .await
    {
        Ok(r) => {
            let text = r.text().await.unwrap_or_default();
            let mut trace = parse_trace(&text);
            if out.public_ip.is_none() {
                out.public_ip = trace.remove("ip");
            }
            out.country = trace.remove("loc");
            out.cloudflare_warp = trace.remove("warp");
        }
        Err(e) => {
            if out.problem.is_none() {
                out.problem = Some(format!("could not reach cloudflare.com: {e}"));
            }
        }
    }
    out
}

pub(crate) fn parse_trace(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_names() {
        assert_eq!("1.1.1.1".parse::<DnsMode>().unwrap(), DnsMode::Cloudflare);
        assert_eq!(
            "Cloudflare".parse::<DnsMode>().unwrap(),
            DnsMode::Cloudflare
        );
        assert_eq!("quad9".parse::<DnsMode>().unwrap(), DnsMode::Quad9);
        assert_eq!("system".parse::<DnsMode>().unwrap(), DnsMode::System);
        assert!("8.8.4.4.4".parse::<DnsMode>().is_err());
    }

    #[test]
    fn proxies_always_use_remote_dns() {
        assert_eq!(
            normalise_proxy("socks5://127.0.0.1:1080").unwrap(),
            "socks5h://127.0.0.1:1080"
        );
        assert_eq!(
            normalise_proxy("SOCKS5://user:pw@10.0.0.1:1080").unwrap(),
            "socks5h://user:pw@10.0.0.1:1080"
        );
        assert_eq!(
            normalise_proxy("127.0.0.1:9050").unwrap(),
            "socks5h://127.0.0.1:9050"
        );
        assert_eq!(
            normalise_proxy("http://proxy.test:8080").unwrap(),
            "http://proxy.test:8080"
        );
        assert!(normalise_proxy("socks4://x:1").is_err());
        assert!(normalise_proxy("nonsense").is_err());

        let tor = Privacy {
            route: Route::Tor(Some("127.0.0.1:9150".into())),
            ..Default::default()
        };
        assert_eq!(
            tor.proxy_url().unwrap().as_deref(),
            Some("socks5h://127.0.0.1:9150")
        );
    }

    #[test]
    fn descriptions_hide_passwords() {
        let p = Privacy {
            route: Route::Proxy("socks5h://me:secret@10.0.0.1:1080".into()),
            ..Default::default()
        };
        assert!(!p.describe().contains("secret"));
        assert!(p.describe().contains("****"));
        let a = Privacy::anonymous();
        assert!(a.describe().starts_with("Tor"));
        assert!(a.describe().contains("generic browser identity"));
    }

    #[test]
    fn parses_cloudflare_trace() {
        let t = parse_trace("fl=1\nip=203.0.113.9\nloc=IE\nwarp=off\n");
        assert_eq!(t["ip"], "203.0.113.9");
        assert_eq!(t["loc"], "IE");
    }

    #[tokio::test]
    async fn tor_mode_fails_closed_when_tor_is_missing() {
        // Port 1 is never a Tor port.
        let err = find_tor(Some("127.0.0.1:1")).await.unwrap_err();
        assert!(err.to_string().contains("Nothing was sent"));
        let mut p = Privacy {
            route: Route::Tor(Some("127.0.0.1:1".into())),
            ..Default::default()
        };
        assert!(p.prepare().await.is_err());
    }
}
