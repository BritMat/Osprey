//! HLS downloading: fetch the playlist, pick a quality, download all
//! segments in parallel, decrypt AES-128 where needed and stitch the
//! pieces back together in order.

pub mod parser;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures::{stream, StreamExt};
use reqwest::header::RANGE;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use url::Url;

pub use parser::{
    parse, pick_audio, pick_variant, MasterPlaylist, MediaPlaylist, Playlist, Segment,
};

use crate::client::{ensure_success, Clients};
use crate::error::{Error, Result};
use crate::progress::{Progress, ProgressFn, Unit};

#[derive(Debug, Clone)]
pub struct HlsOptions {
    /// Segments downloaded at the same time.
    pub concurrency: usize,
    /// Highest video height wanted, for example 720. None means best.
    pub max_height: Option<u32>,
    pub retries: u32,
    /// Fetch like a video player: small random gaps between segments.
    /// Pair it with a low `concurrency` for the most natural pattern.
    pub natural: bool,
    /// Share of pieces allowed to fail (after retries) before the whole
    /// download fails. A missing piece is a short gap in the video, which
    /// is better than losing a long download. 0.02 means 2 percent.
    pub max_missing: f64,
    /// Shared speed limit, when one is set.
    pub limit: Option<std::sync::Arc<crate::limit::SpeedLimit>>,
}

impl Default for HlsOptions {
    fn default() -> Self {
        Self {
            concurrency: 8,
            max_height: None,
            retries: 4,
            natural: false,
            max_missing: 0.02,
            limit: None,
        }
    }
}

/// The raw stitched streams. These still need ffmpeg to become a tidy
/// MP4, which [`crate::fetch`] takes care of.
#[derive(Debug, Clone)]
pub struct HlsOutput {
    pub video: PathBuf,
    pub audio: Option<PathBuf>,
}

/// Downloads and parses a playlist. Returns the URL after redirects too,
/// since relative segment paths resolve against it.
pub async fn load(client: &reqwest::Client, url: &str) -> Result<(Playlist, Url)> {
    let resp = ensure_success(client.get(url).send().await?)?;
    let base = resp.url().clone();
    let text = resp.text().await?;
    Ok((parse(&text, &base)?, base))
}

async fn load_media(client: &reqwest::Client, url: &Url) -> Result<MediaPlaylist> {
    match load(client, url.as_str()).await?.0 {
        Playlist::Media(m) => Ok(m),
        Playlist::Master(_) => Err(Error::Parse(format!(
            "{url} is a master playlist inside a master playlist"
        ))),
    }
}

/// Downloads an HLS stream into `work_dir`. Already downloaded segments
/// are reused, so running it again after a failure resumes.
pub async fn download(
    clients: impl Into<Clients>,
    url: &str,
    work_dir: &Path,
    opts: &HlsOptions,
    progress: ProgressFn,
) -> Result<HlsOutput> {
    fs::create_dir_all(work_dir).await?;
    let clients: Clients = clients.into();
    let client = clients.first();
    let (playlist, _) = load(client, url).await?;

    let (video_pl, audio_pl) = match playlist {
        Playlist::Media(m) => (m, None),
        Playlist::Master(master) => {
            let variant = pick_variant(&master, opts.max_height)
                .ok_or_else(|| Error::Parse("no usable variant".into()))?;
            progress(Progress::Status(format!(
                "picked {} out of {} variants",
                variant.describe(),
                master.variants.len()
            )));
            let video = load_media(client, &variant.uri).await?;
            let audio = match pick_audio(&master, variant) {
                Some(r) => {
                    let uri = r
                        .uri
                        .as_ref()
                        .expect("pick_audio only returns renditions with a URI");
                    progress(Progress::Status(format!(
                        "separate audio track: {}",
                        r.name
                            .clone()
                            .or(r.language.clone())
                            .unwrap_or_else(|| r.group_id.clone())
                    )));
                    Some(load_media(client, uri).await?)
                }
                None => None,
            };
            (video, audio)
        }
    };

    if !video_pl.ended {
        progress(Progress::Status(
            "this is a live stream, grabbing the segments listed right now".into(),
        ));
    }

    let keys = KeyCache::default();
    let video = download_media(
        &clients,
        &video_pl,
        work_dir,
        "video",
        opts,
        &keys,
        progress.clone(),
    )
    .await?;
    let audio = match audio_pl {
        Some(pl) => Some(
            download_media(
                &clients,
                &pl,
                work_dir,
                "audio",
                opts,
                &keys,
                progress.clone(),
            )
            .await?,
        ),
        None => None,
    };
    Ok(HlsOutput { video, audio })
}

