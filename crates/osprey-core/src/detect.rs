//! Media detection. This is the "sniffer" half of the app: given a URL
//! (and optionally its content type) it decides whether it is media, and
//! given page text or JSON it pulls out every media link it can find.
//! The browser's native capture feeds real responses into [`classify`].

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use url::Url;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
pub enum MediaKind {
    /// HTTP Live Streaming playlist (.m3u8), made of many small segments.
    Hls,
    /// MPEG-DASH manifest (.mpd), also segmented.
    Dash,
    /// A plain media file that can be fetched in one piece.
    #[default]
    Direct,
    /// A web page. Not media itself, but it may contain or lead to media.
    Page,
}

const DIRECT_EXTS: &[&str] = &[
    "mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "ts", "mp3", "m4a", "aac", "ogg", "oga",
    "opus", "flac", "wav",
];

/// Decides whether a URL is media, using the content type first and the
/// file extension as a fallback.
pub fn classify(url: &str, content_type: Option<&str>) -> Option<MediaKind> {
    if let Some(ct) = content_type {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("mpegurl") {
            return Some(MediaKind::Hls);
        }
        if ct.contains("dash+xml") {
            return Some(MediaKind::Dash);
        }
        if ct.starts_with("video/") || ct.starts_with("audio/") {
            return Some(MediaKind::Direct);
        }
        if ct.starts_with("text/html") || ct.starts_with("application/xhtml") {
            return Some(MediaKind::Page);
        }
    }
    let ext = extension_of(url)?;
    match ext.as_str() {
        "m3u8" => Some(MediaKind::Hls),
        "mpd" => Some(MediaKind::Dash),
        e if DIRECT_EXTS.contains(&e) => Some(MediaKind::Direct),
        _ => None,
    }
}

fn extension_of(url: &str) -> Option<String> {
    let path = match Url::parse(url) {
        Ok(u) => u.path().to_string(),
        Err(_) => url.split(['?', '#']).next().unwrap_or(url).to_string(),
    };
    let last = path.rsplit('/').next()?;
    let (_, ext) = last.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FoundMedia {
    pub url: String,
    pub kind: MediaKind,
}

fn link_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:https?:)?//[^\s"'<>`\\]+|[a-z0-9_\-./%~]+\.(?:m3u8|mpd|mp4|m4v|webm|mkv|mov|mp3|m4a|aac|ogg|opus|flac|wav)(?:\?[^\s"'<>`\\]*)?"#,
        )
        .expect("static regex is valid")
    })
}

/// Scans HTML, JavaScript or JSON for media links. Escaped slashes as
/// found in JSON (`\/`) are handled. Relative links are resolved against
/// `base`. Results keep page order and contain no duplicates.
pub fn scan_text(text: &str, base: &Url) -> Vec<FoundMedia> {
    let text = text
        .replace("\\/", "/")
        .replace("\\u002F", "/")
        .replace("\\u002f", "/");
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in link_regex().find_iter(&text) {
        let raw = m.as_str().trim_end_matches(['.', ',', ')', ';']);
        let Ok(abs) = base.join(raw) else { continue };
        if !matches!(abs.scheme(), "http" | "https") {
            continue;
        }
        let abs = abs.to_string();
        if let Some(kind) = classify(&abs, None) {
            if seen.insert(abs.clone()) {
                out.push(FoundMedia { url: abs, kind });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_content_type_and_extension() {
        assert_eq!(
            classify("https://x/a", Some("application/vnd.apple.mpegurl")),
            Some(MediaKind::Hls)
        );
        assert_eq!(
            classify("https://x/a", Some("application/dash+xml")),
            Some(MediaKind::Dash)
        );
        assert_eq!(
            classify("https://x/a", Some("video/mp4")),
            Some(MediaKind::Direct)
        );
        assert_eq!(
            classify("https://x/master.m3u8?token=1", None),
            Some(MediaKind::Hls)
        );
        assert_eq!(
            classify("https://x/clip.MP4", None),
            Some(MediaKind::Direct)
        );
        assert_eq!(classify("https://x/page.html", None), None);
        assert_eq!(classify("https://x/video.mp4/thumb.jpg", None), None);
    }

    #[test]
    fn scans_html_and_json() {
        let base = Url::parse("https://site.test/watch/42").unwrap();
        let page = r#"
            <video src="/media/clip.mp4"></video>
            <script>var cfg = {"hls":"https:\/\/cdn.test\/v\/master.m3u8?sig=abc","poster":"/p.jpg"};</script>
            <a href="//cdn.test/audio/song.mp3">song</a>
            <source src="/media/clip.mp4">
        "#;
        let found = scan_text(page, &base);
        let urls: Vec<_> = found.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://site.test/media/clip.mp4",
                "https://cdn.test/v/master.m3u8?sig=abc",
                "https://cdn.test/audio/song.mp3",
            ]
        );
        assert_eq!(found[1].kind, MediaKind::Hls);
    }
}
