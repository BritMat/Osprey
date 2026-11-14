//! Native request capture, the way 1DM+ does it with Android's
//! shouldInterceptRequest: the web engine tells BM Osprey about every
//! response it receives, from every frame, so nothing has to be guessed
//! from how a link looks.
//!
//! This part is the same on every system. It decides what a response is
//! from its real status and content type, and which of the request headers
//! can be sent again. The hook into the engine is in `capture_win.rs` and
//! `capture_linux.rs`. macOS has no general hook, so it only has the
//! sniffer.

use std::sync::OnceLock;

use osprey_core::{classify, MediaKind};
use regex::Regex;
use reqwest::header::{HeaderName, HeaderValue};
use tauri::{AppHandle, Url};

use crate::{media, probe};

/// One response, as the engine saw it.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub url: String,
    pub method: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub content_range: Option<String>,
    pub content_length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judged {
    /// Worth asking the server about.
    Media(MediaKind),
    /// One piece of a stream. The playlist is what gets listed.
    Piece,
    /// Certainly not media, with the reason.
    NotMedia(String),
    /// Says nothing either way, like a preflight or a redirect.
    Unclear,
}

fn main_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn piece_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(^|[/_-])(seg|segment|chunk|frag)[-_]?\d+\.(mp4|m4a|m4v|aac)$")
            .expect("valid pattern")
    })
}

fn is_piece(url: &str, content_type: Option<&str>) -> bool {
    let t = content_type.map(main_type).unwrap_or_default();
    if matches!(
        t.as_str(),
        "video/mp2t" | "audio/mp2t" | "video/iso.segment" | "audio/iso.segment"
    ) {
        return true;
    }
    let Ok(u) = Url::parse(url) else {
        return false;
    };
    // The byte range is in the address, like `?bytes=0-1815` on ok.ru.
    if u.query_pairs().any(|(k, v)| {
        matches!(k.as_ref(), "bytes" | "range")
            && v.split_once('-').is_some_and(|(a, b)| {
                !a.is_empty() && [a, b].iter().all(|n| n.chars().all(|c| c.is_ascii_digit()))
            })
    }) {
        return true;
    }
    let name = u.path().rsplit('/').next().unwrap_or("");
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(ext.as_deref(), Some("ts" | "m4s" | "cmfv" | "cmfa")) || piece_name().is_match(name)
}

/// Types that settle it: a playlist or file is never sent as one of these,
/// so the server does not need to be asked. `text/plain` and
/// `application/octet-stream` settle nothing, since playlists use them.
fn clearly_not_media(t: &str) -> bool {
    t.starts_with("image/")
        || t.starts_with("font/")
        || matches!(
            t,
            "text/css"
                | "text/javascript"
                | "application/javascript"
                | "application/x-javascript"
                | "application/json"
                | "application/wasm"
        )
}

pub fn judge(r: &Seen) -> Judged {
    if r.method != "GET" || (300..400).contains(&r.status) && r.status != 304 {
        return Judged::Unclear;
    }
    if is_piece(&r.url, r.content_type.as_deref()) {
        return Judged::Piece;
    }
    // 204 has no body, Dailymotion answers a playlist address with it.
    if r.status >= 400 || r.status == 204 {
        return Judged::NotMedia(format!("the server answered {}", r.status));
    }
    let by_url = classify(&r.url, None).filter(|k| *k != MediaKind::Page);
    let t = r.content_type.as_deref().map(main_type).unwrap_or_default();
    match classify(&r.url, r.content_type.as_deref()) {
        // An error page at a media address is possible, and so is a
        // playlist labelled as a page. The check reads the bytes.
        Some(MediaKind::Page) => by_url
            .map(Judged::Media)
            .unwrap_or_else(|| Judged::NotMedia(t)),
        Some(k) => Judged::Media(k),
        None if clearly_not_media(&t) => Judged::NotMedia(t),
        None => Judged::Unclear,
    }
}

/// The full size of the file, from a partial answer's Content-Range or a
/// whole answer's Content-Length.
pub fn total_size(r: &Seen) -> Option<u64> {
    r.content_range
        .as_deref()
        .and_then(|v| v.rsplit('/').next())
        .and_then(|t| t.trim().parse().ok())
        .or(r.content_length.filter(|_| r.status == 200))
}