#[derive(Default, Clone)]
struct KeyCache(Arc<Mutex<HashMap<Url, [u8; 16]>>>);

impl KeyCache {
    async fn get(&self, client: &reqwest::Client, uri: &Url, retries: u32) -> Result<[u8; 16]> {
        let mut map = self.0.lock().await;
        if let Some(k) = map.get(uri) {
            return Ok(*k);
        }
        let bytes = fetch_bytes(client, uri, None, retries).await?;
        let key: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
            Error::Parse(format!(
                "key at {uri} is {} bytes, expected 16",
                bytes.len()
            ))
        })?;
        map.insert(uri.clone(), key);
        Ok(key)
    }
}

/// A pause of 50 to 250 ms that differs per segment without needing a
/// random number library. Short enough to keep downloads quick, uneven
/// enough to look like a player rather than a burst.
fn natural_gap(i: usize) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ ((i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    Duration::from_millis(50 + x % 200)
}

/// When a key has no IV, HLS uses the segment's media sequence number.
fn sequence_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

pub(crate) fn decrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| Error::Parse("segment decryption failed, wrong key or damaged data".into()))
}

async fn fetch_bytes(
    client: &reqwest::Client,
    uri: &Url,
    range: Option<parser::ByteRange>,
    retries: u32,
) -> Result<Vec<u8>> {
    fetch_counted(client, uri, range, retries, None).await
}

/// Called with each chunk as it arrives: reports bytes and applies the
/// speed limit while a piece downloads, not only after it is complete,
/// so the speed shown is smooth and the limit holds.
type OnChunk<'a> = &'a (dyn Fn(u64) -> futures::future::BoxFuture<'static, ()> + Send + Sync);

async fn fetch_counted(
    client: &reqwest::Client,
    uri: &Url,
    range: Option<parser::ByteRange>,
    retries: u32,
    on_chunk: Option<OnChunk<'_>>,
) -> Result<Vec<u8>> {
    let mut attempt = 0;
    loop {
        let mut req = client.get(uri.clone());
        if let Some(r) = range {
            req = req.header(RANGE, r.header_value());
        }
        let result = async {
            let mut resp = ensure_success(req.send().await?)?;
            let mut body =
                Vec::with_capacity(resp.content_length().unwrap_or(0).min(64 << 20) as usize);
            while let Some(chunk) = resp.chunk().await? {
                body.extend_from_slice(&chunk);
                if let Some(f) = on_chunk {
                    f(chunk.len() as u64).await;
                }
            }
            Ok::<_, Error>(body)
        }
        .await;
        match result {
            Ok(b) => return Ok(b),
            // 4xx other than 429 will not get better by retrying.
            Err(Error::Status { status, .. }) if (400..500).contains(&status) && status != 429 => {
                return result;
            }
            Err(e) if attempt >= retries => return Err(e),
            Err(_) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
            }
        }
    }
}

async fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = crate::direct::with_suffix(path, ".tmp");
    fs::write(&tmp, data).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

