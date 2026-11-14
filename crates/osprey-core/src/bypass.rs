//! Link bypass add-on. Follows a short or wrapped link to where it really
//! goes, without opening it in a browser, and records every hop:
//!
//! - HTTP redirects (301, 302, 303, 307, 308)
//! - redirect wrappers that carry the target in the URL, like
//!   `?url=https://...` or `google.com/url?q=...`
//! - targets hidden as base64 in a parameter or path segment
//! - targets in the address fragment, like `/warning#https%3A%2F%2F...`
//! - `<meta http-equiv="refresh">` pages
//! - small interstitial pages that jump on with `location = "..."`
//!
//! The final link is cleaned of tracking parameters (utm_*, fbclid and
//! similar). Shorteners that make you solve a captcha or wait for a
//! server side timer cannot be skipped this way, and the result says so
//! when it notices one.
//!
//! [`resolve`] adds bypass.vip's paid API as an optional last resort for
//! links these rules cannot pass. It needs the user's own key and is off
//! otherwise, because the link then leaves the machine.

use std::collections::HashSet;
use std::sync::OnceLock;

use base64::Engine as _;
use regex::Regex;
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde::Serialize;
use url::Url;

use crate::client::{build_client_no_redirect, RequestOptions};
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Step {
    Redirect { status: u16 },
    UrlParameter { name: String },
    Base64,
    Fragment,
    PathTarget,
    MetaRefresh,
    ScriptRedirect,
    TrackingRemoved,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hop {
    pub url: String,
    pub via: Step,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bypassed {
    pub start: String,
    pub hops: Vec<Hop>,
    pub final_url: String,
    /// A hint when the chain stopped at something we cannot pass.
    pub note: Option<String>,
    /// The chain stopped at a captcha or timer page.
    pub human_check: bool,
}

/// Parameter names that commonly hold the destination of a redirect.
/// `redirect_uri`, `next` and `return` are left out on purpose: they
/// belong to login flows, not to link wrappers.
const TARGET_PARAMS: &[&str] = &[
    "url",
    "u",
    "link",
    "target",
    "dest",
    "destination",
    "redirect",
    "redirect_url",
    "redir",
    "to",
    "out",
    "go",
    "goto",
    "href",
    "rurl",
    "r",
    "l",
];

const TRACKING_PARAMS: &[&str] = &[
    "fbclid",
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "twclid",
    "ttclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_hsenc",
    "_hsmi",
    "mkt_tok",
    "ref_src",
    "ref_url",
    "oly_enc_id",
    "oly_anon_id",
    "vero_id",
    "wickedid",
    "rb_clickid",
    "s_cid",
    "_openstat",
    "si",
];

/// Follows `url` for at most `max_hops` steps.
pub async fn bypass(opts: &RequestOptions, url: &str, max_hops: usize) -> Result<Bypassed> {
    let client = build_client_no_redirect(opts)?;
    let mut current = Url::parse(url.trim())?;
    let mut hops: Vec<Hop> = Vec::new();
    let mut visited: HashSet<String> = HashSet::from([current.to_string()]);
    let mut note = None;
    let mut human_check = false;

    for _ in 0..max_hops {
        // Free steps first. They need no network at all.
        if let Some((next, via)) = target_in_url(&current) {
            if !visited.insert(next.to_string()) {
                break;
            }
            hops.push(Hop {
                url: next.to_string(),
                via,
            });
            current = next;
            continue;
        }

        let resp = match client.get(current.clone()).send().await {
            Ok(r) => r,
            Err(e) => {
                note = Some(format!("stopped, could not open {current}: {e}"));
                break;
            }
        };
        let status = resp.status();

        if status.is_redirection() {
            let Some(next) = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|loc| current.join(loc).ok())
            else {
                break;
            };
            if !visited.insert(next.to_string()) {
                note = Some("redirect loop detected".into());
                break;
            }
            hops.push(Hop {
                url: next.to_string(),
                via: Step::Redirect {
                    status: status.as_u16(),
                },
            });
            current = next;
            continue;
        }

        if !status.is_success() {
            note = Some(format!("the last link answered {status}"));
            break;
        }

        let is_html = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|ct| ct.contains("html"));
        if !is_html {
            break;
        }
        let body = resp.text().await.unwrap_or_default();

        let found = meta_refresh_target(&body)
            .map(|t| (t, Step::MetaRefresh))
            .or_else(|| {
                // Only trust script redirects on small pages. Big pages
                // set `location` for all sorts of reasons.
                (body.len() < 60_000)
                    .then(|| script_redirect_target(&body))
                    .flatten()
                    .map(|t| (t, Step::ScriptRedirect))
            });

        match found.and_then(|(t, via)| current.join(&t).ok().map(|u| (u, via))) {
            Some((next, via)) if is_web(&next) && visited.insert(next.to_string()) => {
                hops.push(Hop {
                    url: next.to_string(),
                    via,
                });
                current = next;
            }
            _ => {
                if needs_human(&body) {
                    human_check = true;
                    note = Some(
                        "this page wants a human check (captcha or timer). Open it in a browser to finish"
                            .into(),
                    );
                }
                break;
            }
        }
    }

    let cleaned = strip_tracking(&current);
    if cleaned != current {
        hops.push(Hop {
            url: cleaned.to_string(),
            via: Step::TrackingRemoved,
        });
        current = cleaned;
    }

    Ok(Bypassed {
        start: url.to_string(),
        hops,
        final_url: current.to_string(),
        note,
        human_check,
    })
}

