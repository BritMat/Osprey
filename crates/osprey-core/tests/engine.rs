//! End to end tests against a tiny local HTTP server, so they run offline
//! in CI on every platform.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use osprey_core::{
    build_client, direct, hls, probe, progress, DirectOptions, HlsOptions, RequestOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

type Files = Arc<HashMap<String, Vec<u8>>>;

/// Serves `files`. Paths starting with /norange/ ignore Range headers,
/// like many simple servers do.
async fn serve(files: HashMap<String, Vec<u8>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let files: Files = Arc::new(files);
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            tokio::spawn(handle(sock, files.clone()));
        }
    });
    addr
}

async fn handle(mut sock: TcpStream, files: Files) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let req = String::from_utf8_lossy(&buf).to_string();
    let mut lines = req.lines();
    let path = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("/");
    let path = path.split('?').next().unwrap().to_string();
    let range = lines
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("range")
                .then(|| v.trim().to_string())
        })
        .filter(|_| !path.starts_with("/norange/"));

    let Some(body) = files.get(&path) else {
        let _ = sock
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    let ctype = if path.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else {
        "application/octet-stream"
    };

    let (status, extra, slice) = match range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
        Some(spec) => {
            let (a, b) = spec.split_once('-').unwrap();
            let start: usize = a.parse().unwrap();
            let end: usize = if b.is_empty() {
                body.len() - 1
            } else {
                b.parse::<usize>().unwrap().min(body.len() - 1)
            };
            (
                "206 Partial Content",
                format!(
                    "Content-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\n",
                    body.len()
                ),
                &body[start..=end],
            )
        }
        None => ("200 OK", String::new(), &body[..]),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
        slice.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(slice).await;
    let _ = sock.shutdown().await;
}

/// Deterministic filler data.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[tokio::test]
async fn multi_connection_download_matches_source() {
    let data = noise(5 * 1024 * 1024 + 123, 1);
    let addr = serve(HashMap::from([(
        "/files/big.bin".to_string(),
        data.clone(),
    )]))
    .await;
    let client = build_client(&RequestOptions::default()).unwrap();

    let info = probe(&client, &format!("http://{addr}/files/big.bin"))
        .await
        .unwrap();
    assert!(info.accepts_ranges);
    assert_eq!(info.size, Some(data.len() as u64));
    assert_eq!(info.filename.as_deref(), Some("big.bin"));

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("big.bin");
    let opts = DirectOptions {
        connections: 6,
        ..Default::default()
    };
    direct::download(&client, &info, &dest, &opts, progress::silent())
        .await
        .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!dir.path().join("big.bin.part").exists());
    assert!(!dir.path().join("big.bin.osprey.json").exists());
}

#[tokio::test]
async fn falls_back_to_single_connection_without_ranges() {
    let data = noise(3 * 1024 * 1024, 2);
    let addr = serve(HashMap::from([(
        "/norange/clip.mp4".to_string(),
        data.clone(),
    )]))
    .await;
    let client = build_client(&RequestOptions::default()).unwrap();

    let info = probe(&client, &format!("http://{addr}/norange/clip.mp4"))
        .await
        .unwrap();
    assert!(!info.accepts_ranges);

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("clip.mp4");
    direct::download(
        &client,
        &info,
        &dest,
        &DirectOptions::default(),
        progress::silent(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn resumes_a_partial_multi_connection_download() {
    let data = noise(4 * 1024 * 1024, 3);
    let addr = serve(HashMap::from([("/files/r.bin".to_string(), data.clone())])).await;
    let url = format!("http://{addr}/files/r.bin");
    let client = build_client(&RequestOptions::default()).unwrap();
    let info = probe(&client, &url).await.unwrap();

    // Fake an interrupted run: the first half of each of two parts is on
    // disk and the state file says so. The rest of the .part file is junk
    // that must be overwritten.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("r.bin");
    let size = data.len() as u64;
    let half = size / 2;
    let mut part = vec![0xAAu8; data.len()];
    part[..1000].copy_from_slice(&data[..1000]);
    part[half as usize..half as usize + 500]
        .copy_from_slice(&data[half as usize..half as usize + 500]);
    std::fs::write(dir.path().join("r.bin.part"), &part).unwrap();
    let state = serde_json::json!({
        "url": info.final_url,
        "size": size,
        "parts": [
            {"start": 0, "end": half - 1, "done": 1000},
            {"start": half, "end": size - 1, "done": 500}
        ]
    });
    std::fs::write(dir.path().join("r.bin.osprey.json"), state.to_string()).unwrap();

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let progress: osprey_core::ProgressFn = Arc::new(move |e| {
        if let osprey_core::Progress::Advance(n) = e {
            seen2.lock().unwrap().push(n);
        }
    });
    direct::download(&client, &info, &dest, &DirectOptions::default(), progress)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), data);
    // The first Advance reports what was already on disk.
    assert_eq!(seen.lock().unwrap()[0], 1500);
}

fn encrypt(plain: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plain)
}

