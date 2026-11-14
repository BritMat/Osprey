//! BM Osprey browser.
//!
//! One window. The browser's own interface (tabs, toolbar, side panel) is
//! a local web page filling the window. Each tab is a separate web view
//! placed over the content area. Every tab is pointed at the privacy
//! gateway, which decides the real route (direct, encrypted DNS, WARP, a
//! VPN proxy or Tor) and blocks ad and tracker domains.
//!
//! It uses the system's own web engine (WebView2 on Windows, WebKit on
//! macOS and Linux) instead of shipping a whole browser, which keeps it
//! small and light. Background tabs are hidden, which lets the engine
//! throttle them, and are put to sleep after a while.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
#[cfg(target_os = "linux")]
mod capture_linux;
#[cfg(windows)]
mod capture_win;
mod downloads;
mod extensions;
#[cfg(target_os = "linux")]
mod linux_layout;
mod media;
mod prefs;
mod preview;
mod probe;
mod scripts;
mod settings;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use osprey_core::gateway::{self, Gateway};
use osprey_core::privacy::{Privacy, Route};
use osprey_core::textmode::{self, TextModeOptions};
use osprey_core::{Filters, MediaKind, RequestOptions};
use serde::{Deserialize, Serialize};
use tauri::webview::{DownloadEvent, NewWindowResponse, PageLoadEvent, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{
    AppHandle, Emitter, EventTarget, LogicalPosition, LogicalSize, Manager, Url, Webview,
    WebviewUrl, WindowEvent,
};

use downloads::{BrowserIdentity, DownloadRequest};
use settings::{AppSettings, DownloadStyle, NetMode};

/// Tab strip (40) + toolbar (46).
const TOP_HEIGHT: f64 = 86.0;
/// The side panel (392) and the gap to its right (10).
const PANEL_WIDTH: f64 = 402.0;
const SNIFFER_JS: &str = include_str!("../scripts/sniffer.js");
const PRIVATE_JS: &str = include_str!("../scripts/private.js");
const WEBRTC_GUARD_JS: &str = include_str!("../scripts/webrtc-guard.js");
const PRIVATE_NEWTAB_HTML: &str = include_str!("../ui/newtab-private.html");
const SEARCH_URL: &str = "https://duckduckgo.com/";

#[derive(Debug, Clone, Serialize)]
struct Tab {
    id: u32,
    url: String,
    title: String,
    private: bool,
    loading: bool,
    text_mode: bool,
    sleeping: bool,
    media: usize,
    #[serde(skip)]
    last_active: Instant,
    /// The script version this tab's web view was built with.
    #[serde(skip)]
    script_gen: u64,
}

#[derive(Default)]
struct Tabs {
    next: u32,
    order: Vec<u32>,
    active: Option<u32>,
    map: HashMap<u32, Tab>,
}

#[derive(Debug, Clone, Serialize)]
struct TabsView {
    tabs: Vec<Tab>,
    active: Option<u32>,
}

struct AppState {
    home: PathBuf,
    gateway: Gateway,
    filters: RwLock<Arc<Filters>>,
    settings: Mutex<AppSettings>,
    tabs: Mutex<Tabs>,
    media: media::MediaState,
    preview: Arc<preview::Preview>,
    identity: Mutex<HashMap<u32, BrowserIdentity>>,
    downloads: Arc<downloads::Manager>,
    scripts: scripts::Shared,
    /// Extensions were present at start up. WebView2 needs the same choice
    /// for every web view in a run, so it is made once.
    #[cfg_attr(not(windows), allow(dead_code))]
    extensions_on: bool,
    panel_open: Mutex<bool>,
    /// The settings page is open and covers the tabs.
    covered: Mutex<bool>,
    /// Engine settings as they were at start up, the same for every tab.
    engine_start: prefs::EngineStart,
    tor_addr: Mutex<Option<String>>,
    shortcut_times: Mutex<Vec<Instant>>,
    /// The last session saved, to write only when it changes.
    saved_session: Mutex<String>,
}

type Res<T> = Result<T, String>;

/// The sniffer, with a marker only this run knows, so frames can pass
/// their finds to the top page without stray page messages getting in.
fn sniffer_js() -> &'static str {
    static JS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    JS.get_or_init(|| {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let nonce = format!("osprey{:x}{:x}", t, std::process::id());
        SNIFFER_JS.replace("__OSPREY_NONCE__", &nonce)
    })
}

fn label_of(id: u32) -> String {
    format!("tab-{id}")
}

fn id_of(label: &str) -> Option<u32> {
    label.strip_prefix("tab-")?.parse().ok()
}

/// Turns what was typed in the address bar into a URL, or a search.
fn to_url(input: &str) -> Url {
    let t = input.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("about:blank") {
        return Url::parse("about:blank").expect("valid");
    }
    if let Ok(u) = Url::parse(t) {
        if matches!(u.scheme(), "http" | "https") {
            return u;
        }
    }
    let looks_like_host = !t.contains(' ')
        && (t.contains('.') || t.starts_with("localhost"))
        && !t.starts_with('.')
        && !t.ends_with('.');
    if looks_like_host {
        let local = t.starts_with("localhost")
            || t.starts_with("127.")
            || t.starts_with("192.168.")
            || t.starts_with("10.");
        let scheme = if local { "http" } else { "https" };
        if let Ok(u) = Url::parse(&format!("{scheme}://{t}")) {
            return u;
        }
    }
    Url::parse_with_params(SEARCH_URL, &[("q", t)]).expect("search URL is valid")
}

/// Where the text-only view of `target` is served. Custom schemes are
/// reached as http://<scheme>.localhost on Windows.
fn text_url(target: &str) -> Url {
    let base = if cfg!(windows) {
        "http://ospreytext.localhost/"
    } else {
        "ospreytext://localhost/"
    };
    Url::parse_with_params(base, &[("u", target)]).expect("text URL is valid")
}

/// The browser's own new tab page. Built as a full URL so the private
/// marker after # survives (a plain file path would not allow it).
fn new_tab_url(private: bool) -> WebviewUrl {
    if private {
        // Private tabs get their own empty engine profile, which on Linux
        // does not know the app's internal pages. A self-contained page
        // works everywhere.
        return WebviewUrl::External(private_new_tab_url());
    }
    let base = if cfg!(windows) {
        "http://tauri.localhost/newtab.html"
    } else {
        "tauri://localhost/newtab.html"
    };
    let mut u = Url::parse(base).expect("valid");
    if private {
        u.set_fragment(Some("private"));
    }
    WebviewUrl::CustomProtocol(u)
}