fn is_web(u: &Url) -> bool {
    matches!(u.scheme(), "http" | "https")
}

fn parse_web_url(s: &str) -> Option<Url> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    Url::parse(s).ok().filter(is_web)
}

/// Reads a web address that is plain or URL encoded, once or twice.
fn decode_web_url(s: &str) -> Option<Url> {
    let mut text = s.to_string();
    for _ in 0..3 {
        if let Some(u) = parse_web_url(&text) {
            return Some(u);
        }
        let next = percent_decode(&text);
        if next == text {
            return None;
        }
        text = next;
    }
    None
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn decode_base64_url(s: &str) -> Option<Url> {
    // A `+` in a query reads as a space by the time it gets here.
    let s = s.trim().trim_end_matches('=').replace(' ', "+");
    let s = s.as_str();
    if s.len() < 12 {
        return None;
    }
    let engines = [
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ];
    for engine in engines {
        if let Ok(bytes) = engine.decode(s) {
            if let Ok(text) = String::from_utf8(bytes) {
                if let Some(u) = parse_web_url(&text) {
                    return Some(u);
                }
            }
        }
    }
    None
}

/// Finds a destination carried inside the URL itself.
pub fn target_in_url(u: &Url) -> Option<(Url, Step)> {
    let google_style = u.path() == "/url";
    for (name, value) in u.query_pairs() {
        let lname = name.to_ascii_lowercase();
        let is_target = TARGET_PARAMS.contains(&lname.as_str()) || (google_style && lname == "q");
        if is_target {
            if let Some(t) = decode_web_url(&value) {
                return Some((
                    t,
                    Step::UrlParameter {
                        name: name.into_owned(),
                    },
                ));
            }
        }
        if let Some(t) = decode_base64_url(&value) {
            return Some((t, Step::Base64));
        }
    }
    for seg in u.path_segments().into_iter().flatten() {
        if let Some(t) = decode_base64_url(seg) {
            return Some((t, Step::Base64));
        }
    }
    if let Some(t) = target_in_fragment(u) {
        return Some((t, Step::Fragment));
    }
    target_in_path(u).map(|t| (t, Step::PathTarget))
}

/// A destination after the `#`: the whole fragment, plain, URL encoded or
/// base64, or one of the usual names as in `#url=https...`.
fn target_in_fragment(u: &Url) -> Option<Url> {
    let fragment = u.fragment()?;
    if let Some(t) = decode_web_url(fragment) {
        return Some(t);
    }
    if let Some(t) = decode_base64_url(fragment) {
        return Some(t);
    }
    url::form_urlencoded::parse(fragment.as_bytes()).find_map(|(name, value)| {
        let known = TARGET_PARAMS.contains(&name.to_ascii_lowercase().as_str());
        known.then(|| decode_web_url(&value)).flatten()
    })
}

/// A destination written into the path, as in `/out/https://dest.test/a`.
/// Web archives are left alone, since their address is the page itself.
fn target_in_path(u: &Url) -> Option<Url> {
    if u.host_str().is_some_and(|h| h.contains("archive.")) {
        return None;
    }
    let path = u.path();
    let lower = path.to_ascii_lowercase();
    let at = ["/http:", "/https:", "/http%3a", "/https%3a"]
        .iter()
        .filter_map(|m| lower.find(m))
        .min()?;
    let mut text = percent_decode(&path[at + 1..]);
    // A browser folds `https://a` in a path to `https:/a`.
    for scheme in ["https:/", "http:/"] {
        if text.starts_with(scheme) && !text[scheme.len()..].starts_with('/') {
            text.insert(scheme.len(), '/');
        }
    }
    if let Some(q) = u.query() {
        text = format!("{text}?{q}");
    }
    parse_web_url(&text)
}

/// bypass.vip's paid API, as its own web page calls it (`assets/js/scripts.js`,
/// read on 5 Nov 2026). It has no public documentation.
pub const BYPASS_VIP_ENDPOINT: &str = "https://api.bypass.vip/premium/bypass";

/// The web page, for people without a key.
pub const BYPASS_VIP_PAGE: &str = "https://bypass.vip/";

/// Where to ask, and with which key.
#[derive(Clone, Copy)]
pub struct Fallback<'a> {
    pub endpoint: &'a str,
    pub key: &'a str,
}

