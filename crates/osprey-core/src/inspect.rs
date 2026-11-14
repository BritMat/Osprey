//! Media inspection, the way 1DM does it: instead of trusting how a link
//! looks, ask the server what is really there.
//!
//! [`inspect`] fetches the first bytes of a candidate and decides from the
//! status, the content type and the bytes themselves:
//!
//! - an HLS playlist: every quality is listed with its resolution,
//!   bitrate, duration and estimated size
//! - a video or audio file: its size, and for MP4 its resolution and
//!   duration, read from the file header
//! - anything else (an error page, an image, a template URL, a tiny ad
//!   clip): dropped, with the reason
//!
//! Only up to 64 KB are read per candidate, plus one playlist for the
//! duration of a stream.

use reqwest::header::{CONTENT_RANGE, CONTENT_TYPE, RANGE};
use serde::Serialize;

use crate::detect::MediaKind;
use crate::error::{Error, Result};
use crate::hls::{self, Playlist};

const PEEK: u64 = 64 * 1024;
/// Smaller than this is not worth listing: previews, ad clips, beacons.
const MIN_VIDEO: u64 = 200 * 1024;
const MIN_AUDIO: u64 = 40 * 1024;

/// Whether a file of this size is too small to be worth listing.
pub fn too_small(size: u64, audio: bool) -> bool {
    size < if audio { MIN_AUDIO } else { MIN_VIDEO }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Quality {
    /// The variant playlist.
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Bits per second, as the playlist states it.
    pub bandwidth: u64,
    pub codecs: Option<String>,
    /// bandwidth x duration. A guide, not an exact size.
    pub est_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MediaInfo {
    pub url: String,
    pub kind: MediaKind,
    pub content_type: Option<String>,
    /// Exact size for files.
    pub size: Option<u64>,
    /// Seconds.
    pub duration: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// HLS qualities, best first.
    pub qualities: Vec<Quality>,
    pub separate_audio: bool,
    pub live: bool,
    /// Why it cannot be saved, when it cannot (DRM, DASH for now).
    pub blocked: Option<String>,
    pub audio_only: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum Verdict {
    Media(Box<MediaInfo>),
    /// Not media, or not worth listing, with the reason.
    Skip(String),
}

fn base(url: &str, kind: MediaKind, ct: Option<String>) -> MediaInfo {
    MediaInfo {
        url: url.to_string(),
        kind,
        content_type: ct,
        size: None,
        duration: None,
        width: None,
        height: None,
        qualities: Vec::new(),
        separate_audio: false,
        live: false,
        blocked: None,
        audio_only: false,
    }
}

/// Looks at what `url` really is. `client` should carry the page's
/// cookies, referrer and user agent, as many servers refuse otherwise.
pub async fn inspect(client: &reqwest::Client, url: &str) -> Result<Verdict> {
    let resp = client
        .get(url)
        .header(RANGE, format!("bytes=0-{}", PEEK - 1))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        return Ok(Verdict::Skip(format!(
            "server answered {}",
            status.as_u16()
        )));
    }
    let final_url = resp.url().to_string();
    let headers = resp.headers().clone();
    let ct = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase());
    let total = headers
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|t| t.trim().parse::<u64>().ok())
        .or_else(|| {
            (status.as_u16() == 200)
                .then(|| resp.content_length())
                .flatten()
        });
    let body = resp.bytes().await?.to_vec();

    let text_start = String::from_utf8_lossy(&body[..body.len().min(512)])
        .trim_start_matches('\u{feff}')
        .trim_start()
        .to_string();
    let ctv = ct.clone().unwrap_or_default();

    if text_start.starts_with("#EXTM3U") || ctv.contains("mpegurl") {
        return inspect_hls(client, &final_url, &body, total, ct).await;
    }
    if ctv.contains("dash+xml") || text_start.contains("<MPD") {
        let mut info = base(&final_url, MediaKind::Dash, ct);
        info.blocked = Some("DASH streams are not supported yet".into());
        return Ok(Verdict::Media(Box::new(info)));
    }

    let magic = sniff_magic(&body);
    let is_video = ctv.starts_with("video/")
        || matches!(
            magic,
            Some(Magic::Mp4 | Magic::Matroska | Magic::Flv | Magic::Ts)
        );
    let is_audio = !is_video
        && (ctv.starts_with("audio/")
            || matches!(
                magic,
                Some(Magic::Mp3 | Magic::Ogg | Magic::Flac | Magic::Wav)
            ));
    if !is_video && !is_audio {
        let what = if ctv.is_empty() {
            "unknown content".to_string()
        } else {
            ctv.split(';').next().unwrap_or("").to_string()
        };
        return Ok(Verdict::Skip(format!("not media ({what})")));
    }
    // A video type with bytes that are clearly a web page is a fake.
    if text_start.to_ascii_lowercase().starts_with("<!doctype")
        || text_start.to_ascii_lowercase().starts_with("<html")
    {
        return Ok(Verdict::Skip("a web page, not media".into()));
    }
    if let Some(size) = total.filter(|s| too_small(*s, is_audio)) {
        return Ok(Verdict::Skip(format!(
            "only {size} bytes, likely a preview or ad"
        )));
    }

    let mut info = base(&final_url, MediaKind::Direct, ct);
    info.size = total;
    info.audio_only = is_audio;
    if magic == Some(Magic::Mp4) {
        let mp4 = read_mp4_header(&body);
        info.width = mp4.width;
        info.height = mp4.height;
        info.duration = mp4.duration;
    }
    Ok(Verdict::Media(Box::new(info)))
}