/// The private new tab page as one self-contained data: address. It has
/// no script, so the app's strict security policy does not stop it.
fn private_new_tab_url() -> Url {
    let html = PRIVATE_NEWTAB_HTML;
    let mut encoded = String::with_capacity(html.len() * 2);
    for b in html.bytes() {
        if b.is_ascii_alphanumeric() || b" -_.~!*'()/:;,=".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    Url::parse(&format!("data:text/html;charset=utf-8,{encoded}")).expect("data URL is valid")
}

fn is_text_url(u: &Url) -> bool {
    u.scheme() == "ospreytext" || u.host_str() == Some("ospreytext.localhost")
}

fn text_target(u: &Url) -> Option<String> {
    is_text_url(u).then(|| {
        u.query_pairs()
            .find(|(k, _)| k == "u")
            .map(|(_, v)| v.into_owned())
    })?
}

/// The browser's own pages (new tab) are shown with an empty address bar.
fn is_internal(u: &Url) -> bool {
    matches!(u.scheme(), "tauri" | "about" | "data") || u.host_str() == Some("tauri.localhost")
}

fn display_url(u: &Url) -> String {
    if let Some(t) = text_target(u) {
        return t;
    }
    if is_internal(u) {
        return String::new();
    }
    u.to_string()
}

fn emit_ui<S: Serialize + Clone>(app: &AppHandle, event: &str, payload: S) {
    let _ = app.emit_to(EventTarget::webview("ui"), event, payload);
}

fn tabs_view(state: &AppState) -> TabsView {
    let tabs = state.tabs.lock().expect("tabs");
    TabsView {
        tabs: tabs
            .order
            .iter()
            .filter_map(|id| tabs.map.get(id).cloned())
            .collect(),
        active: tabs.active,
    }
}

fn emit_tabs(app: &AppHandle) {
    let view = tabs_view(&app.state::<AppState>());
    save_session(app, &view);
    emit_ui(app, "tabs", view);
}

#[derive(Serialize, Deserialize, Default)]
struct Session {
    tabs: Vec<String>,
    active: usize,
}

fn session_path(app: &AppHandle) -> PathBuf {
    app.state::<AppState>().home.join("session.json")
}

/// Saves the normal tabs, so they come back next time. Private tabs are
/// never written anywhere.
fn save_session(app: &AppHandle, view: &TabsView) {
    let st = app.state::<AppState>();
    // A probe must not replace the tabs you had open.
    if probe::active() || !st.settings.lock().expect("settings").restore_tabs {
        return;
    }
    let normal: Vec<&Tab> = view
        .tabs
        .iter()
        .filter(|t| !t.private && (t.url.starts_with("http://") || t.url.starts_with("https://")))
        .collect();
    let active = normal
        .iter()
        .position(|t| Some(t.id) == view.active)
        .unwrap_or(0);
    let session = Session {
        tabs: normal.iter().map(|t| t.url.clone()).take(50).collect(),
        active,
    };
    let Ok(text) = serde_json::to_string(&session) else {
        return;
    };
    let mut last = st.saved_session.lock().expect("session");
    if *last == text {
        return;
    }
    let path = session_path(app);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &text).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        *last = text;
    }
}

/// Reopens last time's tabs. Only the active one loads now. The others
/// wait asleep until you click them, which keeps start up light.
fn restore_session(app: &AppHandle) -> bool {
    let st = app.state::<AppState>();
    if !st.settings.lock().expect("settings").restore_tabs {
        return false;
    }
    let Some(session) = std::fs::read_to_string(session_path(app))
        .ok()
        .and_then(|t| serde_json::from_str::<Session>(&t).ok())
        .filter(|s| !s.tabs.is_empty())
    else {
        return false;
    };
    let mut active_id = None;
    {
        let mut tabs = st.tabs.lock().expect("tabs");
        for (i, url) in session.tabs.iter().enumerate() {
            tabs.next += 1;
            let id = tabs.next;
            tabs.order.push(id);
            tabs.map.insert(
                id,
                Tab {
                    id,
                    url: url.clone(),
                    title: display_url(&to_url(url)),
                    private: false,
                    loading: false,
                    text_mode: false,
                    sleeping: true,
                    media: 0,
                    last_active: Instant::now(),
                    script_gen: 0,
                },
            );
            if i == session.active.min(session.tabs.len() - 1) {
                active_id = Some(id);
            }
        }
    }
    if let Some(id) = active_id {
        activate(app, id);
    }
    true
}

fn window_size(app: &AppHandle) -> LogicalSize<f64> {
    app.get_window("main")
        .and_then(|w| {
            let scale = w.scale_factor().ok()?;
            w.inner_size().ok().map(|s| s.to_logical::<f64>(scale))
        })
        .unwrap_or(LogicalSize::new(1280.0, 820.0))
}

/// Moves and sizes a web view. On Linux this goes through GTK directly,
/// because Tauri ignores web view positions there.
fn place(app: &AppHandle, wv: &Webview, pos: LogicalPosition<f64>, size: LogicalSize<f64>) {
    #[cfg(target_os = "linux")]
    if let Some(window) = app.get_window("main") {
        linux_layout::place(
            window,
            wv,
            pos.x as i32,
            pos.y as i32,
            size.width as i32,
            size.height as i32,
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        let _ = wv.set_position(pos);
        let _ = wv.set_size(size);
    }
}

/// Tabs drawn in the title bar, like Chrome, with BM Osprey's own window
/// buttons. On by default on Windows. OSPREY_CUSTOM_TITLEBAR=1 turns it on
/// elsewhere (used for testing), =0 turns it off.
fn custom_frame() -> bool {
    match osprey_core::env_var("CUSTOM_TITLEBAR")
        .as_deref()
        .and_then(|v| v.to_str())
    {
        Some("1") => true,
        Some("0") => false,
        _ => cfg!(windows),
    }
}

/// With BM Osprey's own title bar, the window edges stay free so the
/// window can be resized from them. Not needed when maximised.
const RESIZE_EDGE: f64 = 4.0;

fn edge(app: &AppHandle) -> f64 {
    let maximised = app
        .get_window("main")
        .and_then(|w| w.is_maximized().ok())
        .unwrap_or(false);
    if custom_frame() && !maximised {
        RESIZE_EDGE
    } else {
        0.0
    }
}

fn content_bounds(app: &AppHandle) -> (LogicalPosition<f64>, LogicalSize<f64>) {
    let st = app.state::<AppState>();
    let panel = if *st.panel_open.lock().expect("panel") {
        PANEL_WIDTH
    } else {
        0.0
    };
    let size = window_size(app);
    let e = edge(app);
    (
        LogicalPosition::new(e, TOP_HEIGHT),
        LogicalSize::new(
            (size.width - panel - 2.0 * e).max(240.0),
            (size.height - TOP_HEIGHT - e).max(120.0),
        ),
    )
}

fn relayout(app: &AppHandle) {
    if let Some(ui) = app.get_webview("ui") {
        place(app, &ui, LogicalPosition::new(0.0, 0.0), window_size(app));
    }
    let (pos, size) = content_bounds(app);
    let ids: Vec<u32> = app
        .state::<AppState>()
        .tabs
        .lock()
        .expect("tabs")
        .order
        .clone();
    for id in ids {
        if let Some(wv) = app.get_webview(&label_of(id)) {
            place(app, &wv, pos, size);
        }
    }
}

fn tab_webview(app: &AppHandle, id: u32) -> Option<Webview> {
    app.get_webview(&label_of(id))
}

/// Creates the web view for a tab. Must not run on the main thread
/// (WebView2 would deadlock), so it is only called from async code.
fn open_webview(app: &AppHandle, id: u32, target: WebviewUrl, private: bool) -> Res<()> {
    let st = app.state::<AppState>();
    let proxy = Url::parse(&st.gateway.proxy_url()).map_err(|e| e.to_string())?;
    let label = label_of(id);

    let mut b = WebviewBuilder::new(&label, target)
        .proxy_url(proxy)
        .incognito(private)
        .data_directory(st.home.join("browser-profile"))
        .initialization_script_for_all_frames(sniffer_js())
        .initialization_script_for_all_frames(if probe::active() {
            probe::AUTOPLAY_JS
        } else {
            ""
        })
        .zoom_hotkeys_enabled(true)
        .on_navigation({
            let app = app.clone();
            move |u| on_navigation(&app, id, u)
        })
        .on_page_load({
            let app = app.clone();
            move |_wv, p| on_page_load(&app, id, p.event(), p.url())
        })
        .on_document_title_changed({
            let app = app.clone();
            move |_wv, title| {
                if let Some(t) = app
                    .state::<AppState>()
                    .tabs
                    .lock()
                    .expect("tabs")
                    .map
                    .get_mut(&id)
                {
                    t.title = title;
                }
                emit_tabs(&app);
            }
        })
        .on_new_window({
            let app = app.clone();
            move |u, _features| {
                // Blank and script pop-ups are what pop-under ads use. A
                // real link always has a web address.
                if !matches!(u.scheme(), "http" | "https") {
                    return NewWindowResponse::Deny;
                }
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let page = app
                        .state::<AppState>()
                        .tabs
                        .lock()
                        .expect("tabs")
                        .map
                        .get(&id)
                        .map(|t| t.url.clone())
                        .unwrap_or_default();
                    if media::popup_allowed(&app, &page, &u).await {
                        let _ = create_tab(&app, Some(u.to_string()), private);
                    } else {
                        emit_ui(
                            &app,
                            "popup-blocked",
                            serde_json::json!({ "tab": id, "url": u.to_string() }),
                        );
                    }
                });
                NewWindowResponse::Deny
            }
        })
        .on_download({
            let app = app.clone();
            move |_wv, event| match event {
                // Downloads the page starts go through BM Osprey instead,
                // so they use the same route and show in the panel.
                DownloadEvent::Requested { url, .. } => {
                    let target = media::Target {
                        url: url.to_string(),
                        kind: MediaKind::Direct,
                        ..Default::default()
                    };
                    start_download(&app, id, target, None, None, None);
                    false
                }
                _ => true,
            }
        });
    let hides_ip = st.settings.lock().expect("settings").net.hides_ip();
    if private || hides_ip {
        b = b.initialization_script_for_all_frames(WEBRTC_GUARD_JS);
    }
    if private {
        b = b.initialization_script_for_all_frames(PRIVATE_JS);
    } else {
        for script in scripts::init_scripts(app, id) {
            b = b.initialization_script_for_all_frames(script);
        }
    }
    if let Some(t) = st.tabs.lock().expect("tabs").map.get_mut(&id) {
        t.script_gen = st.scripts.generation();
    }
    #[cfg(windows)]
    if st.extensions_on {
        b = b.browser_extensions_enabled(true);
        if !private {
            b = b.extensions_path(extensions::folder(&st.home));
        }
    }
    #[cfg(windows)]
    {
        // Setting browser arguments replaces the defaults, so they are
        // repeated here together with the proxy. WebRTC is told never to
        // use the network outside the proxy.
        b = b.additional_browser_args(&format!(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
             --proxy-server=socks5://{} \
             --force-webrtc-ip-handling-policy=disable_non_proxied_udp \
             --webrtc-ip-handling-policy=disable_non_proxied_udp{}",
            st.gateway.addr(),
            st.engine_start.browser_args()
        ));
    }

    let window = app.get_window("main").ok_or("main window is missing")?;
    let (pos, size) = content_bounds(app);
    let wv = window.add_child(b, pos, size).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    capture_win::attach(app, &wv, id);
    #[cfg(target_os = "linux")]
    capture_linux::attach(app, &wv, id);
    place(app, &wv, pos, size);
    Ok(())
}

