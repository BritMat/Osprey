//! `--probe-page URL`: a test run for one page, made so a person or a
//! local Claude Code session can check a real site without screenshots.
//!
//! The browser opens the page, mutes and starts every video it can find
//! (in all frames, clicking the usual play buttons), waits, then writes a
//! JSON report and exits. The report lists what the Media panel shows and
//! every candidate, with what happened to it: checked and kept, dropped
//! with the reason, filtered as an ad, or over a limit. It also lists every
//! response the web engine showed, and anything the sniffer found that the
//! engine did not show, which is how to tell if native capture misses
//! something (frames, workers).
//!
//! Options:
//!   --probe-page URL     the page to test
//!   --probe-wait SECS    how long to watch it (default 30, 5 to 300)
//!   --probe-out FILE     where to write the report
//!                        (default: <BM Osprey home>/probes/probe-<time>.json)
//!   --probe-format P     after the wait, preview and download the format of
//!                        this height from a yt-dlp card, the same way the
//!                        panel's buttons do, into the report's folder
//!   --probe-script-dir D install every .user.js in this folder first, and
//!                        save downloads next to the report
//!   --probe-answers A,B  answers to press for script questions, in order:
//!                        deny, once or always. Others stay unanswered
//!   --probe-panel FILE   after the wait, work the Scripts panel with its
//!                        real buttons: paste FILE in, install, switch it
//!                        off and on, reload the tab, press every script
//!                        menu command and resolve --probe-link
//!   --probe-link URL     the link for that last step
//!   --probe-extra-match P also run every script on this @match pattern, for
//!                        a test page standing in for a site. The script's
//!                        own text is not changed

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::capture::{Judged, Seen};
use crate::downloads::JobState;
use crate::AppState;

pub struct ProbeCfg {
    pub url: String,
    pub wait: u64,
    pub out: Option<PathBuf>,
    pub format_height: Option<u32>,
    pub script_dir: Option<PathBuf>,
    pub answers: Vec<String>,
    pub panel: Option<PathBuf>,
    pub link: Option<String>,
    pub extra_match: Option<String>,
}

static CFG: OnceLock<Option<ProbeCfg>> = OnceLock::new();
static LOG: OnceLock<Log> = OnceLock::new();

/// The probe settings, when this run is a probe.
pub fn config() -> Option<&'static ProbeCfg> {
    CFG.get_or_init(|| parse(std::env::args().skip(1).collect()))
        .as_ref()
}

pub fn active() -> bool {
    config().is_some()
}

fn parse(args: Vec<String>) -> Option<ProbeCfg> {
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let url = value("--probe-page")?;
    let url = if url.contains("://") {
        url
    } else {
        format!("https://{url}")
    };
    let wait = value("--probe-wait")
        .and_then(|w| w.parse().ok())
        .unwrap_or(30)
        .clamp(5, 300);
    Some(ProbeCfg {
        url,
        wait,
        out: value("--probe-out").map(PathBuf::from),
        format_height: value("--probe-format").and_then(|h| h.parse().ok()),
        script_dir: value("--probe-script-dir").map(PathBuf::from),
        answers: value("--probe-answers")
            .map(|a| a.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        panel: value("--probe-panel").map(PathBuf::from),
        link: value("--probe-link"),
        extra_match: value("--probe-extra-match"),
    })
}

/// A format test saves next to the report, not in the Downloads folder.
pub fn download_dir() -> Option<PathBuf> {
    let cfg = config()?;
    if cfg.format_height.is_none() && cfg.script_dir.is_none() {
        return None;
    }
    cfg.out
        .as_ref()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
}

/// What the buttons on a yt-dlp card really did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FormatTest {
    pub format: Option<String>,
    pub joined_with: Option<String>,
    /// What the player reported after playing for a while.
    pub preview: Option<String>,
    pub state: Option<String>,
    pub path: Option<String>,
    pub note: Option<String>,
    pub error: Option<String>,
}

static PREVIEW: Mutex<Option<String>> = Mutex::new(None);
static INSTALLS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static ANSWERED: AtomicUsize = AtomicUsize::new(0);