fn seq_iv(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

#[tokio::test]
async fn hls_master_with_encrypted_video_and_separate_audio() {
    let key = *b"0123456789abcdef";
    let mut files = HashMap::new();
    files.insert("/keys/k1".to_string(), key.to_vec());

    files.insert(
        "/show/master.m3u8".to_string(),
        br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",NAME="English",DEFAULT=YES,URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=640x360,AUDIO="a"
low/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,AUDIO="a"
high/index.m3u8
"#
        .to_vec(),
    );

    // Video: 12 segments starting at media sequence 50. The first 4 are
    // clear, the rest use AES-128 with the IV taken from the sequence
    // number (no IV attribute), the last 3 with an explicit IV.
    let explicit_iv = [9u8; 16];
    let mut video_pl = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:50\n");
    let mut expected_video = Vec::new();
    for i in 0..12u64 {
        let seq = 50 + i;
        let plain = noise(10_000 + i as usize * 777, 100 + i);
        expected_video.extend_from_slice(&plain);
        let body = match i {
            0..=3 => plain,
            4..=8 => encrypt(&plain, &key, &seq_iv(seq)),
            _ => encrypt(&plain, &key, &explicit_iv),
        };
        if i == 4 {
            video_pl.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"/keys/k1\"\n");
        }
        if i == 9 {
            video_pl.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"/keys/k1\",IV=0x09090909090909090909090909090909\n");
        }
        video_pl.push_str(&format!("#EXTINF:4.0,\nseg{i}.ts?token=abc\n"));
        files.insert(format!("/show/high/seg{i}.ts"), body);
    }
    video_pl.push_str("#EXT-X-ENDLIST\n");
    files.insert("/show/high/index.m3u8".to_string(), video_pl.into_bytes());
    files.insert(
        "/show/low/index.m3u8".to_string(),
        b"#EXTM3U\n#EXTINF:4,\nnope.ts\n#EXT-X-ENDLIST\n".to_vec(),
    );

    // Audio: one file addressed with byte ranges, like many fMP4 sites.
    let audio_blob = noise(30_000, 7);
    let audio_pl = "#EXTM3U\n#EXT-X-MAP:URI=\"a.mp4\",BYTERANGE=\"1000@0\"\n\
        #EXTINF:4,\n#EXT-X-BYTERANGE:12000@1000\na.mp4\n\
        #EXTINF:4,\n#EXT-X-BYTERANGE:17000\na.mp4\n#EXT-X-ENDLIST\n";
    files.insert(
        "/show/audio/en.m3u8".to_string(),
        audio_pl.as_bytes().to_vec(),
    );
    files.insert("/show/audio/a.mp4".to_string(), audio_blob.clone());

    let addr = serve(files).await;
    let client = build_client(&RequestOptions::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();

    let out = hls::download(
        &client,
        &format!("http://{addr}/show/master.m3u8"),
        dir.path(),
        &HlsOptions {
            concurrency: 5,
            ..Default::default()
        },
        progress::silent(),
    )
    .await
    .unwrap();

    assert_eq!(out.video.extension().unwrap(), "ts");
    assert_eq!(std::fs::read(&out.video).unwrap(), expected_video);

    let audio = out.audio.expect("separate audio downloaded");
    assert_eq!(audio.extension().unwrap(), "mp4");
    assert_eq!(std::fs::read(&audio).unwrap(), audio_blob);
}

