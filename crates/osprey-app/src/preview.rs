//! Preview, like 1DM's preview dialog: play a found video in the Media
//! panel before (or while) downloading it.
//!
//! Two things stand in the way on a desktop, and this module handles both:
//!
//! - Windows' web engine (WebView2, Chromium based) cannot play HLS by
//!   itself. The panel uses hls.js, the standard open source HLS player,
//!   which works on every platform.
//! - Video servers expect the page's cookies, referrer and user agent,
//!   and a player in BM Osprey's own panel has none of them. So the player
//!   is pointed at a small local server that fetches everything with the
//!   tab's identity, through the tab's route, and rewrites playlists so
//!   every piece goes through it too.
//!
//! The server only listens on 127.0.0.1 and only serves sessions it
//! created, each behind a random token.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

const MAX_SESSIONS: usize = 16;
const MAX_PLAYLIST: usize = 8 * 1024 * 1024;
/// Preview pictures on Media panel cards.
const MAX_IMAGES: usize = 64;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// One preview picture: the only address its token may fetch.
struct Image {
    client: reqwest::Client,
    url: String,
    created: Instant,
}

struct Session {
    client: reqwest::Client,
    created: Instant,
    /// A playlist made here that joins a video and its separate audio.
    joined: Option<String>,
}

pub struct Preview {
    addr: SocketAddr,
    sessions: Mutex<HashMap<String, Session>>,
    images: Mutex<HashMap<String, Image>>,
    counter: AtomicU64,
}