/// Installs the scripts of `--probe-script-dir`, the way the panel does.
pub async fn install_scripts(app: &AppHandle) {
    let Some(dir) = config().and_then(|c| c.script_dir.as_ref()) else {
        return;
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".user.js"))
        .collect();
    files.sort();
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap_or_default();
        let result = crate::scripts::install(app, Some(source), None).await;
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
        INSTALLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(json!({ "file": name, "result": format!("{result:?}") }));
    }
}

/// Presses the next answer from `--probe-answers` on a question's own button.
pub fn answer_ask(app: &AppHandle, ask: u32) {
    let Some(cfg) = config() else { return };
    let next = ANSWERED.fetch_add(1, Ordering::Relaxed);
    let Some(answer) = cfg.answers.get(next).cloned() else {
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(ui) = app.get_webview("ui") {
            let _ = ui.eval(format!(
                "if (state.panel !== 'scripts') togglePanel('scripts');\n\
                 setTimeout(() => document.querySelector('[data-ask=\"{ask}\"][data-answer=\"{answer}\"]').click(), 200);"
            ));
        }
    });
}

/// Works the Scripts panel with real clicks and reports what it showed.
fn panel_script(source: &str, link: Option<&str>) -> String {
    let source = serde_json::to_string(source).unwrap_or_default();
    let link = serde_json::to_string(&link).unwrap_or_default();
    format!(
        r#"(async () => {{
  const out = {{}};
  const pause = (ms) => new Promise((done) => setTimeout(done, ms));
  const names = () => [...document.querySelectorAll('[data-script] .script-name')].map((n) => n.textContent);
  try {{
    if (state.panel !== 'scripts') togglePanel('scripts');
    await pause(300);
    out.before = names();
    $('script-paste').value = {source};
    $('script-paste-go').click();
    await pause(1500);
    out.after_install = names();
    out.stale_note = !$('scripts-stale').hidden;
    const row = [...document.querySelectorAll('[data-script]')].pop();
    row.querySelector('[data-toggle]').click();
    await pause(400);
    out.off = !document.querySelector(`[data-script="${{row.dataset.script}}"] [data-toggle]`).checked;
    document.querySelector(`[data-script="${{row.dataset.script}}"] [data-toggle]`).click();
    await pause(400);
    $('reload').click();
    await pause(6000);
    out.ran_after_reload = [...document.querySelectorAll('#scripts-ran .script-ran')].map((n) => n.textContent);
    const menus = [...document.querySelectorAll('[data-menu]')];
    out.menus = menus.map((m) => m.textContent);
    menus.forEach((m) => m.click());
    await pause(2500);
    if ({link}) {{
      $('link-input').value = {link};
      $('link-go').click();
      await pause(6000);
      out.link = $('link-result').textContent;
      out.vip_button = !$('link-vip-open').hidden;
      if (out.vip_button) {{
        $('link-vip-open').click();
        await pause(9000);
        out.tabs = state.tabs.map((t) => [t.url, t.title]);
      }}
    }}
  }} catch (e) {{ out.error = String(e); }}
  invoke('probe_preview', {{ report: JSON.stringify(out) }});
}})();"#
    )
}

async fn test_panel(app: &AppHandle, file: &Path, link: Option<&str>) -> Option<String> {
    let source = std::fs::read_to_string(file).ok()?;
    let ui = app.get_webview("ui")?;
    ui.eval(panel_script(&source, link)).ok()?;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let done = PREVIEW.lock().unwrap_or_else(|e| e.into_inner()).take();
        if done.is_some() {
            return done;
        }
    }
    Some("the panel test did not finish".into())
}

pub fn preview_result(report: String) {
    *PREVIEW.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
}