/// Pieces can look like plain files (`/v/12/345.mp4`). The same address
/// with only the numbers changed, seen many times, is a stream. Query
/// names count, because some servers use one path for everything and a
/// playlist there must not join the series of its own pieces.
pub fn series_key(url: &str) -> String {
    let Ok(u) = Url::parse(url) else {
        return url.to_string();
    };
    let mut key = u.host_str().unwrap_or("").to_string();
    let mut in_digits = false;
    for c in u.path().chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                key.push('#');
            }
            in_digits = true;
        } else {
            key.push(c);
            in_digits = false;
        }
    }
    let mut names: Vec<String> = u.query_pairs().map(|(k, _)| k.into_owned()).collect();
    names.sort();
    names.dedup();
    key.push('?');
    key.push_str(&names.join("&"));
    key
}

/// Most request headers worth keeping, and their sizes. Page scripts can
/// set some of them, so they are treated as untrusted.
const MAX_HEADERS: usize = 64;
const MAX_HEADER_VALUE: usize = 16 * 1024;

/// Which headers from a captured request can be sent again. Those about
/// this one connection or this one transfer are left out. A range or
/// cache check would change what the server answers, and reqwest handles
/// compression itself.
pub fn reusable(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    const NOT_THESE: &[&str] = &[
        "host",
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
        "accept-encoding",
        "range",
        "if-range",
        "if-match",
        "if-none-match",
        "if-modified-since",
        "if-unmodified-since",
        "cache-control",
        "pragma",
    ];
    headers
        .into_iter()
        .filter(|(k, v)| {
            !NOT_THESE.contains(&k.to_ascii_lowercase().as_str())
                && v.len() <= MAX_HEADER_VALUE
                // Also drops HTTP/2 names like ":authority".
                && HeaderName::from_bytes(k.as_bytes()).is_ok()
                && HeaderValue::from_str(v).is_ok()
        })
        .take(MAX_HEADERS)
        .collect()
}

/// Looks up a header by name, any case.
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Called by the engine hook for every response. The hook runs on the
/// window thread, so this has to stay cheap, and the request headers are
/// only read for what is kept.
#[cfg_attr(target_os = "macos", allow(dead_code))] // no hook there
pub fn on_response(
    app: &AppHandle,
    tab: u32,
    seen: Seen,
    read_headers: impl FnOnce() -> Vec<(String, String)>,
) {
    if !seen.url.starts_with("http") || is_own_address(&seen.url) {
        return;
    }
    let judged = judge(&seen);
    probe::network(&seen, &judged);
    let headers = match judged {
        Judged::Media(_) => read_headers(),
        Judged::NotMedia(_) => Vec::new(),
        Judged::Piece | Judged::Unclear => return,
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        media::captured(&app, tab, seen, judged, reusable(headers));
    });
}

