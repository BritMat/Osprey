//! The privacy gateway: a small SOCKS5 proxy that runs inside the app on
//! 127.0.0.1. Every browser tab and every download is pointed at it, and
//! it decides where traffic really goes:
//!
//! - **Direct**, optionally with names looked up over encrypted DNS
//! - **through a SOCKS5 proxy**: Tor, Cloudflare WARP's proxy mode, a VPN
//!   app's proxy. Site names are passed on, never looked up locally.
//! - **through an HTTP proxy** with `CONNECT`
//!
//! Because pages and downloads leave through the same exit, a site sees
//! one viewer, which also keeps IP-locked media links working. The route
//! can be switched live: open connections on the old route are closed at
//! once, so nothing keeps flowing the old way. If the chosen proxy is
//! down, connections fail. There is never a fallback to direct.
//!
//! When filters are set, connections to ad and tracker domains are
//! refused before anything is sent.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::error::{Error, Result};
use crate::filter::{Filters, RequestKind};
use crate::privacy::{self, DnsMode};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const BLOCK_CACHE_LIMIT: usize = 20_000;

/// Where the gateway sends traffic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Upstream {
    Direct {
        dns: DnsMode,
    },
    Socks5 {
        addr: String,
        user: Option<String>,
        pass: Option<String>,
    },
    Http {
        addr: String,
        user: Option<String>,
        pass: Option<String>,
    },
}

impl Default for Upstream {
    fn default() -> Self {
        Self::Direct {
            dns: DnsMode::System,
        }
    }
}

impl Upstream {
    /// Reads `socks5://user:pass@host:port`, `http://host:port` or a bare
    /// `host:port` (taken as SOCKS5).
    pub fn from_proxy_url(url: &str) -> Result<Self> {
        let normalised = privacy::normalise_proxy(url)?;
        let parsed = url::Url::parse(&normalised)
            .map_err(|e| Error::Unsupported(format!("bad proxy '{url}': {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| Error::Unsupported(format!("proxy '{url}' has no host")))?;
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| Error::Unsupported(format!("proxy '{url}' has no port")))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let addr = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let user = (!parsed.username().is_empty()).then(|| percent_decode(parsed.username()));
        let pass = parsed.password().map(percent_decode);
        Ok(match parsed.scheme() {
            "socks5h" | "socks5" => Self::Socks5 { addr, user, pass },
            "http" | "https" => Self::Http { addr, user, pass },
            other => {
                return Err(Error::Unsupported(format!(
                    "unsupported proxy type '{other}'"
                )))
            }
        })
    }

    /// One line for the screen. Never shows passwords.
    pub fn describe(&self) -> String {
        match self {
            Self::Direct { dns } => format!("direct, {}", dns.describe()),
            Self::Socks5 { addr, user, .. } => {
                format!(
                    "SOCKS5 proxy {}{addr}",
                    user.as_ref().map(|u| format!("{u}@")).unwrap_or_default()
                )
            }
            Self::Http { addr, user, .. } => {
                format!(
                    "HTTP proxy {}{addr}",
                    user.as_ref().map(|u| format!("{u}@")).unwrap_or_default()
                )
            }
        }
    }

    fn proxy_addr(&self) -> Option<&str> {
        match self {
            Self::Direct { .. } => None,
            Self::Socks5 { addr, .. } | Self::Http { addr, .. } => Some(addr),
        }
    }
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| s.to_string())
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GatewayStats {
    pub route: String,
    /// Connections handled since start.
    pub connections: u64,
    /// Connections open right now.
    pub active: u64,
    /// Connections refused by the ad and tracker filters.
    pub blocked: u64,
    /// Connections that could not reach the route (proxy down, DNS failed).
    pub failed: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Counters {
    connections: AtomicU64,
    active: AtomicU64,
    blocked: AtomicU64,
    failed: AtomicU64,
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
}

struct Inner {
    upstream: RwLock<Upstream>,
    filters: RwLock<Option<Arc<Filters>>>,
    block_cache: Mutex<HashMap<String, bool>>,
    /// Bumped on every route change. Open connections watch it and close.
    generation: watch::Sender<u64>,
    counters: Counters,
    last_error: Mutex<Option<String>>,
}

/// A running gateway. Dropping it does not stop it. It lives as long as
/// the async runtime, which is what an app wants.
#[derive(Clone)]
pub struct Gateway {
    addr: SocketAddr,
    inner: Arc<Inner>,
}

impl Gateway {
    /// Starts on a free port on 127.0.0.1. Only this computer can use it.
    pub async fn start(upstream: Upstream, filters: Option<Arc<Filters>>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (generation, _) = watch::channel(0u64);
        let inner = Arc::new(Inner {
            upstream: RwLock::new(upstream),
            filters: RwLock::new(filters),
            block_cache: Mutex::new(HashMap::new()),
            generation,
            counters: Counters::default(),
            last_error: Mutex::new(None),
        });
        let accept_inner = inner.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    continue;
                };
                if !peer.ip().is_loopback() {
                    continue;
                }
                let _ = stream.set_nodelay(true);
                let inner = accept_inner.clone();
                tokio::spawn(async move {
                    let _ = handle(inner, stream).await;
                });
            }
        });
        Ok(Self { addr, inner })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// URL for webviews and download clients. Browsers pass names to a
    /// SOCKS5 proxy, so DNS happens here, never in the browser.
    pub fn proxy_url(&self) -> String {
        format!("socks5://{}", self.addr)
    }

