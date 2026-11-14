//! Filtering add-on, in two layers.
//!
//! 1. Ad and tracker blocking with the same filter lists and syntax as
//!    uBlock Origin (EasyList, EasyPrivacy, uBlock filters and so on),
//!    powered by Brave's adblock-rust engine. Network rules decide whether
//!    a URL is junk. Cosmetic rules (`##.selector`) are used by text-only
//!    mode to drop ad blocks from pages.
//! 2. 1DM style media rules: minimum and maximum size, allowed types and
//!    extensions, blocked hosts and keywords.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::Engine;
use serde::Serialize;

use crate::client::ensure_success;
use crate::detect::MediaKind;
use crate::error::{Error, Result};

const BUILTIN_RULES: &str = include_str!("builtin_filters.txt");
pub const USER_RULES_FILE: &str = "my-filters.txt";

pub struct FilterList {
    pub id: &'static str,
    pub name: &'static str,
    /// Tried in order until one works.
    pub urls: &'static [&'static str],
}

/// The lists `osprey filters update` downloads. Same defaults as uBlock
/// Origin, minus the cosmetic-only regional lists.
pub const DEFAULT_LISTS: &[FilterList] = &[
    FilterList {
        id: "easylist",
        name: "EasyList (ads)",
        urls: &[
            "https://easylist.to/easylist/easylist.txt",
            "https://raw.githubusercontent.com/easylist/easylist/gh-pages/easylist.txt",
        ],
    },
    FilterList {
        id: "easyprivacy",
        name: "EasyPrivacy (trackers)",
        urls: &[
            "https://easylist.to/easylist/easyprivacy.txt",
            "https://raw.githubusercontent.com/easylist/easylist/gh-pages/easyprivacy.txt",
        ],
    },
    FilterList {
        id: "ubo-filters",
        name: "uBlock filters",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/filters.txt",
            "https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/filters.txt",
        ],
    },
    FilterList {
        id: "ubo-privacy",
        name: "uBlock filters, Privacy",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/privacy.txt",
            "https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/privacy.txt",
        ],
    },
    FilterList {
        id: "ubo-badware",
        name: "uBlock filters, Badware risks",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/badware.txt",
            "https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/badware.txt",
        ],
    },
    FilterList {
        id: "peter-lowe",
        name: "Peter Lowe's ad and tracking server list",
        urls: &["https://pgl.yoyo.org/adservers/serverlist.php?hostformat=adblockplus&showintro=1&mimetype=plaintext"],
    },
];

/// Where BM Osprey keeps its data. `OSPREY_HOME` overrides it. Otherwise
/// %APPDATA%\bm-osprey on Windows, ~/Library/Application Support/bm-osprey
/// on macOS and ~/.local/share/bm-osprey on Linux.
///
/// BM Osprey was called BM Fetch, with its data in `bm-fetch`. If that
/// folder is there and `bm-osprey` is not, it is moved, so settings,
/// scripts and tools are kept.
pub fn home_dir() -> PathBuf {
    if let Some(h) = crate::env_var("HOME") {
        return PathBuf::from(h);
    }
    match dirs::data_dir() {
        Some(d) => move_old_home(&d.join("bm-fetch"), &d.join("bm-osprey")),
        None => PathBuf::from(".bm-osprey"),
    }
}

/// Moves the old data folder to the new name when only the old one exists.
/// If the move fails, for example because BM Fetch is still running and
/// holds files in it, the old folder is used as it is, so nothing is lost.
pub fn move_old_home(old: &Path, new: &Path) -> PathBuf {
    if new.exists() || !old.exists() {
        return new.to_path_buf();
    }
    match std::fs::rename(old, new) {
        Ok(()) => new.to_path_buf(),
        Err(_) => old.to_path_buf(),
    }
}

pub fn filters_dir(home: &Path) -> PathBuf {
    home.join("filters")
}

/// The type of request being checked. It matters because filters can
/// target types, for example `$media` or `$image`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Media,
    Image,
    Script,
    Xhr,
    Document,
    Other,
}

impl RequestKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Image => "image",
            Self::Script => "script",
            Self::Xhr => "xmlhttprequest",
            Self::Document => "document",
            Self::Other => "other",
        }
    }
}