async fn download_media(
    clients: &Clients,
    pl: &MediaPlaylist,
    work_dir: &Path,
    name: &str,
    opts: &HlsOptions,
    keys: &KeyCache,
    progress: ProgressFn,
) -> Result<PathBuf> {
    let client = clients.first();
    let seg_dir = work_dir.join(format!("{name}_segments"));
    fs::create_dir_all(&seg_dir).await?;

    // fMP4 streams start with an init section. Collect each distinct one.
    let mut maps: Vec<parser::InitMap> = Vec::new();
    for s in &pl.segments {
        if let Some(m) = &s.map {
            if !maps.contains(m) {
                maps.push(m.clone());
            }
        }
    }
    let mut map_files = HashMap::new();
    for (i, m) in maps.iter().enumerate() {
        let path = seg_dir.join(format!("init_{i}.mp4"));
        if !fs::try_exists(&path).await? {
            let data = fetch_bytes(client, &m.uri, m.byte_range, opts.retries).await?;
            write_atomic(&path, &data).await?;
        }
        map_files.insert(m.clone(), path);
    }

    progress(Progress::Start {
        label: name.to_string(),
        total: Some(pl.segments.len() as u64),
        unit: Unit::Segments,
    });

    let seg_dir_ref = &seg_dir;
    let progress_ref = &progress;
    let failed: std::sync::Mutex<Vec<(usize, Error)>> = std::sync::Mutex::new(Vec::new());
    let failed_ref = &failed;
    let allowed_missing =
        (pl.segments.len() as f64 * opts.max_missing.clamp(0.0, 0.2)).floor() as usize;
    stream::iter(pl.segments.iter().enumerate())
        .for_each_concurrent(opts.concurrency.max(1), |(i, seg)| async move {
            // Stop starting new pieces once too many have failed.
            if failed_ref.lock().expect("failed lock").len() > allowed_missing {
                return;
            }
            let path = seg_dir_ref.join(format!("{i:06}.seg"));
            let reusable = fs::metadata(&path)
                .await
                .map(|m| m.len() > 0)
                .unwrap_or(false);
            if !reusable {
                if opts.natural {
                    // Small random gaps, like a player filling its buffer.
                    tokio::time::sleep(natural_gap(i)).await;
                }
                let client = clients.pick(i);
                let on_chunk = {
                    let progress = progress_ref.clone();
                    let limit = opts.limit.clone();
                    move |n: u64| -> futures::future::BoxFuture<'static, ()> {
                        progress(Progress::Bytes(n));
                        let limit = limit.clone();
                        Box::pin(async move {
                            if let Some(l) = limit {
                                l.take(n).await;
                            }
                        })
                    }
                };
                let piece = async {
                    let raw = fetch_counted(
                        client,
                        &seg.uri,
                        seg.byte_range,
                        opts.retries,
                        Some(&on_chunk),
                    )
                    .await?;
                    let data = match &seg.key {
                        Some(k) => {
                            let key = keys.get(client, &k.uri, opts.retries).await?;
                            let iv = k.iv.unwrap_or_else(|| sequence_iv(seg.sequence));
                            decrypt(&raw, &key, &iv)?
                        }
                        None => raw,
                    };
                    write_atomic(&path, &data).await?;
                    Ok::<(), Error>(())
                };
                match piece.await {
                    Ok(()) => {}
                    Err(e) => {
                        failed_ref.lock().expect("failed lock").push((i, e));
                        return;
                    }
                }
            }
            progress_ref(Progress::Advance(1));
        })
        .await;
    progress(Progress::Finish);

    let failed = failed.into_inner().expect("failed lock");
    if failed.len() > allowed_missing {
        let (_, first) = failed.into_iter().next().expect("at least one failure");
        return Err(first);
    }
    let missing: std::collections::HashSet<usize> = failed.iter().map(|(i, _)| *i).collect();
    if !missing.is_empty() {
        progress(Progress::Status(format!(
            "{} of {} pieces could not be fetched, the video has short gaps there",
            missing.len(),
            pl.segments.len()
        )));
    }

    // Join everything in playlist order, inserting init sections where
    // they start or change.
    let ext = if maps.is_empty() { "ts" } else { "mp4" };
    let out_path = work_dir.join(format!("{name}.{ext}"));
    let mut out = fs::File::create(&out_path).await?;
    let mut current_map: Option<&parser::InitMap> = None;
    for (i, seg) in pl.segments.iter().enumerate() {
        if let Some(m) = &seg.map {
            if current_map != Some(m) {
                let mut f = fs::File::open(&map_files[m]).await?;
                tokio::io::copy(&mut f, &mut out).await?;
                current_map = Some(m);
            }
        }
        if missing.contains(&i) {
            continue;
        }
        let mut f = fs::File::open(seg_dir.join(format!("{i:06}.seg"))).await?;
        tokio::io::copy(&mut f, &mut out).await?;
    }
    out.flush().await?;
    Ok(out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    #[test]
    fn sequence_iv_is_big_endian() {
        let iv = sequence_iv(0x0102);
        assert_eq!(&iv[..14], &[0u8; 14]);
        assert_eq!(&iv[14..], &[1, 2]);
    }

    #[test]
    fn decrypt_round_trip() {
        let key = [7u8; 16];
        let iv = sequence_iv(5);
        let plain = b"segment payload that is not block aligned".to_vec();
        let enc = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(&plain);
        assert_eq!(decrypt(&enc, &key, &iv).unwrap(), plain);
        let wrong = decrypt(&enc, &[8u8; 16], &iv);
        assert!(wrong.map(|d| d != plain).unwrap_or(true));
    }
}