    pub fn upstream(&self) -> Upstream {
        self.inner.upstream.read().expect("upstream lock").clone()
    }

    /// Switches the route. Connections still open on the old route are
    /// closed right away.
    pub fn set_upstream(&self, upstream: Upstream) {
        *self.inner.upstream.write().expect("upstream lock") = upstream;
        *self.inner.last_error.lock().expect("error lock") = None;
        self.inner.generation.send_modify(|g| *g += 1);
    }

    pub fn set_filters(&self, filters: Option<Arc<Filters>>) {
        *self.inner.filters.write().expect("filters lock") = filters;
        self.inner.block_cache.lock().expect("cache lock").clear();
    }

    pub fn stats(&self) -> GatewayStats {
        let c = &self.inner.counters;
        GatewayStats {
            route: self.upstream().describe(),
            connections: c.connections.load(Ordering::Relaxed),
            active: c.active.load(Ordering::Relaxed),
            blocked: c.blocked.load(Ordering::Relaxed),
            failed: c.failed.load(Ordering::Relaxed),
            bytes_up: c.bytes_up.load(Ordering::Relaxed),
            bytes_down: c.bytes_down.load(Ordering::Relaxed),
            last_error: self.inner.last_error.lock().expect("error lock").clone(),
        }
    }

    /// Checks that the route's proxy answers, without sending anything
    /// through it. Direct always passes.
    pub async fn check_upstream(&self) -> Result<()> {
        let up = self.upstream();
        let Some(addr) = up.proxy_addr() else {
            return Ok(());
        };
        match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr)).await {
            Ok(Ok(_)) => Ok(()),
            _ => Err(Error::Unsupported(format!(
                "{} is not answering. Nothing is being sent until it is back",
                up.describe()
            ))),
        }
    }
}

enum Target {
    Ip(IpAddr),
    Domain(String),
}

impl Target {
    fn host(&self) -> String {
        match self {
            Self::Ip(IpAddr::V6(ip)) => format!("[{ip}]"),
            Self::Ip(ip) => ip.to_string(),
            Self::Domain(d) => d.clone(),
        }
    }
}

// SOCKS5 reply codes.
const REP_OK: u8 = 0x00;
const REP_FAILURE: u8 = 0x01;
const REP_NOT_ALLOWED: u8 = 0x02;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_REFUSED: u8 = 0x05;
const REP_CMD_UNSUPPORTED: u8 = 0x07;
const REP_ATYP_UNSUPPORTED: u8 = 0x08;