/// Runs in the panel: starts the preview, lets it play, reports back.
fn preview_script(tab: u32, url: &str, format: &str) -> String {
    let args = serde_json::json!([tab, url, "probe", format]).to_string();
    format!(
        r#"(async () => {{
  const say = (report) => invoke('probe_preview', {{ report }});
  try {{
    if (state.panel !== 'media') togglePanel('media');
    const [tab, url, name, format] = {args};
    await startPreview(tab, url, name, format);
    const v = $('preview-video');
    v.muted = true;
    v.play().catch(() => {{}});
    await new Promise((done) => setTimeout(done, 15000));
    say(JSON.stringify({{
      native_hls_says: v.canPlayType('application/vnd.apple.mpegurl'),
      player: hlsPlayer ? 'hls.js' : 'engine',
      audio_tracks: hlsPlayer ? hlsPlayer.audioTracks.length : null,
      played_seconds: v.currentTime,
      audio_bytes: v.webkitAudioDecodedByteCount,
      video_bytes: v.webkitVideoDecodedByteCount,
      error: v.error ? 'code ' + v.error.code + ' ' + v.error.message : null,
    }}));
    stopPreview();
  }} catch (e) {{ say('preview failed: ' + e); }}
}})();"#
    )
}

async fn test_format(app: &AppHandle, height: u32) -> FormatTest {
    let mut out = FormatTest::default();
    let st = app.state::<AppState>();
    let tab = st.tabs.lock().expect("tabs").order.first().copied();
    let Some(tab) = tab else {
        out.error = Some("no tab".into());
        return out;
    };
    let items = st.media.visible(tab);
    let found = items.iter().find_map(|item| {
        let x = item.extracted.as_ref()?;
        let best = x
            .formats
            .iter()
            .filter(|f| f.height == Some(height) && !f.audio_only)
            .max_by_key(|f| f.bitrate.unwrap_or(0.0) as u64)?;
        Some((item, best))
    });
    let Some((item, format)) = found else {
        out.error = Some(format!("no yt-dlp card with a {height}p format"));
        return out;
    };
    out.format = Some(format.id.clone());
    out.joined_with = format.pairs_with.clone();

    if let Some(ui) = app.get_webview("ui") {
        let _ = ui.eval(preview_script(tab, &item.url, &format.id));
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let done = PREVIEW.lock().unwrap_or_else(|e| e.into_inner()).take();
            if done.is_some() {
                out.preview = done;
                break;
            }
        }
    }

    let id = match crate::download_item(app, tab, &item.url, None, Some(&format.id)) {
        Ok(id) => id,
        Err(e) => {
            out.error = Some(e);
            return out;
        }
    };
    // A big file takes a while, so this waits far longer than the page did.
    for _ in 0..1800 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let job = st.downloads.snapshot().into_iter().find(|j| j.id == id);
        let Some(job) = job else { break };
        let over = matches!(
            job.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        );
        if over {
            out.state = Some(format!("{:?}", job.state));
            out.path = job.path;
            out.note = job.note;
            out.error = job.error;
            return out;
        }
    }
    out.error = Some("the download did not finish in an hour".into());
    out
}