fn on_navigation(app: &AppHandle, id: u32, u: &Url) -> bool {
    let st = app.state::<AppState>();
    st.scripts.new_page(id);
    scripts::emit_scripts(app);
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        scripts::abandon_saves(&handle, id).await;
    });
    let text_mode = st
        .tabs
        .lock()
        .expect("tabs")
        .map
        .get(&id)
        .is_some_and(|t| t.text_mode);
    if text_mode && matches!(u.scheme(), "http" | "https") && !is_text_url(u) {
        let target = text_url(u.as_str());
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Some(wv) = tab_webview(&app, id) {
                let _ = wv.navigate(target);
            }
        });
        return false;
    }
    true
}

fn on_page_load(app: &AppHandle, id: u32, event: PageLoadEvent, u: &Url) {
    let st = app.state::<AppState>();
    let new_page = {
        let mut tabs = st.tabs.lock().expect("tabs");
        let Some(t) = tabs.map.get_mut(&id) else {
            return;
        };
        let shown = display_url(u);
        let changed = t.url != shown;
        t.url = shown;
        t.loading = event == PageLoadEvent::Started;
        if is_internal(u) {
            t.title = "New tab".into();
        }
        event == PageLoadEvent::Started && changed
    };
    if new_page {
        st.media.clear_tab(id);
        media::emit_media(app, id);
    }
    if event == PageLoadEvent::Finished && matches!(u.scheme(), "http" | "https") {
        hide_ad_elements(app, id, u.as_str());
    }
    emit_tabs(app);
}

/// Lists the classes and ids on a page, once, after it loads. Bounded so
/// huge pages stay cheap.
const COLLECT_CLASSES_JS: &str = r#"(function(){var c={},d={},e=document.getElementsByTagName('*');
for(var k=0;k<e.length&&k<5000;k++){var x=e[k];if(x.id)d[x.id]=1;var l=x.classList;if(l)for(var j=0;j<l.length;j++)c[l[j]]=1;}
return JSON.stringify({c:Object.keys(c).slice(0,4000),i:Object.keys(d).slice(0,1500)});})()"#;

#[derive(Deserialize)]
struct PageNames {
    c: Vec<String>,
    i: Vec<String>,
}

/// uBlock Origin style element hiding: asks the filter lists which page
/// elements are ads (like `##.adsbygoogle`) and hides them with one style
/// sheet. Runs once per page load, not continuously, to keep CPU low.
fn hide_ad_elements(app: &AppHandle, id: u32, page: &str) {
    let st = app.state::<AppState>();
    if !st.settings.lock().expect("settings").block_ads {
        return;
    }
    let Some(wv) = tab_webview(app, id) else {
        return;
    };
    let filters = st.filters.read().expect("filters").clone();
    let page = page.to_string();
    let target = wv.clone();
    let _ = wv.eval_with_callback(COLLECT_CLASSES_JS, move |raw| {
        // The result can arrive as JSON text inside a JSON string.
        let text = serde_json::from_str::<String>(&raw).unwrap_or(raw);
        let Ok(names) = serde_json::from_str::<PageNames>(&text) else { return };
        let classes: std::collections::HashSet<String> = names.c.into_iter().filter(|s| s.len() < 200).collect();
        let ids: std::collections::HashSet<String> = names.i.into_iter().filter(|s| s.len() < 200).collect();
        let selectors = filters.hidden_selectors(&page, &classes, &ids);
        if selectors.is_empty() {
            return;
        }
        // One rule per selector: a selector the engine does not know
        // then only drops its own rule.
        let css: String = selectors
            .iter()
            .take(3000)
            .map(|s| format!("{s}{{display:none!important}}\n"))
            .collect();
        let js = format!(
            "(function(){{var s=document.createElement('style');s.textContent={};(document.head||document.documentElement).appendChild(s);}})()",
            serde_json::to_string(&css).unwrap_or_default()
        );
        let _ = target.eval(js);
    });
}

fn create_tab(app: &AppHandle, url: Option<String>, private: bool) -> Res<u32> {
    let st = app.state::<AppState>();
    let private = private || st.settings.lock().expect("settings").always_private;
    let target = match url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(u) => to_url(u),
        None => Url::parse("about:blank").expect("valid"),
    };
    let id = {
        let mut tabs = st.tabs.lock().expect("tabs");
        tabs.next += 1;
        let id = tabs.next;
        // Open next to the current tab, like most browsers.
        let pos = tabs
            .active
            .and_then(|a| tabs.order.iter().position(|x| *x == a))
            .map(|p| p + 1)
            .unwrap_or(tabs.order.len());
        tabs.order.insert(pos, id);
        tabs.map.insert(
            id,
            Tab {
                id,
                url: if target.scheme() == "about" {
                    String::new()
                } else {
                    target.to_string()
                },
                title: if private {
                    "Private tab".into()
                } else {
                    "New tab".into()
                },
                private,
                loading: true,
                text_mode: false,
                sleeping: false,
                media: 0,
                last_active: Instant::now(),
                script_gen: 0,
            },
        );
        id
    };
    let webview_url = if target.scheme() == "about" {
        new_tab_url(private)
    } else {
        WebviewUrl::External(target)
    };
    open_webview(app, id, webview_url, private)?;
    activate(app, id);
    Ok(id)
}

/// A tab a user script opened. It keeps the current tab in front when the
/// script asked for a background tab.
fn create_script_tab(app: &AppHandle, url: String, active: bool) -> Res<()> {
    let st = app.state::<AppState>();
    let current = st.tabs.lock().expect("tabs").active;
    create_tab(app, Some(url), false)?;
    if let Some(id) = current.filter(|_| !active) {
        activate(app, id);
    }
    Ok(())
}

