//! The script manager in the browser: builds the start up scripts for a tab
//! and answers the GM_ functions.
//!
//! A web page can reach one command here, `gm_call`. Page code can watch the
//! calls a script makes, so a token alone is not treated as proof. Each call
//! is checked against what the script's header asked for: the token must
//! belong to the tab it came from, the function must be in `@grant`, and a
//! network request must go to a host `@connect` names or the user allowed.
//! Scripts never run in private tabs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures::StreamExt;
use osprey_core::userscript::{self, Installed, Loaded, Store};
use osprey_core::{MediaKind, RequestOptions};
use reqwest::header::{HeaderMap, CONTENT_TYPE, LOCATION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager as _};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{oneshot, Notify};
use url::Url;

use crate::{downloads, emit_ui, label_of, media, probe, start_download, AppState, Res};

const BOOTSTRAP_JS: &str = include_str!("../scripts/userscripts.js");

/// A response kept for its script to read, in pieces.
const BODY_PIECE: usize = 2 * 1024 * 1024;
const MAX_BODY: usize = 1024 * 1024 * 1024;
const BODY_LIFETIME: Duration = Duration::from_secs(300);
const MAX_REDIRECTS: usize = 10;
/// How long a question about a new host stays open.
const ASK_TIMEOUT: Duration = Duration::from_secs(90);
const MENU_WAIT: Duration = Duration::from_secs(25);
const TABS_PER_MINUTE: usize = 10;

struct Token {
    script: String,
    tab: u32,
}

struct Body {
    tab: u32,
    bytes: Vec<u8>,
    made: Instant,
}

struct Save {
    tab: u32,
    job: u32,
    temp: PathBuf,
    path: PathBuf,
    file: tokio::fs::File,
    written: u64,
}

/// A script that ran on the tab's page.
#[derive(Debug, Clone, Serialize)]
pub struct Ran {
    pub id: String,
    pub name: String,
    pub url: String,
    pub frame: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MenuEntry {
    pub script: String,
    pub name: String,
    pub instance: String,
    pub id: String,
    pub caption: String,
    #[serde(skip)]
    pub token: String,
}

#[derive(Debug, Clone, Serialize)]
struct Ask {
    ask: u32,
    script: String,
    name: String,
    host: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Once,
    Always,
    Deny,
}

/// Something a probe reports about a script.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeNote {
    pub script: String,
    pub note: String,
}

pub struct Scripts {
    pub store: Store,
    /// Goes up when scripts change. A tab built before that has old scripts
    /// and gets a new web view at its next reload.
    generation: AtomicU64,
    tokens: Mutex<HashMap<String, Token>>,
    ran: Mutex<HashMap<u32, Vec<Ran>>>,
    menus: Mutex<HashMap<u32, Vec<MenuEntry>>>,
    menu_queue: Mutex<HashMap<String, VecDeque<String>>>,
    menu_ring: Notify,
    asks: Mutex<HashMap<u32, oneshot::Sender<Answer>>>,
    open_asks: Mutex<Vec<Ask>>,
    once: Mutex<HashSet<(String, String)>>,
    next: AtomicU32,
    bodies: Mutex<HashMap<u32, Body>>,
    saves: tokio::sync::Mutex<HashMap<u32, Save>>,
    aborts: Mutex<HashMap<String, oneshot::Sender<()>>>,
    opened: Mutex<VecDeque<Instant>>,
    pub probe_notes: Mutex<Vec<ProbeNote>>,
}

