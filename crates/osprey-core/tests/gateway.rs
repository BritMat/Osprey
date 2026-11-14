//! Gateway tests: real sockets, a fake upstream SOCKS5 proxy (standing in
//! for Tor, WARP or a VPN), a fake HTTP proxy and a local web server.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use osprey_core::gateway::{Gateway, Upstream};
use osprey_core::privacy::DnsMode;
use osprey_core::Filters;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn web_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            h.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let body = "hello through the gateway";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    (addr, hits)
}

/// Upstream SOCKS5 that records the requested host and optional login,
/// then relays everything to `target`.
async fn upstream_socks(
    target: SocketAddr,
    need_login: Option<(&'static str, &'static str)>,
) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let (mut c, _) = l.accept().await.unwrap();
            let log = log2.clone();
            tokio::spawn(async move {
                let mut h = [0u8; 2];
                c.read_exact(&mut h).await.unwrap();
                let mut m = vec![0u8; h[1] as usize];
                c.read_exact(&mut m).await.unwrap();
                if let Some((u, p)) = need_login {
                    assert!(m.contains(&2), "client must offer user/pass login");
                    c.write_all(&[5, 2]).await.unwrap();
                    let mut v = [0u8; 2];
                    c.read_exact(&mut v).await.unwrap();
                    let mut user = vec![0u8; v[1] as usize];
                    c.read_exact(&mut user).await.unwrap();
                    let mut pl = [0u8; 1];
                    c.read_exact(&mut pl).await.unwrap();
                    let mut pass = vec![0u8; pl[0] as usize];
                    c.read_exact(&mut pass).await.unwrap();
                    let ok = user == u.as_bytes() && pass == p.as_bytes();
                    c.write_all(&[1, if ok { 0 } else { 1 }]).await.unwrap();
                    log.lock()
                        .unwrap()
                        .push(format!("login {}", if ok { "ok" } else { "bad" }));
                    if !ok {
                        return;
                    }
                } else {
                    c.write_all(&[5, 0]).await.unwrap();
                }
                let mut r = [0u8; 4];
                c.read_exact(&mut r).await.unwrap();
                let host = match r[3] {
                    3 => {
                        let mut len = [0u8; 1];
                        c.read_exact(&mut len).await.unwrap();
                        let mut n = vec![0u8; len[0] as usize];
                        c.read_exact(&mut n).await.unwrap();
                        format!("name {}", String::from_utf8(n).unwrap())
                    }
                    _ => {
                        let mut ip = [0u8; 4];
                        c.read_exact(&mut ip).await.unwrap();
                        format!("ip {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
                    }
                };
                let mut port = [0u8; 2];
                c.read_exact(&mut port).await.unwrap();
                log.lock().unwrap().push(host);
                c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                let mut up = TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut c, &mut up).await;
            });
        }
    });
    (addr, log)
}

/// HTTP CONNECT proxy that records the CONNECT line.
async fn upstream_http(target: SocketAddr) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let (mut c, _) = l.accept().await.unwrap();
            let log = log2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    c.read_exact(&mut b).await.unwrap();
                    buf.push(b[0]);
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                log.lock()
                    .unwrap()
                    .push(text.lines().next().unwrap_or("").to_string());
                if text.contains("Proxy-Authorization: Basic dXNlcjpwdw==") {
                    log.lock().unwrap().push("auth ok".into());
                }
                c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                let mut up = TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut c, &mut up).await;
            });
        }
    });
    (addr, log)
}

fn client_via(gw: &Gateway) -> reqwest::Client {
    // socks5h: the client passes names to the gateway, like a browser does.
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("socks5h://{}", gw.addr())).unwrap())
        .build()
        .unwrap()
}