async fn reply(client: &mut TcpStream, code: u8) -> std::io::Result<()> {
    client.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

/// A login a local client sent. Used only to pick a Tor circuit: Tor
/// gives each distinct login its own circuit (IsolateSOCKSAuth), which
/// lets one download use several circuits in parallel.
type ClientLogin = Option<(Vec<u8>, Vec<u8>)>;

async fn read_request(
    client: &mut TcpStream,
) -> std::io::Result<Option<(Target, u16, ClientLogin)>> {
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Ok(None);
    }
    let mut methods = vec![0u8; head[1] as usize];
    client.read_exact(&mut methods).await?;
    let mut login = None;
    if methods.contains(&2) {
        // Username and password (RFC 1929). Any value is accepted: it is
        // a circuit name, not a secret.
        client.write_all(&[5, 2]).await?;
        let mut ver = [0u8; 2];
        client.read_exact(&mut ver).await?;
        let mut user = vec![0u8; ver[1] as usize];
        client.read_exact(&mut user).await?;
        let mut plen = [0u8; 1];
        client.read_exact(&mut plen).await?;
        let mut pass = vec![0u8; plen[0] as usize];
        client.read_exact(&mut pass).await?;
        client.write_all(&[1, 0]).await?;
        login = Some((user, pass));
    } else if methods.contains(&0) {
        client.write_all(&[5, 0]).await?;
    } else {
        client.write_all(&[5, 0xff]).await?;
        return Ok(None);
    }

    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    if req[1] != 1 {
        reply(client, REP_CMD_UNSUPPORTED).await?;
        return Ok(None);
    }
    let target = match req[3] {
        1 => {
            let mut b = [0u8; 4];
            client.read_exact(&mut b).await?;
            Target::Ip(IpAddr::from(b))
        }
        3 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            client.read_exact(&mut name).await?;
            Target::Domain(String::from_utf8_lossy(&name).to_ascii_lowercase())
        }
        4 => {
            let mut b = [0u8; 16];
            client.read_exact(&mut b).await?;
            Target::Ip(IpAddr::from(b))
        }
        _ => {
            reply(client, REP_ATYP_UNSUPPORTED).await?;
            return Ok(None);
        }
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await?;
    Ok(Some((target, u16::from_be_bytes(port), login)))
}

fn is_blocked(inner: &Inner, host: &str) -> bool {
    let filters = inner.filters.read().expect("filters lock").clone();
    let Some(filters) = filters else { return false };
    if let Some(hit) = inner.block_cache.lock().expect("cache lock").get(host) {
        return *hit;
    }
    // Only the host is visible to a proxy (HTTPS hides the rest), so this
    // applies domain rules like ||doubleclick.net^. The request counts as
    // first party, which keeps $third-party rules from over-blocking.
    let url = format!("https://{host}/");
    let blocked = filters.is_blocked(&url, &url, RequestKind::Other);
    let mut cache = inner.block_cache.lock().expect("cache lock");
    if cache.len() >= BLOCK_CACHE_LIMIT {
        cache.clear();
    }
    cache.insert(host.to_string(), blocked);
    blocked
}

async fn handle(inner: Arc<Inner>, mut client: TcpStream) -> Result<()> {
    let request = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_request(&mut client))
        .await
        .map_err(|_| Error::Unsupported("client handshake timed out".into()))??;
    let Some((target, port, login)) = request else {
        return Ok(());
    };
    inner.counters.connections.fetch_add(1, Ordering::Relaxed);

    if let Target::Domain(host) = &target {
        if is_blocked(&inner, host) {
            inner.counters.blocked.fetch_add(1, Ordering::Relaxed);
            reply(&mut client, REP_NOT_ALLOWED).await?;
            return Ok(());
        }
    }

    let generation = *inner.generation.borrow();
    let mut upstream = inner.upstream.read().expect("upstream lock").clone();
    // Pass a client's circuit name on to a SOCKS upstream (Tor) that has
    // no login of its own. A configured VPN login is never replaced.
    if let (
        Upstream::Socks5 {
            user: user @ None,
            pass,
            ..
        },
        Some((u, p)),
    ) = (&mut upstream, login)
    {
        *user = Some(String::from_utf8_lossy(&u).into_owned());
        *pass = Some(String::from_utf8_lossy(&p).into_owned());
    }

    let mut remote =
        match tokio::time::timeout(CONNECT_TIMEOUT, connect_upstream(&upstream, &target, port))
            .await
        {
            Ok(Ok(s)) => s,
            Ok(Err((code, msg))) => {
                fail(&inner, &mut client, code, msg).await;
                return Ok(());
            }
            Err(_) => {
                fail(
                    &inner,
                    &mut client,
                    REP_HOST_UNREACHABLE,
                    format!("timed out reaching {}", target.host()),
                )
                .await;
                return Ok(());
            }
        };
    let _ = remote.set_nodelay(true);
    reply(&mut client, REP_OK).await?;

    inner.counters.active.fetch_add(1, Ordering::Relaxed);
    let mut changes = inner.generation.subscribe();
    let route_changed = async move {
        loop {
            if changes.changed().await.is_err() || *changes.borrow() != generation {
                break;
            }
        }
    };
    tokio::select! {
        r = tokio::io::copy_bidirectional(&mut client, &mut remote) => {
            if let Ok((up, down)) = r {
                inner.counters.bytes_up.fetch_add(up, Ordering::Relaxed);
                inner.counters.bytes_down.fetch_add(down, Ordering::Relaxed);
            }
        }
        _ = route_changed => {}
    }
    inner.counters.active.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