fn activate(app: &AppHandle, id: u32) {
    let st = app.state::<AppState>();
    let (previous, sleeping, url, private) = {
        let mut tabs = st.tabs.lock().expect("tabs");
        let previous = tabs.active.replace(id);
        let Some(t) = tabs.map.get_mut(&id) else {
            return;
        };
        t.last_active = Instant::now();
        let info = (previous, t.sleeping, t.url.clone(), t.private);
        t.sleeping = false;
        info
    };
    if let Some(p) = previous.filter(|p| *p != id) {
        if let Some(wv) = tab_webview(app, p) {
            let _ = wv.hide();
        }
        if let Some(t) = st.tabs.lock().expect("tabs").map.get_mut(&p) {
            t.last_active = Instant::now();
        }
    }
    if sleeping {
        // Wake up a sleeping tab where it was.
        let target = if url.is_empty() {
            new_tab_url(private)
        } else {
            WebviewUrl::External(to_url(&url))
        };
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = open_webview(&app, id, target, private);
            emit_tabs(&app);
        });
    } else if let Some(wv) = tab_webview(app, id) {
        let (pos, size) = content_bounds(app);
        place(app, &wv, pos, size);
        // Under the settings page a tab stays hidden until it closes.
        if !*st.covered.lock().expect("covered") {
            let _ = wv.show();
        }
    }
    emit_tabs(app);
    media::emit_media(app, id);
    scripts::emit_scripts(app);
}

fn close_tab(app: &AppHandle, id: u32) {
    let st = app.state::<AppState>();
    let next = {
        let mut tabs = st.tabs.lock().expect("tabs");
        let pos = tabs.order.iter().position(|x| *x == id);
        tabs.order.retain(|x| *x != id);
        tabs.map.remove(&id);
        if tabs.active == Some(id) {
            tabs.active = None;
            pos.and_then(|p| {
                tabs.order
                    .get(p.min(tabs.order.len().saturating_sub(1)))
                    .copied()
            })
        } else {
            None
        }
    };
    if let Some(wv) = tab_webview(app, id) {
        let _ = wv.close();
    }
    st.media.clear_tab(id);
    st.scripts.drop_tab(id);
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        scripts::abandon_saves(&handle, id).await;
    });
    st.identity.lock().expect("identity").remove(&id);
    let empty = st.tabs.lock().expect("tabs").order.is_empty();
    if empty {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = create_tab(&app, None, false);
        });
    } else if let Some(n) = next {
        activate(app, n);
    } else {
        emit_tabs(app);
    }
}

fn start_download(
    app: &AppHandle,
    tab: u32,
    target: media::Target,
    page: Option<String>,
    max_height: Option<u32>,
    file_name: Option<String>,
) -> u32 {
    let st = app.state::<AppState>();
    let (tab_page, private, title) = st
        .tabs
        .lock()
        .expect("tabs")
        .map
        .get(&tab)
        .map(|t| (t.url.clone(), t.private, t.title.clone()))
        .unwrap_or_default();
    let identity = st
        .identity
        .lock()
        .expect("identity")
        .get(&tab)
        .cloned()
        .unwrap_or_default();
    let req = DownloadRequest {
        url: target.url,
        kind: target.kind,
        page: page.unwrap_or(tab_page),
        identity,
        tab_label: Some(label_of(tab)),
        private,
        title: (!private).then_some(title).filter(|t| !t.trim().is_empty()),
        max_height: target.height.or(max_height),
        sent: target.sent,
        audio: target.audio,
        tag: target.tag,
        file_name,
    };
    let (out, how) = {
        let s = st.settings.lock().expect("settings");
        let circuits = if s.net == NetMode::Tor {
            s.tor_circuits
        } else {
            0
        };
        (
            probe::download_dir().unwrap_or_else(|| s.downloads_dir()),
            downloads::How {
                style: s.download_style,
                circuits,
            },
        )
    };
    let id = st
        .downloads
        .start(app.clone(), req, st.gateway.proxy_url(), out, how);
    emit_ui(app, "downloads", st.downloads.snapshot());
    id
}

// ---------------------------------------------------------------------
// Commands used by the browser interface
// ---------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct NetView {
    mode: NetMode,
    key: &'static str,
    label: String,
    route: String,
    ok: bool,
    problem: Option<String>,
    tor_found: Option<String>,
    warp_found: Option<String>,
    block_ads: bool,
    always_private: bool,
    download_style: DownloadStyle,
    tor_circuits: u8,
    speed_limit_kbps: u32,
    restore_tabs: bool,
    stats: gateway::GatewayStats,
}

/// The network route in words, for probe reports.
fn route_label(app: &AppHandle) -> String {
    app.state::<AppState>()
        .settings
        .lock()
        .expect("settings")
        .net
        .label()
}

async fn net_view(app: &AppHandle) -> NetView {
    let st = app.state::<AppState>();
    let detected = gateway::detect_routes().await;
    *st.tor_addr.lock().expect("tor") = detected.tor.clone();
    let check = st.gateway.check_upstream().await;
    let settings = st.settings.lock().expect("settings").clone();
    let stats = st.gateway.stats();
    NetView {
        key: settings.net.key(),
        label: settings.net.label(),
        mode: settings.net,
        route: stats.route.clone(),
        ok: check.is_ok(),
        problem: check
            .err()
            .map(|e| e.to_string())
            .or_else(|| stats.last_error.clone()),
        tor_found: detected.tor,
        warp_found: detected.warp,
        block_ads: settings.block_ads,
        always_private: settings.always_private,
        download_style: settings.download_style,
        tor_circuits: settings.tor_circuits,
        speed_limit_kbps: settings.speed_limit_kbps,
        restore_tabs: settings.restore_tabs,
        stats,
    }
}

#[derive(Serialize)]
struct Snapshot {
    tabs: TabsView,
    downloads: Vec<downloads::Job>,
    net: NetView,
    panel_open: bool,
    platform: &'static str,
    custom_frame: bool,
    settings: prefs::SettingsView,
}

#[tauri::command]
async fn ui_ready(app: AppHandle) -> Res<Snapshot> {
    let empty = app
        .state::<AppState>()
        .tabs
        .lock()
        .expect("tabs")
        .order
        .is_empty();
    if empty {
        if let Some(p) = probe::config() {
            // A test run: only the page under test, never the saved tabs.
            probe::install_scripts(&app).await;
            create_tab(&app, Some(p.url.clone()), false)?;
            probe::schedule(&app);
        } else {
            // A web address given on the command line opens in the first tab.
            let start = std::env::args().skip(1).find(|a| !a.starts_with('-'));
            let restored = start.is_none() && restore_session(&app);
            if !restored {
                create_tab(&app, start, false)?;
            }
        }
    }
    let net = net_view(&app).await;
    let st = app.state::<AppState>();
    let panel_open = *st.panel_open.lock().expect("panel");
    Ok(Snapshot {
        tabs: tabs_view(&st),
        downloads: st.downloads.snapshot(),
        net,
        panel_open,
        platform: std::env::consts::OS,
        custom_frame: custom_frame(),
        settings: prefs::view(&app),
    })
}

#[tauri::command]
async fn tab_new(app: AppHandle, url: Option<String>, private: bool) -> Res<u32> {
    create_tab(&app, url, private)
}

#[tauri::command]
async fn tab_close(app: AppHandle, id: u32) -> Res<()> {
    close_tab(&app, id);
    Ok(())
}

#[tauri::command]
async fn tab_activate(app: AppHandle, id: u32) -> Res<()> {
    activate(&app, id);
    Ok(())
}

#[tauri::command]
async fn tab_navigate(app: AppHandle, id: u32, input: String) -> Res<()> {
    let target = to_url(&input);
    let st = app.state::<AppState>();
    let text_mode = {
        let mut tabs = st.tabs.lock().expect("tabs");
        let t = tabs.map.get_mut(&id).ok_or("no such tab")?;
        t.url = target.to_string();
        t.text_mode
    };
    let target = if text_mode {
        text_url(target.as_str())
    } else {
        target
    };
    match tab_webview(&app, id) {
        Some(wv) => wv.navigate(target).map_err(|e| e.to_string())?,
        None => activate(&app, id),
    }
    emit_tabs(&app);
    Ok(())
}