impl Scripts {
    pub fn open(home: &std::path::Path) -> Self {
        Self {
            store: Store::open(home),
            generation: AtomicU64::new(1),
            tokens: Mutex::new(HashMap::new()),
            ran: Mutex::new(HashMap::new()),
            menus: Mutex::new(HashMap::new()),
            menu_queue: Mutex::new(HashMap::new()),
            menu_ring: Notify::new(),
            asks: Mutex::new(HashMap::new()),
            open_asks: Mutex::new(Vec::new()),
            once: Mutex::new(HashSet::new()),
            next: AtomicU32::new(1),
            bodies: Mutex::new(HashMap::new()),
            saves: tokio::sync::Mutex::new(HashMap::new()),
            aborts: Mutex::new(HashMap::new()),
            opened: Mutex::new(VecDeque::new()),
            probe_notes: Mutex::new(Vec::new()),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Forgets what the last page of a tab did.
    pub fn new_page(&self, tab: u32) {
        self.ran.lock().expect("ran").remove(&tab);
        self.menus.lock().expect("menus").remove(&tab);
        self.bodies
            .lock()
            .expect("bodies")
            .retain(|_, b| b.tab != tab);
    }

    /// Forgets a tab whose web view was closed.
    pub fn drop_tab(&self, tab: u32) {
        self.new_page(tab);
        self.tokens
            .lock()
            .expect("tokens")
            .retain(|_, t| t.tab != tab);
    }

    fn note(&self, script: &str, note: String) {
        if probe::active() {
            self.probe_notes.lock().expect("notes").push(ProbeNote {
                script: script.to_string(),
                note,
            });
        }
    }

    pub fn ran_on(&self, tab: u32) -> Vec<Ran> {
        self.ran
            .lock()
            .expect("ran")
            .get(&tab)
            .cloned()
            .unwrap_or_default()
    }

    fn menus_on(&self, tab: u32) -> Vec<MenuEntry> {
        self.menus
            .lock()
            .expect("menus")
            .get(&tab)
            .cloned()
            .unwrap_or_default()
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    // A failure here would be a broken system. The token then only stays
    // empty of randomness for this one tab, so nothing runs with it.
    let _ = getrandom::fill(&mut bytes);
    hex::encode(bytes)
}

fn script_config(loaded: &Loaded, token: &str) -> Value {
    let meta = &loaded.installed.meta;
    json!({
        "token": token,
        "include": meta.include_patterns(),
        "exclude": meta.exclude_patterns(),
        "run_at": meta.run_at,
        "noframes": meta.noframes,
        "in_page": meta.runs_in_page(),
        "grants": meta.grants.iter().map(|g| userscript::grant_base(g)).collect::<Vec<_>>(),
        "info": {
            "scriptHandler": "BM Osprey",
            "version": env!("CARGO_PKG_VERSION"),
            "script": {
                "name": meta.name,
                "namespace": meta.namespace,
                "version": meta.version,
                "description": meta.description,
                "author": meta.author,
                "matches": meta.matches,
                "includes": meta.includes,
                "excludes": meta.excludes,
                "grant": meta.grants,
                "requires": meta.requires,
                "run-at": meta.run_at,
            },
        },
    })
}

/// Wraps one script for the page. Its own text cannot reach the token or
/// the runner, only the functions it is handed.
fn wrap(loaded: &Loaded, key: &str, token: &str) -> String {
    let config = script_config(loaded, token);
    let requires = loaded.requires.join("\n;\n");
    let source = &loaded.source;
    let key = serde_json::to_string(key).unwrap_or_default();
    if loaded.installed.meta.runs_in_page() {
        format!(
            "(function(){{var B=window[{key}];if(!B)return;B.add({config},\
             function(GM_info,GM,unsafeWindow,define,module,exports){{(function(){{\n{requires}\n{source}\n}}).call(window);}});}})();"
        )
    } else {
        format!(
            "(function(){{var B=window[{key}];if(!B)return;B.add({config},\
             function(__osprey_scope,__osprey_self){{with(__osprey_scope){{(function(){{\n{requires}\n{source}\n}}).call(__osprey_self);}}}});}})();"
        )
    }
}

/// The start up scripts for a new tab, in order. Nothing at all when no
/// script is on, so pages are not touched for no reason.
pub fn init_scripts(app: &AppHandle, tab: u32) -> Vec<String> {
    let st = app.state::<AppState>();
    let scripts = &st.scripts;
    let key = format!("__osprey_{}", random_token());
    let mut out = Vec::new();
    let mut made = Vec::new();
    for item in scripts.store.list().iter().filter(|s| s.enabled) {
        let mut loaded = match scripts.store.load(&item.id) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("script {} not loaded: {e}", item.meta.name);
                continue;
            }
        };
        if let Some(extra) = probe::config().and_then(|c| c.extra_match.clone()) {
            loaded.installed.meta.matches.push(extra);
        }
        let token = random_token();
        out.push(wrap(&loaded, &key, &token));
        scripts.tokens.lock().expect("tokens").insert(
            token.clone(),
            Token {
                script: item.id.clone(),
                tab,
            },
        );
        made.push(token);
    }
    if out.is_empty() {
        return out;
    }
    let boot = BOOTSTRAP_JS.replace("__OSPREY_KEY__", &key).replace(
        "__OSPREY_PROBE__",
        if probe::active() { "true" } else { "false" },
    );
    let list = serde_json::to_string(&made).unwrap_or_default();
    let key = serde_json::to_string(&key).unwrap_or_default();
    let end = format!("(function(){{var B=window[{key}];if(B)B.finish({list});}})();");
    let mut all = vec![boot];
    all.append(&mut out);
    all.push(end);
    all
}

fn host_of(url: &Url) -> Option<String> {
    url.host_str().map(str::to_ascii_lowercase)
}

/// True when a script may talk to `host`, asking the user the first time.
async fn may_connect(app: &AppHandle, item: &Installed, host: &str, page: Option<&str>) -> bool {
    let st = app.state::<AppState>();
    if item.meta.connect_allows(host, page) || item.allowed_hosts.iter().any(|h| h == host) {
        return true;
    }
    let once = (item.id.clone(), host.to_string());
    if st.scripts.once.lock().expect("once").contains(&once) {
        return true;
    }
    let ask = st.scripts.next.fetch_add(1, Ordering::Relaxed);
    let (send, receive) = oneshot::channel();
    st.scripts.asks.lock().expect("asks").insert(ask, send);
    let question = Ask {
        ask,
        script: item.id.clone(),
        name: item.meta.name.clone(),
        host: host.to_string(),
    };
    st.scripts
        .open_asks
        .lock()
        .expect("asks")
        .push(question.clone());
    emit_ui(app, "script-ask", question);
    st.scripts.note(&item.id, format!("asked about {host}"));
    probe::answer_ask(app, ask);
    let answer = tokio::time::timeout(ASK_TIMEOUT, receive).await;
    st.scripts.asks.lock().expect("asks").remove(&ask);
    st.scripts
        .open_asks
        .lock()
        .expect("asks")
        .retain(|a| a.ask != ask);
    match answer {
        Ok(Ok(Answer::Always)) => {
            let saved = st.scripts.store.allow_host(&item.id, host);
            st.scripts.note(&item.id, format!("always allowed {host}"));
            saved.is_ok()
        }
        Ok(Ok(Answer::Once)) => {
            st.scripts.once.lock().expect("once").insert(once);
            true
        }
        _ => {
            st.scripts.note(&item.id, format!("{host} was not allowed"));
            false
        }
    }
}

#[derive(Deserialize)]
struct XhrBody {
    text: Option<String>,
    data: Option<String>,
    #[serde(rename = "type")]
    content_type: Option<String>,
}

#[derive(Deserialize)]
struct XhrArgs {
    rid: String,
    url: String,
    method: String,
    #[serde(default)]
    headers: HashMap<String, Value>,
    body: Option<XhrBody>,
    #[serde(default)]
    timeout: u64,
    #[serde(default)]
    anonymous: bool,
}

fn header_list(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn page_host(app: &AppHandle, tab: u32) -> Option<String> {
    let st = app.state::<AppState>();
    let tabs = st.tabs.lock().expect("tabs");
    let page = tabs.map.get(&tab)?;
    Url::parse(&page.url).ok().and_then(|u| host_of(&u))
}

/// Sends a script's request through the browser's route. Redirects are
/// followed here so that each new host is checked as well.
async fn xhr(app: &AppHandle, item: &Installed, tab: u32, a: XhrArgs) -> Res<Value> {
    let st = app.state::<AppState>();
    let page = page_host(app, tab);
    let identity = st
        .identity
        .lock()
        .expect("identity")
        .get(&tab)
        .cloned()
        .unwrap_or_default();
    let opts = RequestOptions {
        user_agent: Some(identity.user_agent).filter(|u| !u.is_empty()),
        privacy: osprey_core::Privacy {
            route: osprey_core::Route::Proxy(st.gateway.proxy_url()),
            ..Default::default()
        },
        ..Default::default()
    };
    let client = osprey_core::client::build_client_no_redirect(&opts).map_err(|e| e.to_string())?;
    let method = reqwest::Method::from_bytes(a.method.to_ascii_uppercase().as_bytes())
        .map_err(|_| "that request method is not valid".to_string())?;
    let body: Option<Vec<u8>> = match &a.body {
        Some(XhrBody { text: Some(t), .. }) => Some(t.clone().into_bytes()),
        Some(XhrBody { data: Some(d), .. }) => Some(
            base64::engine::general_purpose::STANDARD
                .decode(d)
                .map_err(|e| e.to_string())?,
        ),
        _ => None,
    };
    let body_type = a.body.as_ref().and_then(|b| b.content_type.clone());

    let (stop, stopped) = oneshot::channel();
    st.scripts
        .aborts
        .lock()
        .expect("aborts")
        .insert(a.rid.clone(), stop);
    let work = fetch_following(app, item, tab, page, &client, method, &a, body, body_type);
    let limit = if a.timeout == 0 {
        Duration::from_secs(3600)
    } else {
        Duration::from_millis(a.timeout)
    };
    let result = tokio::select! {
        r = tokio::time::timeout(limit, work) => match r {
            Ok(r) => r,
            Err(_) => Err("the request timed out".to_string()),
        },
        _ = stopped => Err("aborted".to_string()),
    };
    st.scripts.aborts.lock().expect("aborts").remove(&a.rid);
    let (status, status_text, final_url, headers, bytes) = result?;

    let content_type = headers
        .iter()
        .find(|(k, _)| k == CONTENT_TYPE.as_str())
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let id = st.scripts.next.fetch_add(1, Ordering::Relaxed);
    let size = bytes.len();
    {
        let mut bodies = st.scripts.bodies.lock().expect("bodies");
        bodies.retain(|_, b| b.made.elapsed() < BODY_LIFETIME);
        bodies.insert(
            id,
            Body {
                tab,
                bytes,
                made: Instant::now(),
            },
        );
    }
    Ok(json!({
        "id": id,
        "size": size,
        "status": status,
        "status_text": status_text,
        "final_url": final_url,
        "headers": headers,
        "content_type": content_type,
    }))
}

type Fetched = (u16, String, String, Vec<(String, String)>, Vec<u8>);

#[allow(clippy::too_many_arguments)]
async fn fetch_following(
    app: &AppHandle,
    item: &Installed,
    tab: u32,
    page: Option<String>,
    client: &reqwest::Client,
    method: reqwest::Method,
    a: &XhrArgs,
    body: Option<Vec<u8>>,
    body_type: Option<String>,
) -> Res<Fetched> {
    let mut url = Url::parse(&a.url).map_err(|e| e.to_string())?;
    let first_host = host_of(&url);
    let mut method = method;
    let mut body = body;
    for _ in 0..=MAX_REDIRECTS {
        if !matches!(url.scheme(), "http" | "https") {
            return Err("a script can only reach web addresses".into());
        }
        let host = host_of(&url).ok_or("that address has no host")?;
        if !may_connect(app, item, &host, page.as_deref()).await {
            return Err(format!("{host} is not allowed for this script"));
        }
        let mut req = client.request(method.clone(), url.clone());
        let same_host = first_host.as_deref() == Some(host.as_str());
        let mut has_cookie = false;
        for (name, value) in &a.headers {
            let lower = name.to_ascii_lowercase();
            // Secrets must not follow a redirect to another site.
            if !same_host && matches!(lower.as_str(), "authorization" | "cookie") {
                continue;
            }
            has_cookie |= lower == "cookie";
            if let Some(text) = value.as_str() {
                req = req.header(name.as_str(), text);
            }
        }
        if !a.anonymous && !has_cookie {
            let label = label_of(tab);
            let cookies = downloads::tab_cookies(app, Some(&label), url.as_str()).await;
            if !cookies.is_empty() {
                req = req.header(reqwest::header::COOKIE, cookies);
            }
        }
        if let Some(b) = &body {
            req = req.body(b.clone());
            if let Some(t) = &body_type {
                req = req.header(CONTENT_TYPE, t.as_str());
            }
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if status.is_redirection() {
            if let Some(next) = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok())
            {
                if matches!(status.as_u16(), 301..=303) {
                    method = reqwest::Method::GET;
                    body = None;
                }
                url = next;
                continue;
            }
        }
        let headers = header_list(resp.headers());
        let text = status.canonical_reason().unwrap_or("").to_string();
        let mut bytes = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(piece) = stream.next().await {
            let piece = piece.map_err(|e| e.to_string())?;
            if bytes.len() + piece.len() > MAX_BODY {
                return Err("the response is too big for a script".into());
            }
            bytes.extend_from_slice(&piece);
        }
        return Ok((status.as_u16(), text, url.to_string(), headers, bytes));
    }
    Err("too many redirects".into())
}

fn take_piece(st: &Scripts, tab: u32, id: u32, offset: usize) -> Res<String> {
    let mut bodies = st.bodies.lock().expect("bodies");
    let body = bodies
        .get(&id)
        .filter(|b| b.tab == tab)
        .ok_or("that response is gone")?;
    let end = (offset + BODY_PIECE).min(body.bytes.len());
    let piece = body.bytes.get(offset..end).ok_or("bad offset")?;
    let text = base64::engine::general_purpose::STANDARD.encode(piece);
    if end >= body.bytes.len() {
        bodies.remove(&id);
    }
    Ok(text)
}

/// A free name in the downloads folder. The folder is chosen here, a script
/// only gives the name, so a path in it goes no further than that folder.
fn clean_save_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    osprey_core::sanitize_filename(last)
}

fn save_path(app: &AppHandle, name: &str) -> PathBuf {
    let st = app.state::<AppState>();
    let dir = {
        let s = st.settings.lock().expect("settings");
        probe::download_dir().unwrap_or_else(|| s.downloads_dir())
    };
    let clean = clean_save_name(name);
    let first = dir.join(&clean);
    if !first.exists() {
        return first;
    }
    let path = std::path::Path::new(&clean);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 1..10_000 {
        let numbered = match &ext {
            Some(e) => format!("{} ({n}).{e}", stem.as_deref().unwrap_or("download")),
            None => format!("{clean} ({n})"),
        };
        if !dir.join(&numbered).exists() {
            return dir.join(numbered);
        }
    }
    first
}

async fn save_begin(app: &AppHandle, tab: u32, name: &str, size: u64) -> Res<u32> {
    let st = app.state::<AppState>();
    let path = save_path(app, name);
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| e.to_string())?;
    }
    let temp = path.with_extension(format!(
        "{}.part",
        st.scripts.next.fetch_add(1, Ordering::Relaxed)
    ));
    let file = tokio::fs::File::create(&temp)
        .await
        .map_err(|e| e.to_string())?;
    let shown = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let job = st.downloads.begin_local(shown, size);
    emit_ui(app, "downloads", st.downloads.snapshot());
    let id = st.scripts.next.fetch_add(1, Ordering::Relaxed);
    st.scripts.saves.lock().await.insert(
        id,
        Save {
            tab,
            job,
            temp,
            path,
            file,
            written: 0,
        },
    );
    Ok(id)
}

