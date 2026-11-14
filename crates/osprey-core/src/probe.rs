use reqwest::header::{CONTENT_DISPOSITION, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use reqwest::StatusCode;
use serde::Serialize;

use crate::client::ensure_success;
use crate::detect::{classify, MediaKind};
use crate::error::Result;

#[derive(Debug, Clone, Serialize)]
pub struct ProbeInfo {
    pub url: String,
    /// The URL after redirects.
    pub final_url: String,
    pub kind: MediaKind,
    pub size: Option<u64>,
    /// True when the server honours `Range` requests, which is what makes
    /// multi connection downloads and resuming possible.
    pub accepts_ranges: bool,
    pub content_type: Option<String>,
    pub filename: Option<String>,
}

/// Asks the server about a URL without downloading it. A small range
/// request is used instead of HEAD because many CDNs answer HEAD badly.
/// The first bytes also show what the content really is: players often
/// serve playlists from addresses like `/source` with a generic type.
pub async fn probe(client: &reqwest::Client, url: &str) -> Result<ProbeInfo> {
    let first = client.get(url).header(RANGE, "bytes=0-511").send().await?;
    // Some servers reject range requests outright (416 for empty files,
    // 400/403 on fussy CDNs). Ask again the plain way before giving up.
    let resp = match first.status().as_u16() {
        400 | 403 | 416 | 501 => ensure_success(client.get(url).send().await?)?,
        _ => ensure_success(first)?,
    };
    let status = resp.status();
    let headers = resp.headers().clone();
    let final_url = resp.url().to_string();
    let header = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };

    let content_type = header(CONTENT_TYPE);
    let (size, accepts_ranges) = if status == StatusCode::PARTIAL_CONTENT {
        let total = header(CONTENT_RANGE).and_then(|cr| total_from_content_range(&cr));
        (total, true)
    } else {
        // A 200 reply to a range request means ranges do not really work,
        // whatever the Accept-Ranges header claims.
        (resp.content_length(), false)
    };
    // Read only the start, even when the server ignored the range.
    let mut head = Vec::new();
    let mut resp = resp;
    while head.len() < 512 {
        match resp.chunk().await {
            Ok(Some(c)) => head.extend_from_slice(&c),
            _ => break,
        }
    }
    drop(resp);

    let filename = header(CONTENT_DISPOSITION)
        .and_then(|cd| filename_from_disposition(&cd))
        .or_else(|| filename_from_url(&final_url));

    let kind = sniff_kind(&head)
        .or_else(|| classify(&final_url, content_type.as_deref()))
        .unwrap_or(MediaKind::Direct);

    Ok(ProbeInfo {
        url: url.to_string(),
        final_url,
        kind,
        size,
        accepts_ranges,
        content_type,
        filename,
    })
}

/// HLS and DASH by their first bytes, whatever the address or type says.
pub(crate) fn sniff_kind(head: &[u8]) -> Option<MediaKind> {
    let text = String::from_utf8_lossy(&head[..head.len().min(512)]);
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with("#EXTM3U") {
        Some(MediaKind::Hls)
    } else if t.starts_with("<?xml") && t.contains("<MPD") || t.starts_with("<MPD") {
        Some(MediaKind::Dash)
    } else {
        None
    }
}

fn total_from_content_range(v: &str) -> Option<u64> {
    // "bytes 0-0/12345" or "bytes 0-0/*"
    v.rsplit('/').next()?.trim().parse().ok()
}

fn filename_from_disposition(v: &str) -> Option<String> {
    for part in v.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            // RFC 5987: UTF-8''name%20here
            let encoded = rest.split("''").nth(1).unwrap_or(rest);
            let decoded = percent_decode(encoded.trim_matches('"'));
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    for part in v.split(';') {
        if let Some(rest) = part.trim().strip_prefix("filename=") {
            let name = rest.trim_matches('"').to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

pub(crate) fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.rev().find(|s| !s.is_empty())?;
    let name = percent_decode(last);
    (!name.is_empty()).then_some(name)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlists_are_known_by_their_bytes() {
        assert_eq!(
            sniff_kind(b"#EXTM3U\n#EXT-X-VERSION:3"),
            Some(MediaKind::Hls)
        );
        assert_eq!(sniff_kind(b"\xef\xbb\xbf  #EXTM3U"), Some(MediaKind::Hls));
        assert_eq!(
            sniff_kind(b"<?xml version=\"1.0\"?><MPD xmlns="),
            Some(MediaKind::Dash)
        );
        assert_eq!(sniff_kind(b"\x00\x00\x00\x18ftypmp42"), None);
    }

    #[test]
    fn reads_content_range_total() {
        assert_eq!(total_from_content_range("bytes 0-0/12345"), Some(12345));
        assert_eq!(total_from_content_range("bytes 0-0/*"), None);
    }

    #[test]
    fn reads_filenames() {
        assert_eq!(
            filename_from_disposition(r#"attachment; filename="My Clip.mp4""#).as_deref(),
            Some("My Clip.mp4")
        );
        assert_eq!(
            filename_from_disposition("attachment; filename*=UTF-8''caf%C3%A9.mp3").as_deref(),
            Some("café.mp3")
        );
        assert_eq!(
            filename_from_url("https://x.test/a/b/clip%201.mp4?x=1").as_deref(),
            Some("clip 1.mp4")
        );
        assert_eq!(filename_from_url("https://x.test/").as_deref(), None);
    }
}