#[tokio::test]
async fn direct_route_and_ad_blocking() {
    let (web, hits) = web_server().await;
    let filters = Arc::new(Filters::from_rules("||ads.example^\n||tracker.example^\n"));
    let gw = Gateway::start(
        Upstream::Direct {
            dns: DnsMode::System,
        },
        Some(filters),
    )
    .await
    .unwrap();
    let c = client_via(&gw);

    let body = c
        .get(format!("http://localhost:{}/page", web.port()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "hello through the gateway");

    assert!(c.get("http://ads.example/banner.js").send().await.is_err());
    assert!(c
        .get("http://cdn.tracker.example/p.gif")
        .send()
        .await
        .is_err());
    let s = gw.stats();
    assert_eq!(s.blocked, 2);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    gw.set_filters(None);
    // With filtering off the gateway would try the real site, which does
    // not exist, so it now fails on lookup instead of being blocked.
    assert!(c.get("http://ads.example/banner.js").send().await.is_err());
    assert_eq!(gw.stats().blocked, 2);
}

#[tokio::test]
async fn socks_upstream_gets_names_and_login() {
    let (web, _) = web_server().await;
    let (up, log) = upstream_socks(web, Some(("user", "pw"))).await;
    let gw = Gateway::start(
        Upstream::Socks5 {
            addr: up.to_string(),
            user: Some("user".into()),
            pass: Some("pw".into()),
        },
        None,
    )
    .await
    .unwrap();
    let body = client_via(&gw)
        .get("http://video.cdn.invalid/clip.mp4")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "hello through the gateway");
    assert_eq!(
        *log.lock().unwrap(),
        vec!["login ok".to_string(), "name video.cdn.invalid".to_string()]
    );
}

#[tokio::test]
async fn http_proxy_upstream() {
    let (web, _) = web_server().await;
    let (up, log) = upstream_http(web).await;
    let gw = Gateway::start(
        Upstream::from_proxy_url(&format!("http://user:pw@{up}")).unwrap(),
        None,
    )
    .await
    .unwrap();
    let body = client_via(&gw)
        .get("http://media.invalid/a.mp4")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "hello through the gateway");
    let log = log.lock().unwrap().clone();
    assert_eq!(log[0], "CONNECT media.invalid:80 HTTP/1.1");
    assert_eq!(log[1], "auth ok");
}

#[tokio::test]
async fn fails_closed_when_the_route_is_down() {
    let (web, hits) = web_server().await;
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let gw = Gateway::start(
        Upstream::Socks5 {
            addr: dead.to_string(),
            user: None,
            pass: None,
        },
        None,
    )
    .await
    .unwrap();
    assert!(gw.check_upstream().await.is_err());
    let r = client_via(&gw)
        .get(format!("http://localhost:{}/secret", web.port()))
        .send()
        .await;
    assert!(r.is_err(), "must fail, never go direct");
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let s = gw.stats();
    assert_eq!(s.failed, 1);
    assert!(s.last_error.unwrap().contains("not answering"));
}

