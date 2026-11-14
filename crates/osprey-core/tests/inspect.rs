//! The 1DM style checks: what a link really is, decided by the server's
//! answer, not by how the link looks.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use osprey_core::inspect::{inspect, Verdict};
use osprey_core::MediaKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// path -> (content type, body). Range requests are honoured.
type Site = HashMap<&'static str, (&'static str, Vec<u8>)>;

async fn serve(site: Site) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let site = Arc::new(site);
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let site = site.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                let path = req
                    .split(' ')
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap()
                    .to_string();
                let range = req.lines().find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("range: bytes=")
                        .map(str::to_string)
                });
                let Some((ct, body)) = site.get(path.as_str()) else {
                    let page = b"<html>Not found</html>";
                    let head = format!("HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", page.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(page).await;
                    return;
                };
                let (status, extra, part) = match range {
                    Some(r) => {
                        let (a, b) = r.trim().split_once('-').unwrap();
                        let a: usize = a.parse().unwrap();
                        let b: usize = b
                            .parse::<usize>()
                            .unwrap_or(body.len() - 1)
                            .min(body.len() - 1);
                        (
                            "206 Partial Content",
                            format!("Content-Range: bytes {a}-{b}/{}\r\n", body.len()),
                            &body[a..=b],
                        )
                    }
                    None => ("200 OK", String::new(), &body[..]),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    part.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(part).await;
            });
        }
    });
    addr
}

/// MP4 header (1280x720, 95 s) followed by filler.
fn mp4(seconds: u32, total: usize) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&[0, 0, 0, 16]);
    v.extend_from_slice(b"ftypisom\0\0\0\0");
    let mut mvhd = vec![0u8; 108];
    mvhd[0..4].copy_from_slice(&108u32.to_be_bytes());
    mvhd[4..8].copy_from_slice(b"mvhd");
    mvhd[20..24].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[24..28].copy_from_slice(&(seconds * 1000).to_be_bytes());
    let mut tkhd = vec![0u8; 92];
    tkhd[0..4].copy_from_slice(&92u32.to_be_bytes());
    tkhd[4..8].copy_from_slice(b"tkhd");
    tkhd[84..88].copy_from_slice(&(1280u32 << 16).to_be_bytes());
    tkhd[88..92].copy_from_slice(&(720u32 << 16).to_be_bytes());
    v.extend_from_slice(&((108 + 92 + 8) as u32).to_be_bytes());
    v.extend_from_slice(b"moov");
    v.extend(mvhd);
    v.extend(tkhd);
    v.resize(total, 0xAB);
    v
}

fn site() -> Site {
    let master = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Japanese\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO=\"a\"\n360.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,AUDIO=\"a\"\n1080.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,AUDIO=\"a\"\n720.m3u8\n";
    let mut media = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:10\n");
    for i in 0..144 {
        media.push_str(&format!("#EXTINF:10.0,\nseg{i}.ts\n"));
    }
    media.push_str("#EXT-X-ENDLIST\n");
    HashMap::from([
        // A stream served with a misleading type and no .m3u8 in the name.
        (
            "/api/source/playlist",
            ("text/plain", master.as_bytes().to_vec()),
        ),
        (
            "/api/source/360.m3u8",
            ("application/vnd.apple.mpegurl", media.clone().into_bytes()),
        ),
        (
            "/api/source/1080.m3u8",
            ("application/vnd.apple.mpegurl", media.clone().into_bytes()),
        ),
        (
            "/api/source/720.m3u8",
            ("application/vnd.apple.mpegurl", media.clone().into_bytes()),
        ),
        (
            "/api/source/audio.m3u8",
            ("application/vnd.apple.mpegurl", media.into_bytes()),
        ),
        (
            "/v/episode.mp4",
            ("application/octet-stream", mp4(95, 300 * 1024)),
        ),
        ("/ads/preroll.mp4", ("video/mp4", mp4(5, 30 * 1024))),
        (
            "/fake/clip.mp4",
            ("video/mp4", b"<!DOCTYPE html><html>nope</html>".to_vec()),
        ),
        (
            "/img/poster.mp4.jpg",
            ("image/jpeg", vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
        ),
    ])
}

fn media(v: Verdict) -> Box<osprey_core::inspect::MediaInfo> {
    match v {
        Verdict::Media(m) => m,
        Verdict::Skip(why) => panic!("expected media, got skip: {why}"),
    }
}

fn skip(v: Verdict) -> String {
    match v {
        Verdict::Skip(why) => why,
        Verdict::Media(m) => panic!("expected skip, got {m:?}"),
    }
}

#[tokio::test]
async fn hls_lists_every_quality_with_size() {
    let addr = serve(site()).await;
    let c = reqwest::Client::new();
    let m = media(
        inspect(&c, &format!("http://{addr}/api/source/playlist"))
            .await
            .unwrap(),
    );
    assert_eq!(m.kind, MediaKind::Hls);
    assert_eq!(m.duration, Some(1440.0));
    assert!(m.separate_audio);
    assert!(!m.live);
    let heights: Vec<_> = m.qualities.iter().map(|q| q.height).collect();
    assert_eq!(heights, vec![Some(1080), Some(720), Some(360)]);
    // 5 Mbit/s for 24 minutes is 900 MB.
    assert_eq!(m.qualities[0].est_size, Some(900_000_000));
    assert_eq!((m.width, m.height), (Some(1920), Some(1080)));
}

#[tokio::test]
async fn files_are_checked_by_their_bytes() {
    let addr = serve(site()).await;
    let c = reqwest::Client::new();
    let m = media(
        inspect(&c, &format!("http://{addr}/v/episode.mp4"))
            .await
            .unwrap(),
    );
    assert_eq!(m.kind, MediaKind::Direct);
    assert_eq!(m.size, Some(300 * 1024));
    assert_eq!(
        (m.width, m.height, m.duration),
        (Some(1280), Some(720), Some(95.0))
    );
}

#[tokio::test]
async fn junk_is_dropped_with_a_reason() {
    let addr = serve(site()).await;
    let c = reqwest::Client::new();
    let r = skip(
        inspect(&c, &format!("http://{addr}/player/_TPL_.mp4"))
            .await
            .unwrap(),
    );
    assert!(r.contains("404"), "{r}");
    let r = skip(
        inspect(&c, &format!("http://{addr}/ads/preroll.mp4"))
            .await
            .unwrap(),
    );
    assert!(r.contains("preview or ad"), "{r}");
    let r = skip(
        inspect(&c, &format!("http://{addr}/fake/clip.mp4"))
            .await
            .unwrap(),
    );
    assert!(r.contains("web page"), "{r}");
    let r = skip(
        inspect(&c, &format!("http://{addr}/img/poster.mp4.jpg"))
            .await
            .unwrap(),
    );
    assert!(r.contains("image/jpeg"), "{r}");
}