impl Preview {
    pub async fn start() -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let me = Arc::new(Self {
            addr: listener.local_addr()?,
            sessions: Mutex::new(HashMap::new()),
            images: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        });
        let server = me.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, peer)) = listener.accept().await else {
                    continue;
                };
                if !peer.ip().is_loopback() {
                    continue;
                }
                let server = server.clone();
                tokio::spawn(async move {
                    let _ = server.handle(sock).await;
                });
            }
        });
        Ok(me)
    }

    fn token(&self) -> String {
        // Two hashes from the standard library's randomly seeded hasher.
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut out = String::new();
        for salt in [0x5bd1_e995u64, 0x9e37_79b9] {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(n ^ salt);
            h.write_u128(Instant::now().elapsed().as_nanos() ^ u128::from(std::process::id()));
            out.push_str(&format!("{:016x}", h.finish()));
        }
        out
    }

    /// Makes `client` (which carries the tab's identity) usable for a
    /// preview and returns the address the player should load `target` from.
    pub fn open(&self, client: reqwest::Client, target: &str) -> String {
        let token = self.add_session(client, None);
        proxied(self.addr, &token, target)
    }

    /// The same for a video and audio kept in separate streams. The player
    /// gets one playlist naming both, so it plays them together.
    pub fn open_joined(&self, client: reqwest::Client, video: &str, audio: &str) -> String {
        let token = self.token();
        let text = joined_playlist(
            &proxied(self.addr, &token, video),
            &proxied(self.addr, &token, audio),
        );
        self.insert_session(token.clone(), client, Some(text));
        format!("http://{}/j/{token}", self.addr)
    }

    /// A local address for one picture, fetched with the tab's identity.
    /// Pictures have their own small store, so they never push out a
    /// running preview. The answer must really be an image.
    pub fn open_image(&self, client: reqwest::Client, target: &str) -> String {
        let token = self.token();
        let mut images = self.images.lock().expect("images");
        if images.len() >= MAX_IMAGES {
            if let Some(oldest) = images
                .iter()
                .min_by_key(|(_, i)| i.created)
                .map(|(k, _)| k.clone())
            {
                images.remove(&oldest);
            }
        }
        images.insert(
            token.clone(),
            Image {
                client,
                url: target.to_string(),
                created: Instant::now(),
            },
        );
        format!("http://{}/i/{token}", self.addr)
    }

    async fn serve_image(&self, sock: &mut TcpStream, token: &str) -> std::io::Result<()> {
        let found = self
            .images
            .lock()
            .expect("images")
            .get(token)
            .map(|i| (i.client.clone(), i.url.clone()));
        let Some((client, url)) = found else {
            return reply(sock, "403 Forbidden", "text/plain", b"unknown picture", &[]).await;
        };
        match fetch_image(&client, &url).await {
            Some((ct, body)) => reply(sock, "200 OK", &ct, &body, &[]).await,
            None => reply(sock, "502 Bad Gateway", "text/plain", b"no picture", &[]).await,
        }
    }

    fn add_session(&self, client: reqwest::Client, joined: Option<String>) -> String {
        let token = self.token();
        self.insert_session(token.clone(), client, joined);
        token
    }

    fn insert_session(&self, token: String, client: reqwest::Client, joined: Option<String>) {
        let mut sessions = self.sessions.lock().expect("sessions");
        sessions.retain(|_, s| s.created.elapsed() < Duration::from_secs(3 * 3600));
        if sessions.len() >= MAX_SESSIONS {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.created)
                .map(|(k, _)| k.clone())
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            token,
            Session {
                client,
                created: Instant::now(),
                joined,
            },
        );
    }

    async fn handle(&self, mut sock: TcpStream) -> std::io::Result<()> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::time::timeout(Duration::from_secs(20), sock.read(&mut chunk))
                .await
                .map_err(|_| std::io::Error::other("slow client"))??;
            if n == 0 || buf.len() > 32 * 1024 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let mut lines = head.lines();
        let request = lines.next().unwrap_or_default().to_string();
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let range = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("range"))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| v.len() < 100 && v.starts_with("bytes="));

        if method == "OPTIONS" {
            return reply(&mut sock, "204 No Content", "text/plain", b"", &[]).await;
        }
        if method != "GET" && method != "HEAD" {
            return reply(&mut sock, "405 Method Not Allowed", "text/plain", b"", &[]).await;
        }

        // /p/<token>?u=<target>
        let Ok(local) = Url::parse(&format!("http://127.0.0.1{path}")) else {
            return reply(
                &mut sock,
                "400 Bad Request",
                "text/plain",
                b"bad request",
                &[],
            )
            .await;
        };
        if let Some(token) = local.path().strip_prefix("/i/") {
            return self.serve_image(&mut sock, token).await;
        }
        if let Some(token) = local.path().strip_prefix("/j/") {
            let text = self
                .sessions
                .lock()
                .expect("sessions")
                .get(token)
                .and_then(|s| s.joined.clone());
            return match text {
                Some(text) => {
                    reply(
                        &mut sock,
                        "200 OK",
                        "application/vnd.apple.mpegurl",
                        text.as_bytes(),
                        &[],
                    )
                    .await
                }
                None => {
                    reply(
                        &mut sock,
                        "403 Forbidden",
                        "text/plain",
                        b"unknown preview",
                        &[],
                    )
                    .await
                }
            };
        }
        let token = local
            .path()
            .strip_prefix("/p/")
            .unwrap_or_default()
            .to_string();
        let target = local
            .query_pairs()
            .find(|(k, _)| k == "u")
            .map(|(_, v)| v.into_owned());
        let client = self
            .sessions
            .lock()
            .expect("sessions")
            .get(&token)
            .map(|s| s.client.clone());
        let (Some(client), Some(target)) = (client, target) else {
            return reply(
                &mut sock,
                "403 Forbidden",
                "text/plain",
                b"unknown preview",
                &[],
            )
            .await;
        };
        let Some(target) = Url::parse(&target)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        else {
            return reply(
                &mut sock,
                "400 Bad Request",
                "text/plain",
                b"bad target",
                &[],
            )
            .await;
        };

        let mut req = client.get(target.clone());
        if let Some(r) = &range {
            req = req.header(reqwest::header::RANGE, r);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("could not reach the video server: {e}");
                return reply(
                    &mut sock,
                    "502 Bad Gateway",
                    "text/plain",
                    msg.as_bytes(),
                    &[],
                )
                .await;
            }
        };
        let status = resp.status();
        let final_url = resp.url().clone();
        let header = |name: reqwest::header::HeaderName| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let ct = header(reqwest::header::CONTENT_TYPE)
            .unwrap_or_else(|| "application/octet-stream".into());
        let content_range = header(reqwest::header::CONTENT_RANGE);
        let length = resp.content_length();
        let status_line = format!(
            "{} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        );

        let mut stream = resp.bytes_stream();
        let first = match stream.next().await {
            Some(Ok(b)) => b.to_vec(),
            _ => Vec::new(),
        };
        let looks_like_playlist = String::from_utf8_lossy(&first[..first.len().min(64)])
            .trim_start_matches('\u{feff}')
            .trim_start()
            .starts_with("#EXTM3U");

        if looks_like_playlist {
            let mut body = first;
            while let Some(Ok(more)) = stream.next().await {
                body.extend_from_slice(&more);
                if body.len() > MAX_PLAYLIST {
                    break;
                }
            }
            let text = rewrite_playlist(
                &String::from_utf8_lossy(&body),
                &final_url,
                self.addr,
                &token,
            );
            return reply(
                &mut sock,
                &status_line,
                "application/vnd.apple.mpegurl",
                text.as_bytes(),
                &[],
            )
            .await;
        }

        // Anything else streams straight through, with range support.
        let mut extra: Vec<(&str, String)> = vec![("Accept-Ranges", "bytes".into())];
        if let Some(cr) = content_range {
            extra.push(("Content-Range", cr));
        }
        let mut head = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: {ct}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: Content-Range, Content-Length\r\nCache-Control: no-store\r\nConnection: close\r\n"
        );
        if let Some(l) = length {
            head.push_str(&format!("Content-Length: {l}\r\n"));
        }
        for (k, v) in &extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        sock.write_all(head.as_bytes()).await?;
        if method == "HEAD" {
            return Ok(());
        }
        sock.write_all(&first).await?;
        while let Some(Ok(chunk)) = stream.next().await {
            sock.write_all(&chunk).await?;
        }
        let _ = sock.shutdown().await;
        Ok(())
    }
}

