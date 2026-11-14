//! Downloads started from the browser.
//!
//! Each download copies the identity of the tab it came from: the same
//! user agent, browser hints, languages, cookies and referring page, and
//! it leaves through the same gateway route. To the site it looks like
//! the page's own player fetching the media, not like a separate tool.
//! Fewer parallel connections are used than the command line tool uses,
//! closer to what a player does.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use osprey_core::limit::SpeedLimit;
use osprey_core::privacy::{Privacy, Route};
use osprey_core::{FetchOptions, MediaKind, Progress, RequestOptions, Unit};
use serde::Serialize;
use tauri::Emitter;
use tauri::{AppHandle, EventTarget, Manager as _, Runtime};
use tokio::sync::Semaphore;

use crate::capture;
use crate::settings::DownloadStyle;
use url::Url;

/// Downloads running at once. More would mostly compete for bandwidth.
const MAX_PARALLEL: usize = 3;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Merging,
    Paused,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: u32,
    pub name: String,
    pub url: String,
    pub kind: MediaKind,
    pub private: bool,
    pub state: JobState,
    /// Progress in `unit`: bytes for files, pieces for streams.
    pub done: u64,
    pub total: Option<u64>,
    /// "bytes" or "segments".
    pub unit: &'static str,
    /// Bytes received so far in this run, for streams as well.
    pub bytes: u64,
    /// Bytes per second, averaged over the last few seconds.
    pub speed: u64,
    /// Seconds left, when it can be estimated.
    pub eta: Option<u64>,
    pub note: Option<String>,
    pub path: Option<String>,
    pub error: Option<String>,
    /// Automatic retries used.
    pub retries: u32,
}

/// What a tab knows about itself, sent with every media report.
#[derive(Debug, Clone, Default)]
pub struct BrowserIdentity {
    pub user_agent: String,
    pub languages: Vec<String>,
    pub platform: String,
    pub mobile: bool,
    pub brands: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct DownloadRequest {
    pub url: String,
    pub kind: MediaKind,
    /// The page or frame the media was found in.
    pub page: String,
    pub identity: BrowserIdentity,
    pub tab_label: Option<String>,
    pub private: bool,
    /// The tab's title, used to name streams. Not used for private tabs,
    /// so the file name does not tell which site it came from.
    pub title: Option<String>,
    /// For streams: the quality picked in the Media panel (picture height).
    pub max_height: Option<u32>,
    /// The headers the player really sent, when the web engine showed
    /// them. They replace the ones BM Osprey would work out itself.
    pub sent: Vec<(String, String)>,
    /// Sound to fetch and join, for a video that has none of its own.
    pub audio: Option<osprey_core::SeparateAudio>,
    /// Added to the file name, when two choices would otherwise share one.
    pub tag: Option<String>,
    /// The file name a user script asked for.
    pub file_name: Option<String>,
}

/// Everything needed to start a download again (resume, retry).
#[derive(Clone)]
struct Spec {
    req: DownloadRequest,
    gateway_url: String,
    out_dir: PathBuf,
    how: How,
}

pub struct Manager {
    jobs: Mutex<Vec<Job>>,
    tasks: Mutex<HashMap<u32, tauri::async_runtime::JoinHandle<()>>>,
    specs: Mutex<HashMap<u32, Spec>>,
    next: AtomicU32,
    dirty: AtomicBool,
    slots: Arc<Semaphore>,
    /// How many downloads may run at once, the size of `slots`.
    parallel: Mutex<usize>,
    /// Shared by every download. 0 means no limit.
    pub limit: Arc<SpeedLimit>,
}

impl Default for Manager {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            tasks: Mutex::new(HashMap::new()),
            specs: Mutex::new(HashMap::new()),
            next: AtomicU32::new(1),
            dirty: AtomicBool::new(false),
            slots: Arc::new(Semaphore::new(MAX_PARALLEL)),
            parallel: Mutex::new(MAX_PARALLEL),
            limit: Arc::new(SpeedLimit::new(0)),
        }
    }
}