async fn fail(inner: &Inner, client: &mut TcpStream, code: u8, msg: String) {
    inner.counters.failed.fetch_add(1, Ordering::Relaxed);
    *inner.last_error.lock().expect("error lock") = Some(msg);
    let _ = reply(client, code).await;
}

type ConnectError = (u8, String);

async fn connect_upstream(
    upstream: &Upstream,
    target: &Target,
    port: u16,
) -> std::result::Result<TcpStream, ConnectError> {
    match upstream {
        Upstream::Direct { dns } => connect_direct(*dns, target, port).await,
        Upstream::Socks5 { addr, user, pass } => {
            let mut s = TcpStream::connect(addr.as_str())
                .await
                .map_err(|e| (REP_REFUSED, format!("proxy {addr} is not answering ({e})")))?;
            socks5_connect(&mut s, target, port, user.as_deref(), pass.as_deref())
                .await
                .map_err(|e| (REP_FAILURE, format!("proxy {addr}: {e}")))?;
            Ok(s)
        }
        Upstream::Http { addr, user, pass } => {
            let mut s = TcpStream::connect(addr.as_str())
                .await
                .map_err(|e| (REP_REFUSED, format!("proxy {addr} is not answering ({e})")))?;
            http_connect(&mut s, target, port, user.as_deref(), pass.as_deref())
                .await
                .map_err(|e| (REP_FAILURE, format!("proxy {addr}: {e}")))?;
            Ok(s)
        }
    }
}

async fn connect_direct(
    dns: DnsMode,
    target: &Target,
    port: u16,
) -> std::result::Result<TcpStream, ConnectError> {
    let addrs: Vec<SocketAddr> = match target {
        Target::Ip(ip) => vec![SocketAddr::new(*ip, port)],
        Target::Domain(host) if dns == DnsMode::System => {
            tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|e| {
                    (
                        REP_HOST_UNREACHABLE,
                        format!("could not look up {host}: {e}"),
                    )
                })?
                .collect()
        }
        Target::Domain(host) => privacy::doh_lookup(dns, host)
            .await
            .map_err(|e| (REP_HOST_UNREACHABLE, format!("{host}: {e}")))?
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect(),
    };
    let mut last = String::from("no addresses");
    // IPv4 first: it is the more reliable choice on home networks.
    let mut ordered = addrs;
    ordered.sort_by_key(|a| a.is_ipv6());
    for a in ordered {
        match tokio::time::timeout(Duration::from_secs(8), TcpStream::connect(a)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = format!("{a} timed out"),
        }
    }
    Err((
        REP_HOST_UNREACHABLE,
        format!("could not connect to {}: {last}", target.host()),
    ))
}