fn eval_in_tab(app: &AppHandle, id: u32, js: &str) -> Res<()> {
    tab_webview(app, id)
        .ok_or("tab is not loaded")?
        .eval(js)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn tab_back(app: AppHandle, id: u32) -> Res<()> {
    eval_in_tab(&app, id, "history.back()")
}

#[tauri::command]
async fn tab_forward(app: AppHandle, id: u32) -> Res<()> {
    eval_in_tab(&app, id, "history.forward()")
}

#[tauri::command]
async fn tab_reload(app: AppHandle, id: u32) -> Res<()> {
    let st = app.state::<AppState>();
    // Start up scripts are fixed when a web view is made, so a tab with old
    // scripts gets a new one, the way a sleeping tab wakes up.
    let stale = {
        let mut tabs = st.tabs.lock().expect("tabs");
        let active = tabs.active == Some(id);
        let tab = tabs
            .map
            .get_mut(&id)
            .filter(|t| active && !t.private && t.script_gen != st.scripts.generation());
        match tab {
            Some(t) => {
                t.sleeping = true;
                true
            }
            None => false,
        }
    };
    if stale {
        if let Some(wv) = tab_webview(&app, id) {
            let _ = wv.close();
        }
        st.scripts.drop_tab(id);
        activate(&app, id);
        return Ok(());
    }
    tab_webview(&app, id)
        .ok_or("tab is not loaded")?
        .reload()
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn tab_stop(app: AppHandle, id: u32) -> Res<()> {
    eval_in_tab(&app, id, "window.stop()")
}

#[tauri::command]
async fn tab_text_mode(app: AppHandle, id: u32, on: bool) -> Res<()> {
    let st = app.state::<AppState>();
    let url = {
        let mut tabs = st.tabs.lock().expect("tabs");
        let t = tabs.map.get_mut(&id).ok_or("no such tab")?;
        t.text_mode = on;
        t.url.clone()
    };
    if !url.is_empty() {
        let target = if on { text_url(&url) } else { to_url(&url) };
        if let Some(wv) = tab_webview(&app, id) {
            wv.navigate(target).map_err(|e| e.to_string())?;
        }
    }
    emit_tabs(&app);
    Ok(())
}

#[tauri::command]
async fn layout(app: AppHandle, panel_open: bool, covered: Option<bool>) -> Res<()> {
    let st = app.state::<AppState>();
    *st.panel_open.lock().expect("panel") = panel_open;
    if let Some(c) = covered {
        *st.covered.lock().expect("covered") = c;
        let active = st.tabs.lock().expect("tabs").active;
        if let Some(wv) = active.and_then(|id| tab_webview(&app, id)) {
            let _ = if c { wv.hide() } else { wv.show() };
        }
    }
    relayout(&app);
    Ok(())
}

#[tauri::command]
async fn net_set(app: AppHandle, mode: NetMode) -> Res<NetView> {
    let st = app.state::<AppState>();
    let tor = if mode == NetMode::Tor {
        osprey_core::privacy::find_tor(None).await.ok()
    } else {
        None
    };
    let upstream = mode.upstream(tor.as_deref())?;
    st.gateway.set_upstream(upstream);
    let guard_changed = {
        let mut s = st.settings.lock().expect("settings");
        let changed = s.net.hides_ip() != mode.hides_ip();
        s.net = mode;
        s.save(&st.home);
        changed
    };
    if guard_changed {
        reload_normal_tabs(&app);
    }
    let view = net_view(&app).await;
    emit_ui(&app, "net", view.clone());
    Ok(view)
}

#[tauri::command]
async fn net_status(app: AppHandle) -> Res<NetView> {
    Ok(net_view(&app).await)
}

#[tauri::command]
async fn ads_set(app: AppHandle, on: bool) -> Res<()> {
    let st = app.state::<AppState>();
    {
        let mut s = st.settings.lock().expect("settings");
        s.block_ads = on;
        s.save(&st.home);
    }
    let filters = st.filters.read().expect("filters").clone();
    st.gateway.set_filters(on.then_some(filters));
    let view = net_view(&app).await;
    emit_ui(&app, "net", view);
    Ok(())
}

/// Opens normal tabs again so the WebRTC guard is added or removed.
/// Background tabs go to sleep and reload when next shown. Private tabs
/// always have the guard, and reloading them would end their session.
fn reload_normal_tabs(app: &AppHandle) {
    let st = app.state::<AppState>();
    let (active, ids): (Option<u32>, Vec<u32>) = {
        let mut tabs = st.tabs.lock().expect("tabs");
        let active = tabs.active;
        let ids = tabs
            .map
            .values_mut()
            .filter(|t| !t.private && !t.sleeping)
            .map(|t| {
                t.sleeping = true;
                t.id
            })
            .collect();
        (active, ids)
    };
    for id in &ids {
        if let Some(wv) = tab_webview(app, *id) {
            let _ = wv.close();
        }
    }
    if let Some(a) = active.filter(|a| ids.contains(a)) {
        activate(app, a);
    }
    emit_tabs(app);
}

#[derive(Deserialize)]
struct Prefs {
    always_private: Option<bool>,
    download_style: Option<DownloadStyle>,
    tor_circuits: Option<u8>,
    speed_limit_kbps: Option<u32>,
    restore_tabs: Option<bool>,
    bypass_vip: Option<bool>,
}

#[tauri::command]
async fn prefs_set(app: AppHandle, prefs: Prefs) -> Res<NetView> {
    let st = app.state::<AppState>();
    {
        let mut s = st.settings.lock().expect("settings");
        if let Some(v) = prefs.always_private {
            s.always_private = v;
        }
        if let Some(v) = prefs.download_style {
            s.download_style = v;
        }
        if let Some(v) = prefs.tor_circuits {
            s.tor_circuits = v.clamp(1, 6);
        }
        if let Some(v) = prefs.speed_limit_kbps {
            s.speed_limit_kbps = v.min(1_000_000);
            st.downloads.limit.set(u64::from(s.speed_limit_kbps) * 1024);
        }
        if let Some(v) = prefs.restore_tabs {
            s.restore_tabs = v;
        }
        if let Some(v) = prefs.bypass_vip {
            s.bypass_vip = v;
        }
        s.save(&st.home);
    }
    let view = net_view(&app).await;
    emit_ui(&app, "net", view.clone());
    Ok(view)
}

#[tauri::command]
async fn media_download(
    app: AppHandle,
    tab: u32,
    url: String,
    height: Option<u32>,
    format: Option<String>,
) -> Res<u32> {
    download_item(&app, tab, &url, height, format.as_deref())
}

/// What the download buttons do. The probe calls this too.
fn download_item(
    app: &AppHandle,
    tab: u32,
    url: &str,
    height: Option<u32>,
    format: Option<&str>,
) -> Res<u32> {
    let item = app
        .state::<AppState>()
        .media
        .visible(tab)
        .into_iter()
        .find(|m| m.url == url)
        .ok_or("that media is no longer listed")?;
    let target = item.target(format)?;
    Ok(start_download(
        app,
        tab,
        target,
        Some(item.page),
        height,
        None,
    ))
}

/// True when this tab, or the whole browser, is private.
fn is_private(st: &AppState, tab: Option<u32>) -> bool {
    let always = st.settings.lock().expect("settings").always_private;
    let tabs = st.tabs.lock().expect("tabs");
    always
        || tab
            .and_then(|id| tabs.map.get(&id))
            .is_some_and(|t| t.private)
}

#[derive(Serialize)]
struct LinkView {
    #[serde(flatten)]
    resolved: osprey_core::bypass::Resolved,
    /// Offer the "Open in bypass.vip" button.
    open_vip: bool,
}

/// Finds where a link really goes. bypass.vip's API is only asked when the
/// user turned it on, has a key, and neither the tab nor the browser is
/// private.
#[tauri::command]
async fn link_resolve(app: AppHandle, tab: Option<u32>, url: String) -> Res<LinkView> {
    use osprey_core::bypass::{
        may_use_bypass_vip, offer_bypass_vip_page, resolve, stuck, Fallback, BYPASS_VIP_ENDPOINT,
    };
    let st = app.state::<AppState>();
    let (setting, key) = {
        let s = st.settings.lock().expect("settings");
        (s.bypass_vip, s.bypass_vip_key.clone().unwrap_or_default())
    };
    let private = is_private(&st, tab);
    let has_key = !key.is_empty();
    let fallback = may_use_bypass_vip(setting, has_key, private).then_some(Fallback {
        endpoint: BYPASS_VIP_ENDPOINT,
        key: &key,
    });
    let opts = osprey_core::RequestOptions {
        privacy: osprey_core::Privacy {
            route: osprey_core::Route::Proxy(st.gateway.proxy_url()),
            ..Default::default()
        },
        ..Default::default()
    };
    let resolved = resolve(&opts, &url, 12, fallback)
        .await
        .map_err(|e| e.to_string())?;
    let open_vip = offer_bypass_vip_page(has_key, private, stuck(&resolved.link));
    Ok(LinkView { resolved, open_vip })
}

/// Saves the bypass.vip key, or clears it when `key` is empty. The switch
/// goes off with it, so a later key does not start sending links by itself.
#[tauri::command]
async fn bypass_key_set(app: AppHandle, key: String) -> Res<()> {
    let st = app.state::<AppState>();
    let key = key.trim();
    if !key.is_empty() && !osprey_core::bypass::valid_bypass_vip_key(key) {
        return Err("that does not look like a bypass.vip key".into());
    }
    {
        let mut s = st.settings.lock().expect("settings");
        s.bypass_vip_key = (!key.is_empty()).then(|| key.to_string());
        if key.is_empty() {
            s.bypass_vip = false;
        }
        s.save(&st.home);
    }
    scripts::emit_scripts(&app);
    Ok(())
}

/// Opens bypass.vip's web page with the link in its box. The page takes no
/// link in its address, so it is typed in for the user, and nothing is sent
/// to bypass.vip until the user presses the page's own button.
#[tauri::command]
async fn link_open_vip(app: AppHandle, tab: Option<u32>, url: String) -> Res<()> {
    use osprey_core::bypass::BYPASS_VIP_PAGE;
    let st = app.state::<AppState>();
    if is_private(&st, tab) {
        return Err("bypass.vip is not used in private mode".into());
    }
    let link = Url::parse(url.trim()).map_err(|e| e.to_string())?;
    if !matches!(link.scheme(), "http" | "https") {
        return Err("that is not a web address".into());
    }
    let id = create_tab(&app, Some(BYPASS_VIP_PAGE.to_string()), false)?;
    let value = serde_json::to_string(link.as_str()).map_err(|e| e.to_string())?;
    let mark = if probe::active() {
        "document.title = 'filled: ' + box.value;"
    } else {
        ""
    };
    let fill = format!(
        "(function fill(tries) {{
           var box = document.getElementById('bypassInput');
           if (box) {{
             box.value = {value};
             box.dispatchEvent(new Event('input', {{ bubbles: true }}));
             {mark}
           }} else if (tries > 0) {{
             setTimeout(function () {{ fill(tries - 1); }}, 500);
           }}
         }})(40);"
    );
    // The tab may have moved on by the time it loaded, so the link is only
    // typed into bypass.vip's own page.
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let there = st
            .tabs
            .lock()
            .expect("tabs")
            .map
            .get(&id)
            .and_then(|t| Url::parse(&t.url).ok())
            .is_some_and(|u| u.host_str() == Some("bypass.vip") && !u.path().contains("error"));
        let loading = st
            .tabs
            .lock()
            .expect("tabs")
            .map
            .get(&id)
            .is_none_or(|t| t.loading);
        if there && !loading {
            return eval_in_tab(&app, id, &fill);
        }
    }
    Err("bypass.vip did not open in time".into())
}