impl Manager {
    /// Changes how many downloads run at once. More starts waiting ones
    /// now. Fewer takes effect as running downloads finish, none is stopped.
    pub fn set_parallel(&self, n: usize) {
        let n = n.clamp(1, 10);
        let mut now = self.parallel.lock().expect("parallel");
        if n > *now {
            self.slots.add_permits(n - *now);
        } else if n < *now {
            let fewer = (*now - n) as u32;
            let slots = self.slots.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(p) = slots.acquire_many_owned(fewer).await {
                    p.forget();
                }
            });
        }
        *now = n;
    }
}

/// Network trouble is worth another try. A refusal (403, 404), DRM or a
/// broken stream is not.
/// Whether a status note still matters once a download is finished.
fn worth_keeping(note: &str) -> bool {
    ["could not", "gaps", "not found", "separate", "raw"]
        .iter()
        .any(|w| note.contains(w))
}

fn worth_retrying(e: &osprey_core::Error) -> bool {
    match e {
        osprey_core::Error::Http(_) | osprey_core::Error::Io(_) => true,
        osprey_core::Error::Status { status, .. } => {
            *status >= 500 || *status == 429 || *status == 408
        }
        _ => false,
    }
}

/// Live numbers for one run: speed and time left.
struct Meter {
    window: Instant,
    window_bytes: u64,
    pieces_fetched: u64,
    bytes_fetched: u64,
    /// Bytes received per sample, for a sliding average.
    samples: std::collections::VecDeque<(Instant, u64)>,
}

impl Meter {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            window_bytes: 0,
            pieces_fetched: 0,
            bytes_fetched: 0,
            samples: std::collections::VecDeque::new(),
        }
    }
}

/// The name shown in the panel: the page title (and quality) for streams.
fn job_name(req: &DownloadRequest) -> String {
    if let Some(name) = &req.file_name {
        return name.clone();
    }
    let stream = matches!(req.kind, MediaKind::Hls | MediaKind::Dash);
    match req
        .title
        .as_deref()
        .filter(|t| stream && !t.trim().is_empty())
    {
        Some(t) => format!(
            "{t}{}",
            req.max_height.map(|h| format!(" {h}p")).unwrap_or_default()
        ),
        None => name_for(&req.url, req.kind),
    }
}

fn name_for(url: &str, kind: MediaKind) -> String {
    let parsed = Url::parse(url).ok();
    let last = parsed
        .as_ref()
        .and_then(|u| u.path_segments())
        .and_then(|mut s| s.next_back().map(str::to_string))
        .unwrap_or_default();
    if kind == MediaKind::Hls || kind == MediaKind::Dash || last.is_empty() {
        let host = parsed
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        return format!("stream from {host}");
    }
    last
}

impl Manager {
    fn update(&self, id: u32, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self
            .jobs
            .lock()
            .expect("jobs")
            .iter_mut()
            .find(|j| j.id == id)
        {
            f(job);
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn state_of(&self, id: u32) -> Option<JobState> {
        self.jobs
            .lock()
            .expect("jobs")
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.state)
    }

    /// Adds a file a page or script is saving itself. There is nothing to
    /// fetch, so pause and resume do not apply.
    pub fn begin_local(&self, name: String, size: u64) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.jobs.lock().expect("jobs").push(Job {
            id,
            name,
            url: String::new(),
            kind: MediaKind::Direct,
            private: false,
            state: JobState::Running,
            done: 0,
            total: (size > 0).then_some(size),
            unit: "bytes",
            bytes: 0,
            speed: 0,
            eta: None,
            note: None,
            path: None,
            error: None,
            retries: 0,
        });
        self.dirty.store(true, Ordering::Relaxed);
        id
    }

    pub fn local_progress(&self, id: u32, written: u64) {
        self.update(id, |j| {
            j.done = written;
            j.bytes = written;
        });
    }

    pub fn local_done(&self, id: u32, path: String) {
        self.update(id, |j| {
            j.state = JobState::Done;
            j.path = Some(path);
        });
    }

    pub fn local_failed(&self, id: u32, why: &str) {
        self.update(id, |j| {
            if j.state != JobState::Cancelled {
                j.state = JobState::Failed;
                j.error = Some(why.to_string());
            }
        });
    }