async fn save_write(app: &AppHandle, tab: u32, id: u32, position: u64, data: &str) -> Res<()> {
    let st = app.state::<AppState>();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())?;
    let mut saves = st.scripts.saves.lock().await;
    let save = saves
        .get_mut(&id)
        .filter(|s| s.tab == tab)
        .ok_or("that save is gone")?;
    if st.downloads.is_cancelled(save.job) {
        return Err("the download was cancelled".into());
    }
    save.file
        .seek(std::io::SeekFrom::Start(position))
        .await
        .map_err(|e| e.to_string())?;
    save.file
        .write_all(&bytes)
        .await
        .map_err(|e| e.to_string())?;
    save.written = save.written.max(position + bytes.len() as u64);
    st.downloads.local_progress(save.job, save.written);
    Ok(())
}

async fn save_end(app: &AppHandle, tab: u32, id: u32, keep: bool) -> Res<()> {
    let st = app.state::<AppState>();
    let save = {
        let mut saves = st.scripts.saves.lock().await;
        match saves.get(&id) {
            Some(s) if s.tab == tab => saves.remove(&id),
            _ => None,
        }
    }
    .ok_or("that save is gone")?;
    let Save {
        job,
        temp,
        path,
        mut file,
        ..
    } = save;
    let flushed = file.flush().await;
    drop(file);
    if !keep || st.downloads.is_cancelled(job) {
        let _ = tokio::fs::remove_file(&temp).await;
        st.downloads.local_failed(job, "stopped before it finished");
        emit_ui(app, "downloads", st.downloads.snapshot());
        return Ok(());
    }
    let done = match flushed {
        Ok(()) => tokio::fs::rename(&temp, &path).await,
        Err(e) => Err(e),
    };
    match done {
        Ok(()) => st
            .downloads
            .local_done(job, path.to_string_lossy().into_owned()),
        Err(e) => st.downloads.local_failed(job, &e.to_string()),
    }
    emit_ui(app, "downloads", st.downloads.snapshot());
    Ok(())
}