/// A short real clip made by ffmpeg. None when ffmpeg is not installed.
async fn clip(dir: &std::path::Path, name: &str, source: &[&str]) -> Option<Vec<u8>> {
    let path = dir.join(name);
    let made = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .args(source)
        .arg(&path)
        .status()
        .await;
    made.ok()?.success().then_some(())?;
    tokio::fs::read(&path).await.ok()
}

#[tokio::test]
async fn separate_video_and_audio_files_become_one() {
    let dir = tempfile::tempdir().unwrap();
    let video = clip(
        dir.path(),
        "v.mp4",
        &["testsrc=d=1:s=64x64", "-pix_fmt", "yuv420p"],
    )
    .await;
    let audio = clip(dir.path(), "a.m4a", &["sine=d=1"]).await;
    let (Some(video), Some(audio)) = (video, audio) else {
        eprintln!("skipped, ffmpeg is not installed");
        return;
    };
    let mut files = HashMap::new();
    files.insert("/v.mp4".to_string(), video);
    files.insert("/a.m4a".to_string(), audio);
    let addr = serve(files).await;

    let out = dir.path().join("joined.mp4");
    let opts = osprey_core::FetchOptions {
        output: Some(out.clone()),
        audio: Some(osprey_core::SeparateAudio {
            url: format!("http://{addr}/a.m4a"),
            kind: osprey_core::MediaKind::Direct,
            ext: "mp4".into(),
        }),
        ..Default::default()
    };
    let saved = osprey_core::fetch(&format!("http://{addr}/v.mp4"), &opts, progress::silent())
        .await
        .unwrap();
    assert_eq!(saved, out);

    let streams = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(&out)
        .output()
        .await
        .unwrap();
    let streams = String::from_utf8_lossy(&streams.stdout);
    assert!(
        streams.contains("video") && streams.contains("audio"),
        "{streams}"
    );
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.starts_with(".osprey-"))
        .collect();
    assert!(left.is_empty(), "work folder removed: {left:?}");
}