    pub fn is_cancelled(&self, id: u32) -> bool {
        self.state_of(id) == Some(JobState::Cancelled)
    }

    pub fn snapshot(&self) -> Vec<Job> {
        self.jobs.lock().expect("jobs").clone()
    }

    pub fn path_of(&self, id: u32) -> Option<String> {
        self.jobs
            .lock()
            .expect("jobs")
            .iter()
            .find(|j| j.id == id)
            .and_then(|j| j.path.clone())
    }

    /// Sends the download list to the UI at most twice a second, and only
    /// when something changed. Keeps CPU use flat during big downloads.
    pub fn spawn_emitter<R: Runtime>(self: &Arc<Self>, app: AppHandle<R>) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tick.tick().await;
                if me.dirty.swap(false, Ordering::Relaxed) {
                    let _ = app.emit_to(EventTarget::webview("ui"), "downloads", me.snapshot());
                }
            }
        });
    }

    fn stop_task(&self, id: u32) {
        if let Some(handle) = self.tasks.lock().expect("tasks").remove(&id) {
            handle.abort();
        }
    }

    pub fn cancel(&self, id: u32) {
        self.stop_task(id);
        self.update(id, |j| {
            if matches!(
                j.state,
                JobState::Queued | JobState::Running | JobState::Merging | JobState::Paused
            ) {
                j.state = JobState::Cancelled;
                j.speed = 0;
                j.eta = None;
            }
        });
    }

    /// Stops a download and keeps what it has. Not while ffmpeg is
    /// joining the pieces, which only takes a moment.
    pub fn pause(&self, id: u32) -> Result<(), String> {
        match self.state_of(id) {
            Some(JobState::Queued | JobState::Running) => {}
            Some(JobState::Merging) => return Err("almost done, joining the pieces".into()),
            _ => return Ok(()),
        }
        self.stop_task(id);
        self.update(id, |j| {
            j.state = JobState::Paused;
            j.speed = 0;
            j.eta = None;
            j.note = Some("paused, the pieces already saved are kept".into());
        });
        Ok(())
    }

    /// Resumes a paused download, or retries a failed or cancelled one.
    /// Pieces already on disk are reused, so it continues where it was.
    pub fn resume<R: Runtime>(self: &Arc<Self>, app: AppHandle<R>, id: u32) -> Result<(), String> {
        match self.state_of(id) {
            Some(JobState::Paused | JobState::Failed | JobState::Cancelled) => {}
            _ => return Ok(()),
        }
        if !self.specs.lock().expect("specs").contains_key(&id) {
            return Err("this download can no longer be resumed".into());
        }
        self.update(id, |j| {
            j.state = JobState::Queued;
            j.error = None;
            j.note = None;
            j.retries = 0;
        });
        self.run(app, id);
        Ok(())
    }

    pub fn start<R: Runtime>(
        self: &Arc<Self>,
        app: AppHandle<R>,
        req: DownloadRequest,
        gateway_url: String,
        out_dir: PathBuf,
        how: How,
    ) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.jobs.lock().expect("jobs").push(Job {
            id,
            name: job_name(&req),
            url: req.url.clone(),
            kind: req.kind,
            private: req.private,
            state: JobState::Queued,
            done: 0,
            total: None,
            unit: "bytes",
            bytes: 0,
            speed: 0,
            eta: None,
            note: None,
            path: None,
            error: None,
            retries: 0,
        });
        self.specs.lock().expect("specs").insert(
            id,
            Spec {
                req,
                gateway_url,
                out_dir,
                how,
            },
        );
        self.dirty.store(true, Ordering::Relaxed);
        self.run(app, id);
        id
    }

    fn run<R: Runtime>(self: &Arc<Self>, app: AppHandle<R>, id: u32) {
        let Some(spec) = self.specs.lock().expect("specs").get(&id).cloned() else {
            return;
        };
        let me = self.clone();
        let handle = tauri::async_runtime::spawn(async move {
            let _slot = me.slots.clone().acquire_owned().await;
            if let Err(e) = tokio::fs::create_dir_all(&spec.out_dir).await {
                me.update(id, |j| {
                    j.state = JobState::Failed;
                    j.error = Some(format!(
                        "could not create the folder {}: {e}",
                        spec.out_dir.display()
                    ));
                });
                return;
            }

            const AUTO_RETRIES: u32 = 2;
            let mut attempt = 0;
            loop {
                me.update(id, |j| {
                    j.state = JobState::Running;
                    j.bytes = 0;
                    j.speed = 0;
                    j.eta = None;
                });
                // Cookies are read again each time: they may have changed.
                let cookies = tab_cookies(&app, spec.req.tab_label.as_deref(), &spec.req.url).await;
                let mut opts = fetch_options(
                    &spec.req,
                    &cookies,
                    spec.gateway_url.clone(),
                    spec.out_dir.clone(),
                    spec.how,
                );
                opts.direct.limit = Some(me.limit.clone());
                opts.hls.limit = Some(me.limit.clone());

                let progress = me.progress_for(id);
                match osprey_core::fetch(&spec.req.url, &opts, progress).await {
                    Ok(path) => {
                        me.update(id, |j| {
                            j.state = JobState::Done;
                            j.speed = 0;
                            j.eta = None;
                            // Keep only notes worth reading afterwards, such
                            // as gaps or a raw stream kept, not stage names.
                            if !j.note.as_deref().is_some_and(worth_keeping) {
                                j.note = None;
                            }
                            j.name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            j.path = Some(path.display().to_string());
                        });
                        break;
                    }
                    Err(e) if worth_retrying(&e) && attempt < AUTO_RETRIES => {
                        attempt += 1;
                        let wait = 3 * u64::from(attempt * attempt);
                        me.update(id, |j| {
                            j.retries = attempt;
                            j.speed = 0;
                            j.eta = None;
                            j.note = Some(format!("connection problem, trying again in {wait} s ({attempt} of {AUTO_RETRIES})"));
                        });
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                    }
                    Err(e) => {
                        me.update(id, |j| {
                            j.state = JobState::Failed;
                            j.speed = 0;
                            j.eta = None;
                            j.error = Some(e.to_string());
                        });
                        break;
                    }
                }
            }
            me.tasks.lock().expect("tasks").remove(&id);
        });
        self.tasks.lock().expect("tasks").insert(id, handle);
    }

    /// Turns engine progress into the numbers the panel shows. Speed is
    /// averaged over the last five seconds, so it does not jump around.
    fn progress_for(self: &Arc<Self>, id: u32) -> osprey_core::ProgressFn {
        let me = self.clone();
        let meter = Arc::new(Mutex::new(Meter::new()));
        Arc::new(move |event| {
            let mut m = meter.lock().expect("meter");
            match event {
                Progress::Start { label, total, unit } => {
                    *m = Meter::new();
                    me.update(id, |j| {
                        j.total = total;
                        j.done = 0;
                        j.unit = if unit == Unit::Bytes {
                            "bytes"
                        } else {
                            "segments"
                        };
                        j.note = Some(label);
                    });
                }
                Progress::Advance(n) => {
                    let bytes_unit = me
                        .jobs
                        .lock()
                        .expect("jobs")
                        .iter()
                        .find(|j| j.id == id)
                        .is_some_and(|j| j.unit == "bytes");
                    if bytes_unit {
                        m.window_bytes += n;
                        m.bytes_fetched += n;
                    } else {
                        m.pieces_fetched += 1;
                    }
                    let sample = tick(&mut m);
                    me.update(id, |j| {
                        j.done += n;
                        if bytes_unit {
                            j.bytes += n;
                        }
                        apply(j, &m, sample);
                    });
                }
                Progress::Bytes(n) => {
                    m.window_bytes += n;
                    m.bytes_fetched += n;
                    let sample = tick(&mut m);
                    me.update(id, |j| {
                        j.bytes += n;
                        apply(j, &m, sample);
                    });
                }
                Progress::Status(msg) => me.update(id, |j| {
                    if msg.contains("ffmpeg") {
                        j.state = JobState::Merging;
                        j.eta = None;
                    }
                    j.note = Some(msg);
                }),
                Progress::Finish => {}
            }
        })
    }
}