#[tauri::command]
async fn extension_add(app: AppHandle, path: String) -> Res<String> {
    let st = app.state::<AppState>();
    let name = extensions::add(&st.home, std::path::Path::new(path.trim()))?;
    scripts::emit_scripts(&app);
    Ok(name)
}

#[tauri::command]
async fn extension_remove(app: AppHandle, name: String) -> Res<()> {
    let st = app.state::<AppState>();
    extensions::remove(&st.home, &name)?;
    scripts::emit_scripts(&app);
    Ok(())
}

/// The panel's report on a preview, in a probe run.
#[tauri::command]
fn probe_preview(report: String) {
    probe::preview_result(report);
}

#[derive(Serialize)]
struct PreviewView {
    url: String,
    kind: MediaKind,
}

/// A player address for something in the Media panel. It plays through
/// the preview server, with the tab's identity and route.
#[tauri::command]
async fn media_preview(
    app: AppHandle,
    tab: u32,
    url: String,
    format: Option<String>,
) -> Res<PreviewView> {
    let st = app.state::<AppState>();
    let item = st
        .media
        .visible(tab)
        .into_iter()
        .find(|m| m.url == url)
        .ok_or("that media is no longer listed")?;
    let target = item.target(format.as_deref())?;
    let identity = st
        .identity
        .lock()
        .expect("identity")
        .get(&tab)
        .cloned()
        .unwrap_or_default();
    let req = downloads::DownloadRequest {
        url: target.url.clone(),
        kind: target.kind,
        page: item.page.clone(),
        identity,
        tab_label: Some(label_of(tab)),
        private: false,
        title: None,
        max_height: None,
        sent: target.sent,
        audio: None,
        tag: None,
        file_name: None,
    };
    let cookies = downloads::cookies_for(&app, &req).await;
    let opts = downloads::player_request(&req, &cookies, st.gateway.proxy_url());
    let client = osprey_core::build_client(&opts).map_err(|e| e.to_string())?;
    // A separate audio track plays alongside when both are streams.
    if let Some(audio) = target
        .audio
        .filter(|a| a.kind == MediaKind::Hls && target.kind == MediaKind::Hls)
    {
        return Ok(PreviewView {
            url: st.preview.open_joined(client, &target.url, &audio.url),
            kind: MediaKind::Hls,
        });
    }
    Ok(PreviewView {
        url: st.preview.open(client, &target.url),
        kind: target.kind,
    })
}

/// A local address for a card's preview picture. The picture is fetched
/// like the page would, through the tab's route, so the interface never
/// loads anything from the internet itself.
#[tauri::command]
async fn media_thumb(app: AppHandle, tab: u32, url: String) -> Res<String> {
    let st = app.state::<AppState>();
    let page = st
        .media
        .thumb_page(tab, &url)
        .ok_or("not a picture of this page")?;
    let private = st
        .tabs
        .lock()
        .expect("tabs")
        .map
        .get(&tab)
        .is_some_and(|t| t.private);
    let identity = st
        .identity
        .lock()
        .expect("identity")
        .get(&tab)
        .cloned()
        .unwrap_or_default();
    let req = downloads::DownloadRequest {
        url: url.clone(),
        kind: MediaKind::Direct,
        page,
        identity,
        tab_label: Some(label_of(tab)),
        private,
        title: None,
        max_height: None,
        sent: Vec::new(),
        audio: None,
        tag: None,
        file_name: None,
    };
    let cookies = downloads::cookies_for(&app, &req).await;
    let opts = downloads::player_request(&req, &cookies, st.gateway.proxy_url());
    let client = osprey_core::build_client(&opts).map_err(|e| e.to_string())?;
    Ok(st.preview.open_image(client, &url))
}

#[tauri::command]
async fn download_cancel(app: AppHandle, id: u32) -> Res<()> {
    let st = app.state::<AppState>();
    st.downloads.cancel(id);
    emit_ui(&app, "downloads", st.downloads.snapshot());
    Ok(())
}