/// Played in every frame of a probed page: load lazy frames, mute and
/// start the media, and press the play button players draw over the video.
/// Runs a few times, since players often appear late. A lazy frame would
/// otherwise wait for a scroll that never comes, and a player with a button
/// not on the list gets a click in the middle of the frame, the same place
/// a person would click.
pub const AUTOPLAY_JS: &str = r#"(() => {
  if (window.__ospreyAutoplay) return;
  window.__ospreyAutoplay = true;
  const buttons = [
    '.vjs-big-play-button', '.jw-display-icon-container', '.jw-icon-display',
    '.plyr__control--overlaid', '.ytp-large-play-button', '.videoplayer_btn_play',
    '.play-button', '.btn-play', '.play-btn', '#play', '.fp-play',
    '.vid_play', '.html5-vpl_panel_play',
    '[aria-label="Play"]', '[aria-label="Play video"]', '[title="Play"]',
    '[data-plyr="play"]'
  ];
  const wakeFrames = () => {
    for (const f of document.querySelectorAll('iframe')) {
      if (f.loading === 'lazy') f.loading = 'eager';
      const later = f.dataset.src || f.dataset.lazySrc;
      if (later && !f.src) f.src = later;
    }
  };
  const clickMiddle = () => {
    const el = document.elementFromPoint(innerWidth / 2, innerHeight / 2);
    if (!el || el.closest('a[href]') || el.tagName === 'IFRAME') return;
    for (const t of ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click']) {
      el.dispatchEvent(new MouseEvent(t, { bubbles: true, cancelable: true }));
    }
  };
  let rounds = 0;
  const kick = () => {
    rounds++;
    wakeFrames();
    for (const m of document.querySelectorAll('video, audio')) {
      if (m.paused) { m.muted = true; m.play().catch(() => {}); }
    }
    if (rounds <= 4) {
      let pressed = false;
      for (const s of buttons) {
        const b = document.querySelector(s);
        if (b && b.getClientRects().length) { b.click(); pressed = true; break; }
      }
      // Only inside a player: the middle of a site's page could be anything.
      const player = window !== top || /embed|player/i.test(location.pathname);
      if (!pressed && player) clickMiddle();
    }
  };
  const timer = setInterval(() => { kick(); if (rounds >= 10) clearInterval(timer); }, 2500);
})();"#;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Milliseconds since the probe started.
    pub at_ms: u64,
    pub url: String,
    /// The page or frame it came from.
    pub page: String,
    /// "captured" (by the engine), "reported" (by the sniffer), "kept",
    /// "dropped", "ad_filtered" or "over_limit".
    pub what: &'static str,
    pub detail: Option<String>,
}

/// One response the web engine showed.
#[derive(Debug, Clone, Serialize)]
pub struct NetEntry {
    pub at_ms: u64,
    pub url: String,
    pub method: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub judged: String,
}

struct Log {
    start: Instant,
    events: Mutex<Vec<Event>>,
    net: Mutex<Vec<NetEntry>>,
    responses: AtomicUsize,
}

fn log() -> &'static Log {
    LOG.get_or_init(|| Log {
        start: Instant::now(),
        events: Mutex::new(Vec::new()),
        net: Mutex::new(Vec::new()),
        responses: AtomicUsize::new(0),
    })
}

/// Records a response from the web engine. Does nothing outside a probe.
pub fn network(seen: &Seen, judged: &Judged) {
    if !active() {
        return;
    }
    let l = log();
    l.responses.fetch_add(1, Ordering::Relaxed);
    let mut net = l.net.lock().unwrap_or_else(|e| e.into_inner());
    if net.len() >= 4000 {
        return;
    }
    net.push(NetEntry {
        at_ms: l.start.elapsed().as_millis() as u64,
        url: seen.url.chars().take(2048).collect(),
        method: seen.method.clone(),
        status: seen.status,
        content_type: seen.content_type.clone(),
        judged: format!("{judged:?}"),
    });
}

/// Records what happened to a candidate. Does nothing outside a probe.
pub fn note(url: &str, page: &str, what: &'static str, detail: Option<String>) {
    if !active() {
        return;
    }
    let l = log();
    let mut events = l.events.lock().unwrap_or_else(|e| e.into_inner());
    if events.len() >= 2000 {
        return;
    }
    events.push(Event {
        at_ms: l.start.elapsed().as_millis() as u64,
        url: url.chars().take(2048).collect(),
        page: page.chars().take(512).collect(),
        what,
        detail,
    });
}

#[derive(Serialize)]
struct Summary {
    /// Responses the web engine showed, of any type.
    engine_responses: usize,
    /// Candidates that came from the web engine.
    captured: usize,
    /// Candidates that came from the sniffer.
    reported: usize,
    listed: usize,
    dropped: usize,
    ad_filtered: usize,
    over_limit: usize,
    /// Sniffer finds the engine never showed. Native capture missed these.
    sniffer_only: usize,
}