/// A speed sample once at least a second has passed.
fn tick(m: &mut Meter) -> Option<u64> {
    // Speed over the last few seconds rather than the last one. Parallel
    // connections and the speed limit deliver bytes in bursts, and a one
    // second sample would jump around with them.
    const SPAN: Duration = Duration::from_secs(5);
    let elapsed = m.window.elapsed();
    let first = m.samples.is_empty();
    if elapsed < Duration::from_secs(if first { 2 } else { 1 }) {
        return None;
    }
    let now = Instant::now();
    m.samples.push_back((now, m.window_bytes));
    m.window = now;
    m.window_bytes = 0;
    while m
        .samples
        .front()
        .is_some_and(|(t, _)| now.duration_since(*t) > SPAN)
    {
        m.samples.pop_front();
    }
    let since = m.samples.front().map(|(t, _)| *t).unwrap_or(now);
    // The oldest sample's bytes arrived before its timestamp, so the span
    // starts one sample earlier than `since`.
    let span = (now.duration_since(since) + elapsed).as_secs_f64();
    let total: u64 = m.samples.iter().map(|(_, b)| b).sum();
    Some((total as f64 / span.max(0.5)) as u64)
}

/// Updates the speed and the time left.
fn apply(j: &mut Job, m: &Meter, sample: Option<u64>) {
    if let Some(rate) = sample {
        j.speed = rate;
    }
    if j.speed == 0 {
        return;
    }
    let left_bytes = match (j.unit, j.total) {
        ("bytes", Some(total)) => Some(total.saturating_sub(j.done)),
        // Streams: pieces left times the average piece size so far.
        (_, Some(total)) if m.pieces_fetched > 0 => {
            let avg = m.bytes_fetched / m.pieces_fetched;
            Some(total.saturating_sub(j.done) * avg)
        }
        _ => None,
    };
    j.eta = left_bytes.map(|b| b / j.speed.max(1));
}