/// The browser's own pages and its link to the app, served from custom
/// addresses on Windows.
fn is_own_address(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| {
        u.host_str().is_some_and(|h| {
            matches!(
                h,
                "tauri.localhost" | "ospreytext.localhost" | "ipc.localhost"
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(url: &str, status: u16, ct: Option<&str>) -> Seen {
        Seen {
            url: url.into(),
            method: "GET".into(),
            status,
            content_type: ct.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn real_type_beats_the_address() {
        use Judged::*;
        let j = |u, ct| judge(&seen(u, 200, ct));
        // No extension at all, only the response says what it is.
        assert_eq!(
            j("https://a.test/x?id=1", Some("video/mp4; codecs=avc1")),
            Media(MediaKind::Direct)
        );
        assert_eq!(
            j("https://a.test/get", Some("application/x-mpegURL")),
            Media(MediaKind::Hls)
        );
        assert_eq!(
            j("https://a.test/get", Some("application/dash+xml")),
            Media(MediaKind::Dash)
        );
        // A link that only looks like a playlist.
        assert_eq!(
            j("https://a.test/playlist/next", Some("application/json")),
            NotMedia("application/json".into())
        );
        assert_eq!(
            j("https://a.test/stream", Some("text/html; charset=utf-8")),
            NotMedia("text/html".into())
        );
    }

    #[test]
    fn unclear_types_fall_back_to_the_address() {
        use Judged::*;
        let j = |u, ct| judge(&seen(u, 200, ct));
        assert_eq!(
            j("https://a.test/m.m3u8", Some("text/plain")),
            Media(MediaKind::Hls)
        );
        assert_eq!(
            j("https://a.test/m.m3u8", Some("text/html")),
            Media(MediaKind::Hls),
            "the check reads the bytes"
        );
        assert_eq!(j("https://a.test/m", Some("text/plain")), Unclear);
        assert_eq!(j("https://a.test/m", None), Unclear);
    }

    #[test]
    fn pieces_are_not_listed() {
        let j = |u, ct| judge(&seen(u, 200, ct));
        assert_eq!(j("https://a.test/1.ts", Some("video/mp2t")), Judged::Piece);
        assert_eq!(j("https://a.test/x", Some("video/mp2t")), Judged::Piece);
        assert_eq!(
            j("https://a.test/v/seg-12.m4s", Some("video/mp4")),
            Judged::Piece
        );
        assert_eq!(
            j("https://a.test/v/seg-12-v1.mp4", Some("video/mp4")),
            Judged::Media(MediaKind::Direct)
        );
        assert_eq!(
            j("https://a.test/v/hd_chunk_12.mp4", Some("video/mp4")),
            Judged::Piece
        );
        assert_eq!(
            j("https://a.test/v/frag12.m4a", Some("audio/mp4")),
            Judged::Piece
        );
        assert_eq!(
            j("https://a.test/fragments.mp4", Some("video/mp4")),
            Judged::Media(MediaKind::Direct)
        );
        // The range is in the address, as on ok.ru.
        assert_eq!(
            j("https://a.test/?id=1&bytes=0-1815", Some("video/mp4")),
            Judged::Piece
        );
        assert_eq!(
            j("https://a.test/?range=1816-99", Some("audio/mp4")),
            Judged::Piece
        );
        assert_eq!(
            j("https://a.test/?range=recent", Some("video/mp4")),
            Judged::Media(MediaKind::Direct)
        );
    }

    #[test]
    fn status_and_method_count() {
        let mut r = seen("https://a.test/v.mp4", 403, Some("text/html"));
        assert_eq!(
            judge(&r),
            Judged::NotMedia("the server answered 403".into())
        );
        r.status = 204;
        assert_eq!(
            judge(&r),
            Judged::NotMedia("the server answered 204".into())
        );
        r.status = 302;
        assert_eq!(judge(&r), Judged::Unclear, "the next hop is what counts");
        r.status = 304;
        r.content_type = None;
        assert_eq!(judge(&r), Judged::Media(MediaKind::Direct));
        r.status = 200;
        r.method = "OPTIONS".into();
        assert_eq!(judge(&r), Judged::Unclear, "a preflight has no body");
    }

    #[test]
    fn size_comes_from_the_full_length() {
        let mut r = seen("https://a.test/v.mp4", 206, Some("video/mp4"));
        r.content_length = Some(1024);
        assert_eq!(total_size(&r), None, "a part is not the whole");
        r.content_range = Some("bytes 0-1023/987654".into());
        assert_eq!(total_size(&r), Some(987_654));
        r.content_range = Some("bytes 0-1023/*".into());
        assert_eq!(total_size(&r), None);
        r.status = 200;
        r.content_range = None;
        assert_eq!(total_size(&r), Some(1024));
    }

    #[test]
    fn numbered_addresses_share_a_series() {
        assert_eq!(
            series_key("https://a.test/v/12/345.mp4?t=1"),
            series_key("https://a.test/v/13/9.mp4?t=2")
        );
        assert_ne!(
            series_key("https://a.test/v/1/a.mp4"),
            series_key("https://b.test/v/1/a.mp4")
        );
        // One path for everything: the query tells them apart.
        assert_ne!(
            series_key("https://a.test/?id=1&ch=2"),
            series_key("https://a.test/?id=1&seq=2")
        );
    }

    #[test]
    fn only_safe_headers_are_sent_again() {
        let kept = reusable(
            [
                ("Referer", "https://site.test/"),
                ("Cookie", "a=1"),
                ("X-Token", "abc"),
                ("Range", "bytes=0-"),
                ("accept-encoding", "gzip, br"),
                ("If-None-Match", "\"x\""),
                ("Host", "cdn.test"),
                (":authority", "cdn.test"),
                ("Bad", "line\r\nInjected: 1"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .to_vec(),
        );
        let names: Vec<&str> = kept.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["Referer", "Cookie", "X-Token"]);
        assert_eq!(header(&kept, "COOKIE"), Some("a=1"));
        assert_eq!(header(&kept, "Origin"), None);
    }

    #[test]
    fn own_pages_are_ignored() {
        assert!(is_own_address("http://tauri.localhost/newtab.html"));
        assert!(is_own_address("http://ospreytext.localhost/?u=x"));
        assert!(is_own_address("http://ipc.localhost/media_found"));
        assert!(!is_own_address("https://site.test/"));
    }
}
