use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Response;

use crate::error::{Error, Result};

pub const DEFAULT_USER_AGENT: &str = concat!("BM-Osprey/", env!("CARGO_PKG_VERSION"));

/// Extra request settings, mostly for sites that check the Referer,
/// cookies or the user agent before serving media.
#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub headers: Vec<(String, String)>,
    pub user_agent: Option<String>,
    /// Route, DNS and identity. Applies to every request built from
    /// these options, including filter list updates and link bypassing.
    pub privacy: crate::privacy::Privacy,
    /// In Tor mode, the circuit to use. Different values get different
    /// Tor circuits (Tor isolates streams by SOCKS login).
    pub circuit: Option<String>,
}

impl RequestOptions {
    /// Adds a header written as `Name: value`.
    pub fn push_header_line(&mut self, line: &str) -> Result<()> {
        let (k, v) = parse_header_line(line).ok_or_else(|| {
            Error::Unsupported(format!("header must look like 'Name: value', got '{line}'"))
        })?;
        self.headers.push((k, v));
        Ok(())
    }
}

pub fn parse_header_line(line: &str) -> Option<(String, String)> {
    let (k, v) = line.split_once(':')?;
    let k = k.trim();
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.trim().to_string()))
}

pub fn build_client(opts: &RequestOptions) -> Result<reqwest::Client> {
    builder(opts)?.build().map_err(Into::into)
}

/// A client that reports redirects instead of following them. The link
/// bypasser uses it to record every hop.
pub fn build_client_no_redirect(opts: &RequestOptions) -> Result<reqwest::Client> {
    builder(opts)?
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(Into::into)
}

fn builder(opts: &RequestOptions) -> Result<reqwest::ClientBuilder> {
    let mut headers = HeaderMap::new();
    for (k, v) in &opts.headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| Error::Unsupported(format!("bad header name '{k}': {e}")))?;
        let value = HeaderValue::from_str(v)
            .map_err(|e| Error::Unsupported(format!("bad header value for '{k}': {e}")))?;
        headers.insert(name, value);
    }
    let privacy = &opts.privacy;
    let ua = match (&opts.user_agent, privacy.generic_identity) {
        (Some(ua), _) => ua.clone(),
        (None, true) => crate::privacy::GENERIC_USER_AGENT.to_string(),
        (None, false) => DEFAULT_USER_AGENT.to_string(),
    };
    if privacy.generic_identity {
        let defaults = [
            (
                "accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
            ("accept-language", crate::privacy::GENERIC_ACCEPT_LANGUAGE),
        ];
        for (k, v) in defaults {
            if !headers.contains_key(k) {
                headers.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
            }
        }
    }

    // Tor and proxies are slower to connect. Give them more time.
    let connect_timeout = if privacy.route == crate::privacy::Route::Direct {
        15
    } else {
        45
    };

    let mut b = reqwest::Client::builder()
        .default_headers(headers)
        .user_agent(ua)
        .connect_timeout(Duration::from_secs(connect_timeout))
        .read_timeout(Duration::from_secs(90));

    // Setting a proxy also switches off any system proxy, so the chosen
    // route is the only route. An unreachable proxy makes requests fail.
    // They never fall back to a direct connection.
    if let Some(mut proxy) = privacy.proxy_url()? {
        // A circuit name goes in as the SOCKS login, unless the proxy
        // already has a login of its own.
        if let Some(c) = &opts.circuit {
            let has_login = proxy
                .split("://")
                .nth(1)
                .is_some_and(|rest| rest.contains('@'));
            if proxy.starts_with("socks5h://") && !has_login {
                proxy = proxy.replacen("socks5h://", &format!("socks5h://{c}:osprey@"), 1);
            }
        }
        b = b.proxy(reqwest::Proxy::all(&proxy)?);
    } else if privacy.dns != crate::privacy::DnsMode::System {
        let resolver = privacy.resolver().ok_or_else(|| {
            Error::Unsupported("could not start the encrypted DNS resolver".into())
        })?;
        b = b.dns_resolver(resolver);
    }
    Ok(b)
}

/// A small set of HTTP clients used together for one download. Normally
/// one. In Tor mode with several circuits, each client uses its own Tor
/// circuit, so parallel connections do not share one slow path.
#[derive(Clone)]
pub struct Clients(std::sync::Arc<Vec<reqwest::Client>>);

impl Clients {
    /// The client for connection or segment number `i`.
    pub fn pick(&self, i: usize) -> &reqwest::Client {
        &self.0[i % self.0.len()]
    }
    pub fn first(&self) -> &reqwest::Client {
        &self.0[0]
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&reqwest::Client> for Clients {
    fn from(c: &reqwest::Client) -> Self {
        Self(std::sync::Arc::new(vec![c.clone()]))
    }
}

impl From<reqwest::Client> for Clients {
    fn from(c: reqwest::Client) -> Self {
        Self(std::sync::Arc::new(vec![c]))
    }
}

impl From<&Clients> for Clients {
    fn from(c: &Clients) -> Self {
        c.clone()
    }
}

/// Builds the clients for one download. Several when `privacy.circuits`
/// is above 1 and the route is Tor, or a SOCKS proxy that leads to Tor
/// (like the browser's gateway in Tor mode). Callers only set circuits
/// for Tor, since an ordinary proxy may refuse unknown logins.
pub fn build_clients(opts: &RequestOptions) -> Result<Clients> {
    let socks = match &opts.privacy.route {
        crate::privacy::Route::Tor(_) => true,
        crate::privacy::Route::Proxy(p) => {
            crate::privacy::normalise_proxy(p).is_ok_and(|p| p.starts_with("socks5h://"))
        }
        crate::privacy::Route::Direct => false,
    };
    let n = if socks {
        opts.privacy.circuits.clamp(1, 8) as usize
    } else {
        1
    };
    if n == 1 {
        return Ok(build_client(opts)?.into());
    }
    // A fresh tag per download, so separate downloads cannot be linked by circuit.
    let tag = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() % 1_000_000_007)
        .unwrap_or(0);
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let mut o = opts.clone();
        o.circuit = Some(format!("osprey-{tag}-{i}"));
        v.push(build_client(&o)?);
    }
    Ok(Clients(std::sync::Arc::new(v)))
}

/// Turns a non 2xx response into a readable error.
pub fn ensure_success(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        Err(Error::Status {
            status: status.as_u16(),
            url: resp.url().to_string(),
        })
    }
}
