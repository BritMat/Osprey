//! What the Media panel shows, and how it gets there.
//!
//! Candidates come from the web engine itself where it allows that (see
//! `capture.rs`), and from the sniffer in pages and frames. Each one is
//! then checked the way 1DM does it: BM Osprey asks the server, through the
//! tab's route and with the request headers the player really sent, what
//! is there. Only real media is listed, with its qualities, size and
//! length. Error pages, images, template links and tiny ad clips are
//! dropped.
//!
//! This module also decides about pop-ups: a new window opens as a tab
//! only when you clicked a link that opens one, or it stays on the same
//! site. Everything else is blocked, and the toolbar offers to open it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use osprey_core::inspect::{self, MediaInfo, Verdict};
use osprey_core::{classify, MediaKind, RequestKind};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Url};
use tokio::sync::Semaphore;

use osprey_core::ytdlp::{self, Extracted, Outcome};
use osprey_core::{tools, Privacy, RequestOptions, Route, SeparateAudio};

use crate::capture::{self, Judged, Seen};
use crate::downloads::{self, BrowserIdentity, DownloadRequest};
use crate::probe;
use crate::{emit_tabs, emit_ui, id_of, label_of, AppState};

/// Most candidates checked per tab per page, so a busy page cannot keep
/// BM Osprey busy.
const MAX_CHECKS_PER_PAGE: usize = 60;
const MAX_LISTED: usize = 60;
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
/// Addresses remembered per page as not media, so the sniffer does not
/// report them for a server check.
const MAX_NOT_MEDIA: usize = 2000;
/// A numbered address seen more often than this is pieces of a stream.
const MAX_SERIES: u32 = 3;
/// Pages per tab that can go to yt-dlp.
const MAX_ASKED: usize = 8;
/// How often to see if a tool check is due. `tools.rs` decides that.
const TOOLS_CHECK: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Checking,
    Ready,
    /// Real media that cannot be saved (DRM, DASH for now).
    Blocked,
}

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The web engine's own view of the request and response.
    Engine,
    /// The sniffer script in the page.
    Page,
    /// yt-dlp knows the page. The item holds all its formats.
    Extractor,
}