/// Ends saves whose page is gone. It can never finish them, and until now
/// each one holds a file open and shows as running.
pub async fn abandon_saves(app: &AppHandle, tab: u32) {
    let st = app.state::<AppState>();
    let gone: Vec<u32> = {
        let saves = st.scripts.saves.lock().await;
        saves
            .iter()
            .filter(|(_, s)| s.tab == tab)
            .map(|(id, _)| *id)
            .collect()
    };
    for id in gone {
        let _ = save_end(app, tab, id, false).await;
    }
}

fn may_open_tab(st: &Scripts) -> bool {
    let mut opened = st.opened.lock().expect("opened");
    while opened
        .front()
        .is_some_and(|t| t.elapsed() > Duration::from_secs(60))
    {
        opened.pop_front();
    }
    if opened.len() >= TABS_PER_MINUTE {
        return false;
    }
    opened.push_back(Instant::now());
    true
}

/// The grant a call needs, or `None` for what every script may do.
fn needed_grant(op: &str) -> Option<&'static [&'static str]> {
    const VALUES: &[&str] = &["getvalue", "setvalue", "deletevalue", "listvalues"];
    match op {
        "values" => Some(VALUES),
        "value_set" => Some(&["setvalue", "deletevalue"]),
        "xhr" | "xhr_body" | "xhr_abort" => Some(&["xmlhttprequest"]),
        "download" | "download_state" | "download_cancel" => Some(&["download"]),
        "menu_add" | "menu_remove" | "menu_wait" => {
            Some(&["registermenucommand", "unregistermenucommand"])
        }
        "open_tab" => Some(&["openintab"]),
        _ => None,
    }
}