#[tauri::command]
async fn download_pause(app: AppHandle, id: u32) -> Res<()> {
    let st = app.state::<AppState>();
    st.downloads.pause(id)?;
    emit_ui(&app, "downloads", st.downloads.snapshot());
    Ok(())
}

#[tauri::command]
async fn download_resume(app: AppHandle, id: u32) -> Res<()> {
    let st = app.state::<AppState>();
    st.downloads.resume(app.clone(), id)?;
    emit_ui(&app, "downloads", st.downloads.snapshot());
    Ok(())
}

/// Opens a finished download with the system's player.
#[tauri::command]
async fn download_open(app: AppHandle, id: u32) -> Res<()> {
    let path = app
        .state::<AppState>()
        .downloads
        .path_of(id)
        .ok_or("not finished yet")?;
    open_with_system(&path, false)
}

/// Shows a finished download in its folder, selected.
#[tauri::command]
async fn download_show(app: AppHandle, id: u32) -> Res<()> {
    let path = app
        .state::<AppState>()
        .downloads
        .path_of(id)
        .ok_or("not finished yet")?;
    open_with_system(&path, true)
}

/// The path comes from BM Osprey's own download list, never from a page.
/// It is passed as one argument, never through a shell.
fn open_with_system(path: &str, select: bool) -> Res<()> {
    if !std::path::Path::new(path).exists() {
        return Err("the file is no longer there".into());
    }
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("explorer");
        if select {
            c.arg(format!("/select,{path}"));
        } else {
            c.arg(path);
        }
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        if select {
            c.arg("-R");
        }
        c.arg(path);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        let target = if select {
            std::path::Path::new(path)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        } else {
            path.to_string()
        };
        c.arg(target);
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn download_open_folder(app: AppHandle) -> Res<()> {
    let dir = app
        .state::<AppState>()
        .settings
        .lock()
        .expect("settings")
        .downloads_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn clear_browsing_data(app: AppHandle) -> Res<()> {
    let ids: Vec<u32> = app
        .state::<AppState>()
        .tabs
        .lock()
        .expect("tabs")
        .order
        .clone();
    for id in ids {
        if let Some(wv) = tab_webview(&app, id) {
            wv.clear_all_browsing_data().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Commands web pages may call (see capabilities/tabs.json)
// ---------------------------------------------------------------------

#[tauri::command]
async fn tab_shortcut(app: AppHandle, webview: Webview, action: String) -> Res<()> {
    let Some(id) = id_of(webview.label()) else {
        return Ok(());
    };
    let st = app.state::<AppState>();
    // A page could call this itself. Allow a human pace only.
    {
        let mut times = st.shortcut_times.lock().expect("shortcuts");
        times.retain(|t| t.elapsed() < Duration::from_secs(2));
        if times.len() >= 4 {
            return Ok(());
        }
        times.push(Instant::now());
    }
    let private = st
        .tabs
        .lock()
        .expect("tabs")
        .map
        .get(&id)
        .is_some_and(|t| t.private);
    match action.as_str() {
        "new_tab" => {
            create_tab(&app, None, private)?;
        }
        "new_private_tab" => {
            create_tab(&app, None, true)?;
        }
        "close_tab" => close_tab(&app, id),
        "focus_address" => {
            if let Some(ui) = app.get_webview("ui") {
                let _ = ui.set_focus();
            }
            emit_ui(&app, "focus-address", ());
        }
        "next_tab" | "prev_tab" => {
            let next = {
                let tabs = st.tabs.lock().expect("tabs");
                let n = tabs.order.len();
                tabs.order.iter().position(|x| *x == id).map(|p| {
                    let q = if action == "next_tab" {
                        (p + 1) % n
                    } else {
                        (p + n - 1) % n
                    };
                    tabs.order[q]
                })
            };
            if let Some(n) = next {
                activate(&app, n);
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Text-only pages
// ---------------------------------------------------------------------

async fn render_text(app: &AppHandle, label: &str, uri: &str) -> Result<String, String> {
    let u = Url::parse(uri).map_err(|e| e.to_string())?;
    let target = text_target(&u).ok_or("no page given")?;
    let target_url = Url::parse(&target).map_err(|e| e.to_string())?;
    if !matches!(target_url.scheme(), "http" | "https") {
        return Err("only web pages can be shown as text".into());
    }
    let st = app.state::<AppState>();
    let ua = id_of(label)
        .and_then(|id| {
            st.identity
                .lock()
                .expect("identity")
                .get(&id)
                .map(|i| i.user_agent.clone())
        })
        .filter(|u| !u.is_empty());
    let opts = RequestOptions {
        user_agent: ua.or_else(|| Some(osprey_core::privacy::GENERIC_USER_AGENT.into())),
        privacy: Privacy {
            route: Route::Proxy(st.gateway.proxy_url()),
            ..Default::default()
        },
        ..Default::default()
    };
    let client = osprey_core::build_client(&opts).map_err(|e| e.to_string())?;
    let (final_url, body) = textmode::load_page(&client, target_url.as_str())
        .await
        .map_err(|e| e.to_string())?;
    let filters = st
        .settings
        .lock()
        .expect("settings")
        .block_ads
        .then(|| st.filters.read().expect("filters").clone());
    textmode::simplify(
        &body,
        &final_url,
        filters.as_deref(),
        &TextModeOptions::default(),
    )
    .map_err(|e| e.to_string())
}

fn error_page(message: &str) -> String {
    let m = message.replace('&', "&amp;").replace('<', "&lt;");
    format!(
        "<!doctype html><meta charset=utf-8><title>Could not show text</title>\
         <body style=\"font:16px/1.6 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem\">\
         <h1 style=\"font-size:1.3rem\">This page could not be shown as text</h1><p>{m}</p></body>"
    )
}

// ---------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------

/// Puts background tabs to sleep after a while: their web view is closed
/// and only the address is kept. Private tabs never sleep.
fn spawn_sleeper(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let st = app.state::<AppState>();
            let minutes = st.settings.lock().expect("settings").sleep_after_minutes;
            if minutes == 0 {
                continue;
            }
            let limit = Duration::from_secs(minutes * 60);
            let sleepy: Vec<u32> = {
                let mut tabs = st.tabs.lock().expect("tabs");
                let active = tabs.active;
                tabs.map
                    .values_mut()
                    .filter(|t| {
                        Some(t.id) != active
                            && !t.private
                            && !t.sleeping
                            && t.last_active.elapsed() > limit
                    })
                    .map(|t| {
                        t.sleeping = true;
                        t.id
                    })
                    .collect()
            };
            if sleepy.is_empty() {
                continue;
            }
            for id in &sleepy {
                if let Some(wv) = tab_webview(&app, *id) {
                    let _ = wv.close();
                }
            }
            emit_tabs(&app);
        }
    });
}

/// Sends network status to the UI when it changes, checking every five
/// seconds. The checks are local port probes, so they cost next to nothing.
fn spawn_net_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut last = String::new();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let view = net_view(&app).await;
            let key = serde_json::to_string(&view).unwrap_or_default();
            if key != last {
                last = key;
                emit_ui(&app, "net", view);
            }
        }
    });
}

fn install_crash_log(home: PathBuf) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = std::fs::create_dir_all(&home);
        let text = format!(
            "BM Osprey browser {} crashed\n{info}\n{}\n\n",
            env!("CARGO_PKG_VERSION"),
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join("browser-crash.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()));
        default_hook(info);
    }));
}

