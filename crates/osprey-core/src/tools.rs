//! yt-dlp and Deno are not shipped with BM Osprey. They are downloaded on
//! first use into `<BM Osprey home>/tools` and kept up to date from then on.
//! yt-dlp needs Deno for YouTube.
//!
//! Each download is checked against the SHA-256 its project publishes next
//! to the release. That catches a damaged or cut off download. The sum comes
//! from the same place as the file, so it cannot catch a hacked release.
//!
//! The published sum is also how an update is found: when it differs from
//! the one recorded at install time, there is a new release. All requests
//! use the client handed in, so the caller decides the route.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::error::{Error, Result};

const YTDLP_RELEASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
const DENO_RELEASE: &str = "https://github.com/denoland/deno/releases/latest/download";
const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;
const MAX_UNPACKED: u64 = 600 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
struct Source {
    /// The file name in the tools folder.
    binary: String,
    url: String,
    sums_url: String,
    /// The name to look for in a shared list of sums. Without one, the
    /// file holds a single sum.
    listed_as: Option<String>,
    zipped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Installed,
    Updated,
    AlreadyCurrent,
}

#[derive(Debug)]
pub struct Report {
    pub tool: String,
    /// What happened, or why it failed.
    pub result: std::result::Result<Step, String>,
}

pub fn tools_dir(home: &Path) -> PathBuf {
    home.join("tools")
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The downloaded Deno, once it is there.
pub fn deno_path(home: &Path) -> Option<PathBuf> {
    Some(tools_dir(home).join(exe("deno"))).filter(|p| p.is_file())
}

fn ytdlp_asset(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("yt-dlp.exe"),
        ("macos", _) => Some("yt-dlp_macos"),
        ("linux", "x86_64") => Some("yt-dlp_linux"),
        ("linux", "aarch64") => Some("yt-dlp_linux_aarch64"),
        _ => None,
    }
}

fn deno_asset(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("deno-x86_64-pc-windows-msvc.zip"),
        ("macos", "x86_64") => Some("deno-x86_64-apple-darwin.zip"),
        ("macos", "aarch64") => Some("deno-aarch64-apple-darwin.zip"),
        ("linux", "x86_64") => Some("deno-x86_64-unknown-linux-gnu.zip"),
        ("linux", "aarch64") => Some("deno-aarch64-unknown-linux-gnu.zip"),
        _ => None,
    }
}

fn sources() -> Vec<Source> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let mut out = Vec::new();
    if let Some(asset) = ytdlp_asset(os, arch) {
        out.push(Source {
            binary: exe("yt-dlp"),
            url: format!("{YTDLP_RELEASE}/{asset}"),
            sums_url: format!("{YTDLP_RELEASE}/SHA2-256SUMS"),
            listed_as: Some(asset.to_string()),
            zipped: false,
        });
    }
    if let Some(asset) = deno_asset(os, arch) {
        out.push(Source {
            binary: exe("deno"),
            url: format!("{DENO_RELEASE}/{asset}"),
            sums_url: format!("{DENO_RELEASE}/{asset}.sha256sum"),
            listed_as: None,
            zipped: true,
        });
    }
    out
}

fn is_sum(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Finds a sum in a `hash  name` list, or the only sum in a file. Deno's
/// file has a different layout depending on the system that made it.
fn published_sum(text: &str, listed_as: Option<&str>) -> Option<String> {
    let found = match listed_as {
        Some(name) => text.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            let sum = parts.next()?;
            let file = parts.next()?.trim_start_matches('*');
            (file == name && is_sum(sum)).then_some(sum)
        }),
        None => text
            .split(|c: char| !c.is_ascii_hexdigit())
            .find(|t| is_sum(t)),
    };
    found.map(str::to_ascii_lowercase)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Status {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    Ok(response.text().await?)
}