/// The cookies a tab would send to `url`. Read on a worker thread: on
/// Windows, reading them on the main thread would freeze the app.
pub async fn tab_cookies<R: Runtime>(app: &AppHandle<R>, label: Option<&str>, url: &str) -> String {
    let (Some(label), Ok(target)) = (label, Url::parse(url)) else {
        return String::new();
    };
    let Some(wv) = app.get_webview(label) else {
        return String::new();
    };
    tokio::task::spawn_blocking(move || wv.cookies_for_url(target))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default()
        .iter()
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The cookies to send, unless the request already has the real ones.
pub async fn cookies_for<R: Runtime>(app: &AppHandle<R>, req: &DownloadRequest) -> String {
    if capture::header(&req.sent, "Cookie").is_some() {
        return String::new();
    }
    tab_cookies(app, req.tab_label.as_deref(), &req.url).await
}

/// Request settings that make a check or download look like the tab's
/// own player: same identity, cookies and referrer, same route. What the
/// player really sent is used when it is known.
pub fn player_request(req: &DownloadRequest, cookies: &str, gateway_url: String) -> RequestOptions {
    let user_agent = capture::header(&req.sent, "User-Agent")
        .map(str::to_string)
        .or(Some(req.identity.user_agent.clone()))
        .filter(|u| !u.is_empty());
    RequestOptions {
        headers: browser_headers(req, cookies),
        user_agent,
        privacy: Privacy {
            route: Route::Proxy(gateway_url),
            ..Default::default()
        },
        circuit: None,
    }
}

/// How one download should run.
#[derive(Debug, Clone, Copy)]
pub struct How {
    pub style: DownloadStyle,
    /// Tor circuits to spread over. 0 or 1 when not in Tor mode.
    pub circuits: u8,
}

fn fetch_options(
    req: &DownloadRequest,
    cookies: &str,
    gateway_url: String,
    out_dir: PathBuf,
    how: How,
) -> FetchOptions {
    let natural = how.style == DownloadStyle::Natural;
    // Over Tor each circuit is slow on its own, so give each one work.
    let fast = 8usize.max(how.circuits as usize * 2);
    let connections = if natural { 4 } else { fast };
    let mut request = player_request(req, cookies, gateway_url);
    request.privacy.circuits = how.circuits;
    FetchOptions {
        request,
        output: Some(match &req.title {
            // A user script chose the name. Only its last part counts, so
            // the file stays in the downloads folder.
            _ if req.file_name.is_some() => {
                let name = req.file_name.as_deref().unwrap_or_default();
                let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
                out_dir.join(osprey_core::sanitize_filename(last))
            }
            // Streams have no file name of their own. Use the page title.
            Some(t)
                if req.audio.is_some() || matches!(req.kind, MediaKind::Hls | MediaKind::Dash) =>
            {
                let quality = req.max_height.map(|h| format!(" {h}p")).unwrap_or_default();
                let tag = req
                    .tag
                    .as_deref()
                    .map(|t| format!(" {t}"))
                    .unwrap_or_default();
                let ext = req.audio.as_ref().map_or("mp4", |a| a.ext.as_str());
                let name = format!("{t}{quality}{tag}.{ext}");
                out_dir.join(osprey_core::sanitize_filename(&name))
            }
            _ => out_dir,
        }),
        audio: req.audio.clone(),
        direct: osprey_core::DirectOptions {
            connections,
            ..Default::default()
        },
        hls: osprey_core::HlsOptions {
            concurrency: connections,
            natural,
            max_height: req.max_height,
            ..Default::default()
        },
    }
}

/// How the site and the media relate, as browsers report it in
/// Sec-Fetch-Site.
fn fetch_site(page: &Url, media: &Url) -> &'static str {
    if page.origin() == media.origin() {
        return "same-origin";
    }
    let site = |u: &Url| {
        let host = u.host_str().unwrap_or("");
        let parts: Vec<&str> = host.rsplit('.').take(2).collect();
        format!(
            "{}://{}",
            u.scheme(),
            parts.into_iter().rev().collect::<Vec<_>>().join(".")
        )
    };
    if site(page) == site(media) {
        "same-site"
    } else {
        "cross-site"
    }
}