/// True for a key of the form bypass.vip's page accepts.
pub fn valid_bypass_vip_key(key: &str) -> bool {
    (8..=512).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Who found the final link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    LocalRule,
    BypassVip,
}

impl ResolvedBy {
    pub fn label(self) -> &'static str {
        match self {
            Self::LocalRule => "local rule",
            Self::BypassVip => "bypass.vip",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    #[serde(flatten)]
    pub link: Bypassed,
    pub resolved_by: ResolvedBy,
}

/// Whether the API may see a link: switched on, with a key, and not
/// private. `private` always wins.
pub fn may_use_bypass_vip(setting_on: bool, has_key: bool, private: bool) -> bool {
    setting_on && has_key && !private
}

/// Whether to offer the web page for a link the rules could not resolve.
/// Only without a key, and never in private mode.
pub fn offer_bypass_vip_page(has_key: bool, private: bool, stuck: bool) -> bool {
    stuck && !has_key && !private
}

/// True when the local rules got nowhere with a link: nothing was followed,
/// or a captcha or timer page stopped them. Removing tracking does not count
/// as getting somewhere. A link that was followed to a dead page is not
/// stuck, since an outside service has nothing to add and need not see it.
pub fn stuck(result: &Bypassed) -> bool {
    let moved = result.hops.iter().any(|h| h.via != Step::TrackingRemoved);
    !moved || result.human_check
}

/// Reads bypass.vip's answer. It must hold a web address. The service has
/// answered `"status": "success"` with a sentence in `result`, so the status
/// alone proves nothing.
pub fn read_bypass_vip_answer(body: &str) -> std::result::Result<Url, String> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| "bypass.vip sent an answer that could not be read".to_string())?;
    if v["status"] != "success" {
        let why = v["message"].as_str().unwrap_or("no reason given");
        return Err(format!("bypass.vip could not resolve it: {why}"));
    }
    let result = v["result"].as_str().unwrap_or("");
    parse_web_url(result)
        .filter(|_| !result.trim().contains(char::is_whitespace))
        .ok_or_else(|| format!("bypass.vip gave no link. It said: {}", shorten(result)))
}

fn shorten(text: &str) -> String {
    text.chars().take(160).collect()
}

/// Asks bypass.vip about a link, over the same route as everything else.
pub async fn ask_bypass_vip(
    opts: &RequestOptions,
    fallback: Fallback<'_>,
    link: &str,
) -> std::result::Result<Url, String> {
    let client = crate::client::build_client(opts).map_err(|e| e.to_string())?;
    let resp = client
        .get(fallback.endpoint)
        .query(&[("url", link)])
        .header("x-api-key", fallback.key)
        .send()
        .await
        .map_err(|e| format!("could not reach bypass.vip: {e}"))?;
    let body = resp
        .text()
        .await
        .map_err(|e| format!("could not read the answer from bypass.vip: {e}"))?;
    read_bypass_vip_answer(&body)
}