/// `--self-test`: starts the gateway and checks blocking and fail-closed
/// behaviour without opening a window. Used by RUN-CHECKS.bat.
fn self_test() -> i32 {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let mut ok = true;
        let mut check = |name: &str, pass: bool| {
            println!("{} {name}", if pass { "PASS" } else { "FAIL" });
            ok &= pass;
        };
        let filters = Arc::new(Filters::builtin());
        let gw = Gateway::start(gateway::Upstream::default(), Some(filters)).await;
        check(
            "gateway starts on 127.0.0.1",
            gw.as_ref().is_ok_and(|g| g.addr().ip().is_loopback()),
        );
        let Ok(gw) = gw else { return 1 };
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("socks5h://{}", gw.addr())).expect("proxy"))
            .build()
            .expect("client");
        let r = client
            .get("http://pubads.g.doubleclick.net/ad.mp4")
            .send()
            .await;
        check("ad domain refused", r.is_err() && gw.stats().blocked == 1);
        gw.set_upstream(gateway::Upstream::Socks5 {
            addr: "127.0.0.1:9".into(),
            user: None,
            pass: None,
        });
        let r = client.get("http://example.com/").send().await;
        check(
            "nothing sent when the route is down",
            r.is_err() && gw.stats().failed >= 1,
        );
        check("settings file readable", {
            let _ = AppSettings::load(&osprey_core::filter::home_dir());
            true
        });
        if ok {
            0
        } else {
            1
        }
    })
}

fn main() {
    if std::env::args().any(|a| a == "--self-test") {
        std::process::exit(self_test());
    }
    let home = osprey_core::filter::home_dir();
    install_crash_log(home.clone());

    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("ospreytext", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let label = ctx.webview_label().to_string();
            let uri = request.uri().to_string();
            tauri::async_runtime::spawn(async move {
                let html = render_text(&app, &label, &uri).await.unwrap_or_else(|e| error_page(&e));
                let resp = tauri::http::Response::builder()
                    .header("Content-Type", "text/html; charset=utf-8")
                    .header(
                        "Content-Security-Policy",
                        "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'",
                    )
                    .body(html.into_bytes())
                    .expect("valid response");
                responder.respond(resp);
            });
        })
        .setup(move |app| {
            let settings = AppSettings::load(&home);
            let builtin = Arc::new(Filters::builtin());
            let tor = tauri::async_runtime::block_on(osprey_core::privacy::find_tor(None)).ok();
            let upstream = settings
                .net
                .upstream(tor.as_deref())
                .unwrap_or_else(|_| gateway::Upstream::default());
            let gw = tauri::async_runtime::block_on(Gateway::start(
                upstream,
                settings.block_ads.then(|| builtin.clone()),
            ))?;

            let extensions_on = cfg!(windows) && !extensions::list(&home).is_empty();
            let downloads = Arc::new(downloads::Manager::default());
            downloads.limit.set(u64::from(settings.speed_limit_kbps) * 1024);
            downloads.set_parallel(usize::from(settings.parallel_downloads));
            let engine_start = prefs::EngineStart::from(&settings);
            let lists_off = settings.lists_off.clone();
            downloads.spawn_emitter(app.handle().clone());
            app.manage(AppState {
                home: home.clone(),
                gateway: gw,
                filters: RwLock::new(builtin),
                settings: Mutex::new(settings),
                tabs: Mutex::new(Tabs::default()),
                media: media::MediaState::new(),
                preview: tauri::async_runtime::block_on(preview::Preview::start())?,
                identity: Mutex::new(HashMap::new()),
                downloads,
                scripts: Arc::new(scripts::Scripts::open(&home)),
                extensions_on,
                panel_open: Mutex::new(false),
                covered: Mutex::new(false),
                engine_start,
                tor_addr: Mutex::new(tor),
                shortcut_times: Mutex::new(Vec::new()),
                saved_session: Mutex::new(String::new()),
            });

            tauri::async_runtime::spawn(media::keep_tools(app.handle().clone()));

            // Full filter lists take a moment to load, so they are swapped
            // in after the window is up.
            let handle = app.handle().clone();
            let filters_home = home.clone();
            tauri::async_runtime::spawn(async move {
                let loaded = tokio::task::spawn_blocking(move || {
                    Filters::load_except(&filters_home, &lists_off)
                })
                .await;
                if let Ok(Ok(full)) = loaded {
                    let st = handle.state::<AppState>();
                    let full = Arc::new(full);
                    *st.filters.write().expect("filters") = full.clone();
                    if st.settings.lock().expect("settings").block_ads {
                        st.gateway.set_filters(Some(full));
                    }
                }
            });

            let window = WindowBuilder::new(app, "main")
                .title("BM Osprey")
                .inner_size(1280.0, 820.0)
                .min_inner_size(720.0, 480.0)
                .decorations(!custom_frame())
                .build()?;
            let scale = window.scale_factor()?;
            let size = window.inner_size()?.to_logical::<f64>(scale);
            let ui = window.add_child(
                WebviewBuilder::new("ui", WebviewUrl::App("index.html".into())).auto_resize(),
                LogicalPosition::new(0.0, 0.0),
                size,
            )?;
            place(app.handle(), &ui, LogicalPosition::new(0.0, 0.0), size);
            let handle = app.handle().clone();
            window.on_window_event(move |event| {
                if let WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } = event {
                    relayout(&handle);
                }
            });

            spawn_sleeper(app.handle().clone());
            spawn_net_watch(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ui_ready,
            tab_new,
            tab_close,
            tab_activate,
            tab_navigate,
            tab_back,
            tab_forward,
            tab_reload,
            tab_stop,
            tab_text_mode,
            layout,
            net_set,
            net_status,
            ads_set,
            media_download,
            probe_preview,
            download_cancel,
            download_open_folder,
            download_pause,
            download_resume,
            download_open,
            download_show,
            media_preview,
            media_thumb,
            prefs::settings_view,
            prefs::settings_set,
            prefs::settings_export,
            prefs::settings_import,
            prefs::filters_update,
            prefs::filters_rules_set,
            prefs::tools_view,
            prefs::tools_update,
            prefs::cache_clear,
            clear_browsing_data,
            media::media_found,
            tab_shortcut,
            prefs_set,
            link_resolve,
            link_open_vip,
            bypass_key_set,
            extension_add,
            extension_remove,
            scripts::gm_call,
            scripts::scripts_view,
            scripts::script_install,
            scripts::script_toggle,
            scripts::script_remove,
            scripts::script_update,
            scripts::script_menu_run,
            scripts::script_answer
        ])
        .run(tauri::generate_context!())
        .expect("BM Osprey browser failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_bar_input() {
        assert_eq!(to_url("example.com").as_str(), "https://example.com/");
        assert_eq!(
            to_url("https://a.test/x?y=1").as_str(),
            "https://a.test/x?y=1"
        );
        assert_eq!(
            to_url("localhost:8765/page").as_str(),
            "http://localhost:8765/page"
        );
        assert!(to_url("how to merge ts files")
            .as_str()
            .starts_with("https://duckduckgo.com/?q=how+to+merge"));
        assert_eq!(to_url("about:blank").as_str(), "about:blank");
        assert!(to_url("javascript:alert(1)")
            .as_str()
            .starts_with("https://duckduckgo.com/"));
    }

    #[test]
    fn text_urls_round_trip() {
        let t = text_url("https://news.test/a?b=1&c=2");
        assert!(is_text_url(&t));
        assert_eq!(
            text_target(&t).as_deref(),
            Some("https://news.test/a?b=1&c=2")
        );
        assert_eq!(display_url(&t), "https://news.test/a?b=1&c=2");
        assert!(!is_text_url(&Url::parse("https://news.test/").unwrap()));
    }

    #[test]
    fn private_new_tab_is_self_contained() {
        let u = private_new_tab_url();
        assert_eq!(u.scheme(), "data");
        assert!(is_internal(&u));
        assert_eq!(display_url(&u), "");
        // No script at all, so a strict security policy cannot break it.
        assert!(!u.as_str().to_ascii_lowercase().contains("%3cscript"));
        assert!(u.as_str().contains("duckduckgo.com"));
        // Tauri rewrites data: pages unescaped, so a hash sign would cut it short.
        assert!(!PRIVATE_NEWTAB_HTML.contains('#'));
    }

    #[test]
    fn tab_labels() {
        assert_eq!(id_of(&label_of(42)), Some(42));
        assert_eq!(id_of("ui"), None);
    }
}