/// What the engine saw for an item, safe to show and to write to a probe
/// report. Cookie values and other secrets stay out of it.
#[derive(Debug, Clone, Serialize)]
pub struct CaptureInfo {
    pub status: u16,
    pub content_type: Option<String>,
    pub referer: Option<String>,
    pub origin: Option<String>,
    pub sent_cookies: bool,
    pub header_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaItem {
    pub url: String,
    pub kind: MediaKind,
    /// Page or frame the media was found in. Sent as the referrer.
    pub page: String,
    pub name: String,
    pub host: String,
    pub state: ItemState,
    pub info: Option<MediaInfo>,
    /// Picture size and length reported by the page's player, when known.
    pub player_w: Option<u32>,
    pub player_h: Option<u32>,
    pub player_dur: Option<f64>,
    pub source: Source,
    pub captured: Option<CaptureInfo>,
    /// The request headers the player really sent, used to fetch this
    /// item again. Can hold cookies, so it never leaves the app.
    #[serde(skip)]
    pub sent: Vec<(String, String)>,
    /// What yt-dlp listed, on an item with [`Source::Extractor`].
    pub extracted: Option<Box<Extracted>>,
}

/// Lengths closer than this are one video.
const SAME_LENGTH_SECS: f64 = 3.0;

/// Whether a captured item is a video yt-dlp already has a card for. Signed
/// addresses change with every request, so they cannot be compared. The
/// same site and the same length can, and that also keeps an ad apart.
fn same_video(item: &MediaItem, card: &Extracted) -> bool {
    if item.extracted.is_some() {
        return false;
    }
    let length = item
        .info
        .as_ref()
        .and_then(|i| i.duration)
        .or(item.player_dur);
    let (Some(a), Some(b)) = (length, card.duration) else {
        return false;
    };
    let same_site_as = |format: &ytdlp::Format| {
        http_url(&item.url)
            .zip(http_url(&format.url))
            .is_some_and(|(x, y)| same_site(&x, &y))
    };
    (a - b).abs() < SAME_LENGTH_SECS && card.formats.iter().any(same_site_as)
}

/// What to fetch for an item in the panel.
#[derive(Debug, Default, PartialEq)]
pub struct Target {
    pub url: String,
    pub kind: MediaKind,
    pub sent: Vec<(String, String)>,
    /// Sound to join, for a video that has none of its own.
    pub audio: Option<SeparateAudio>,
    pub height: Option<u32>,
    pub tag: Option<String>,
}

impl MediaItem {
    /// The address to fetch, for this item or one of its formats.
    pub fn target(&self, format: Option<&str>) -> Result<Target, String> {
        if let Some(why) = self.info.as_ref().and_then(|i| i.blocked.clone()) {
            return Err(why);
        }
        let Some(extracted) = &self.extracted else {
            return Ok(Target {
                url: self.url.clone(),
                kind: self.kind,
                sent: self.sent.clone(),
                ..Default::default()
            });
        };
        let format = extracted
            .formats
            .iter()
            .find(|f| Some(f.id.as_str()) == format)
            .ok_or("that format is no longer listed")?;
        if format.kind == MediaKind::Dash {
            return Err("DASH streams are not supported yet".to_string());
        }
        let audio = format
            .pairs_with
            .as_ref()
            .and_then(|id| extracted.formats.iter().find(|f| &f.id == id))
            .map(|a| SeparateAudio {
                url: a.url.clone(),
                kind: a.kind,
                ext: ytdlp::joined_ext(format, a).to_string(),
            });
        // Two choices at one height would save to the same name.
        let shared = extracted
            .formats
            .iter()
            .any(|f| f.id != format.id && f.height == format.height && !f.audio_only);
        Ok(Target {
            url: format.url.clone(),
            kind: format.kind,
            sent: capture::reusable(format.headers.clone()),
            audio,
            height: format.height,
            tag: shared.then(|| format.codec_name.clone()).flatten(),
        })
    }
}

#[derive(Default)]
pub struct TabMedia {
    pub items: Vec<MediaItem>,
    /// URLs already looked at on this page, kept or not.
    pub seen: HashSet<String>,
    pub checks: usize,
    /// URLs the engine showed are not media, with the reason.
    pub not_media: HashMap<String, String>,
    /// How often each numbered address came up.
    pub series: HashMap<String, u32>,
    /// Pages already handed to yt-dlp.
    pub asked: HashSet<String>,
    /// The page's preview picture (`og:image`), if it has one.
    pub thumb: Option<String>,
}

impl TabMedia {
    fn remember_not_media(&mut self, url: String, why: String) {
        if self.not_media.len() < MAX_NOT_MEDIA {
            self.not_media.insert(url, why);
        }
    }
}

pub struct MediaState {
    pub tabs: std::sync::Mutex<HashMap<u32, TabMedia>>,
    /// Links you clicked that open a new tab: (url, when).
    pub opened: std::sync::Mutex<Vec<(String, Instant)>>,
    pub slots: Arc<Semaphore>,
    /// yt-dlp runs at a time. Each is a whole program, so memory matters.
    pub extractors: Arc<Semaphore>,
}

impl MediaState {
    pub fn new() -> Self {
        Self {
            tabs: Default::default(),
            opened: Default::default(),
            // Three checks at a time across all tabs keeps it light.
            slots: Arc::new(Semaphore::new(3)),
            extractors: Arc::new(Semaphore::new(2)),
        }
    }

    /// The page a preview picture belongs to, when `url` is one this tab
    /// really has: its `og:image` or a yt-dlp thumbnail. Anything else is
    /// refused, so the interface cannot be used to fetch other addresses.
    pub fn thumb_page(&self, id: u32, url: &str) -> Option<String> {
        let tabs = self.tabs.lock().expect("media");
        let t = tabs.get(&id)?;
        let first_page = || t.items.first().map(|m| m.page.clone());
        if t.thumb.as_deref() == Some(url) {
            return first_page();
        }
        t.items
            .iter()
            .find(|m| {
                m.extracted
                    .as_ref()
                    .is_some_and(|x| x.thumbnail.as_deref() == Some(url))
            })
            .map(|m| m.page.clone())
    }

    pub fn clear_tab(&self, id: u32) {
        self.tabs.lock().expect("media").remove(&id);
    }