/// Saves a download and returns its SHA-256.
async fn download(client: &reqwest::Client, url: &str, to: &Path) -> Result<String> {
    let mut response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Status {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    let mut file = tokio::fs::File::create(to).await?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    while let Some(chunk) = response.chunk().await? {
        total += chunk.len() as u64;
        if total > MAX_DOWNLOAD {
            return Err(Error::Unsupported(format!(
                "{url} is larger than expected, stopped"
            )));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(hex(&hash.finalize()))
}

/// Takes one named file out of a zip. Only that name is used for the output
/// path, so nothing in the zip can choose where a file lands.
fn unzip_one(zip_path: &Path, name: &str, to: &Path) -> Result<()> {
    let bad = |e: String| Error::Unsupported(format!("could not unpack {name}: {e}"));
    let mut zip =
        zip::ZipArchive::new(std::fs::File::open(zip_path)?).map_err(|e| bad(e.to_string()))?;
    let entry = zip.by_name(name).map_err(|e| bad(e.to_string()))?;
    let mut out = std::fs::File::create(to)?;
    let written = std::io::copy(&mut entry.take(MAX_UNPACKED + 1), &mut out)?;
    if written > MAX_UNPACKED {
        return Err(bad("larger than expected".to_string()));
    }
    Ok(())
}

#[cfg(unix)]
fn make_runnable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_runnable(_path: &Path) -> Result<()> {
    Ok(())
}

async fn install(client: &reqwest::Client, dir: &Path, src: &Source) -> Result<Step> {
    tokio::fs::create_dir_all(dir).await?;
    let binary = dir.join(&src.binary);
    let marker = dir.join(format!("{}.sha256", src.binary));
    let sums = fetch_text(client, &src.sums_url).await?;
    let wanted = published_sum(&sums, src.listed_as.as_deref()).ok_or_else(|| {
        Error::Unsupported(format!(
            "no checksum for {} in {}",
            src.binary, src.sums_url
        ))
    })?;
    let existed = binary.is_file();
    let recorded = tokio::fs::read_to_string(&marker).await.unwrap_or_default();
    if existed && recorded.trim() == wanted {
        return Ok(Step::AlreadyCurrent);
    }

    let part = dir.join(format!("{}.part", src.binary));
    let new = dir.join(format!("{}.new", src.binary));
    let replaced = async {
        let got = download(client, &src.url, &part).await?;
        if got != wanted {
            return Err(Error::Unsupported(format!(
                "{} failed its checksum, it was not installed",
                src.binary
            )));
        }
        if src.zipped {
            let (from, to, name) = (part.clone(), new.clone(), src.binary.clone());
            tokio::task::spawn_blocking(move || unzip_one(&from, &name, &to))
                .await
                .map_err(|e| Error::Unsupported(e.to_string()))??;
        } else {
            tokio::fs::rename(&part, &new).await?;
        }
        make_runnable(&new)?;
        // Fails on Windows while the tool is running. The next check tries
        // again, and the old copy keeps working until then.
        tokio::fs::rename(&new, &binary).await?;
        tokio::fs::write(&marker, &wanted).await?;
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&part).await;
    let _ = tokio::fs::remove_file(&new).await;
    replaced?;
    Ok(if existed {
        Step::Updated
    } else {
        Step::Installed
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|t| now.saturating_sub(t) >= CHECK_EVERY_SECS)
}

/// Installs what is missing, and looks for updates once a day. Does nothing
/// when neither is needed, unless `force` is set. One tool failing does not
/// stop the other, and the next check tries again.
pub async fn refresh(home: &Path, client: &reqwest::Client, force: bool) -> Vec<Report> {
    let dir = tools_dir(home);
    let stamp = dir.join("last-check");
    let list = sources();
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|t| t.trim().parse().ok());
    let missing = list.iter().any(|s| !dir.join(&s.binary).is_file());
    if !force && !missing && !is_due(last, now()) {
        return Vec::new();
    }
    let mut reports = Vec::new();
    for src in &list {
        reports.push(Report {
            tool: src.binary.clone(),
            result: install(client, &dir, src).await.map_err(|e| e.to_string()),
        });
    }
    if reports.iter().all(|r| r.result.is_ok()) {
        let _ = std::fs::write(&stamp, now().to_string());
    }
    reports
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::AsyncReadExt as _;
    use tokio::net::TcpListener;

    use super::*;

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    /// Serves fixed bodies by path, and counts downloads of `/file`.
    async fn serve(files: Vec<(&'static str, Vec<u8>)>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                if path == "/file" {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                let reply = match files.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = sock.write_all(&reply).await;
                let _ = sock.shutdown().await;
            }
        });
        (base, hits)
    }

    fn source(base: &str, zipped: bool) -> Source {
        Source {
            binary: "tool".into(),
            url: format!("{base}/file"),
            sums_url: format!("{base}/sums"),
            listed_as: Some("asset".into()),
            zipped,
        }
    }

    fn listing(body: &[u8]) -> Vec<u8> {
        format!("{}  other\n{}  asset\n", "0".repeat(64), sha(body)).into_bytes()
    }

    #[test]
    fn sums_are_found_in_every_published_layout() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let list = format!("{a}  yt-dlp\n{b} *yt-dlp.exe\n");
        assert_eq!(
            published_sum(&list, Some("yt-dlp")).as_deref(),
            Some(a.as_str())
        );
        assert_eq!(
            published_sum(&list, Some("yt-dlp.exe")),
            Some("b".repeat(64))
        );
        assert_eq!(published_sum(&list, Some("yt")), None, "the whole name");

        let powershell =
            format!("\r\nAlgorithm : SHA256\r\nHash      : {b}\r\nPath      : d.zip\r\n");
        assert_eq!(published_sum(&powershell, None), Some("b".repeat(64)));
        let plain = format!("{a}  deno.zip\n");
        assert_eq!(published_sum(&plain, None), Some(a));
        assert_eq!(published_sum("nothing here", None), None);
    }

    #[test]
    fn every_system_has_its_download() {
        for (os, arch) in [
            ("windows", "x86_64"),
            ("macos", "aarch64"),
            ("macos", "x86_64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
        ] {
            assert!(ytdlp_asset(os, arch).is_some(), "{os} {arch}");
            assert!(deno_asset(os, arch).is_some(), "{os} {arch}");
        }
        assert_eq!(ytdlp_asset("freebsd", "x86_64"), None);
    }

    #[test]
    fn checks_happen_once_a_day() {
        assert!(is_due(None, 100));
        assert!(!is_due(Some(100), 100 + CHECK_EVERY_SECS - 1));
        assert!(is_due(Some(100), 100 + CHECK_EVERY_SECS));
    }

    #[tokio::test]
    async fn installs_then_updates_only_when_the_release_changes() {
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();

        let (base, hits) =
            serve(vec![("/file", b"one".to_vec()), ("/sums", listing(b"one"))]).await;
        let src = source(&base, false);
        let step = install(&client, dir.path(), &src).await.unwrap();
        assert_eq!(step, Step::Installed);
        assert_eq!(std::fs::read(dir.path().join("tool")).unwrap(), b"one");
        let step = install(&client, dir.path(), &src).await.unwrap();
        assert_eq!(step, Step::AlreadyCurrent);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "not downloaded again");

        let (base, _) = serve(vec![("/file", b"two".to_vec()), ("/sums", listing(b"two"))]).await;
        let step = install(&client, dir.path(), &source(&base, false))
            .await
            .unwrap();
        assert_eq!(step, Step::Updated);
        assert_eq!(std::fs::read(dir.path().join("tool")).unwrap(), b"two");
    }

    #[tokio::test]
    async fn a_bad_download_leaves_the_old_copy_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool"), b"old").unwrap();
        let (base, _) = serve(vec![
            ("/file", b"tampered".to_vec()),
            ("/sums", listing(b"expected")),
        ])
        .await;
        let err = install(&reqwest::Client::new(), dir.path(), &source(&base, false))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
        assert_eq!(std::fs::read(dir.path().join("tool")).unwrap(), b"old");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "no leftovers");
    }

    #[tokio::test]
    async fn a_missing_sum_stops_the_install() {
        let dir = tempfile::tempdir().unwrap();
        let (base, hits) = serve(vec![
            ("/file", b"one".to_vec()),
            ("/sums", b"nothing useful".to_vec()),
        ])
        .await;
        assert!(
            install(&reqwest::Client::new(), dir.path(), &source(&base, false))
                .await
                .is_err()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(!dir.path().join("tool").exists());
    }

    fn zip_with(name: &str, body: &[u8]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(body).unwrap();
        zip.finish().unwrap();
        out.into_inner()
    }

    #[tokio::test]
    async fn a_zipped_tool_is_unpacked() {
        let dir = tempfile::tempdir().unwrap();
        let archive = zip_with("tool", b"program");
        let (base, _) = serve(vec![("/sums", listing(&archive)), ("/file", archive)]).await;
        install(&reqwest::Client::new(), dir.path(), &source(&base, true))
            .await
            .unwrap();
        assert_eq!(std::fs::read(dir.path().join("tool")).unwrap(), b"program");
        assert!(!dir.path().join("tool.part").exists());
    }

    #[tokio::test]
    async fn a_zip_without_the_tool_fails() {
        let dir = tempfile::tempdir().unwrap();
        let archive = zip_with("../../evil", b"x");
        let (base, _) = serve(vec![("/sums", listing(&archive)), ("/file", archive)]).await;
        assert!(
            install(&reqwest::Client::new(), dir.path(), &source(&base, true))
                .await
                .is_err()
        );
        assert!(!dir.path().join("tool").exists());
    }
}
