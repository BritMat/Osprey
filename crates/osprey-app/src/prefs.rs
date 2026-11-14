//! The settings page: what it shows and what its controls change.
//!
//! Only settings with real code behind them are here. Network and privacy
//! choices still go through `net_set`, `ads_set` and `prefs_set` in main.rs,
//! and scripts through scripts.rs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use osprey_core::filter::{self, ListUpdate, DEFAULT_LISTS, USER_RULES_FILE};
use osprey_core::privacy::{Privacy, Route};
use osprey_core::{tools, ytdlp, Filters, RequestOptions};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::settings::{AppSettings, DownloadStyle};
use crate::{emit_ui, AppState, Res};

pub const THEMES: &[&str] = &["sea", "ember", "aurora", "slate"];

/// Settings that the web engine only reads when it starts, fixed for this
/// run so every tab gets the same ones. WebView2 refuses a tab whose
/// options differ from the others in the same profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineStart {
    pub hardware_acceleration: bool,
    pub cache_mb: u32,
}

impl EngineStart {
    pub fn from(s: &AppSettings) -> Self {
        Self {
            hardware_acceleration: s.hardware_acceleration,
            cache_mb: s.cache_mb,
        }
    }

    /// Extra WebView2 arguments for these settings.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn browser_args(&self) -> String {
        let mut out = String::new();
        if !self.hardware_acceleration {
            out.push_str(" --disable-gpu");
        }
        if self.cache_mb > 0 {
            out.push_str(&format!(
                " --disk-cache-size={}",
                u64::from(self.cache_mb) * 1024 * 1024
            ));
        }
        out
    }
}

#[derive(Serialize, Clone)]
pub struct ListView {
    id: &'static str,
    name: &'static str,
    on: bool,
    /// Downloaded at least once.
    have: bool,
    /// When the copy on disk was saved, seconds since 1970.
    saved: Option<u64>,
}

#[derive(Serialize, Clone)]
pub struct SettingsView {
    version: &'static str,
    theme: String,
    reduce_effects: bool,
    downloads_dir: String,
    download_style: DownloadStyle,
    speed_limit_kbps: u32,
    parallel_downloads: u8,
    sleep_after_minutes: u64,
    /// The two settings below only work on Windows so far.
    engine_settings: bool,
    hardware_acceleration: bool,
    cache_mb: u32,
    /// Bytes the tabs' disk cache uses now.
    cache_used: u64,
    /// A start-up setting was changed and waits for a restart.
    restart_needed: bool,
    block_ads: bool,
    lists: Vec<ListView>,
    custom_rules: String,
    bypass_vip_key_set: bool,
}

fn saved_at(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn folder_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => folder_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// The tabs' disk cache folders inside the WebView2 profile.
fn cache_dirs(home: &Path) -> Vec<PathBuf> {
    let base = home
        .join("browser-profile")
        .join("EBWebView")
        .join("Default");
    vec![base.join("Cache"), base.join("Code Cache")]
}

pub fn view(app: &AppHandle) -> SettingsView {
    let st = app.state::<AppState>();
    let s = st.settings.lock().expect("settings").clone();
    let dir = filter::filters_dir(&st.home);
    let _ = std::fs::create_dir_all(&dir);
    let _ = filter::ensure_user_rules_file(&dir);
    let lists = DEFAULT_LISTS
        .iter()
        .map(|l| {
            let path = dir.join(format!("{}.txt", l.id));
            ListView {
                id: l.id,
                name: l.name,
                on: !s.lists_off.iter().any(|o| o == l.id),
                have: path.is_file(),
                saved: saved_at(&path),
            }
        })
        .collect();
    let restart_needed = EngineStart::from(&s) != st.engine_start;
    SettingsView {
        version: env!("CARGO_PKG_VERSION"),
        theme: s.theme.clone(),
        reduce_effects: s.reduce_effects,
        downloads_dir: s.downloads_dir().display().to_string(),
        download_style: s.download_style,
        speed_limit_kbps: s.speed_limit_kbps,
        parallel_downloads: s.parallel_downloads,
        sleep_after_minutes: s.sleep_after_minutes,
        engine_settings: cfg!(windows),
        hardware_acceleration: s.hardware_acceleration,
        cache_mb: s.cache_mb,
        cache_used: cache_dirs(&st.home).iter().map(|d| folder_size(d)).sum(),
        restart_needed,
        block_ads: s.block_ads,
        lists,
        custom_rules: std::fs::read_to_string(dir.join(USER_RULES_FILE)).unwrap_or_default(),
        bypass_vip_key_set: s.bypass_vip_key.as_deref().is_some_and(|k| !k.is_empty()),
    }
}

#[tauri::command]
pub async fn settings_view(app: AppHandle) -> Res<SettingsView> {
    Ok(view(&app))
}

/// One change from the settings page. Only the fields given change.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Change {
    theme: Option<String>,
    reduce_effects: Option<bool>,
    downloads_dir: Option<String>,
    parallel_downloads: Option<u8>,
    sleep_after_minutes: Option<u64>,
    hardware_acceleration: Option<bool>,
    cache_mb: Option<u32>,
    /// A filter list switched on or off: (id, on).
    list: Option<(String, bool)>,
}