/// Fetches a picture. Only raster images up to 4 MB pass. SVG is left out,
/// since it can carry script.
async fn fetch_image(client: &reqwest::Client, url: &str) -> Option<(String, Vec<u8>)> {
    let resp = tokio::time::timeout(Duration::from_secs(20), client.get(url).send())
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })?;
    if !ct.starts_with("image/") || ct.contains("svg") {
        return None;
    }
    if resp
        .content_length()
        .is_some_and(|l| l as usize > MAX_IMAGE_BYTES)
    {
        return None;
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.ok()?);
        if body.len() > MAX_IMAGE_BYTES {
            return None;
        }
    }
    Some((ct, body))
}

/// A master playlist with one video and one audio track. The bandwidth is
/// only a placeholder, since there is nothing to choose between.
fn joined_playlist(video: &str, audio: &str) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:6\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"{audio}\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO=\"audio\"\n{video}\n"
    )
}

fn proxied(addr: SocketAddr, token: &str, target: &str) -> String {
    let enc: String = url::form_urlencoded::byte_serialize(target.as_bytes()).collect();
    format!("http://{addr}/p/{token}?u={enc}")
}

/// Points every address in a playlist (pieces, keys, variants, audio
/// tracks, init sections) at the preview server.
pub(crate) fn rewrite_playlist(text: &str, base: &Url, addr: SocketAddr, token: &str) -> String {
    let abs = |u: &str| {
        base.join(u.trim())
            .map(|x| x.to_string())
            .unwrap_or_else(|_| u.to_string())
    };
    let mut out = String::with_capacity(text.len() * 2);
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            out.push('\n');
            continue;
        }
        if t.starts_with('#') {
            // Tags like EXT-X-KEY, EXT-X-MAP and EXT-X-MEDIA carry URI="...".
            let mut rest = t;
            let mut rebuilt = String::new();
            while let Some(i) = rest.find("URI=\"") {
                let (before, after) = rest.split_at(i + 5);
                rebuilt.push_str(before);
                let end = after.find('"').unwrap_or(after.len());
                rebuilt.push_str(&proxied(addr, token, &abs(&after[..end])));
                rest = &after[end..];
            }
            rebuilt.push_str(rest);
            out.push_str(&rebuilt);
        } else {
            out.push_str(&proxied(addr, token, &abs(t)));
        }
        out.push('\n');
    }
    out
}

async fn reply(
    sock: &mut TcpStream,
    status: &str,
    ct: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Range\r\nAccess-Control-Allow-Methods: GET, HEAD, OPTIONS\r\nCache-Control: no-store\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    let _ = sock.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlists_are_rewritten_through_the_preview_server() {
        let addr: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let base = Url::parse("https://cdn.test/show/ep1/index.m3u8").unwrap();
        let pl = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"../keys/k1\",IV=0x01\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6,\nseg0.ts?sig=a&b=c\n";
        let out = rewrite_playlist(pl, &base, addr, "tok");
        assert!(out.contains("URI=\"http://127.0.0.1:5555/p/tok?u=https%3A%2F%2Fcdn.test%2Fshow%2Fkeys%2Fk1\",IV=0x01"), "{out}");
        assert!(out.contains("URI=\"http://127.0.0.1:5555/p/tok?u=https%3A%2F%2Fcdn.test%2Fshow%2Fep1%2Finit.mp4\""), "{out}");
        assert!(out.contains("\nhttp://127.0.0.1:5555/p/tok?u=https%3A%2F%2Fcdn.test%2Fshow%2Fep1%2Fseg0.ts%3Fsig%3Da%26b%3Dc\n"), "{out}");
        assert!(out.starts_with("#EXTM3U\n"));
    }

    #[tokio::test]
    async fn unknown_tokens_are_refused() {
        let p = Preview::start().await.unwrap();
        let url = format!("http://{}/p/nope?u=https%3A%2F%2Fx.test%2F", p.addr);
        let r = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        assert_ne!(p.token(), p.token());
    }

    /// A one-shot server that answers every request with `ct` and `body`.
    async fn serve(ct: &'static str, body: &'static [u8]) -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let _ = reply(&mut s, "200 OK", ct, body, &[]).await;
            }
        });
        format!("http://{addr}/x")
    }

    #[tokio::test]
    async fn pictures_pass_and_other_answers_do_not() {
        let p = Preview::start().await.unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let png = serve("image/png", b"PNG fake").await;
        let html = serve("text/html", b"<script>x</script>").await;
        let svg = serve("image/svg+xml", b"<svg/>").await;

        let r = client
            .get(p.open_image(client.clone(), &png))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.headers()["content-type"], "image/png");
        assert_eq!(&r.bytes().await.unwrap()[..], b"PNG fake");

        for bad in [html, svg] {
            let r = client
                .get(p.open_image(client.clone(), &bad))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 502, "{bad}");
        }
        let unknown = format!("http://{}/i/nope", p.addr);
        assert_eq!(
            client.get(unknown).send().await.unwrap().status().as_u16(),
            403
        );
    }
}
