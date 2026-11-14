//! Leak tests. A fake SOCKS5 proxy records exactly what it is asked for.
//! The hosts used end in `.invalid`, a name that can never be resolved,
//! so a request only succeeds if the proxy (Tor in real life) did the
//! lookup. A local DNS lookup would fail the test.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use osprey_core::privacy::{Privacy, Route};
use osprey_core::{build_client, bypass, probe, RequestOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A web server that answers every request with the request headers it
/// received, so tests can see exactly what was sent. Counts hits.
async fn echo_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            hits2.fetch_add(1, Ordering::SeqCst);
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
                let body = String::from_utf8_lossy(&buf).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (addr, hits)
}

#[derive(Debug, Clone, PartialEq)]
enum Asked {
    Name(String, u16),
    Ip(String),
}

/// Minimal SOCKS5 proxy. Records every CONNECT, then relays everything
/// to `target` whatever host was asked for.
async fn socks_proxy(target: SocketAddr) -> (SocketAddr, Arc<Mutex<Vec<Asked>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let (mut c, _) = listener.accept().await.unwrap();
            let log = log2.clone();
            tokio::spawn(async move {
                let mut head = [0u8; 2];
                c.read_exact(&mut head).await.unwrap();
                let mut methods = vec![0u8; head[1] as usize];
                c.read_exact(&mut methods).await.unwrap();
                c.write_all(&[5, 0]).await.unwrap();

                let mut req = [0u8; 4];
                c.read_exact(&mut req).await.unwrap();
                let asked = match req[3] {
                    3 => {
                        let mut len = [0u8; 1];
                        c.read_exact(&mut len).await.unwrap();
                        let mut name = vec![0u8; len[0] as usize];
                        c.read_exact(&mut name).await.unwrap();
                        let mut port = [0u8; 2];
                        c.read_exact(&mut port).await.unwrap();
                        Asked::Name(String::from_utf8(name).unwrap(), u16::from_be_bytes(port))
                    }
                    1 => {
                        let mut ip = [0u8; 6];
                        c.read_exact(&mut ip).await.unwrap();
                        Asked::Ip(format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]))
                    }
                    _ => {
                        let mut ip = [0u8; 18];
                        c.read_exact(&mut ip).await.unwrap();
                        Asked::Ip("ipv6".into())
                    }
                };
                log.lock().unwrap().push(asked);
                c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                let mut upstream = TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut c, &mut upstream).await;
            });
        }
    });
    (addr, log)
}

fn opts(privacy: Privacy) -> RequestOptions {
    RequestOptions {
        privacy,
        ..Default::default()
    }
}

#[tokio::test]
async fn proxy_gets_the_hostname_never_a_local_dns_answer() {
    let (web, _) = echo_server().await;
    let (proxy, asked) = socks_proxy(web).await;

    // Given as socks5://, which on its own would resolve names locally.
    // BM Osprey upgrades it to remote DNS.
    let mut p = Privacy {
        route: Route::Proxy(format!("socks5://{proxy}")),
        ..Default::default()
    };
    p.prepare().await.unwrap();
    let client = build_client(&opts(p)).unwrap();

    let body = client
        .get("http://video.cdn.invalid/clip.mp4")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.starts_with("GET /clip.mp4"), "{body}");
    assert_eq!(
        *asked.lock().unwrap(),
        vec![Asked::Name("video.cdn.invalid".into(), 80)]
    );
}

#[tokio::test]
async fn tor_mode_routes_everything_and_hides_the_app_identity() {
    let (web, _) = echo_server().await;
    let (fake_tor, asked) = socks_proxy(web).await;

    let mut p = Privacy {
        generic_identity: true,
        ..Privacy::anonymous()
    };
    p.route = Route::Tor(Some(fake_tor.to_string()));
    p.prepare().await.unwrap();
    let o = opts(p);

    // Plain request, a probe and the link bypasser all go the same way.
    let body = build_client(&o)
        .unwrap()
        .get("http://site.invalid/watch")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let _ = probe(&build_client(&o).unwrap(), "http://media.invalid/a.mp4")
        .await
        .unwrap();
    let _ = bypass(&o, "http://short.invalid/x", 3).await.unwrap();

    let lower = body.to_ascii_lowercase();
    assert!(
        lower.contains(
            "user-agent: mozilla/5.0 (windows nt 10.0; rv:140.0) gecko/20100101 firefox/140.0"
        ),
        "{body}"
    );
    assert!(lower.contains("accept-language: en-us,en;q=0.5"), "{body}");
    assert!(!lower.contains("bm-osprey"), "{body}");

    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 3);
    assert!(
        asked
            .iter()
            .all(|a| matches!(a, Asked::Name(n, _) if n.ends_with(".invalid"))),
        "{asked:?}"
    );
}

#[tokio::test]
async fn nothing_is_sent_when_the_proxy_is_down() {
    let (web, hits) = echo_server().await;
    // Find a free port, then close it so nothing listens there.
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let p = Privacy {
        route: Route::Proxy(format!("socks5://{dead}")),
        ..Default::default()
    };
    let client = build_client(&opts(p)).unwrap();
    let result = client.get(format!("http://{web}/secret")).send().await;
    assert!(
        result.is_err(),
        "request must fail, not fall back to direct"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the site was contacted directly"
    );
}