    /// Ready and blocked items, without stream variants already listed as
    /// qualities of a master playlist.
    pub fn visible(&self, id: u32) -> Vec<MediaItem> {
        let tabs = self.tabs.lock().expect("media");
        let Some(t) = tabs.get(&id) else {
            return Vec::new();
        };
        let variants: HashSet<&str> = t
            .items
            .iter()
            .filter_map(|m| m.info.as_ref())
            .flat_map(|i| i.qualities.iter().map(|q| q.url.as_str()))
            .collect();
        let cards: Vec<&Extracted> = t
            .items
            .iter()
            .filter_map(|m| m.extracted.as_deref())
            .collect();
        let mut out: Vec<MediaItem> = t
            .items
            .iter()
            .filter(|m| !variants.contains(m.url.as_str()))
            .filter(|m| !cards.iter().any(|c| same_video(m, c)))
            .cloned()
            .collect();
        // Streams first, then the biggest files: usually what is playing.
        out.sort_by_key(|m| {
            let stream = matches!(m.kind, MediaKind::Hls | MediaKind::Dash);
            let size = m.info.as_ref().and_then(|i| i.size).unwrap_or(0);
            (
                m.extracted.is_none(),
                m.state == ItemState::Checking,
                !stream,
                std::cmp::Reverse(size),
            )
        });
        out
    }
}

#[derive(Serialize, Clone)]
struct MediaView {
    tab: u32,
    items: Vec<MediaItem>,
    /// The page's preview picture, for cards that have none of their own.
    thumb: Option<String>,
}

pub fn emit_media(app: &AppHandle, id: u32) {
    let st = app.state::<AppState>();
    let items = st.media.visible(id);
    let ready = items.iter().filter(|m| m.state == ItemState::Ready).count();
    if let Some(t) = st.tabs.lock().expect("tabs").map.get_mut(&id) {
        t.media = ready;
    }
    let thumb = st
        .media
        .tabs
        .lock()
        .expect("media")
        .get(&id)
        .and_then(|t| t.thumb.clone());
    emit_ui(
        app,
        "media",
        MediaView {
            tab: id,
            items,
            thumb,
        },
    );
}

/// A candidate on its way into the panel.
struct Candidate {
    url: Url,
    page: String,
    kind: Option<MediaKind>,
    size: (Option<u32>, Option<u32>, Option<f64>),
    source: Source,
    captured: Option<CaptureInfo>,
    sent: Vec<(String, String)>,
}

/// Adds a candidate to a tab. True when it needs a check with the server.
fn admit(tab: &mut TabMedia, c: Candidate, ads: Option<&osprey_core::Filters>) -> bool {
    let url = c.url.to_string();
    let (w, h, dur) = c.size;
    // A known item may get its player size or real headers later.
    if let Some(existing) = tab.items.iter_mut().find(|m| m.url == url) {
        if existing.player_h.is_none() && h.is_some() {
            (existing.player_w, existing.player_h, existing.player_dur) = (w, h, dur);
        }
        if existing.sent.is_empty() && !c.sent.is_empty() {
            existing.sent = c.sent;
            existing.captured = c.captured;
            existing.page = c.page;
        }
        return false;
    }
    if !tab.seen.insert(url.clone()) {
        return false;
    }
    if c.source == Source::Page {
        probe::note(&url, &c.page, "reported", None);
    }
    if tab.checks >= MAX_CHECKS_PER_PAGE || tab.items.len() >= MAX_LISTED {
        probe::note(&url, &c.page, "over_limit", None);
        return false;
    }
    if ads.is_some_and(|f| f.is_blocked(&url, &c.page, RequestKind::Media)) {
        probe::note(&url, &c.page, "ad_filtered", None);
        return false;
    }
    tab.checks += 1;
    let name = c
        .url
        .path_segments()
        .and_then(|mut s| s.next_back().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| c.url.host_str().unwrap_or("media").to_string());
    tab.items.push(MediaItem {
        host: c.url.host_str().unwrap_or("").to_string(),
        kind: c
            .kind
            .or_else(|| classify(&url, None))
            .filter(|k| *k != MediaKind::Page)
            .unwrap_or(MediaKind::Direct),
        url,
        page: c.page,
        name,
        state: ItemState::Checking,
        info: None,
        player_w: w,
        player_h: h,
        player_dur: dur,
        source: c.source,
        captured: c.captured,
        sent: c.sent,
        extracted: None,
    });
    true
}

/// Asks yt-dlp about the pages a candidate came from, once each. Only
/// pages with media get here, so browsing does not start a program for
/// every page.
fn ask_extractor(app: &AppHandle, id: u32, pages: [String; 2]) {
    for page in pages {
        let new = {
            let st = app.state::<AppState>();
            let mut tabs = st.media.tabs.lock().expect("media");
            let tab = tabs.entry(id).or_default();
            page.starts_with("http")
                && tab.asked.len() < MAX_ASKED
                && tab.asked.insert(page.clone())
        };
        if new {
            let app = app.clone();
            tauri::async_runtime::spawn(async move { extract_page(app, id, page).await });
        }
    }
}

async fn extract_page(app: AppHandle, id: u32, page: String) {
    let st = app.state::<AppState>();
    let _slot = st.media.extractors.clone().acquire_owned().await;
    let user_agent = st
        .identity
        .lock()
        .expect("identity")
        .get(&id)
        .map(|i| i.user_agent.clone())
        .filter(|u| !u.is_empty());
    let opts = ytdlp::ExtractOptions {
        proxy: Some(st.gateway.proxy_url()),
        user_agent,
        referer: None,
        deno: tools::deno_path(&st.home),
    };
    let found = match ytdlp::extract(&ytdlp::binary(&st.home), &page, &opts).await {
        Ok(Outcome::Found(found)) => found,
        Ok(Outcome::NotKnown(why)) => {
            probe::note(&page, &page, "not_extracted", Some(why));
            return;
        }
        Err(e) => {
            probe::note(&page, &page, "not_extracted", Some(e.to_string()));
            return;
        }
    };
    probe::note(
        &page,
        &page,
        "extracted",
        Some(format!(
            "{}, {} formats",
            found.extractor,
            found.formats.len()
        )),
    );
    {
        let mut tabs = st.media.tabs.lock().expect("media");
        let Some(tab) = tabs.get_mut(&id) else { return };
        // The tab may have moved on while yt-dlp was working.
        if !tab.asked.contains(&page) {
            return;
        }
        tab.items.push(extractor_item(page, *found));
    }
    emit_media(&app, id);
    emit_tabs(&app);
}

fn extractor_item(page: String, found: Extracted) -> MediaItem {
    MediaItem {
        host: http_url(&page)
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default(),
        name: found.title.clone().unwrap_or_else(|| page.clone()),
        url: page.clone(),
        page,
        kind: MediaKind::Page,
        state: ItemState::Ready,
        info: None,
        player_w: None,
        player_h: None,
        player_dur: found.duration,
        source: Source::Extractor,
        captured: None,
        sent: Vec::new(),
        extracted: Some(Box::new(found)),
    }
}

/// Fetches yt-dlp and Deno on first run, then keeps them current. Goes
/// through the gateway like everything else, so it follows the chosen
/// route. Without them, the panel still works from BM Osprey's own capture.
pub async fn keep_tools(app: AppHandle) {
    if probe::active() {
        return;
    }
    loop {
        let st = app.state::<AppState>();
        let opts = RequestOptions {
            privacy: Privacy {
                route: Route::Proxy(st.gateway.proxy_url()),
                ..Default::default()
            },
            ..Default::default()
        };
        match osprey_core::build_client(&opts) {
            Ok(client) => {
                for r in tools::refresh(&st.home, &client, false).await {
                    if let Err(why) = r.result {
                        eprintln!("could not get {}: {why}", r.tool);
                    }
                }
            }
            Err(e) => eprintln!("could not get the tools: {e}"),
        }
        tokio::time::sleep(TOOLS_CHECK).await;
    }
}

/// Called with what the web engine saw. Runs off the window thread.
pub fn captured(app: &AppHandle, id: u32, seen: Seen, judged: Judged, sent: Vec<(String, String)>) {
    let st = app.state::<AppState>();
    let tab_page = match st.tabs.lock().expect("tabs").map.get(&id) {
        Some(t) => t.url.clone(),
        None => return,
    };
    let Some(url) = http_url(&seen.url) else {
        return;
    };
    // The same spelling the panel uses.
    let seen = Seen {
        url: url.to_string(),
        ..seen
    };
    let referer = capture::header(&sent, "Referer").map(str::to_string);
    let page = referer.clone().unwrap_or_else(|| tab_page.clone());

    let block_ads = st.settings.lock().expect("settings").block_ads;
    let filters = st.filters.read().expect("filters").clone();
    let check = {
        let mut tabs = st.media.tabs.lock().expect("media");
        let tab = tabs.entry(id).or_default();
        let kind = match judged {
            Judged::Media(kind) => kind,
            Judged::NotMedia(why) => {
                tab.remember_not_media(seen.url, why);
                return;
            }
            Judged::Piece | Judged::Unclear => return,
        };
        // It may have failed or looked wrong before. Now it is media.
        tab.not_media.remove(&seen.url);
        let new = !tab.seen.contains(&seen.url);
        if new {
            if let Some(why) = not_worth_a_check(tab, &seen, kind) {
                probe::note(&seen.url, &page, "dropped", Some(why.clone()));
                tab.seen.insert(seen.url.clone());
                tab.remember_not_media(seen.url, why);
                return;
            }
            probe::note(
                &seen.url,
                &page,
                "captured",
                Some(format!(
                    "{} {}",
                    seen.status,
                    seen.content_type.as_deref().unwrap_or("no content type")
                )),
            );
        }
        let captured = CaptureInfo {
            status: seen.status,
            content_type: seen.content_type.clone(),
            referer,
            origin: capture::header(&sent, "Origin").map(str::to_string),
            sent_cookies: capture::header(&sent, "Cookie").is_some(),
            header_names: sent.iter().map(|(k, _)| k.clone()).collect(),
        };
        let admitted = admit(
            tab,
            Candidate {
                url,
                page: page.clone(),
                kind: Some(kind),
                size: (None, None, None),
                source: Source::Engine,
                captured: Some(captured),
                sent: sent.clone(),
            },
            block_ads.then_some(&*filters),
        );
        admitted.then_some((seen.url, page, sent))
    };
    if let Some((url, page, sent)) = check {
        ask_extractor(app, id, [page.clone(), tab_page]);
        spawn_check(app.clone(), id, url, page, sent);
        emit_media(app, id);
        emit_tabs(app);
    }
}

/// Tiny files and numbered series can be told from the response alone.
fn not_worth_a_check(tab: &mut TabMedia, seen: &Seen, kind: MediaKind) -> Option<String> {
    if kind != MediaKind::Direct {
        return None;
    }
    let audio = seen
        .content_type
        .as_deref()
        .is_some_and(|t| t.to_ascii_lowercase().starts_with("audio/"));
    if let Some(size) = capture::total_size(seen).filter(|s| inspect::too_small(*s, audio)) {
        return Some(format!("only {size} bytes, likely a preview or ad"));
    }
    let key = capture::series_key(&seen.url);
    let count = tab.series.entry(key.clone()).or_insert(0);
    *count += 1;
    if *count <= MAX_SERIES {
        return None;
    }
    // The first few looked like files. Now it is clear they are not.
    tab.items.retain(|m| {
        m.source != Source::Engine
            || m.kind != MediaKind::Direct
            || capture::series_key(&m.url) != key
    });
    Some("a numbered series, likely pieces of a stream".to_string())
}

#[derive(Deserialize)]
pub struct Brand {
    brand: String,
    version: String,
}

#[derive(Deserialize)]
pub struct ReportItem {
    url: String,
    /// The frame it was found in, when not the top page.
    #[serde(default)]
    frame: Option<String>,
    #[serde(default)]
    w: Option<f64>,
    #[serde(default)]
    h: Option<f64>,
    #[serde(default)]
    dur: Option<f64>,
}

#[derive(Deserialize)]
pub struct MediaReport {
    page: String,
    ua: String,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    mobile: bool,
    #[serde(default)]
    brands: Vec<Brand>,
    #[serde(default)]
    items: Vec<ReportItem>,
    /// Links clicked that open a new tab.
    #[serde(default)]
    opened: Vec<String>,
    /// The page's `og:image`.
    #[serde(default)]
    thumb: Option<String>,
}

fn http_url(s: &str) -> Option<Url> {
    if s.len() > 4096 {
        return None;
    }
    Url::parse(s)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
}

fn whole(v: Option<f64>) -> Option<u32> {
    v.filter(|x| x.is_finite() && *x >= 1.0 && *x < 100_000.0)
        .map(|x| x as u32)
}

/// Called by the sniffer in the top page of a tab. Everything a page
/// sends is untrusted: sizes are capped, only http(s) is kept, ad filters
/// apply, and the number of checks per page is limited.
#[tauri::command]
pub fn media_found(app: AppHandle, webview: tauri::Webview, report: MediaReport) {
    let Some(id) = id_of(webview.label()) else {
        return;
    };
    let st = app.state::<AppState>();
    if report.items.len() > 50
        || report.opened.len() > 10
        || report.page.len() > 4096
        || report.ua.len() > 512
    {
        return;
    }
    st.identity.lock().expect("identity").insert(
        id,
        BrowserIdentity {
            user_agent: report.ua.clone(),
            languages: report
                .languages
                .iter()
                .take(4)
                .map(|l| l.chars().take(20).collect())
                .collect(),
            platform: report.platform.chars().take(30).collect(),
            mobile: report.mobile,
            brands: report
                .brands
                .iter()
                .take(5)
                .map(|b| {
                    (
                        b.brand.chars().take(60).collect(),
                        b.version.chars().take(20).collect(),
                    )
                })
                .collect(),
        },
    );

    {
        let mut opened = st.media.opened.lock().expect("opened");
        opened.retain(|(_, t)| t.elapsed() < Duration::from_secs(10));
        for o in report.opened.iter().filter_map(|o| http_url(o)) {
            opened.push((o.to_string(), Instant::now()));
        }
    }

    let block_ads = st.settings.lock().expect("settings").block_ads;
    let filters = st.filters.read().expect("filters").clone();
    let mut to_check = Vec::new();
    {
        let mut tabs = st.media.tabs.lock().expect("media");
        let tab = tabs.entry(id).or_default();
        if let Some(t) = report.thumb.as_deref().and_then(http_url) {
            tab.thumb = Some(t.to_string());
        }
        for item in report.items {
            let Some(u) = http_url(&item.url) else {
                continue;
            };
            let url = u.to_string();
            let page = item
                .frame
                .as_deref()
                .and_then(http_url)
                .map(|f| f.to_string())
                .unwrap_or_else(|| report.page.clone());
            // The engine already saw the real answer, no need to ask again.
            if let Some(why) = tab.not_media.get(&url).cloned() {
                if tab.seen.insert(url.clone()) {
                    probe::note(&url, &page, "reported", None);
                    probe::note(&url, &page, "dropped", Some(why));
                }
                continue;
            }
            let size = (
                whole(item.w),
                whole(item.h),
                item.dur.filter(|d| d.is_finite() && *d > 0.0),
            );
            let admitted = admit(
                tab,
                Candidate {
                    url: u,
                    page: page.clone(),
                    kind: None,
                    size,
                    source: Source::Page,
                    captured: None,
                    sent: Vec::new(),
                },
                block_ads.then_some(&*filters),
            );
            if admitted {
                to_check.push((url, page));
            }
        }
    }
    for (url, page) in to_check {
        ask_extractor(&app, id, [page.clone(), report.page.clone()]);
        spawn_check(app.clone(), id, url, page, Vec::new());
    }
    emit_media(&app, id);
    emit_tabs(&app);
}

/// Checks one candidate in the background and updates the panel.
fn spawn_check(app: AppHandle, tab: u32, url: String, page: String, sent: Vec<(String, String)>) {
    tauri::async_runtime::spawn(async move {
        let st = app.state::<AppState>();
        let _slot = st.media.slots.clone().acquire_owned().await;
        let identity = st
            .identity
            .lock()
            .expect("identity")
            .get(&tab)
            .cloned()
            .unwrap_or_default();
        let req = DownloadRequest {
            url: url.clone(),
            kind: MediaKind::Direct,
            page: page.clone(),
            identity,
            tab_label: Some(label_of(tab)),
            private: false,
            title: None,
            max_height: None,
            sent,
            audio: None,
            tag: None,
            file_name: None,
        };
        let cookies = downloads::cookies_for(&app, &req).await;
        let opts = downloads::player_request(&req, &cookies, st.gateway.proxy_url());
        let verdict = match osprey_core::build_client(&opts) {
            Ok(client) => {
                match tokio::time::timeout(CHECK_TIMEOUT, inspect::inspect(&client, &url)).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => Verdict::Skip(e.to_string()),
                    Err(_) => Verdict::Skip("the server did not answer in time".into()),
                }
            }
            Err(e) => Verdict::Skip(e.to_string()),
        };
        {
            let mut tabs = st.media.tabs.lock().expect("media");
            let Some(t) = tabs.get_mut(&tab) else { return };
            match verdict {
                Verdict::Skip(why) => {
                    probe::note(&url, &page, "dropped", Some(why));
                    t.items.retain(|m| m.url != url)
                }
                Verdict::Media(info) => {
                    probe::note(
                        &url,
                        &page,
                        "kept",
                        Some(format!(
                            "{:?}{}",
                            info.kind,
                            info.blocked
                                .as_deref()
                                .map(|b| format!(", cannot save: {b}"))
                                .unwrap_or_default()
                        )),
                    );
                    if let Some(m) = t.items.iter_mut().find(|m| m.url == url) {
                        m.kind = info.kind;
                        m.state = if info.blocked.is_some() {
                            ItemState::Blocked
                        } else {
                            ItemState::Ready
                        };
                        m.info = Some(*info);
                    }
                }
            }
        }
        emit_media(&app, tab);
        emit_tabs(&app);
    });
}