/// The headers a browser sends when its player loads this media.
fn browser_headers(req: &DownloadRequest, cookies: &str) -> Vec<(String, String)> {
    if !req.sent.is_empty() {
        let mut h: Vec<(String, String)> = req
            .sent
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("user-agent"))
            .cloned()
            .collect();
        if !cookies.is_empty() {
            h.push(("Cookie".into(), cookies.to_string()));
        }
        return h;
    }
    let mut h: Vec<(String, String)> = Vec::new();
    let id = &req.identity;
    let page = Url::parse(&req.page).ok();
    let media = Url::parse(&req.url).ok();

    let langs = if id.languages.is_empty() {
        "en-US,en;q=0.9".to_string()
    } else {
        id.languages
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    l.clone()
                } else {
                    format!("{l};q=0.{}", 9 - i.min(8))
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    h.push(("Accept".into(), "*/*".into()));
    h.push(("Accept-Language".into(), langs));
    if let Some(p) = &page {
        // Browsers send only the origin as Referer to other sites.
        let referer = match &media {
            Some(m) if m.origin() != p.origin() => p.origin().ascii_serialization() + "/",
            _ => p.to_string(),
        };
        h.push(("Referer".into(), referer));
    }

    // Streams are loaded by script players (fetch or XHR), plain files by
    // a <video> or <audio> element. Browsers label the two differently.
    let streamed = matches!(req.kind, MediaKind::Hls | MediaKind::Dash);
    let is_audio = req
        .url
        .to_ascii_lowercase()
        .split(['?', '#'])
        .next()
        .is_some_and(|p| {
            [
                ".mp3", ".m4a", ".aac", ".ogg", ".oga", ".opus", ".flac", ".wav",
            ]
            .iter()
            .any(|e| p.ends_with(e))
        });
    if let (Some(p), Some(m)) = (&page, &media) {
        let site = fetch_site(p, m);
        if streamed {
            if site != "same-origin" {
                h.push(("Origin".into(), p.origin().ascii_serialization()));
            }
            h.push(("Sec-Fetch-Dest".into(), "empty".into()));
            h.push(("Sec-Fetch-Mode".into(), "cors".into()));
        } else {
            h.push((
                "Sec-Fetch-Dest".into(),
                if is_audio { "audio" } else { "video" }.into(),
            ));
            h.push(("Sec-Fetch-Mode".into(), "no-cors".into()));
        }
        h.push(("Sec-Fetch-Site".into(), site.into()));
    }

    // Chromium based engines (WebView2) also send these hints.
    if !id.brands.is_empty() {
        let list = id
            .brands
            .iter()
            .map(|(b, v)| format!("\"{b}\";v=\"{v}\""))
            .collect::<Vec<_>>()
            .join(", ");
        h.push(("sec-ch-ua".into(), list));
        h.push((
            "sec-ch-ua-mobile".into(),
            if id.mobile { "?1" } else { "?0" }.into(),
        ));
        if !id.platform.is_empty() {
            h.push(("sec-ch-ua-platform".into(), format!("\"{}\"", id.platform)));
        }
    }
    if !cookies.is_empty() {
        h.push(("Cookie".into(), cookies.to_string()));
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(url: &str, page: &str, kind: MediaKind) -> DownloadRequest {
        DownloadRequest {
            url: url.into(),
            kind,
            page: page.into(),
            identity: BrowserIdentity {
                user_agent: "UA".into(),
                languages: vec!["en-IE".into(), "en".into()],
                platform: "Windows".into(),
                mobile: false,
                brands: vec![("Microsoft Edge WebView2".into(), "140".into())],
            },
            tab_label: None,
            private: false,
            title: Some("Episode 7: The Gateway".into()),
            max_height: None,
            sent: Vec::new(),
            audio: None,
            tag: None,
            file_name: None,
        }
    }

    fn get<'a>(h: &'a [(String, String)], k: &str) -> Option<&'a str> {
        h.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn stream_from_another_site_looks_like_a_script_player() {
        let r = req(
            "https://cdn.video.test/s/master.m3u8",
            "https://www.site.test/watch/1",
            MediaKind::Hls,
        );
        let h = browser_headers(&r, "sid=1; pref=x");
        assert_eq!(get(&h, "Origin"), Some("https://www.site.test"));
        assert_eq!(get(&h, "Referer"), Some("https://www.site.test/"));
        assert_eq!(get(&h, "Sec-Fetch-Mode"), Some("cors"));
        assert_eq!(get(&h, "Sec-Fetch-Site"), Some("cross-site"));
        assert_eq!(get(&h, "Accept-Language"), Some("en-IE,en;q=0.8"));
        assert_eq!(
            get(&h, "sec-ch-ua"),
            Some("\"Microsoft Edge WebView2\";v=\"140\"")
        );
        assert_eq!(get(&h, "sec-ch-ua-platform"), Some("\"Windows\""));
        assert_eq!(get(&h, "Cookie"), Some("sid=1; pref=x"));
    }

    #[test]
    fn file_on_the_same_site_looks_like_a_video_element() {
        let r = req(
            "https://media.site.test/v/clip.mp4",
            "https://www.site.test/watch/1",
            MediaKind::Direct,
        );
        let h = browser_headers(&r, "");
        assert_eq!(get(&h, "Origin"), None);
        assert_eq!(get(&h, "Sec-Fetch-Dest"), Some("video"));
        assert_eq!(get(&h, "Sec-Fetch-Mode"), Some("no-cors"));
        assert_eq!(get(&h, "Sec-Fetch-Site"), Some("same-site"));
        assert_eq!(get(&h, "Cookie"), None);

        let same = req(
            "https://www.site.test/a.mp3",
            "https://www.site.test/watch/1",
            MediaKind::Direct,
        );
        let h = browser_headers(&same, "");
        assert_eq!(get(&h, "Sec-Fetch-Dest"), Some("audio"));
        assert_eq!(get(&h, "Sec-Fetch-Site"), Some("same-origin"));
        assert_eq!(get(&h, "Referer"), Some("https://www.site.test/watch/1"));
    }

    #[test]
    fn natural_and_fast_styles() {
        let r = req(
            "https://cdn.test/m.m3u8",
            "https://site.test/w",
            MediaKind::Hls,
        );
        let n = fetch_options(
            &r,
            "",
            "socks5h://127.0.0.1:1".into(),
            "/tmp".into(),
            How {
                style: DownloadStyle::Natural,
                circuits: 0,
            },
        );
        assert!(n.hls.natural);
        assert_eq!((n.hls.concurrency, n.direct.connections), (4, 4));
        let f = fetch_options(
            &r,
            "",
            "socks5h://127.0.0.1:1".into(),
            "/tmp".into(),
            How {
                style: DownloadStyle::Fast,
                circuits: 4,
            },
        );
        assert!(!f.hls.natural);
        assert_eq!(f.hls.concurrency, 8);
        assert_eq!(f.request.privacy.circuits, 4);
        assert_eq!(
            f.output.unwrap(),
            PathBuf::from("/tmp/Episode 7_ The Gateway.mp4")
        );

        // A quality picked in the Media panel goes to the engine and the name.
        let mut picked = req(
            "https://cdn.test/m.m3u8",
            "https://site.test/w",
            MediaKind::Hls,
        );
        picked.max_height = Some(720);
        let p = fetch_options(
            &picked,
            "",
            "socks5h://127.0.0.1:1".into(),
            "/tmp".into(),
            How {
                style: DownloadStyle::Fast,
                circuits: 0,
            },
        );
        assert_eq!(p.hls.max_height, Some(720));
        assert_eq!(
            p.output.unwrap(),
            PathBuf::from("/tmp/Episode 7_ The Gateway 720p.mp4")
        );
    }

    #[test]
    fn a_joined_download_is_named_after_the_page() {
        let mut r = req(
            "https://cdn.test/v",
            "https://site.test/w",
            MediaKind::Direct,
        );
        r.max_height = Some(1080);
        r.tag = Some("AV1".into());
        r.audio = Some(osprey_core::SeparateAudio {
            url: "https://cdn.test/a".into(),
            kind: MediaKind::Direct,
            ext: "mkv".into(),
        });
        let how = How {
            style: DownloadStyle::Natural,
            circuits: 0,
        };
        let opts = fetch_options(&r, "", "socks5h://127.0.0.1:1".into(), "out".into(), how);
        assert_eq!(
            opts.output.unwrap().file_name().unwrap().to_string_lossy(),
            "Episode 7_ The Gateway 1080p AV1.mkv"
        );
        assert!(opts.audio.is_some());
    }

    #[test]
    fn real_headers_replace_worked_out_ones() {
        let mut r = req("https://cdn.test/m", "https://site.test/w", MediaKind::Hls);
        r.identity.user_agent = "guess".into();
        r.sent = vec![
            ("Referer".into(), "https://player.test/".into()),
            ("User-Agent".into(), "real".into()),
            ("X-Token".into(), "abc".into()),
        ];
        let opts = player_request(&r, "", "socks5h://127.0.0.1:1".into());
        assert_eq!(opts.user_agent.as_deref(), Some("real"));
        assert_eq!(
            capture::header(&opts.headers, "Referer"),
            Some("https://player.test/"),
            "not the page the item was found on"
        );
        assert_eq!(capture::header(&opts.headers, "X-Token"), Some("abc"));
        assert_eq!(capture::header(&opts.headers, "User-Agent"), None);
        assert_eq!(capture::header(&opts.headers, "Sec-Fetch-Mode"), None);

        // No cookie seen on the request: the tab's cookies fill in.
        let opts = player_request(&r, "a=1", "socks5h://127.0.0.1:1".into());
        assert_eq!(capture::header(&opts.headers, "Cookie"), Some("a=1"));

        // Without captured headers nothing changes.
        r.sent.clear();
        let opts = player_request(&r, "", "socks5h://127.0.0.1:1".into());
        assert_eq!(opts.user_agent.as_deref(), Some("guess"));
        assert_eq!(
            capture::header(&opts.headers, "Referer"),
            Some("https://site.test/")
        );
    }

    #[test]
    fn names() {
        assert_eq!(
            name_for("https://x.test/a/clip.mp4?t=1", MediaKind::Direct),
            "clip.mp4"
        );
        assert_eq!(
            name_for("https://cdn.x.test/master.m3u8", MediaKind::Hls),
            "stream from cdn.x.test"
        );
    }
}
