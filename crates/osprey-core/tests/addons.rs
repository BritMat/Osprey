//! Tests for the add-ons (bypass, filters with real pages) against a
//! tiny local server that can send redirects.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use osprey_core::bypass::Step;
use osprey_core::{bypass, RequestOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
enum Reply {
    Redirect(u16, String),
    Html(String),
}

async fn serve(routes: HashMap<&'static str, Reply>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    serve_on(listener, routes)
}

fn serve_on(listener: TcpListener, routes: HashMap<&'static str, Reply>) -> SocketAddr {
    let addr = listener.local_addr().unwrap();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req = String::from_utf8_lossy(&buf);
                let path = req
                    .split(' ')
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap()
                    .to_string();
                let host = addr.to_string();
                let resp = match routes.get(path.as_str()) {
                    Some(Reply::Redirect(code, loc)) => {
                        let loc = loc.replace("{HOST}", &host);
                        format!("HTTP/1.1 {code} Moved\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    }
                    Some(Reply::Html(body)) => {
                        let body = body.replace("{HOST}", &host);
                        format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    }
                    None => {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into()
                    }
                };
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn follows_a_full_shortener_chain() {
    // short link -> 301 -> meta refresh page -> script jump -> ?url=
    // wrapper -> base64 wrapper -> final page with tracking parameters
    // Bind first so the port is known while building the links.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let final_url = format!("http://{host}/final?id=7&utm_source=short&fbclid=xyz");
    let b64 = base64_encode(&final_url);
    let wrapper = format!(
        "http://{{HOST}}/out?url={}",
        urlencode(&format!("http://{{HOST}}/go/{b64}"))
    );
    let routes = HashMap::from([
        ("/s/abc", Reply::Redirect(301, "/interstitial".into())),
        (
            "/interstitial",
            Reply::Html(r#"<html><head><meta content="5;url=/wait" http-equiv="refresh"></head><body>Please wait</body></html>"#.into()),
        ),
        (
            "/wait",
            Reply::Html(format!(r#"<p>Redirecting</p><script>location.replace("{wrapper}")</script>"#)),
        ),
        ("/final", Reply::Html("<p>Destination</p>".into())),
    ]);
    serve_on(listener, routes);

    let r = bypass(
        &RequestOptions::default(),
        &format!("http://{host}/s/abc"),
        12,
    )
    .await
    .unwrap();
    let steps: Vec<_> = r.hops.iter().map(|h| h.via.clone()).collect();
    assert_eq!(
        steps,
        vec![
            Step::Redirect { status: 301 },
            Step::MetaRefresh,
            Step::ScriptRedirect,
            Step::UrlParameter { name: "url".into() },
            Step::Base64,
            Step::TrackingRemoved,
        ],
        "{:#?}",
        r.hops
    );
    assert_eq!(r.final_url, format!("http://{host}/final?id=7"));
    assert!(r.note.is_none());
}

fn base64_encode(s: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let b = s.as_bytes();
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..=c.len() {
            out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn reports_captcha_walls() {
    let routes = HashMap::from([(
        "/gate",
        Reply::Html(r#"<html><body><div class="g-recaptcha" data-sitekey="x"></div>Prove you are human</body></html>"#.into()),
    )]);
    let addr = serve(routes).await;
    let r = bypass(
        &RequestOptions::default(),
        &format!("http://{addr}/gate"),
        5,
    )
    .await
    .unwrap();
    assert!(r.hops.is_empty());
    assert!(r.note.unwrap().contains("human check"));
}

#[tokio::test]
async fn stops_on_redirect_loops() {
    let routes = HashMap::from([
        ("/a", Reply::Redirect(302, "/b".into())),
        ("/b", Reply::Redirect(302, "/a".into())),
    ]);
    let addr = serve(routes).await;
    let r = bypass(&RequestOptions::default(), &format!("http://{addr}/a"), 10)
        .await
        .unwrap();
    assert_eq!(r.hops.len(), 1);
    assert_eq!(r.note.as_deref(), Some("redirect loop detected"));
    assert!(matches!(r.hops[0].via, Step::Redirect { status: 302 }));
}