/// Rough "same site": the last two parts of the host name.
pub fn same_site(a: &Url, b: &Url) -> bool {
    let site = |u: &Url| {
        let h = u
            .host_str()
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_ascii_lowercase();
        let parts: Vec<&str> = h.rsplit('.').take(2).collect();
        parts.into_iter().rev().collect::<Vec<_>>().join(".")
    };
    !site(a).is_empty() && site(a) == site(b)
}

/// Whether a pop-up from tab page `page` to `target` should open. Waits a
/// moment first, so the click report from the page can arrive.
pub async fn popup_allowed(app: &AppHandle, page: &str, target: &Url) -> bool {
    tokio::time::sleep(Duration::from_millis(250)).await;
    let st = app.state::<AppState>();
    if st.settings.lock().expect("settings").block_ads
        && st.filters.read().expect("filters").is_blocked(
            target.as_str(),
            page,
            RequestKind::Document,
        )
    {
        return false;
    }
    let clicked = st
        .media
        .opened
        .lock()
        .expect("opened")
        .iter()
        .any(|(o, t)| {
            t.elapsed() < Duration::from_secs(5)
                && Url::parse(o).is_ok_and(|o| {
                    o.as_str() == target.as_str()
                        || (o.host_str() == target.host_str() && o.path() == target.path())
                })
        });
    if clicked {
        return true;
    }
    Url::parse(page).is_ok_and(|p| same_site(&p, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn engine_item(url: &str, sent: Vec<(String, String)>) -> Candidate {
        Candidate {
            url: u(url),
            page: "https://player.test/".into(),
            kind: Some(MediaKind::Hls),
            size: (None, None, None),
            source: Source::Engine,
            captured: None,
            sent,
        }
    }

    fn page_item(url: &str, h: Option<u32>) -> Candidate {
        Candidate {
            url: u(url),
            page: "https://site.test/watch".into(),
            kind: None,
            size: (Some(1280), h, Some(60.0)),
            source: Source::Page,
            captured: None,
            sent: Vec::new(),
        }
    }

    #[test]
    fn both_sources_make_one_item() {
        let url = "https://cdn.test/get?id=1";
        let sent = vec![("Cookie".to_string(), "a=1".to_string())];
        let mut tab = TabMedia::default();
        assert!(admit(&mut tab, engine_item(url, sent.clone()), None));
        assert!(!admit(&mut tab, page_item(url, Some(720)), None));
        assert_eq!(tab.items.len(), 1);
        let item = &tab.items[0];
        assert_eq!(item.source, Source::Engine);
        assert_eq!(item.kind, MediaKind::Hls, "the real type is kept");
        assert_eq!(item.player_h, Some(720), "the player still adds its size");
        assert_eq!(item.sent, sent);

        // The other order: the real headers replace the guessed page.
        let mut tab = TabMedia::default();
        assert!(admit(&mut tab, page_item(url, None), None));
        assert!(!admit(&mut tab, engine_item(url, sent.clone()), None));
        assert_eq!(tab.items[0].sent, sent);
        assert_eq!(tab.items[0].page, "https://player.test/");
    }

    #[test]
    fn addresses_are_compared_in_one_spelling() {
        let mut tab = TabMedia::default();
        assert!(admit(
            &mut tab,
            page_item("HTTPS://CDN.test/a b.mp4", None),
            None
        ));
        let spelled = http_url("https://cdn.test/a%20b.mp4").unwrap().to_string();
        assert_eq!(tab.items[0].url, spelled);
    }

    #[test]
    fn real_headers_are_never_serialised() {
        let mut tab = TabMedia::default();
        let sent = vec![("Cookie".to_string(), "secret=1".to_string())];
        admit(&mut tab, engine_item("https://cdn.test/m", sent), None);
        let json = serde_json::to_string(&tab.items[0]).unwrap();
        assert!(!json.contains("secret"), "{json}");
    }

    #[test]
    fn numbered_series_stops_being_checked() {
        let mut tab = TabMedia::default();
        let seen = |i: u32| Seen {
            url: format!("https://cdn.test/v/{i}.mp4"),
            method: "GET".into(),
            status: 200,
            content_type: Some("video/mp4".into()),
            ..Default::default()
        };
        for i in 0..MAX_SERIES {
            assert_eq!(
                not_worth_a_check(&mut tab, &seen(i), MediaKind::Direct),
                None
            );
        }
        admit(
            &mut tab,
            Candidate {
                kind: Some(MediaKind::Direct),
                ..engine_item("https://cdn.test/v/0.mp4", Vec::new())
            },
            None,
        );
        assert!(not_worth_a_check(&mut tab, &seen(9), MediaKind::Direct).is_some());
        assert!(tab.items.is_empty(), "earlier pieces leave the panel");
        // A playlist at the same address as its pieces stays.
        let mut tab = TabMedia::default();
        admit(
            &mut tab,
            engine_item("https://cdn.test/v/0.mp4", Vec::new()),
            None,
        );
        for i in 0..=MAX_SERIES {
            not_worth_a_check(&mut tab, &seen(i), MediaKind::Direct);
        }
        assert_eq!(tab.items.len(), 1);
        // Streams never count as a series.
        assert_eq!(not_worth_a_check(&mut tab, &seen(9), MediaKind::Hls), None);

        let mut tiny = seen(1);
        tiny.content_length = Some(1000);
        assert!(not_worth_a_check(&mut TabMedia::default(), &tiny, MediaKind::Direct).is_some());
    }

    fn extracted_item() -> MediaItem {
        let format = |id: &str, kind| ytdlp::Format {
            id: id.into(),
            url: format!("https://cdn.test/{id}"),
            kind,
            ext: None,
            width: None,
            height: None,
            fps: None,
            video_codec: None,
            audio_codec: None,
            audio_only: false,
            video_only: false,
            codec_name: None,
            language: None,
            note: None,
            pairs_with: None,
            bitrate: None,
            size: None,
            size_is_estimate: false,
            headers: vec![
                ("Referer".into(), "https://site.test/".into()),
                ("Host".into(), "cdn.test".into()),
            ],
        };
        extractor_item(
            "https://site.test/watch".into(),
            Extracted {
                extractor: "site".into(),
                title: Some("Episode".into()),
                duration: Some(60.0),
                thumbnail: None,
                live: false,
                formats: vec![format("h", MediaKind::Hls), format("d", MediaKind::Dash)],
                subtitles: Vec::new(),
                drm_formats: 0,
            },
        )
    }

    #[test]
    fn a_format_is_fetched_with_its_own_headers() {
        let item = extracted_item();
        assert_eq!(item.name, "Episode");
        let t = item.target(Some("h")).unwrap();
        assert_eq!(t.url, "https://cdn.test/h");
        assert_eq!(t.kind, MediaKind::Hls);
        assert_eq!(
            t.sent,
            [("Referer".to_string(), "https://site.test/".to_string())],
            "Host is about one connection"
        );
        assert_eq!(t.audio, None);
        assert!(item.target(Some("gone")).is_err());
        assert!(item.target(None).is_err(), "the page itself is not media");
        assert!(item.target(Some("d")).is_err(), "no DASH until item 3");
    }

    fn split_card() -> MediaItem {
        let mut item = extracted_item();
        let x = item.extracted.as_mut().unwrap();
        x.formats[0].height = Some(1080);
        x.formats[0].video_only = true;
        x.formats[0].codec_name = Some("H.264".into());
        x.formats[0].pairs_with = Some("a".into());
        x.formats[1].kind = MediaKind::Hls;
        x.formats[1].height = Some(1080);
        x.formats[1].id = "other".into();
        let mut audio = x.formats[0].clone();
        audio.id = "a".into();
        audio.url = "https://cdn.test/a".into();
        audio.height = None;
        audio.audio_only = true;
        audio.video_only = false;
        audio.pairs_with = None;
        x.formats.push(audio);
        item
    }

    #[test]
    fn a_video_only_format_brings_its_audio() {
        let item = split_card();
        let t = item.target(Some("h")).unwrap();
        let audio = t.audio.expect("joined");
        assert_eq!(audio.url, "https://cdn.test/a");
        assert_eq!(audio.ext, "mkv", "no container known in this test");
        assert_eq!(t.height, Some(1080));
        assert_eq!(t.tag.as_deref(), Some("H.264"), "another format is 1080p");
        let alone = item.target(Some("a")).unwrap();
        assert_eq!(alone.audio, None, "audio only is its own choice");
        assert_eq!(alone.tag, None);
    }

    #[test]
    fn a_captured_stream_joins_the_card_of_its_video() {
        let state = MediaState::new();
        let mut tab = TabMedia::default();
        let long = "https://vod.cdn.test/sec(abc)/manifest.m3u8";
        let ad = "https://ads.test/spot.m3u8";
        let other = "https://vod.cdn.test/sec(xyz)/manifest.m3u8";
        for (url, dur) in [(long, 61.5), (ad, 61.0), (other, 300.0)] {
            admit(&mut tab, engine_item(url, Vec::new()), None);
            tab.items.last_mut().unwrap().player_dur = Some(dur);
        }
        tab.items.push(extracted_item());
        state.tabs.lock().unwrap().insert(1, tab);
        let listed = state.visible(1);
        let urls: Vec<&str> = listed.iter().map(|m| m.url.as_str()).collect();
        assert!(!urls.contains(&long), "same site, same length: {urls:?}");
        assert!(urls.contains(&ad), "same length, another site");
        assert!(urls.contains(&other), "same site, another length");
        assert_eq!(listed.len(), 3);
    }

    #[test]
    fn known_pages_are_listed_first() {
        let state = MediaState::new();
        let mut tab = TabMedia::default();
        admit(
            &mut tab,
            engine_item("https://cdn.test/m", Vec::new()),
            None,
        );
        tab.items.push(extracted_item());
        state.tabs.lock().unwrap().insert(1, tab);
        let listed = state.visible(1);
        assert_eq!(listed[0].source, Source::Extractor);
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn same_site_rules() {
        assert!(same_site(
            &u("https://www.site.test/a"),
            &u("https://cdn.site.test/b")
        ));
        assert!(!same_site(
            &u("https://site.test/"),
            &u("https://ads.example/x")
        ));
        assert!(!same_site(&u("about:blank"), &u("about:blank")));
    }

    #[test]
    fn numbers_from_pages_are_sanitised() {
        assert_eq!(whole(Some(1080.0)), Some(1080));
        assert_eq!(whole(Some(f64::NAN)), None);
        assert_eq!(whole(Some(0.0)), None);
        assert!(http_url("javascript:alert(1)").is_none());
        assert!(http_url(&format!("https://x.test/{}", "a".repeat(5000))).is_none());
    }
}
