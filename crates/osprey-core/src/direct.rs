//! Plain file downloads. When the server supports range requests the file
//! is split into parts that download in parallel over several connections
//! (the IDM / 1DM trick). Progress is saved to `<file>.osprey.json` so an
//! interrupted download picks up where it stopped.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::RANGE;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::client::{ensure_success, Clients};
use crate::error::{Error, Result};
use crate::probe::ProbeInfo;
use crate::progress::{Progress, ProgressFn, Unit};

#[derive(Debug, Clone)]
pub struct DirectOptions {
    /// Number of parallel connections for one file.
    pub connections: usize,
    /// Files smaller than this are fetched over a single connection.
    pub min_split_size: u64,
    /// Retries per part before giving up.
    pub retries: u32,
    /// Shared speed limit, when one is set.
    pub limit: Option<std::sync::Arc<crate::limit::SpeedLimit>>,
}

impl Default for DirectOptions {
    fn default() -> Self {
        Self {
            connections: 8,
            min_split_size: 2 * 1024 * 1024,
            retries: 5,
            limit: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Part {
    start: u64,
    /// Inclusive end offset.
    end: u64,
    done: u64,
}

impl Part {
    fn len(&self) -> u64 {
        self.end - self.start + 1
    }
    fn is_complete(&self) -> bool {
        self.done >= self.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    url: String,
    size: u64,
    parts: Vec<Part>,
}

const WRITE_BUFFER: usize = 512 * 1024;

pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn plan_parts(size: u64, connections: usize) -> Vec<Part> {
    let n = (connections.max(1) as u64).min(size.max(1));
    let chunk = size.div_ceil(n);
    let mut parts = Vec::new();
    let mut start = 0;
    while start < size {
        let end = (start + chunk).min(size) - 1;
        parts.push(Part {
            start,
            end,
            done: 0,
        });
        start = end + 1;
    }
    parts
}

/// Downloads `info.final_url` into `dest`. The data goes to `dest.part`
/// first and is renamed once complete.
pub async fn download(
    clients: impl Into<Clients>,
    info: &ProbeInfo,
    dest: &Path,
    opts: &DirectOptions,
    progress: ProgressFn,
) -> Result<()> {
    let clients: Clients = clients.into();
    let part_path = with_suffix(dest, ".part");
    let state_path = with_suffix(dest, ".osprey.json");
    crate::adopt_old_name(&with_suffix(dest, ".bmf.json"), &state_path);

    match (info.accepts_ranges, info.size) {
        (true, Some(size)) if size >= opts.min_split_size && opts.connections > 1 => {
            segmented(
                &clients,
                &info.final_url,
                size,
                &part_path,
                &state_path,
                opts,
                progress,
            )
            .await?
        }
        _ => {
            single(
                clients.first(),
                info,
                &part_path,
                progress,
                opts.limit.as_deref(),
            )
            .await?
        }
    }

    fs::rename(&part_path, dest).await?;
    let _ = fs::remove_file(&state_path).await;
    Ok(())
}

async fn load_state(state_path: &Path, part_path: &Path, url: &str, size: u64) -> Option<State> {
    let raw = fs::read(state_path).await.ok()?;
    let state: State = serde_json::from_slice(&raw).ok()?;
    let part_ok = fs::metadata(part_path)
        .await
        .map(|m| m.len() == size)
        .unwrap_or(false);
    (state.url == url && state.size == size && part_ok).then_some(state)
}

async fn save_state(state_path: &Path, state: &State) -> Result<()> {
    let tmp = with_suffix(state_path, ".tmp");
    fs::write(&tmp, serde_json::to_vec(state)?).await?;
    fs::rename(&tmp, state_path).await?;
    Ok(())
}

async fn segmented(
    clients: &Clients,
    url: &str,
    size: u64,
    part_path: &Path,
    state_path: &Path,
    opts: &DirectOptions,
    progress: ProgressFn,
) -> Result<()> {
    let state = match load_state(state_path, part_path, url, size).await {
        Some(s) => {
            progress(Progress::Status("resuming earlier download".into()));
            s
        }
        None => {
            let file = fs::File::create(part_path).await?;
            file.set_len(size).await?;
            let s = State {
                url: url.to_string(),
                size,
                parts: plan_parts(size, opts.connections),
            };
            save_state(state_path, &s).await?;
            s
        }
    };

    let already: u64 = state.parts.iter().map(|p| p.done.min(p.len())).sum();
    let connections = state.parts.iter().filter(|p| !p.is_complete()).count();
    progress(Progress::Start {
        label: format!("download ({connections} connections)"),
        total: Some(size),
        unit: Unit::Bytes,
    });
    progress(Progress::Advance(already));

    let state = Arc::new(Mutex::new(state));

    // Writes the state file once a second so a crash loses at most a
    // second of progress.
    let saver = {
        let state = state.clone();
        let state_path = state_path.to_path_buf();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let snapshot = state.lock().expect("state lock").clone();
                let _ = save_state(&state_path, &snapshot).await;
            }
        })
    };

    let count = state.lock().expect("state lock").parts.len();
    let mut tasks = Vec::new();
    for idx in 0..count {
        if state.lock().expect("state lock").parts[idx].is_complete() {
            continue;
        }
        tasks.push(tokio::spawn(fetch_part(
            clients.pick(idx).clone(),
            url.to_string(),
            part_path.to_path_buf(),
            idx,
            state.clone(),
            opts.retries,
            progress.clone(),
            opts.limit.clone(),
        )));
    }

    let mut first_error = None;
    for t in tasks {
        let outcome = match t.await {
            Ok(r) => r,
            Err(e) => Err(Error::Unsupported(format!("download task crashed: {e}"))),
        };
        if let Err(e) = outcome {
            first_error.get_or_insert(e);
        }
    }

    saver.abort();
    let snapshot = state.lock().expect("state lock").clone();
    save_state(state_path, &snapshot).await?;
    if let Some(e) = first_error {
        return Err(e);
    }
    progress(Progress::Finish);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn fetch_part(
    client: reqwest::Client,
    url: String,
    part_path: PathBuf,
    idx: usize,
    state: Arc<Mutex<State>>,
    retries: u32,
    progress: ProgressFn,
    limit: Option<Arc<crate::limit::SpeedLimit>>,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        let (start, end, done) = {
            let s = state.lock().expect("state lock");
            let p = &s.parts[idx];
            (p.start, p.end, p.done)
        };
        if start + done > end {
            return Ok(());
        }
        let from = start + done;

        match fetch_range(
            &client,
            &url,
            &part_path,
            idx,
            from,
            end,
            &state,
            &progress,
            limit.as_deref(),
        )
        .await
        {
            Ok(()) => {
                let complete = state.lock().expect("state lock").parts[idx].is_complete();
                if complete {
                    return Ok(());
                }
                // The server closed early. Treat it like any other failure.
            }
            Err(e @ Error::Unsupported(_)) => return Err(e),
            Err(e) if attempt >= retries => return Err(e),
            Err(_) => {}
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn fetch_range(
    client: &reqwest::Client,
    url: &str,
    part_path: &Path,
    idx: usize,
    from: u64,
    end: u64,
    state: &Arc<Mutex<State>>,
    progress: &ProgressFn,
    limit: Option<&crate::limit::SpeedLimit>,
) -> Result<()> {
    let resp = ensure_success(
        client
            .get(url)
            .header(RANGE, format!("bytes={from}-{end}"))
            .send()
            .await?,
    )?;
    if resp.status() != StatusCode::PARTIAL_CONTENT {
        return Err(Error::Unsupported(
            "server stopped honouring range requests, try again with --connections 1".into(),
        ));
    }

    let mut file = OpenOptions::new().write(true).open(part_path).await?;
    file.seek(SeekFrom::Start(from)).await?;

    let wanted = end - from + 1;
    let mut written: u64 = 0;
    let mut buf: Vec<u8> = Vec::with_capacity(WRITE_BUFFER);
    let mut stream = resp.bytes_stream();
    let mut stream_error = None;

    while written + (buf.len() as u64) < wanted {
        match stream.next().await {
            Some(Ok(chunk)) => {
                let room = (wanted - written - buf.len() as u64) as usize;
                buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if buf.len() >= WRITE_BUFFER {
                    let n = buf.len() as u64;
                    flush(&mut file, &mut buf, &mut written, idx, state, progress).await?;
                    if let Some(l) = limit {
                        l.take(n).await;
                    }
                }
            }
            Some(Err(e)) => {
                stream_error = Some(e);
                break;
            }
            None => break,
        }
    }
    // Keep whatever arrived before an error. It is valid data.
    flush(&mut file, &mut buf, &mut written, idx, state, progress).await?;

    match stream_error {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

async fn flush(
    file: &mut fs::File,
    buf: &mut Vec<u8>,
    written: &mut u64,
    idx: usize,
    state: &Arc<Mutex<State>>,
    progress: &ProgressFn,
) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    file.write_all(buf).await?;
    file.flush().await?;
    let n = buf.len() as u64;
    *written += n;
    state.lock().expect("state lock").parts[idx].done += n;
    progress(Progress::Advance(n));
    buf.clear();
    Ok(())
}

async fn single(
    client: &reqwest::Client,
    info: &ProbeInfo,
    part_path: &Path,
    progress: ProgressFn,
    limit: Option<&crate::limit::SpeedLimit>,
) -> Result<()> {
    let mut start = if info.accepts_ranges {
        fs::metadata(part_path).await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    if info.size.is_some_and(|s| start >= s) {
        start = 0;
    }

    let mut req = client.get(&info.final_url);
    if start > 0 {
        req = req.header(RANGE, format!("bytes={start}-"));
    }
    let resp = ensure_success(req.send().await?)?;
    if start > 0 && resp.status() != StatusCode::PARTIAL_CONTENT {
        start = 0;
    }

    let mut file = if start > 0 {
        progress(Progress::Status("resuming earlier download".into()));
        OpenOptions::new().append(true).open(part_path).await?
    } else {
        fs::File::create(part_path).await?
    };

    progress(Progress::Start {
        label: "download".into(),
        total: info.size,
        unit: Unit::Bytes,
    });
    progress(Progress::Advance(start));

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        progress(Progress::Advance(chunk.len() as u64));
        if let Some(l) = limit {
            l.take(chunk.len() as u64).await;
        }
    }
    file.flush().await?;
    progress(Progress::Finish);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_cover_the_whole_file_without_overlap() {
        for (size, n) in [
            (10u64, 3usize),
            (1, 8),
            (8 * 1024 * 1024 + 7, 8),
            (5, 5),
            (3, 8),
        ] {
            let parts = plan_parts(size, n);
            assert_eq!(parts.first().unwrap().start, 0);
            assert_eq!(parts.last().unwrap().end, size - 1);
            for w in parts.windows(2) {
                assert_eq!(w[0].end + 1, w[1].start);
            }
            assert_eq!(parts.iter().map(Part::len).sum::<u64>(), size);
            assert!(parts.len() <= n);
        }
    }
}