#[tokio::test]
async fn switching_route_cuts_open_connections() {
    // A server that accepts and then just holds the connection open.
    let hold = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hold_addr = hold.local_addr().unwrap();
    tokio::spawn(async move {
        let mut keep = Vec::new();
        loop {
            let (s, _) = hold.accept().await.unwrap();
            keep.push(s);
        }
    });
    let gw = Gateway::start(
        Upstream::Direct {
            dns: DnsMode::System,
        },
        None,
    )
    .await
    .unwrap();

    // Open a tunnel by hand through the gateway.
    let mut c = TcpStream::connect(gw.addr()).await.unwrap();
    c.write_all(&[5, 1, 0]).await.unwrap();
    let mut r = [0u8; 2];
    c.read_exact(&mut r).await.unwrap();
    let mut req = vec![5, 1, 0, 1, 127, 0, 0, 1];
    req.extend_from_slice(&hold_addr.port().to_be_bytes());
    c.write_all(&req).await.unwrap();
    let mut rep = [0u8; 10];
    c.read_exact(&mut rep).await.unwrap();
    assert_eq!(rep[1], 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(gw.stats().active, 1);

    // Switch to Tor (or anything). The old tunnel must close at once.
    gw.set_upstream(Upstream::Socks5 {
        addr: "127.0.0.1:9".into(),
        user: None,
        pass: None,
    });
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(2), c.read(&mut b))
        .await
        .expect("connection should close quickly");
    assert_eq!(n.unwrap(), 0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(gw.stats().active, 0);
}

/// Upstream "Tor" that accepts any login and records it, then relays.
async fn recording_tor(target: SocketAddr) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let (mut c, _) = l.accept().await.unwrap();
            let log = log2.clone();
            tokio::spawn(async move {
                let mut h = [0u8; 2];
                c.read_exact(&mut h).await.unwrap();
                let mut m = vec![0u8; h[1] as usize];
                c.read_exact(&mut m).await.unwrap();
                let mut who = "no login".to_string();
                if m.contains(&2) {
                    c.write_all(&[5, 2]).await.unwrap();
                    let mut v = [0u8; 2];
                    c.read_exact(&mut v).await.unwrap();
                    let mut user = vec![0u8; v[1] as usize];
                    c.read_exact(&mut user).await.unwrap();
                    let mut pl = [0u8; 1];
                    c.read_exact(&mut pl).await.unwrap();
                    let mut pass = vec![0u8; pl[0] as usize];
                    c.read_exact(&mut pass).await.unwrap();
                    c.write_all(&[1, 0]).await.unwrap();
                    who = String::from_utf8(user).unwrap();
                } else {
                    c.write_all(&[5, 0]).await.unwrap();
                }
                let mut r = [0u8; 4];
                c.read_exact(&mut r).await.unwrap();
                let mut len = [0u8; 1];
                c.read_exact(&mut len).await.unwrap();
                let mut n = vec![0u8; len[0] as usize + 2];
                c.read_exact(&mut n).await.unwrap();
                log.lock().unwrap().push(who);
                c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                let mut up = TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut c, &mut up).await;
            });
        }
    });
    (addr, log)
}

#[tokio::test]
async fn one_download_spreads_over_several_tor_circuits() {
    use osprey_core::client::build_clients;
    use osprey_core::privacy::{Privacy, Route};
    use osprey_core::RequestOptions;

    let (web, _) = web_server().await;
    let (tor, logins) = recording_tor(web).await;
    let gw = Gateway::start(
        Upstream::Socks5 {
            addr: tor.to_string(),
            user: None,
            pass: None,
        },
        None,
    )
    .await
    .unwrap();

    // The browser's own tabs log in with nothing: one shared circuit.
    let plain = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("socks5h://{}", gw.addr())).unwrap())
        .build()
        .unwrap();
    plain.get("http://site.invalid/").send().await.unwrap();

    // A download asking for three circuits gets three different names.
    let opts = RequestOptions {
        privacy: Privacy {
            route: Route::Proxy(gw.proxy_url()),
            circuits: 3,
            ..Default::default()
        },
        ..Default::default()
    };
    let clients = build_clients(&opts).unwrap();
    assert_eq!(clients.len(), 3);
    for i in 0..3 {
        clients
            .pick(i)
            .get(format!("http://cdn.invalid/seg{i}.ts"))
            .send()
            .await
            .unwrap();
    }

    let seen = logins.lock().unwrap().clone();
    assert_eq!(seen[0], "no login");
    let names: std::collections::HashSet<_> = seen[1..].iter().collect();
    assert_eq!(names.len(), 3, "{seen:?}");
    assert!(
        seen[1..].iter().all(|n| n.starts_with("osprey-")),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_vpn_login_is_never_replaced_by_a_circuit_name() {
    let (web, _) = web_server().await;
    let (vpn, log) = upstream_socks(web, Some(("vpnuser", "vpnpass"))).await;
    let gw = Gateway::start(
        Upstream::Socks5 {
            addr: vpn.to_string(),
            user: Some("vpnuser".into()),
            pass: Some("vpnpass".into()),
        },
        None,
    )
    .await
    .unwrap();
    let c = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("socks5h://circuit-9:osprey@{}", gw.addr())).unwrap())
        .build()
        .unwrap();
    c.get("http://site.invalid/").send().await.unwrap();
    assert_eq!(log.lock().unwrap()[0], "login ok");
}