/// Follows a link with the local rules, then asks `fallback` when they got
/// stuck. `None` keeps the link on this machine. A failed fallback leaves
/// the local result, with the reason.
pub async fn resolve(
    opts: &RequestOptions,
    url: &str,
    max_hops: usize,
    fallback: Option<Fallback<'_>>,
) -> Result<Resolved> {
    let mut link = bypass(opts, url, max_hops).await?;
    let mut resolved_by = ResolvedBy::LocalRule;
    if let Some(fallback) = fallback.filter(|_| stuck(&link)) {
        match ask_bypass_vip(opts, fallback, &link.final_url).await {
            Ok(found) => {
                link.final_url = strip_tracking(&found).to_string();
                link.note = None;
                resolved_by = ResolvedBy::BypassVip;
            }
            Err(why) => link.note = Some(why),
        }
    }
    Ok(Resolved { link, resolved_by })
}

fn meta_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<meta\b[^>]*>").expect("valid regex"))
}

fn meta_refresh_target(html: &str) -> Option<String> {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    let url_re = URL_RE.get_or_init(|| {
        Regex::new(r#"(?is)content\s*=\s*["']?\s*\d*\s*[;,]?\s*url\s*=\s*['"]?([^"'>\s]+)"#)
            .expect("valid regex")
    });
    for tag in meta_regex().find_iter(html) {
        let tag = tag.as_str();
        if tag.to_ascii_lowercase().contains("refresh") {
            if let Some(c) = url_re.captures(tag) {
                return Some(html_unescape(&c[1]));
            }
        }
    }
    None
}

fn script_redirect_target(html: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?:(?:window|document|top|self)\s*\.\s*)?location(?:\s*\.\s*href)?\s*=\s*["']([^"']+)["']|location\s*\.\s*(?:replace|assign)\s*\(\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    });
    let c = re.captures(html)?;
    let raw = c.get(1).or_else(|| c.get(2))?.as_str();
    Some(raw.replace("\\/", "/"))
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
}

/// True when the page is a human check that cannot be skipped here.
/// Challenge platforms count on any page. Generic captcha words only count
/// on small pages, since big normal pages (like a site's home page) often
/// mention captcha somewhere for their sign up form.
fn needs_human(html: &str) -> bool {
    let l = html.to_ascii_lowercase();
    let strong = [
        "challenges.cloudflare.com",
        "/cdn-cgi/challenge-platform",
        "cf-turnstile",
        "cf-chl-",
    ];
    if strong.iter().any(|w| l.contains(w)) {
        return true;
    }
    l.len() < 50_000
        && ["g-recaptcha", "hcaptcha", "captcha"]
            .iter()
            .any(|w| l.contains(w))
}