async fn inspect_hls(
    client: &reqwest::Client,
    url: &str,
    first: &[u8],
    total: Option<u64>,
    ct: Option<String>,
) -> Result<Verdict> {
    // Playlists are small, but fetch the whole thing if the peek cut it.
    let text = if total.is_some_and(|t| t > first.len() as u64) {
        crate::client::ensure_success(client.get(url).send().await?)?
            .text()
            .await?
    } else {
        String::from_utf8_lossy(first).into_owned()
    };
    let base_url = url::Url::parse(url)?;
    let mut info = base(url, MediaKind::Hls, ct);

    match hls::parse(&text, &base_url) {
        Err(Error::Drm(why)) => {
            info.blocked = Some(format!("DRM protected ({why}), cannot be saved"));
        }
        Err(e) => return Ok(Verdict::Skip(format!("broken playlist: {e}"))),
        Ok(Playlist::Media(m)) => {
            info.duration = Some(m.duration());
            info.live = !m.ended;
        }
        Ok(Playlist::Master(master)) => {
            info.separate_audio =
                !master.audio.is_empty() && master.audio.iter().any(|a| a.uri.is_some());
            // One variant playlist gives the duration for all of them.
            if let Some(v) = master.variants.iter().min_by_key(|v| v.bandwidth) {
                match hls::load(client, v.uri.as_str()).await {
                    Ok((Playlist::Media(m), _)) => {
                        info.duration = Some(m.duration());
                        info.live = !m.ended;
                    }
                    Err(Error::Drm(why)) => {
                        info.blocked = Some(format!("DRM protected ({why}), cannot be saved"))
                    }
                    _ => {}
                }
            }
            let mut q: Vec<Quality> = master
                .variants
                .iter()
                .map(|v| Quality {
                    url: v.uri.to_string(),
                    width: v.resolution.map(|r| r.0),
                    height: v.resolution.map(|r| r.1),
                    bandwidth: v.bandwidth,
                    codecs: v.codecs.clone(),
                    est_size: info
                        .duration
                        .filter(|_| !info.live)
                        .map(|d| (v.bandwidth as f64 * d / 8.0) as u64),
                })
                .collect();
            q.sort_by_key(|v| std::cmp::Reverse((v.height, v.bandwidth)));
            q.dedup_by(|a, b| a.height == b.height && a.bandwidth == b.bandwidth);
            if let Some(best) = q.first() {
                info.width = best.width;
                info.height = best.height;
            }
            info.qualities = q;
        }
    }
    Ok(Verdict::Media(Box::new(info)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Magic {
    Mp4,
    Matroska,
    Flv,
    Ts,
    Mp3,
    Ogg,
    Flac,
    Wav,
}

fn sniff_magic(b: &[u8]) -> Option<Magic> {
    if b.len() >= 12 && &b[4..8] == b"ftyp" {
        return Some(Magic::Mp4);
    }
    if b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(Magic::Matroska);
    }
    if b.starts_with(b"FLV") {
        return Some(Magic::Flv);
    }
    if b.len() > 376 && b[0] == 0x47 && b[188] == 0x47 && b[376] == 0x47 {
        return Some(Magic::Ts);
    }
    if b.starts_with(b"ID3") || (b.len() > 1 && b[0] == 0xFF && b[1] & 0xE0 == 0xE0) {
        return Some(Magic::Mp3);
    }
    if b.starts_with(b"OggS") {
        return Some(Magic::Ogg);
    }
    if b.starts_with(b"fLaC") {
        return Some(Magic::Flac);
    }
    if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WAVE" {
        return Some(Magic::Wav);
    }
    None
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Mp4Header {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<f64>,
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_be_bytes(s.try_into().expect("8 bytes")))
}

/// Reads the size and length of an MP4 from its header boxes, when the
/// header is at the start of the file (most web video is).
pub(crate) fn read_mp4_header(b: &[u8]) -> Mp4Header {
    let mut out = Mp4Header::default();
    let find = |name: &[u8], from: usize| {
        b.get(from..)
            .and_then(|s| s.windows(4).position(|w| w == name))
            .map(|p| p + from)
    };

    // Movie header: timescale and duration.
    if let Some(p) = find(b"mvhd", 0) {
        let start = p - 4; // box size comes before the name
        let version = b.get(start + 8).copied().unwrap_or(0);
        let (scale, dur) = if version == 1 {
            (be32(b, start + 28), be64(b, start + 32))
        } else {
            (be32(b, start + 20), be32(b, start + 24).map(u64::from))
        };
        if let (Some(s), Some(d)) = (scale, dur) {
            if s > 0 && d > 0 && d != u64::from(u32::MAX) {
                out.duration = Some(d as f64 / f64::from(s));
            }
        }
    }
    // Track headers: the first one with a picture size is the video.
    let mut from = 0;
    while let Some(p) = find(b"tkhd", from) {
        from = p + 4;
        let Some(start) = p.checked_sub(4) else {
            continue;
        };
        let version = b.get(start + 8).copied().unwrap_or(0);
        let off = if version == 1 { 96 } else { 84 };
        if let (Some(w), Some(h)) = (be32(b, start + off), be32(b, start + off + 4)) {
            let (w, h) = (w >> 16, h >> 16);
            if w > 0 && h > 0 && w < 16_384 && h < 16_384 {
                out.width = Some(w);
                out.height = Some(h);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal MP4 header: ftyp, then moov with mvhd and two tkhd boxes
    /// (audio first with no size, then a 1280x720 video track).
    pub(crate) fn fake_mp4(seconds: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0, 0, 0, 16]);
        v.extend_from_slice(b"ftypisom\0\0\0\0");
        let mut mvhd = vec![0u8; 108];
        mvhd[0..4].copy_from_slice(&108u32.to_be_bytes());
        mvhd[4..8].copy_from_slice(b"mvhd");
        mvhd[20..24].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[24..28].copy_from_slice(&(seconds * 1000).to_be_bytes());
        let tkhd = |w: u32, h: u32| {
            let mut t = vec![0u8; 92];
            t[0..4].copy_from_slice(&92u32.to_be_bytes());
            t[4..8].copy_from_slice(b"tkhd");
            t[84..88].copy_from_slice(&(w << 16).to_be_bytes());
            t[88..92].copy_from_slice(&(h << 16).to_be_bytes());
            t
        };
        let mut moov = Vec::new();
        moov.extend(mvhd);
        moov.extend(tkhd(0, 0));
        moov.extend(tkhd(1280, 720));
        v.extend_from_slice(&((moov.len() + 8) as u32).to_be_bytes());
        v.extend_from_slice(b"moov");
        v.extend(moov);
        v
    }

    #[test]
    fn reads_mp4_size_and_length() {
        let h = read_mp4_header(&fake_mp4(95));
        assert_eq!((h.width, h.height), (Some(1280), Some(720)));
        assert_eq!(h.duration, Some(95.0));
        assert_eq!(read_mp4_header(b"not an mp4"), Mp4Header::default());
    }

    #[test]
    fn recognises_file_types_by_their_bytes() {
        assert_eq!(sniff_magic(&fake_mp4(1)), Some(Magic::Mp4));
        assert_eq!(
            sniff_magic(&[0x1A, 0x45, 0xDF, 0xA3, 1]),
            Some(Magic::Matroska)
        );
        assert_eq!(sniff_magic(b"ID3\x04rest"), Some(Magic::Mp3));
        assert_eq!(sniff_magic(b"<!doctype html>"), None);
        let mut ts = vec![0u8; 400];
        ts[0] = 0x47;
        ts[188] = 0x47;
        ts[376] = 0x47;
        assert_eq!(sniff_magic(&ts), Some(Magic::Ts));
    }
}