#[tokio::test]
async fn drm_streams_are_refused() {
    let pl = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
    let addr = serve(HashMap::from([(
        "/drm.m3u8".to_string(),
        pl.as_bytes().to_vec(),
    )]))
    .await;
    let client = build_client(&RequestOptions::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = hls::download(
        &client,
        &format!("http://{addr}/drm.m3u8"),
        dir.path(),
        &HlsOptions::default(),
        progress::silent(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, osprey_core::Error::Drm(_)), "{err}");
}

#[tokio::test]
async fn playlist_at_an_address_without_extension_is_a_stream() {
    // Many players serve the playlist from /source or /api/play with a
    // generic type. It must be downloaded as a stream, not saved as text.
    let pl = b"#EXTM3U\n#EXTINF:4,\nseg0.ts\n#EXT-X-ENDLIST\n".to_vec();
    let addr = serve(HashMap::from([("/api/source".to_string(), pl)])).await;
    let client = build_client(&RequestOptions::default()).unwrap();
    let info = probe(&client, &format!("http://{addr}/api/source"))
        .await
        .unwrap();
    assert_eq!(info.kind, osprey_core::MediaKind::Hls);
}

/// A stream of `n` pieces where the pieces in `gone` return 404.
fn stream_with_holes(n: usize, gone: &[usize]) -> HashMap<String, Vec<u8>> {
    let mut files = HashMap::new();
    let mut pl = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
    for i in 0..n {
        pl.push_str(&format!("#EXTINF:4,\ns{i}.ts\n"));
        if !gone.contains(&i) {
            files.insert(format!("/s{i}.ts"), vec![i as u8; 1000]);
        }
    }
    pl.push_str("#EXT-X-ENDLIST\n");
    files.insert("/v.m3u8".into(), pl.into_bytes());
    files
}

#[tokio::test]
async fn a_few_missing_pieces_still_give_a_video() {
    // 1DM's "merge with missing parts": one dead piece out of 60 should
    // leave a short gap, not throw away the other 59.
    let addr = serve(stream_with_holes(60, &[17])).await;
    let client = build_client(&RequestOptions::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let notes = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = notes.clone();
    let out = hls::download(
        &client,
        &format!("http://{addr}/v.m3u8"),
        dir.path(),
        &HlsOptions {
            retries: 0,
            ..HlsOptions::default()
        },
        Arc::new(move |p| {
            if let progress::Progress::Status(s) = p {
                seen.lock().unwrap().push(s);
            }
        }),
    )
    .await
    .unwrap();
    let joined = std::fs::read(&out.video).unwrap();
    assert_eq!(joined.len(), 59 * 1000);
    assert!(!joined.contains(&17u8));
    assert!(
        notes.lock().unwrap().iter().any(|s| s.contains("1 of 60")),
        "{:?}",
        notes.lock().unwrap()
    );
}

#[tokio::test]
async fn too_many_missing_pieces_is_an_error() {
    let addr = serve(stream_with_holes(20, &[3, 9])).await;
    let client = build_client(&RequestOptions::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let res = hls::download(
        &client,
        &format!("http://{addr}/v.m3u8"),
        dir.path(),
        &HlsOptions {
            retries: 0,
            ..HlsOptions::default()
        },
        progress::silent(),
    )
    .await;
    assert!(res.is_err());
}

#[tokio::test]
async fn speed_limit_holds_for_streams() {
    // 10 pieces of 30 KB at 100 KB/s, four at a time: about 3 s, and
    // never under 2.6 s (the limiter allows a quarter second burst).
    let mut files = HashMap::new();
    let mut pl = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
    for i in 0..10 {
        pl.push_str(&format!("#EXTINF:4,\nq{i}.ts\n"));
        files.insert(format!("/q{i}.ts"), vec![7u8; 30_000]);
    }
    pl.push_str("#EXT-X-ENDLIST\n");
    files.insert("/q.m3u8".into(), pl.into_bytes());
    let addr = serve(files).await;
    let client = build_client(&RequestOptions::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let start = std::time::Instant::now();
    hls::download(
        &client,
        &format!("http://{addr}/q.m3u8"),
        dir.path(),
        &HlsOptions {
            concurrency: 4,
            limit: Some(Arc::new(osprey_core::limit::SpeedLimit::new(100_000))),
            ..HlsOptions::default()
        },
        progress::silent(),
    )
    .await
    .unwrap();
    let took = start.elapsed();
    assert!(took >= std::time::Duration::from_millis(2600), "{took:?}");
    assert!(took < std::time::Duration::from_secs(6), "{took:?}");
}

#[tokio::test]
async fn bypass_vip_is_asked_only_when_the_local_rules_are_stuck() {
    use osprey_core::bypass::{resolve, Fallback, ResolvedBy};
    let answer = br#"{"status":"success","result":"https://dest.test/found?utm_source=x"}"#;
    let shut = br#"{"status":"success","result":"FREE API SHUT DOWN"}"#;
    let addr = serve(HashMap::from([
        ("/plain".to_string(), b"<html>a plain page</html>".to_vec()),
        ("/vip".to_string(), answer.to_vec()),
        ("/shut".to_string(), shut.to_vec()),
    ]))
    .await;
    let opts = osprey_core::RequestOptions::default();
    let plain = format!("http://{addr}/plain");
    let vip = format!("http://{addr}/vip");
    let shut = format!("http://{addr}/shut");
    let with_key = |endpoint| Fallback {
        endpoint,
        key: "test-key-1",
    };

    // Off: nothing leaves, and the result says it stopped here.
    let r = resolve(&opts, &plain, 5, None).await.unwrap();
    assert_eq!(r.resolved_by, ResolvedBy::LocalRule);
    assert_eq!(r.link.final_url, plain);

    // On and stuck: bypass.vip answers, and tracking is still removed.
    let r = resolve(&opts, &plain, 5, Some(with_key(&vip)))
        .await
        .unwrap();
    assert_eq!(r.resolved_by, ResolvedBy::BypassVip);
    assert_eq!(r.link.final_url, "https://dest.test/found");

    // On, but a local rule knew the link: bypass.vip is not used.
    let wrapped = format!("http://{addr}/out#{plain}");
    let r = resolve(&opts, &wrapped, 5, Some(with_key(&vip)))
        .await
        .unwrap();
    assert_eq!(r.resolved_by, ResolvedBy::LocalRule);
    assert_eq!(r.link.final_url, plain);

    // A service that says success with no link does not become the result.
    let r = resolve(&opts, &plain, 5, Some(with_key(&shut)))
        .await
        .unwrap();
    assert_eq!(r.resolved_by, ResolvedBy::LocalRule);
    assert_eq!(r.link.final_url, plain);
    assert!(r.link.note.unwrap().contains("FREE API SHUT DOWN"));
}

#[tokio::test]
async fn the_bypass_vip_key_goes_in_its_header_and_the_link_in_the_query() {
    use osprey_core::bypass::{ask_bypass_vip, Fallback};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = sock.read(&mut buf).await.unwrap();
        let body = br#"{"status":"success","result":"https://dest.test/a"}"#;
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(body).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).to_lowercase()
    });
    let endpoint = format!("http://{addr}/premium/bypass");
    let fallback = Fallback {
        endpoint: &endpoint,
        key: "test-key-1",
    };
    let opts = osprey_core::RequestOptions::default();
    let found = ask_bypass_vip(&opts, fallback, "https://short.test/a?b=1")
        .await
        .unwrap();
    assert_eq!(found.as_str(), "https://dest.test/a");
    let request = server.await.unwrap();
    assert!(request.starts_with("get /premium/bypass?url=https%3a%2f%2fshort.test%2fa%3fb%3d1 "));
    assert!(request.contains("x-api-key: test-key-1\r\n"));
}

#[tokio::test]
async fn scripts_update_from_their_own_address_and_keep_require_files() {
    use osprey_core::userscript::Store;
    use std::sync::{Arc, Mutex};

    // A server whose files can change between requests.
    let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shared = files.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let snapshot = Arc::new(shared.lock().unwrap().clone());
            tokio::spawn(handle(sock, snapshot));
        }
    });
    let script = |version: &str| {
        format!(
            "// ==UserScript==\n// @name U\n// @version {version}\n\
             // @updateURL http://{addr}/u.user.js\n// @require http://{addr}/lib.js\n\
             // ==/UserScript==\nrun({version})\n"
        )
        .into_bytes()
    };
    let put = |name: &str, bytes: Vec<u8>| {
        files.lock().unwrap().insert(name.to_string(), bytes);
    };
    put("/u.user.js", script("1.0"));
    put("/lib.js", b"lib_one()".to_vec());

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let client = reqwest::Client::new();
    let text = String::from_utf8(script("1.0")).unwrap();
    let id = store.install(&client, &text, None).await.unwrap().id;

    // Nothing new yet.
    assert!(store.find_update(&client, &id).await.unwrap().is_none());

    // The same version does not fetch its library again, even if it changed.
    put("/lib.js", b"lib_two()".to_vec());
    store.install(&client, &text, None).await.unwrap();
    assert_eq!(store.load(&id).unwrap().requires, ["lib_one()"]);

    // A new version arrives from the script's own address, and only now
    // does the library come again.
    put("/u.user.js", script("1.1"));
    let new = store.find_update(&client, &id).await.unwrap().unwrap();
    assert!(new.contains("run(1.1)"));
    store.install(&client, &new, None).await.unwrap();
    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.installed.meta.version, "1.1");
    assert_eq!(loaded.requires, ["lib_two()"]);

    // A saved library that was changed on disk does not run.
    let folder = dir.path().join("scripts").join(&id).join("requires");
    let saved = std::fs::read_dir(folder).unwrap().next().unwrap().unwrap();
    std::fs::write(saved.path(), "changed()").unwrap();
    assert!(store.load(&id).is_err());
}