#[derive(Serialize)]
struct Report {
    format_test: Option<FormatTest>,
    panel_test: Option<String>,
    /// Scripts installed for this run, what ran and what they asked for.
    script_installs: Vec<Value>,
    scripts: Value,
    tool: &'static str,
    version: &'static str,
    os: &'static str,
    requested_url: String,
    final_url: String,
    title: String,
    route: String,
    waited_s: u64,
    summary: Summary,
    /// What the Media panel shows at the end, with qualities and sizes.
    listed: Vec<crate::media::MediaItem>,
    /// Every candidate and what happened to it, in order.
    events: Vec<Event>,
    /// The addresses of `summary.sniffer_only`.
    sniffer_only: Vec<String>,
    /// Every response the web engine showed, in order.
    network: Vec<NetEntry>,
}

/// Starts the timer that writes the report and closes the browser.
pub fn schedule(app: &AppHandle) {
    let Some(cfg) = config() else { return };
    log();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(cfg.wait)).await;
        let format_test = match cfg.format_height {
            Some(height) => Some(test_format(&app, height).await),
            None => None,
        };
        let panel_test = match &cfg.panel {
            Some(file) => test_panel(&app, file, cfg.link.as_deref()).await,
            None => None,
        };
        let code = match write_report(&app, cfg, format_test, panel_test).await {
            Ok(path) => {
                println!("probe report: {}", path.display());
                0
            }
            Err(e) => {
                eprintln!("probe failed: {e}");
                1
            }
        };
        app.exit(code);
        // In case a window refuses to close.
        tokio::time::sleep(Duration::from_secs(10)).await;
        std::process::exit(code);
    });
}

async fn write_report(
    app: &AppHandle,
    cfg: &ProbeCfg,
    format_test: Option<FormatTest>,
    panel_test: Option<String>,
) -> Result<PathBuf, String> {
    let st = app.state::<AppState>();
    let (tab_id, final_url, title) = {
        let tabs = st.tabs.lock().expect("tabs");
        let id = tabs.order.first().copied();
        let t = id.and_then(|i| tabs.map.get(&i));
        (
            id,
            t.map(|t| t.url.clone()).unwrap_or_default(),
            t.map(|t| t.title.clone()).unwrap_or_default(),
        )
    };
    let listed = tab_id.map(|i| st.media.visible(i)).unwrap_or_default();
    let route = crate::route_label(app);
    let events = log()
        .events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let network = log().net.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let shown: HashSet<&str> = network.iter().map(|n| n.url.as_str()).collect();
    let sniffer_only: Vec<String> = events
        .iter()
        .filter(|e| e.what == "reported" && !shown.contains(e.url.as_str()))
        .map(|e| e.url.clone())
        .collect();
    let count = |w: &str| events.iter().filter(|e| e.what == w).count();
    let report = Report {
        format_test,
        panel_test,
        script_installs: INSTALLS.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        scripts: crate::scripts::probe_report(app, tab_id),
        tool: "bm-osprey --probe-page",
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        requested_url: cfg.url.clone(),
        final_url,
        title,
        route,
        waited_s: cfg.wait,
        summary: Summary {
            engine_responses: log().responses.load(Ordering::Relaxed),
            captured: count("captured"),
            reported: count("reported"),
            listed: listed.len(),
            dropped: count("dropped"),
            ad_filtered: count("ad_filtered"),
            over_limit: count("over_limit"),
            sniffer_only: sniffer_only.len(),
        },
        listed,
        events,
        sniffer_only,
        network,
    };
    let path = cfg.out.clone().unwrap_or_else(|| {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        st.home.join("probes").join(format!("probe-{t}.json"))
    });
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_probe_options() {
        assert!(parse(args(&["https://a.test"])).is_none());
        let c = parse(args(&[
            "--probe-page",
            "site.test/ep1",
            "--probe-wait",
            "2",
            "--probe-out",
            "r.json",
        ]))
        .unwrap();
        assert_eq!(c.url, "https://site.test/ep1");
        assert_eq!(c.wait, 5, "clamped to the minimum");
        assert_eq!(c.out, Some(PathBuf::from("r.json")));
        let d = parse(args(&["--probe-page", "http://x.test/"])).unwrap();
        assert_eq!((d.url.as_str(), d.wait), ("http://x.test/", 30));
    }
}