/// Removes tracking parameters, keeping everything else in order.
pub fn strip_tracking(u: &Url) -> Url {
    if u.query().is_none() {
        return u.clone();
    }
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !(k.starts_with("utm_") || TRACKING_PARAMS.contains(&k.as_str()))
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let original_count = u.query_pairs().count();
    if kept.len() == original_count {
        return u.clone();
    }
    let mut out = u.clone();
    if kept.is_empty() {
        out.set_query(None);
    } else {
        out.query_pairs_mut().clear().extend_pairs(kept);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn finds_targets_in_urls() {
        let (t, via) = target_in_url(&u(
            "https://out.site/redirect?url=https%3A%2F%2Fdest.test%2Fpage%3Fa%3D1",
        ))
        .unwrap();
        assert_eq!(t.as_str(), "https://dest.test/page?a=1");
        assert_eq!(via, Step::UrlParameter { name: "url".into() });

        let (t, _) =
            target_in_url(&u("https://www.google.com/url?sa=t&q=https://dest.test/x")).unwrap();
        assert_eq!(t.as_str(), "https://dest.test/x");
        // q is a search, not a redirect, outside /url
        assert!(target_in_url(&u("https://www.google.com/search?q=https://dest.test/x")).is_none());

        // base64 of https://dest.test/file.zip, in a parameter and in the path
        let b64 = "aHR0cHM6Ly9kZXN0LnRlc3QvZmlsZS56aXA";
        let (t, via) = target_in_url(&u(&format!("https://short.test/go?d={b64}"))).unwrap();
        assert_eq!(
            (t.as_str(), via),
            ("https://dest.test/file.zip", Step::Base64)
        );
        let (t, _) = target_in_url(&u(&format!("https://short.test/out/{b64}"))).unwrap();
        assert_eq!(t.as_str(), "https://dest.test/file.zip");

        assert!(target_in_url(&u(
            "https://login.test/auth?redirect_uri=https://app.test/cb"
        ))
        .is_none());
        assert!(target_in_url(&u("https://plain.test/watch?v=dQw4w9WgXcQ")).is_none());
    }

    #[test]
    fn finds_targets_carried_in_the_address() {
        // socialmediagirls style: base64 in ?url=.
        let plain = base64::engine::general_purpose::STANDARD.encode("https://dest.test/a?b=1");
        let (t, _) = target_in_url(&u(&format!(
            "https://forum.test/goto/link-confirmation?url={plain}"
        )))
        .unwrap();
        assert_eq!(t.as_str(), "https://dest.test/a?b=1");
        // A base64 value holding `+`, which a query turns into a space.
        let plus = base64::engine::general_purpose::STANDARD.encode("https://dest.test/?a=>>>");
        assert!(plus.contains('+'));
        let (t, _) = target_in_url(&u(&format!("https://forum.test/go?d={plus}"))).unwrap();
        assert_eq!(t.host_str(), Some("dest.test"));

        // Stremio style, URL encoded after the #.
        let (t, via) = target_in_url(&u(
            "https://www.stremio.com/warning#https%3A%2F%2Fdest.test%2Fa%3Fb%3D1",
        ))
        .unwrap();
        assert_eq!(
            (t.as_str(), via),
            ("https://dest.test/a?b=1", Step::Fragment)
        );
        // Other sites: plain, named and base64.
        let (t, _) = target_in_url(&u("https://any.test/warn#https://dest.test/x")).unwrap();
        assert_eq!(t.as_str(), "https://dest.test/x");
        let (t, _) =
            target_in_url(&u("https://any.test/warn#url=https%3A%2F%2Fdest.test%2Fx")).unwrap();
        assert_eq!(t.as_str(), "https://dest.test/x");
        let b64 = "aHR0cHM6Ly9kZXN0LnRlc3QvZmlsZS56aXA";
        let (t, _) = target_in_url(&u(&format!("https://any.test/warn#{b64}"))).unwrap();
        assert_eq!(t.as_str(), "https://dest.test/file.zip");

        // Encoded twice.
        let (t, _) = target_in_url(&u(
            "https://any.test/r?url=https%253A%252F%252Fdest.test%252Fx",
        ))
        .unwrap();
        assert_eq!(t.as_str(), "https://dest.test/x");

        // In the path.
        let (t, via) = target_in_url(&u("https://any.test/out/https://dest.test/a?b=1")).unwrap();
        assert_eq!(
            (t.as_str(), via),
            ("https://dest.test/a?b=1", Step::PathTarget)
        );
        let (t, _) = target_in_url(&u("https://any.test/out/https%3A%2F%2Fdest.test%2Fa")).unwrap();
        assert_eq!(t.as_str(), "https://dest.test/a");

        // Things that only look like it.
        assert!(target_in_url(&u("https://web.archive.org/web/2020/https://dest.test/")).is_none());
        assert!(target_in_url(&u("https://plain.test/page#section-2")).is_none());
        assert!(target_in_url(&u("https://plain.test/page#access_token=abcdefghijkl")).is_none());
    }

    #[test]
    fn bypass_vip_answers_are_checked() {
        assert_eq!(
            read_bypass_vip_answer(r#"{"status":"success","result":"https://dest.test/a"}"#)
                .unwrap()
                .as_str(),
            "https://dest.test/a"
        );
        // The real answer of the old free address on 5 Nov 2026.
        let shut = r#"{"status":"success","result":"FREE API SHUT DOWN THANKS TO LEECHERS, JOIN HERE FOR WORKING BYPASSES: https://bypass.vip/discord"}"#;
        assert!(read_bypass_vip_answer(shut)
            .unwrap_err()
            .contains("FREE API SHUT DOWN"));
        // Starts with an address, then carries on as a sentence.
        assert!(read_bypass_vip_answer(
            r#"{"status":"success","result":"https://bypass.vip/discord is where to go"}"#
        )
        .is_err());
        assert!(
            read_bypass_vip_answer(r#"{"status":"error","message":"Bad link"}"#)
                .unwrap_err()
                .contains("Bad link")
        );
        assert!(read_bypass_vip_answer("<html>").is_err());
    }

    #[test]
    fn bypass_vip_needs_a_key_and_is_never_private() {
        assert!(may_use_bypass_vip(true, true, false));
        assert!(!may_use_bypass_vip(true, false, false), "no key");
        assert!(!may_use_bypass_vip(true, true, true), "private");
        assert!(!may_use_bypass_vip(false, true, false), "switched off");

        assert!(offer_bypass_vip_page(false, false, true));
        assert!(!offer_bypass_vip_page(true, false, true), "has a key");
        assert!(!offer_bypass_vip_page(false, true, true), "private");
        assert!(!offer_bypass_vip_page(false, false, false), "resolved");
    }

    #[test]
    fn checks_the_form_of_a_key() {
        assert!(valid_bypass_vip_key("abcDEF12_-"));
        assert!(!valid_bypass_vip_key("short"));
        assert!(!valid_bypass_vip_key("has space in it"));
        assert!(!valid_bypass_vip_key("line\nbreak-in-key"));
        assert!(!valid_bypass_vip_key(&"a".repeat(513)));
    }

    #[test]
    fn stuck_means_the_rules_got_nowhere() {
        let hop = |via| Hop {
            url: "https://a.test/".into(),
            via,
        };
        let result = |hops, note: Option<&str>, human_check| Bypassed {
            start: "https://s.test/".into(),
            hops,
            final_url: "https://a.test/".into(),
            note: note.map(str::to_string),
            human_check,
        };
        assert!(stuck(&result(vec![], None, false)));
        assert!(stuck(&result(
            vec![hop(Step::TrackingRemoved)],
            None,
            false
        )));
        assert!(stuck(&result(
            vec![hop(Step::Redirect { status: 302 })],
            Some("captcha"),
            true
        )));
        assert!(!stuck(&result(vec![hop(Step::MetaRefresh)], None, false)));
        // Followed to a dead page: resolved, and bypass.vip need not see it.
        assert!(!stuck(&result(
            vec![hop(Step::Fragment)],
            Some("the last link answered 404 Not Found"),
            false
        )));
    }

    #[test]
    fn reads_meta_refresh_and_script_redirects() {
        let html = r#"<html><head><meta content="3; URL='https://dest.test/a?x=1&amp;y=2'" http-equiv="Refresh"></head></html>"#;
        assert_eq!(
            meta_refresh_target(html).as_deref(),
            Some("https://dest.test/a?x=1&y=2")
        );
        assert_eq!(
            meta_refresh_target(r#"<meta http-equiv=refresh content=0;url=/next>"#).as_deref(),
            Some("/next")
        );
        assert!(meta_refresh_target(r#"<meta charset="utf-8">"#).is_none());

        assert_eq!(
            script_redirect_target(
                r#"<script>window.location.href = "https:\/\/dest.test\/b";</script>"#
            )
            .as_deref(),
            Some("https://dest.test/b")
        );
        assert_eq!(
            script_redirect_target(r#"<script>location.replace('https://dest.test/c')</script>"#)
                .as_deref(),
            Some("https://dest.test/c")
        );
    }

    #[test]
    fn human_checks_are_not_guessed_on_big_pages() {
        assert!(needs_human(r#"<div class="g-recaptcha"></div>"#));
        assert!(needs_human(&format!(
            "{}<script src=\"https://challenges.cloudflare.com/turnstile/v0/api.js\">",
            "x".repeat(80_000)
        )));
        // A big normal page that mentions captcha for its sign up form.
        assert!(!needs_human(&format!(
            "{}<script>hcaptcha signup</script>",
            "x".repeat(80_000)
        )));
    }

    #[test]
    fn strips_tracking() {
        let cleaned = strip_tracking(&u(
            "https://shop.test/p?id=5&utm_source=x&utm_medium=y&fbclid=abc",
        ));
        assert_eq!(cleaned.as_str(), "https://shop.test/p?id=5");
        let cleaned = strip_tracking(&u("https://shop.test/p?utm_source=x"));
        assert_eq!(cleaned.as_str(), "https://shop.test/p");
        let same = u("https://shop.test/p?id=5");
        assert_eq!(strip_tracking(&same), same);
    }
}