fn text(args: &Value, key: &str) -> String {
    args[key].as_str().unwrap_or_default().to_string()
}

fn number(args: &Value, key: &str) -> Res<u32> {
    args[key]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("{key} is missing"))
}

/// The one command a web page can reach.
#[tauri::command]
pub async fn gm_call(
    app: AppHandle,
    webview: tauri::Webview,
    token: String,
    op: String,
    args: Value,
) -> Res<Value> {
    let st = app.state::<AppState>();
    let tab = webview
        .label()
        .strip_prefix("tab-")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or("not a tab")?;
    let id = {
        let tokens = st.scripts.tokens.lock().expect("tokens");
        let known = tokens.get(&token).filter(|t| t.tab == tab);
        known.map(|t| t.script.clone()).ok_or("unknown script")?
    };
    let item = st.scripts.store.get(&id).ok_or("unknown script")?;
    if let Some(any_of) = needed_grant(&op) {
        if !any_of.iter().any(|g| item.meta.has_grant(g)) {
            return Err(format!("{} did not ask for this", item.meta.name));
        }
    }
    let scripts = &st.scripts;
    match op.as_str() {
        "ran" => {
            let entry = Ran {
                id: id.clone(),
                name: item.meta.name.clone(),
                url: text(&args, "url"),
                frame: args["frame"].as_bool().unwrap_or(false),
                error: args["error"].as_str().map(str::to_string),
            };
            scripts
                .ran
                .lock()
                .expect("ran")
                .entry(tab)
                .or_default()
                .push(entry);
            emit_scripts(&app);
            Ok(Value::Null)
        }
        "problem" => {
            scripts.note(&id, format!("error: {}", text(&args, "message")));
            Ok(Value::Null)
        }
        "values" => Ok(json!(scripts.store.values(&id))),
        "value_set" => {
            let value = (!args["remove"].as_bool().unwrap_or(false)).then(|| args["value"].clone());
            scripts
                .store
                .set_value(&id, &text(&args, "key"), value)
                .map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "xhr" => {
            let a: XhrArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
            xhr(&app, &item, tab, a).await
        }
        "xhr_body" => {
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            take_piece(scripts, tab, number(&args, "id")?, offset).map(Value::String)
        }
        "xhr_abort" => {
            let stop = scripts
                .aborts
                .lock()
                .expect("aborts")
                .remove(&text(&args, "rid"));
            if let Some(stop) = stop {
                let _ = stop.send(());
            }
            Ok(Value::Null)
        }
        "download" => start_script_download(&app, &item, tab, &args).await,
        "download_state" => {
            let job = st
                .downloads
                .snapshot()
                .into_iter()
                .find(|j| j.id == number(&args, "id").unwrap_or(0))
                .ok_or("that download is gone")?;
            let state = match job.state {
                downloads::JobState::Done => "done",
                downloads::JobState::Failed => "failed",
                downloads::JobState::Cancelled => "cancelled",
                _ => "running",
            };
            Ok(json!({ "state": state, "done": job.done, "total": job.total, "error": job.error }))
        }
        "download_cancel" => {
            st.downloads.cancel(number(&args, "id")?);
            Ok(Value::Null)
        }
        "save_begin" => {
            let size = args["size"].as_u64().unwrap_or(0);
            save_begin(&app, tab, &text(&args, "name"), size)
                .await
                .map(|n| json!(n))
        }
        "save_write" => {
            let position = args["position"].as_u64().unwrap_or(0);
            let data = text(&args, "data");
            save_write(&app, tab, number(&args, "id")?, position, &data).await?;
            Ok(Value::Null)
        }
        "save_end" => {
            save_end(&app, tab, number(&args, "id")?, true).await?;
            Ok(Value::Null)
        }
        "save_abort" => {
            save_end(&app, tab, number(&args, "id")?, false).await?;
            Ok(Value::Null)
        }
        "menu_add" => {
            let entry = MenuEntry {
                script: id,
                name: item.meta.name.clone(),
                instance: text(&args, "instance"),
                id: text(&args, "id"),
                caption: text(&args, "caption"),
                token,
            };
            scripts
                .menus
                .lock()
                .expect("menus")
                .entry(tab)
                .or_default()
                .push(entry);
            emit_scripts(&app);
            Ok(Value::Null)
        }
        "menu_remove" => {
            let (instance, entry) = (text(&args, "instance"), text(&args, "id"));
            if let Some(list) = scripts.menus.lock().expect("menus").get_mut(&tab) {
                list.retain(|m| !(m.instance == instance && m.id == entry));
            }
            emit_scripts(&app);
            Ok(Value::Null)
        }
        "menu_wait" => menu_wait(scripts, &token, &text(&args, "instance")).await,
        "open_tab" => {
            let url = Url::parse(&text(&args, "url")).map_err(|e| e.to_string())?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err("only web addresses can be opened".into());
            }
            if !may_open_tab(scripts) {
                return Err("too many tabs opened at once".into());
            }
            let active = args["active"].as_bool().unwrap_or(true);
            crate::create_script_tab(&app, url.to_string(), active)?;
            Ok(Value::Null)
        }
        _ => Err("unknown function".into()),
    }
}