/// Checks a download folder typed by hand. It must be a full path, and it
/// is made if it is not there yet.
fn check_folder(text: &str) -> Res<PathBuf> {
    let path = PathBuf::from(text.trim());
    if text.trim().is_empty() || !path.is_absolute() {
        return Err("give the full path of a folder".into());
    }
    std::fs::create_dir_all(&path).map_err(|e| format!("cannot use that folder: {e}"))?;
    if !path.is_dir() {
        return Err("that is not a folder".into());
    }
    Ok(path)
}

#[tauri::command]
pub async fn settings_set(app: AppHandle, change: Change) -> Res<SettingsView> {
    let st = app.state::<AppState>();
    let mut lists_changed = false;
    {
        let mut s = st.settings.lock().expect("settings");
        if let Some(t) = change.theme {
            if !THEMES.contains(&t.as_str()) {
                return Err(format!("unknown theme {t}"));
            }
            s.theme = t;
        }
        if let Some(v) = change.reduce_effects {
            s.reduce_effects = v;
        }
        if let Some(d) = change.downloads_dir {
            s.downloads_dir = Some(check_folder(&d)?);
        }
        if let Some(n) = change.parallel_downloads {
            s.parallel_downloads = n.clamp(1, 10);
            st.downloads.set_parallel(usize::from(s.parallel_downloads));
        }
        if let Some(m) = change.sleep_after_minutes {
            s.sleep_after_minutes = m.min(24 * 60);
        }
        if let Some(v) = change.hardware_acceleration {
            s.hardware_acceleration = v;
        }
        if let Some(mb) = change.cache_mb {
            s.cache_mb = mb.min(10_000);
        }
        if let Some((id, on)) = change.list {
            if !DEFAULT_LISTS.iter().any(|l| l.id == id) {
                return Err(format!("unknown filter list {id}"));
            }
            s.lists_off.retain(|o| *o != id);
            if !on {
                s.lists_off.push(id);
            }
            lists_changed = true;
        }
        s.save(&st.home);
    }
    if lists_changed {
        reload_filters(&app).await;
    }
    let v = view(&app);
    emit_ui(&app, "settings", v.clone());
    Ok(v)
}

/// Loads the filter lists again, for example after one was switched off,
/// updated or the user's own rules changed.
pub async fn reload_filters(app: &AppHandle) {
    let st = app.state::<AppState>();
    let home = st.home.clone();
    let off = st.settings.lock().expect("settings").lists_off.clone();
    let loaded = tokio::task::spawn_blocking(move || Filters::load_except(&home, &off)).await;
    if let Ok(Ok(f)) = loaded {
        let f = Arc::new(f);
        *st.filters.write().expect("filters") = f.clone();
        if st.settings.lock().expect("settings").block_ads {
            st.gateway.set_filters(Some(f));
        }
    }
}