pub struct Filters {
    engine: Engine,
    /// Names of the sources that were loaded, for display.
    pub sources: Vec<String>,
}

impl Filters {
    /// Only the small built in set.
    pub fn builtin() -> Self {
        Self::from_sources(vec![("built in".into(), BUILTIN_RULES.to_string())])
    }

    /// Filters from rule text, for tests and custom setups.
    pub fn from_rules(rules: &str) -> Self {
        Self::from_sources(vec![("custom".into(), rules.to_string())])
    }

    /// Built in rules, every downloaded list in `<home>/filters`, and the
    /// user's own `my-filters.txt`.
    pub fn load(home: &Path) -> Result<Self> {
        Self::load_except(home, &[])
    }

    /// The same, leaving out the lists whose ids are in `off`.
    pub fn load_except(home: &Path, off: &[String]) -> Result<Self> {
        let dir = filters_dir(home);
        std::fs::create_dir_all(&dir)?;
        ensure_user_rules_file(&dir)?;

        let mut sources = vec![("built in".to_string(), BUILTIN_RULES.to_string())];
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .filter(|p| {
                let id = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                !off.iter().any(|o| o == id)
            })
            .collect();
        entries.sort();
        for path in entries {
            let text = std::fs::read_to_string(&path)?;
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("list")
                .to_string();
            sources.push((name, text));
        }
        Ok(Self::from_sources(sources))
    }

    fn from_sources(sources: Vec<(String, String)>) -> Self {
        let mut set = FilterSet::new(false);
        let mut names = Vec::new();
        for (name, text) in sources {
            set.add_filter_list(text, ParseOptions::default());
            names.push(name);
        }
        Self {
            engine: Engine::new_with_filter_set(set),
            sources: names,
        }
    }

    /// True when a request for `url`, made from the page `source_url`,
    /// would be blocked by the lists.
    pub fn is_blocked(&self, url: &str, source_url: &str, kind: RequestKind) -> bool {
        match Request::new(url, source_url, kind.as_str(), "GET") {
            Ok(req) => self.engine.check_network_request(&req).should_block(),
            Err(_) => false,
        }
    }

    /// CSS selectors for page elements that should be hidden, given the
    /// classes and ids that appear on the page.
    pub fn hidden_selectors(
        &self,
        page_url: &str,
        classes: &HashSet<String>,
        ids: &HashSet<String>,
    ) -> Vec<String> {
        let res = self.engine.url_cosmetic_resources(page_url);
        let mut out: Vec<String> = res.hide_selectors.into_iter().collect();
        if !res.generichide {
            out.extend(
                self.engine
                    .hidden_class_id_selectors(classes, ids, &res.exceptions),
            );
        }
        out.sort();
        out.dedup();
        out
    }
}