async fn menu_wait(scripts: &Scripts, token: &str, instance: &str) -> Res<Value> {
    let key = format!("{token}:{instance}");
    let wait = async {
        loop {
            let ring = scripts.menu_ring.notified();
            let found = {
                let mut queue = scripts.menu_queue.lock().expect("menu");
                queue.get_mut(&key).and_then(VecDeque::pop_front)
            };
            if let Some(id) = found {
                return id;
            }
            ring.await;
        }
    };
    Ok(tokio::time::timeout(MENU_WAIT, wait)
        .await
        .map_or(Value::Null, Value::String))
}

async fn start_script_download(
    app: &AppHandle,
    item: &Installed,
    tab: u32,
    args: &Value,
) -> Res<Value> {
    let st = app.state::<AppState>();
    let url = Url::parse(&text(args, "url")).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("a script can only download web addresses".into());
    }
    let sent: Vec<(String, String)> = args["headers"]
        .as_object()
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let target = media::Target {
        url: url.to_string(),
        kind: MediaKind::Direct,
        sent,
        ..Default::default()
    };
    let name = args["name"].as_str().filter(|n| !n.trim().is_empty());
    let id = start_download(app, tab, target, None, None, name.map(str::to_string));
    st.scripts
        .note(&item.id, format!("download {id} started for {url}"));
    Ok(json!(id))
}