/// A client that leaves through the same route as the tabs.
fn route_client(app: &AppHandle) -> Res<reqwest::Client> {
    let st = app.state::<AppState>();
    let opts = RequestOptions {
        privacy: Privacy {
            route: Route::Proxy(st.gateway.proxy_url()),
            ..Default::default()
        },
        ..Default::default()
    };
    osprey_core::build_client(&opts).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn filters_update(app: AppHandle) -> Res<Vec<ListUpdate>> {
    let client = route_client(&app)?;
    let home = app.state::<AppState>().home.clone();
    let report = filter::update_lists(&client, &home)
        .await
        .map_err(|e| e.to_string())?;
    reload_filters(&app).await;
    emit_ui(&app, "settings", view(&app));
    Ok(report)
}

#[tauri::command]
pub async fn filters_rules_set(app: AppHandle, text: String) -> Res<SettingsView> {
    if text.len() > 2 * 1024 * 1024 {
        return Err("that is too long for a rules file".into());
    }
    let dir = filter::filters_dir(&app.state::<AppState>().home);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{USER_RULES_FILE}.tmp"));
    std::fs::write(&tmp, &text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(USER_RULES_FILE)).map_err(|e| e.to_string())?;
    reload_filters(&app).await;
    Ok(view(&app))
}

#[derive(Serialize, Clone)]
pub struct ToolsView {
    ytdlp: Option<String>,
    deno: Option<String>,
    /// What the last update did, one line per tool.
    report: Vec<String>,
}

/// The first line a program prints for `--version`, or None if it does
/// not run.
async fn version_of(binary: &Path) -> Option<String> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let out = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

async fn tools_now(app: &AppHandle, report: Vec<String>) -> ToolsView {
    let home = app.state::<AppState>().home.clone();
    let ytdlp = version_of(&ytdlp::binary(&home)).await;
    let deno = match tools::deno_path(&home) {
        Some(p) => version_of(&p).await,
        None => None,
    };
    ToolsView {
        ytdlp,
        deno,
        report,
    }
}

#[tauri::command]
pub async fn tools_view(app: AppHandle) -> Res<ToolsView> {
    Ok(tools_now(&app, Vec::new()).await)
}

#[tauri::command]
pub async fn tools_update(app: AppHandle) -> Res<ToolsView> {
    let client = route_client(&app)?;
    let home = app.state::<AppState>().home.clone();
    let report = tools::refresh(&home, &client, true)
        .await
        .into_iter()
        .map(|r| match r.result {
            Ok(tools::Step::Installed) => format!("{}: installed", r.tool),
            Ok(tools::Step::Updated) => format!("{}: updated", r.tool),
            Ok(tools::Step::AlreadyCurrent) => format!("{}: already up to date", r.tool),
            Err(e) => format!("{}: {e}", r.tool),
        })
        .collect();
    Ok(tools_now(&app, report).await)
}

/// Empties the tabs' disk cache. Cookies and site data stay.
#[tauri::command]
pub async fn cache_clear(app: AppHandle) -> Res<SettingsView> {
    #[cfg(windows)]
    {
        let wv = {
            let st = app.state::<AppState>();
            let tabs = st.tabs.lock().expect("tabs");
            tabs.order
                .iter()
                .find_map(|id| crate::tab_webview(&app, *id))
                .ok_or("open a tab first, the cache is cleared through it")?
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        wv.with_webview(move |platform| {
            use webview2_com::ClearBrowsingDataCompletedHandler;
            use webview2_com::Microsoft::Web::WebView2::Win32::{
                ICoreWebView2Profile2, ICoreWebView2_13,
                COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE,
            };
            use windows_core::Interface;
            // SAFETY: runs on the window thread, where WebView2 objects
            // must be used, with this tab's own controller.
            let started = unsafe {
                platform.controller().CoreWebView2().and_then(|core| {
                    let tx = std::sync::Mutex::new(Some(tx));
                    core.cast::<ICoreWebView2_13>()?
                        .Profile()?
                        .cast::<ICoreWebView2Profile2>()?
                        .ClearBrowsingData(
                            COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE,
                            &ClearBrowsingDataCompletedHandler::create(Box::new(move |r| {
                                if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
                                    let _ = tx.send(r.map_err(|e| e.to_string()));
                                }
                                Ok(())
                            })),
                        )
                })
            };
            if let Err(e) = started {
                eprintln!("could not clear the cache: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => return Err(format!("the web engine could not clear it: {e}")),
            _ => return Err("the web engine did not answer".into()),
        }
        Ok(view(&app))
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Err("clearing only the cache works on Windows so far".into())
    }
}

/// Settings as one file, for moving to another computer. The bypass.vip
/// key is left out unless asked for.
pub fn export_text(s: &AppSettings, with_key: bool) -> Res<String> {
    let mut s = s.clone();
    if !with_key {
        s.bypass_vip_key = None;
    }
    serde_json::to_string_pretty(&s).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn settings_export(app: AppHandle, with_key: bool) -> Res<String> {
    let st = app.state::<AppState>();
    let s = st.settings.lock().expect("settings").clone();
    let text = export_text(&s, with_key)?;
    let dir = s.downloads_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("BM Osprey settings.json");
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

/// Reads an exported file. Missing fields keep their defaults, and a key
/// that is not in the file keeps the one saved here.
pub fn import_text(text: &str, current: &AppSettings) -> Res<AppSettings> {
    if text.len() > 1024 * 1024 {
        return Err("that file is too big to be a settings file".into());
    }
    let mut s: AppSettings =
        serde_json::from_str(text).map_err(|e| format!("not a BM Osprey settings file: {e}"))?;
    if s.bypass_vip_key.is_none() {
        s.bypass_vip_key = current.bypass_vip_key.clone();
    }
    if !THEMES.contains(&s.theme.as_str()) {
        s.theme = "sea".into();
    }
    s.parallel_downloads = s.parallel_downloads.clamp(1, 10);
    s.tor_circuits = s.tor_circuits.clamp(1, 6);
    s.lists_off
        .retain(|id| DEFAULT_LISTS.iter().any(|l| l.id == id));
    // A download folder that does not exist here falls back to the default.
    if s.downloads_dir.as_ref().is_some_and(|d| !d.is_dir()) {
        s.downloads_dir = None;
    }
    // The route is checked like a choice made in the Network panel.
    s.net.upstream(None)?;
    Ok(s)
}

#[tauri::command]
pub async fn settings_import(app: AppHandle, text: String) -> Res<SettingsView> {
    let st = app.state::<AppState>();
    let current = st.settings.lock().expect("settings").clone();
    let new = import_text(&text, &current)?;
    let net = new.net.clone();
    let ads = new.block_ads;
    {
        let mut s = st.settings.lock().expect("settings");
        *s = new;
        st.downloads.limit.set(u64::from(s.speed_limit_kbps) * 1024);
        st.downloads.set_parallel(usize::from(s.parallel_downloads));
        s.save(&st.home);
    }
    // These two go through the same code as the Network panel, which also
    // tells the gateway.
    crate::net_set(app.clone(), net).await?;
    crate::ads_set(app.clone(), ads).await?;
    reload_filters(&app).await;
    let v = view(&app);
    emit_ui(&app, "settings", v.clone());
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_leaves_the_key_out_unless_asked() {
        let s = AppSettings {
            bypass_vip_key: Some("secret-key".into()),
            ..Default::default()
        };
        assert!(!export_text(&s, false).unwrap().contains("secret-key"));
        assert!(export_text(&s, true).unwrap().contains("secret-key"));
    }

    #[test]
    fn import_checks_what_it_reads() {
        let here = AppSettings {
            bypass_vip_key: Some("mine".into()),
            ..Default::default()
        };
        let s = import_text(
            r#"{"theme":"neon","parallel_downloads":99,"lists_off":["easylist","nope"],
                "downloads_dir":"Z:\\no\\such\\folder\\anywhere","speed_limit_kbps":512}"#,
            &here,
        )
        .unwrap();
        assert_eq!(s.theme, "sea");
        assert_eq!(s.parallel_downloads, 10);
        assert_eq!(s.lists_off, vec!["easylist".to_string()]);
        assert_eq!(s.downloads_dir, None);
        assert_eq!(s.speed_limit_kbps, 512);
        assert_eq!(s.bypass_vip_key.as_deref(), Some("mine"));
        assert!(import_text("not json", &here).is_err());
        assert!(import_text(r#"{"net":{"mode":"vpn","proxy":"nonsense"}}"#, &here).is_err());
    }

    #[test]
    fn engine_arguments_follow_the_settings() {
        let on = EngineStart {
            hardware_acceleration: true,
            cache_mb: 0,
        };
        assert_eq!(on.browser_args(), "");
        let off = EngineStart {
            hardware_acceleration: false,
            cache_mb: 250,
        };
        assert_eq!(
            off.browser_args(),
            " --disable-gpu --disk-cache-size=262144000"
        );
    }

    #[test]
    fn a_folder_must_be_a_full_path() {
        assert!(check_folder("relative\\folder").is_err());
        assert!(check_folder("  ").is_err());
        let dir = tempfile::tempdir().unwrap();
        let made = dir.path().join("new one");
        assert_eq!(check_folder(made.to_str().unwrap()).unwrap(), made);
        assert!(made.is_dir());
    }
}