/// Client side of SOCKS5, used to chain to Tor, WARP or a VPN proxy.
/// Names are always sent as names, so the proxy does the DNS lookup.
async fn socks5_connect(
    s: &mut TcpStream,
    target: &Target,
    port: u16,
    user: Option<&str>,
    pass: Option<&str>,
) -> std::io::Result<()> {
    use std::io::Error as IoError;
    let bad = |m: &str| IoError::other(m.to_string());

    let with_auth = user.is_some();
    if with_auth {
        s.write_all(&[5, 2, 0, 2]).await?;
    } else {
        s.write_all(&[5, 1, 0]).await?;
    }
    let mut choice = [0u8; 2];
    s.read_exact(&mut choice).await?;
    match choice[1] {
        0 => {}
        2 if with_auth => {
            let u = user.unwrap_or_default().as_bytes();
            let p = pass.unwrap_or_default().as_bytes();
            if u.len() > 255 || p.len() > 255 {
                return Err(bad("proxy user name or password is too long"));
            }
            let mut msg = vec![1, u.len() as u8];
            msg.extend_from_slice(u);
            msg.push(p.len() as u8);
            msg.extend_from_slice(p);
            s.write_all(&msg).await?;
            let mut status = [0u8; 2];
            s.read_exact(&mut status).await?;
            if status[1] != 0 {
                return Err(bad("the proxy rejected the user name or password"));
            }
        }
        0xff => return Err(bad("the proxy wants a login that was not given")),
        _ => return Err(bad("the proxy asked for an unsupported login method")),
    }

    let mut req = vec![5, 1, 0];
    match target {
        Target::Ip(IpAddr::V4(ip)) => {
            req.push(1);
            req.extend_from_slice(&ip.octets());
        }
        Target::Ip(IpAddr::V6(ip)) => {
            req.push(4);
            req.extend_from_slice(&ip.octets());
        }
        Target::Domain(d) => {
            if d.len() > 255 {
                return Err(bad("host name too long"));
            }
            req.push(3);
            req.push(d.len() as u8);
            req.extend_from_slice(d.as_bytes());
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;

    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    if head[1] != 0 {
        let why = match head[1] {
            2 => "the proxy's rules do not allow this connection",
            3 => "network unreachable",
            4 => "host unreachable",
            5 => "connection refused",
            6 => "connection timed out",
            _ => "the proxy could not connect",
        };
        return Err(bad(why));
    }
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await?;
            l[0] as usize
        }
        _ => return Err(bad("bad reply from proxy")),
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await?;
    Ok(())
}

async fn http_connect(
    s: &mut TcpStream,
    target: &Target,
    port: u16,
    user: Option<&str>,
    pass: Option<&str>,
) -> std::io::Result<()> {
    use std::io::Error as IoError;
    let hostport = format!("{}:{port}", target.host());
    let mut req = format!("CONNECT {hostport} HTTP/1.1\r\nHost: {hostport}\r\n");
    if let Some(u) = user {
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{u}:{}", pass.unwrap_or_default()));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await?;

    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte).await? == 0 || buf.len() > 8192 {
            return Err(IoError::other("the proxy closed the connection"));
        }
        buf.push(byte[0]);
    }
    let status_line = String::from_utf8_lossy(&buf);
    let status = status_line.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        let msg = if status == "407" {
            "the proxy wants a user name and password".to_string()
        } else {
            format!("the proxy answered {status}")
        };
        return Err(IoError::other(msg));
    }
    Ok(())
}

/// Known local proxies that make good fast private routes, if running.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DetectedRoutes {
    /// Tor's SOCKS port, if Tor Browser or the Tor service is running.
    pub tor: Option<String>,
    /// Cloudflare WARP in proxy mode (default port 40000).
    pub warp: Option<String>,
}

pub const WARP_PROXY: &str = "127.0.0.1:40000";

pub async fn detect_routes() -> DetectedRoutes {
    let tor = privacy::find_tor(None).await.ok();
    let warp = match tokio::time::timeout(
        Duration::from_millis(800),
        TcpStream::connect(WARP_PROXY),
    )
    .await
    {
        Ok(Ok(_)) => Some(WARP_PROXY.to_string()),
        _ => None,
    };
    DetectedRoutes { tor, warp }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_urls() {
        assert_eq!(
            Upstream::from_proxy_url("socks5://user:p%40ss@10.64.0.1:1080").unwrap(),
            Upstream::Socks5 {
                addr: "10.64.0.1:1080".into(),
                user: Some("user".into()),
                pass: Some("p@ss".into())
            }
        );
        assert_eq!(
            Upstream::from_proxy_url("127.0.0.1:40000").unwrap(),
            Upstream::Socks5 {
                addr: "127.0.0.1:40000".into(),
                user: None,
                pass: None
            }
        );
        assert!(matches!(
            Upstream::from_proxy_url("http://proxy.test:8080").unwrap(),
            Upstream::Http { .. }
        ));
        let d = Upstream::from_proxy_url("socks5://me:secret@h.test:1")
            .unwrap()
            .describe();
        assert!(d.contains("me@") && !d.contains("secret"), "{d}");
    }
}