// ---------------------------------------------------------------------
// Commands for the panel
// ---------------------------------------------------------------------

#[derive(Serialize, Clone)]
pub struct ScriptView {
    id: String,
    name: String,
    version: String,
    description: String,
    enabled: bool,
    can_update: bool,
}

#[derive(Serialize, Clone)]
pub struct ScriptsView {
    scripts: Vec<ScriptView>,
    ran: Vec<Ran>,
    menus: Vec<MenuEntry>,
    asks: Vec<Value>,
    tab: Option<u32>,
    /// The active tab has older scripts, and a reload picks up the new ones.
    stale: bool,
    private: bool,
    bypass_vip: bool,
    /// Only whether a key is saved. The key itself stays on the Rust side.
    bypass_vip_key_set: bool,
    extensions: Vec<String>,
    extensions_supported: bool,
}

fn view(app: &AppHandle) -> ScriptsView {
    let st = app.state::<AppState>();
    let (tab, stale, private) = {
        let tabs = st.tabs.lock().expect("tabs");
        let tab = tabs.active;
        let t = tab.and_then(|id| tabs.map.get(&id));
        (
            tab,
            t.is_some_and(|t| !t.private && t.script_gen != st.scripts.generation()),
            t.is_some_and(|t| t.private),
        )
    };
    let scripts = st
        .scripts
        .store
        .list()
        .into_iter()
        .map(|s| ScriptView {
            can_update: s.meta.update_url.is_some()
                || s.meta.download_url.is_some()
                || s.source_url.is_some(),
            id: s.id,
            name: s.meta.name,
            version: s.meta.version,
            description: s.meta.description,
            enabled: s.enabled,
        })
        .collect();
    let (settings_vip, key_set) = {
        let s = st.settings.lock().expect("settings");
        (s.bypass_vip, s.bypass_vip_key.is_some())
    };
    let asks: Vec<Value> = st
        .scripts
        .open_asks
        .lock()
        .expect("asks")
        .iter()
        .filter_map(|a| serde_json::to_value(a).ok())
        .collect();
    ScriptsView {
        scripts,
        ran: tab.map(|t| st.scripts.ran_on(t)).unwrap_or_default(),
        menus: tab.map(|t| st.scripts.menus_on(t)).unwrap_or_default(),
        asks,
        tab,
        stale,
        private,
        bypass_vip: settings_vip,
        bypass_vip_key_set: key_set,
        extensions: crate::extensions::list(&st.home),
        extensions_supported: cfg!(windows),
    }
}

pub fn emit_scripts(app: &AppHandle) {
    emit_ui(app, "scripts", view(app));
}

fn client_for(app: &AppHandle) -> Res<reqwest::Client> {
    let st = app.state::<AppState>();
    let opts = RequestOptions {
        privacy: osprey_core::Privacy {
            route: osprey_core::Route::Proxy(st.gateway.proxy_url()),
            ..Default::default()
        },
        ..Default::default()
    };
    osprey_core::build_client(&opts).map_err(|e| e.to_string())
}

/// Installs from pasted text or a file's text, or from a web address.
pub async fn install(app: &AppHandle, source: Option<String>, url: Option<String>) -> Res<String> {
    let st = app.state::<AppState>();
    let client = client_for(app)?;
    let text = match (&source, &url) {
        (Some(s), _) if !s.trim().is_empty() => s.clone(),
        (_, Some(u)) if !u.trim().is_empty() => userscript::fetch_text(&client, u.trim())
            .await
            .map_err(|e| e.to_string())?,
        _ => return Err("nothing to install".into()),
    };
    let item = st
        .scripts
        .store
        .install(&client, &text, url.filter(|u| !u.trim().is_empty()))
        .await
        .map_err(|e| e.to_string())?;
    st.scripts.changed();
    emit_scripts(app);
    Ok(item.meta.name)
}

#[tauri::command]
pub async fn scripts_view(app: AppHandle) -> Res<ScriptsView> {
    Ok(view(&app))
}

#[tauri::command]
pub async fn script_install(
    app: AppHandle,
    source: Option<String>,
    url: Option<String>,
) -> Res<String> {
    install(&app, source, url).await
}

#[tauri::command]
pub async fn script_toggle(app: AppHandle, id: String, on: bool) -> Res<()> {
    let st = app.state::<AppState>();
    st.scripts
        .store
        .set_enabled(&id, on)
        .map_err(|e| e.to_string())?;
    st.scripts.changed();
    emit_scripts(&app);
    Ok(())
}

#[tauri::command]
pub async fn script_remove(app: AppHandle, id: String) -> Res<()> {
    let st = app.state::<AppState>();
    st.scripts.store.remove(&id).map_err(|e| e.to_string())?;
    st.scripts.changed();
    emit_scripts(&app);
    Ok(())
}