pub fn ensure_user_rules_file(dir: &Path) -> Result<()> {
    let path = dir.join(USER_RULES_FILE);
    if !path.exists() {
        std::fs::write(
            &path,
            "! Your own filters. Same syntax as uBlock Origin.\n\
             ! Examples:\n\
             ! ||annoying-ads.example^        block a whole domain\n\
             ! ||cdn.example/ads/*$media       block ad videos from one path\n\
             ! example.com##.popup-box         hide an element in text-only mode\n\
             ! @@||example.com^                never block this site\n",
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ListUpdate {
    pub id: String,
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Downloads every list in [`DEFAULT_LISTS`] into `<home>/filters`. A list
/// that fails keeps its previous copy, so one bad mirror never leaves you
/// without filters.
pub async fn update_lists(client: &reqwest::Client, home: &Path) -> Result<Vec<ListUpdate>> {
    let dir = filters_dir(home);
    tokio::fs::create_dir_all(&dir).await?;
    ensure_user_rules_file(&dir)?;
    let mut report = Vec::new();

    for list in DEFAULT_LISTS {
        let mut last_err = String::new();
        let mut done = false;
        for url in list.urls {
            match download_list(client, url).await {
                Ok(text) => {
                    let path = dir.join(format!("{}.txt", list.id));
                    let tmp = dir.join(format!("{}.txt.tmp", list.id));
                    tokio::fs::write(&tmp, &text).await?;
                    tokio::fs::rename(&tmp, &path).await?;
                    let rules = text
                        .lines()
                        .filter(|l| !l.trim().is_empty() && !l.starts_with('!'))
                        .count();
                    report.push(ListUpdate {
                        id: list.id.into(),
                        name: list.name.into(),
                        ok: true,
                        detail: format!("{rules} rules"),
                    });
                    done = true;
                    break;
                }
                Err(e) => last_err = format!("{url}: {e}"),
            }
        }
        if !done {
            report.push(ListUpdate {
                id: list.id.into(),
                name: list.name.into(),
                ok: false,
                detail: last_err,
            });
        }
    }
    Ok(report)
}

async fn download_list(client: &reqwest::Client, url: &str) -> Result<String> {
    let text = ensure_success(client.get(url).send().await?)?
        .text()
        .await?;
    // Guard against captive portals and error pages saved as lists.
    let head = text
        .trim_start()
        .get(..200)
        .unwrap_or(text.trim_start())
        .to_ascii_lowercase();
    if head.starts_with("<!doctype html") || head.starts_with("<html") {
        return Err(Error::Parse(
            "got a web page instead of a filter list".into(),
        ));
    }
    Ok(text)
}

/// 1DM style rules for which detected media to keep.
#[derive(Debug, Clone, Default)]
pub struct MediaRules {
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Only these kinds, when set.
    pub kinds: Option<Vec<MediaKind>>,
    /// Only these file extensions (lowercase, no dot), when set.
    pub extensions: Option<Vec<String>>,
    /// Hosts to ignore. Subdomains are included.
    pub blocked_hosts: Vec<String>,
    /// Ignore URLs containing any of these words.
    pub blocked_words: Vec<String>,
}

impl MediaRules {
    /// `Ok` when the media passes, otherwise the reason it was dropped.
    pub fn check(
        &self,
        url: &str,
        kind: MediaKind,
        size: Option<u64>,
    ) -> std::result::Result<(), String> {
        if let Some(kinds) = &self.kinds {
            if !kinds.contains(&kind) {
                return Err(format!("type {kind:?} not wanted"));
            }
        }
        let parsed = url::Url::parse(url).ok();
        if let Some(exts) = &self.extensions {
            let ext = parsed
                .as_ref()
                .and_then(|u| u.path().rsplit('/').next().map(str::to_string))
                .and_then(|name| name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()));
            if !ext.is_some_and(|e| exts.contains(&e)) {
                return Err("extension not in the allowed list".into());
            }
        }
        if let Some(host) = parsed.as_ref().and_then(|u| u.host_str()) {
            let host = host.to_ascii_lowercase();
            for blocked in &self.blocked_hosts {
                let b = blocked.trim_start_matches('.').to_ascii_lowercase();
                if host == b || host.ends_with(&format!(".{b}")) {
                    return Err(format!("host {b} is blocked"));
                }
            }
        }
        let lower = url.to_ascii_lowercase();
        for w in &self.blocked_words {
            if lower.contains(&w.to_ascii_lowercase()) {
                return Err(format!("contains '{w}'"));
            }
        }
        if let Some(size) = size {
            if self.min_size.is_some_and(|m| size < m) {
                return Err(format!("smaller than {}", self.min_size.unwrap_or(0)));
            }
            if self.max_size.is_some_and(|m| size > m) {
                return Err(format!("bigger than {}", self.max_size.unwrap_or(0)));
            }
        }
        Ok(())
    }

    pub fn needs_size(&self) -> bool {
        self.min_size.is_some() || self.max_size.is_some()
    }
}

/// Reads sizes like `500`, `300KB`, `1.5MB` or `2GB`.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_uppercase();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    (n >= 0.0).then_some((n * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_home_folder_is_moved_once() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("bm-fetch"), dir.path().join("bm-osprey"));
        // Nothing there: the new name, nothing made.
        assert_eq!(move_old_home(&old, &new), new);
        assert!(!new.exists());
        // Only the old one: moved with what is in it.
        std::fs::create_dir_all(old.join("tools")).unwrap();
        std::fs::write(old.join("browser.json"), "{}").unwrap();
        assert_eq!(move_old_home(&old, &new), new);
        assert!(new.join("browser.json").exists() && new.join("tools").is_dir());
        assert!(!old.exists());
        // Both there: the new one wins and the old one is left alone.
        std::fs::create_dir_all(&old).unwrap();
        assert_eq!(move_old_home(&old, &new), new);
        assert!(old.exists());
    }

    #[test]
    fn builtin_rules_block_ad_networks_only() {
        let f = Filters::builtin();
        let page = "https://news.example/article";
        assert!(f.is_blocked(
            "https://pubads.g.doubleclick.net/gampad/ads?x=1",
            page,
            RequestKind::Xhr
        ));
        assert!(f.is_blocked(
            "https://imasdk.googleapis.com/js/sdkloader/ima3.js",
            page,
            RequestKind::Script
        ));
        assert!(!f.is_blocked(
            "https://cdn.news.example/video/master.m3u8",
            page,
            RequestKind::Media
        ));
    }

    #[test]
    fn custom_rules_with_types_and_exceptions() {
        let f = Filters::from_rules(
            "||cdn.example/ads/*$media\n||tracker.example^\n@@||tracker.example/allowed.js\nexample.com##.promo-box\n",
        );
        let page = "https://example.com/watch";
        assert!(f.is_blocked(
            "https://cdn.example/ads/preroll.mp4",
            page,
            RequestKind::Media
        ));
        assert!(!f.is_blocked(
            "https://cdn.example/videos/movie.mp4",
            page,
            RequestKind::Media
        ));
        assert!(f.is_blocked("https://tracker.example/t.gif", page, RequestKind::Image));
        assert!(!f.is_blocked(
            "https://tracker.example/allowed.js",
            page,
            RequestKind::Script
        ));

        let sel = f.hidden_selectors(page, &HashSet::new(), &HashSet::new());
        assert!(sel.contains(&".promo-box".to_string()));
        assert!(f
            .hidden_selectors("https://other.test/", &HashSet::new(), &HashSet::new())
            .is_empty());
    }

    #[test]
    fn generic_cosmetic_rules_follow_page_classes() {
        let f = Filters::builtin();
        let classes: HashSet<String> = ["adsbygoogle".to_string(), "article".to_string()].into();
        let sel = f.hidden_selectors("https://any.test/", &classes, &HashSet::new());
        assert!(sel.contains(&".adsbygoogle".to_string()), "{sel:?}");
    }

    #[test]
    fn media_rules_like_1dm() {
        let rules = MediaRules {
            min_size: parse_size("1MB"),
            kinds: Some(vec![MediaKind::Direct, MediaKind::Hls]),
            extensions: None,
            blocked_hosts: vec!["ads.example".into()],
            blocked_words: vec!["preroll".into()],
            ..Default::default()
        };
        assert!(rules
            .check("https://cdn.test/a.mp4", MediaKind::Direct, Some(5_000_000))
            .is_ok());
        assert!(rules
            .check("https://cdn.test/a.mp4", MediaKind::Direct, Some(10_000))
            .is_err());
        assert!(rules
            .check("https://cdn.test/a.mp4", MediaKind::Direct, None)
            .is_ok());
        assert!(rules
            .check("https://x.ads.example/a.mp4", MediaKind::Direct, None)
            .is_err());
        assert!(rules
            .check("https://cdn.test/preroll_1.mp4", MediaKind::Direct, None)
            .is_err());
        assert!(rules
            .check("https://cdn.test/a.mpd", MediaKind::Dash, None)
            .is_err());

        let only_mp3 = MediaRules {
            extensions: Some(vec!["mp3".into()]),
            ..Default::default()
        };
        assert!(only_mp3
            .check("https://x.test/song.mp3?x=1", MediaKind::Direct, None)
            .is_ok());
        assert!(only_mp3
            .check("https://x.test/v.mp4", MediaKind::Direct, None)
            .is_err());
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("500"), Some(500));
        assert_eq!(parse_size("2kb"), Some(2048));
        assert_eq!(parse_size("1.5MB"), Some(1_572_864));
        assert_eq!(parse_size("1 GB"), Some(1_073_741_824));
        assert_eq!(parse_size("ten"), None);
        assert_eq!(parse_size("5PB"), None);
    }
}

#[cfg(test)]
mod thread_safety {
    fn assert_send_sync<T: Send + Sync>() {}
    #[test]
    fn filters_can_be_shared_between_threads() {
        assert_send_sync::<super::Filters>();
    }
}