/// Returns the new version, or `None` when the script is up to date.
#[tauri::command]
pub async fn script_update(app: AppHandle, id: String) -> Res<Option<String>> {
    let st = app.state::<AppState>();
    let client = client_for(&app)?;
    let found = st
        .scripts
        .store
        .find_update(&client, &id)
        .await
        .map_err(|e| e.to_string())?;
    let Some(source) = found else {
        return Ok(None);
    };
    let item = st
        .scripts
        .store
        .install(&client, &source, None)
        .await
        .map_err(|e| e.to_string())?;
    st.scripts.changed();
    emit_scripts(&app);
    Ok(Some(item.meta.version))
}

#[tauri::command]
pub async fn script_menu_run(app: AppHandle, tab: u32, instance: String, id: String) -> Res<()> {
    let st = app.state::<AppState>();
    let token = st
        .scripts
        .menus_on(tab)
        .into_iter()
        .find(|m| m.instance == instance && m.id == id)
        .map(|m| m.token)
        .ok_or("that command is gone")?;
    st.scripts
        .menu_queue
        .lock()
        .expect("menu")
        .entry(format!("{token}:{instance}"))
        .or_default()
        .push_back(id);
    st.scripts.menu_ring.notify_waiters();
    Ok(())
}

#[tauri::command]
pub async fn script_answer(app: AppHandle, ask: u32, answer: Answer) -> Res<()> {
    let st = app.state::<AppState>();
    let send = st.scripts.asks.lock().expect("asks").remove(&ask);
    if let Some(send) = send {
        let _ = send.send(answer);
    }
    Ok(())
}

#[derive(Serialize)]
pub struct ProbeScript {
    pub id: String,
    pub name: String,
    pub version: String,
    pub requires: usize,
}

/// What a probe run reports about scripts.
pub fn probe_report(app: &AppHandle, tab: Option<u32>) -> Value {
    let st = app.state::<AppState>();
    let installed: Vec<ProbeScript> = st
        .scripts
        .store
        .list()
        .into_iter()
        .map(|s| ProbeScript {
            requires: s.requires.len(),
            id: s.id,
            name: s.meta.name,
            version: s.meta.version,
        })
        .collect();
    json!({
        "installed": installed,
        "ran": tab.map(|t| st.scripts.ran_on(t)).unwrap_or_default(),
        "menus": tab.map(|t| st.scripts.menus_on(t)).unwrap_or_default(),
        "notes": *st.scripts.probe_notes.lock().expect("notes"),
        "values": st.scripts.store.list().iter()
            .map(|s| (s.id.clone(), json!(st.scripts.store.values(&s.id))))
            .collect::<serde_json::Map<_, _>>(),
    })
}

pub type Shared = Arc<Scripts>;

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(head: &str) -> Loaded {
        let source = format!("// ==UserScript==\n// @name T\n{head}// ==/UserScript==\nrun()\n");
        let meta = userscript::parse(&source).unwrap();
        Loaded {
            installed: Installed {
                id: meta.id(),
                meta,
                enabled: true,
                source_url: None,
                requires: vec![],
                allowed_hosts: vec![],
            },
            source,
            requires: vec!["lib()".into()],
        }
    }

    #[test]
    fn page_scripts_and_sandboxed_scripts_are_wrapped_differently() {
        let page = wrap(&loaded("// @grant none\n"), "k", "tok");
        assert!(page.contains("unsafeWindow"));
        assert!(!page.contains("with("));
        let sandbox = wrap(&loaded("// @grant GM_addStyle\n"), "k", "tok");
        assert!(sandbox.contains("with(__osprey_scope)"));
        // The library comes first, then the script, each on its own line.
        assert!(sandbox.find("lib()").unwrap() < sandbox.find("run()").unwrap());
        // A script may end in a line comment.
        assert!(sandbox.contains("run()\n\n}"));
    }

    #[test]
    fn a_save_name_cannot_leave_the_folder() {
        assert_eq!(clean_save_name("../../escaped.txt"), "escaped.txt");
        assert_eq!(clean_save_name("..\\..\\escaped.txt"), "escaped.txt");
        assert_eq!(clean_save_name("C:\\Windows\\win.ini"), "win.ini");
        assert_eq!(clean_save_name("/etc/passwd"), "passwd");
        assert_eq!(clean_save_name("sub/folder/a.mp4"), "a.mp4");
        for bad in ["..", "../..", "a/..", ""] {
            let name = clean_save_name(bad);
            assert!(!name.contains(['/', '\\']), "{bad}");
            assert_ne!(name, "..", "{bad}");
        }
    }

    #[test]
    fn every_page_reachable_function_names_its_grant() {
        assert_eq!(needed_grant("xhr"), Some(&["xmlhttprequest"][..]));
        assert_eq!(needed_grant("download"), Some(&["download"][..]));
        assert_eq!(needed_grant("open_tab"), Some(&["openintab"][..]));
        assert!(needed_grant("values").is_some());
        assert!(needed_grant("menu_wait").is_some());
        // Saves carry a page's own data and need no grant, as in Tampermonkey.
        assert!(needed_grant("save_begin").is_none());
    }
}
