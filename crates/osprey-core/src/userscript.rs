//! User scripts, in the style of Tampermonkey: reads the header of a
//! `.user.js`, works out which addresses it runs on, and keeps installed
//! scripts on disk with their `@require` files and saved values.
//!
//! Everything that touches a web page lives in the browser crate. This part
//! has no UI so it can be tested on its own.
//!
//! Layout on disk, under `<BM Osprey home>/scripts/<id>/`:
//! `script.user.js`, `meta.json`, `values.json` and `requires/<hash>.js`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::client::ensure_success;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunAt {
    DocumentStart,
    DocumentEnd,
    /// Tampermonkey's default when a script names no time.
    #[default]
    DocumentIdle,
}

/// What a script says about itself in its header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meta {
    pub name: String,
    /// `@name:xx` translations, by language code.
    pub names: BTreeMap<String, String>,
    pub namespace: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub matches: Vec<String>,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub exclude_matches: Vec<String>,
    pub run_at: RunAt,
    pub noframes: bool,
    pub requires: Vec<String>,
    pub connects: Vec<String>,
    pub grants: Vec<String>,
    pub update_url: Option<String>,
    pub download_url: Option<String>,
}

/// Reads the `==UserScript==` block.
pub fn parse(source: &str) -> Result<Meta> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut meta = Meta::default();
    let mut inside = false;
    let mut closed = false;
    for line in source.lines() {
        let line = line.trim();
        if !inside {
            inside = is_marker(line, "==UserScript==");
            continue;
        }
        if is_marker(line, "==/UserScript==") {
            closed = true;
            break;
        }
        let Some(rest) = line.strip_prefix("//") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('@') else {
            continue;
        };
        let (key, value) = match rest.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => (rest, ""),
        };
        meta.read(key, value);
    }
    if !closed {
        return Err(Error::Unsupported(
            "this is not a user script: no ==UserScript== header".into(),
        ));
    }
    if meta.name.is_empty() {
        return Err(Error::Unsupported("the script has no @name".into()));
    }
    Ok(meta)
}

fn is_marker(line: &str, marker: &str) -> bool {
    line.strip_prefix("//")
        .is_some_and(|rest| rest.trim() == marker)
}

impl Meta {
    fn read(&mut self, key: &str, value: &str) {
        let text = || value.to_string();
        match key {
            "name" => self.name = text(),
            "namespace" => self.namespace = text(),
            "version" => self.version = text(),
            "description" => self.description = text(),
            "author" => self.author = text(),
            "match" => self.matches.push(text()),
            "include" => self.includes.push(text()),
            "exclude" => self.excludes.push(text()),
            "exclude-match" => self.exclude_matches.push(text()),
            "noframes" => self.noframes = true,
            "require" => self.requires.push(text()),
            "connect" => self.connects.push(text()),
            "grant" => self.grants.push(text()),
            "updateURL" => self.update_url = Some(text()),
            "downloadURL" => self.download_url = Some(text()),
            "run-at" => {
                self.run_at = match value {
                    "document-start" | "document-body" => RunAt::DocumentStart,
                    "document-end" => RunAt::DocumentEnd,
                    _ => RunAt::DocumentIdle,
                }
            }
            _ => {
                if let Some(lang) = key.strip_prefix("name:") {
                    self.names.insert(lang.to_string(), text());
                }
            }
        }
    }

    /// A stable id, so a new version replaces the old one.
    pub fn id(&self) -> String {
        let hash = Sha256::digest(format!("{}\n{}", self.namespace, self.name));
        hex::encode(&hash[..8])
    }

    /// The patterns as regular expressions, written so both Rust and
    /// JavaScript read them the same way. Both compare without case.
    pub fn include_patterns(&self) -> Vec<String> {
        let matches = self.matches.iter().filter_map(|m| match_regex(m));
        matches
            .chain(self.includes.iter().map(|g| glob_regex(g)))
            .collect()
    }

    pub fn exclude_patterns(&self) -> Vec<String> {
        let matches = self.exclude_matches.iter().filter_map(|m| match_regex(m));
        matches
            .chain(self.excludes.iter().map(|g| glob_regex(g)))
            .collect()
    }

    /// True when the script runs on `url`.
    pub fn applies(&self, url: &str) -> bool {
        let url = url.split('#').next().unwrap_or(url);
        let hit = |p: &String| compile(p).is_some_and(|r| r.is_match(url));
        self.include_patterns().iter().any(hit) && !self.exclude_patterns().iter().any(hit)
    }

    /// A script with no special functions runs in the page itself.
    pub fn runs_in_page(&self) -> bool {
        self.grants.iter().all(|g| g == "none")
    }

    /// True when the script asked for this function. Both spellings count,
    /// so `GM.setValue` and `GM_setValue` are the same grant.
    pub fn has_grant(&self, function: &str) -> bool {
        self.grants.iter().any(|g| grant_base(g) == function)
    }

    /// What `@connect` says about a host. The page's own host is always
    /// fine, as in Tampermonkey.
    pub fn connect_allows(&self, host: &str, page_host: Option<&str>) -> bool {
        if page_host.is_some_and(|p| same_site(host, p)) {
            return true;
        }
        self.connects.iter().any(|c| match c.as_str() {
            // `*` means the public internet. A local address must be named.
            "*" => !is_local_host(host),
            "self" => false,
            name => same_site(host, name),
        })
    }
}

/// `GM_setValue` and `GM.setValue` both become `setvalue`.
pub fn grant_base(grant: &str) -> String {
    grant
        .strip_prefix("GM_")
        .or_else(|| grant.strip_prefix("GM."))
        .unwrap_or(grant)
        .to_ascii_lowercase()
}

fn same_site(host: &str, name: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    host == name || host.ends_with(&format!(".{name}"))
}

/// True for this computer and the home network. Scripts must name these,
/// so a web page cannot reach a router through a script.
pub fn is_local_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

fn compile(pattern: &str) -> Option<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .ok()
}

fn glob_part(text: &str) -> String {
    text.split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*")
}

/// An `@match` pattern: `scheme://host/path`.
fn match_regex(pattern: &str) -> Option<String> {
    if pattern == "<all_urls>" {
        return Some("^(?:https?|file|ftp)://.*$".into());
    }
    let (scheme, rest) = pattern.split_once("://")?;
    let (host, path) = rest.split_once('/')?;
    let scheme = if scheme == "*" {
        "https?".to_string()
    } else {
        regex::escape(scheme)
    };
    let host_part = if host == "*" {
        "[^/]*".to_string()
    } else if let Some(domain) = host.strip_prefix("*.") {
        format!("(?:[^/]*\\.)?{}", regex::escape(domain))
    } else {
        regex::escape(host)
    };
    let port = if host.contains(':') {
        ""
    } else {
        "(?::[0-9]+)?"
    };
    Some(format!(
        "^{scheme}://{host_part}{port}/{}$",
        glob_part(path)
    ))
}

/// An `@include` or `@exclude`: a glob, or `/regex/` as it stands.
fn glob_regex(glob: &str) -> String {
    if glob.len() > 2 && glob.starts_with('/') {
        if let Some(inner) = glob[1..].strip_suffix('/') {
            return inner.to_string();
        }
    }
    format!("^{}$", glob_part(glob))
}

/// True when `new` is a later version than `old`, comparing number by
/// number, so 1.10 is later than 1.9.
pub fn is_newer(new: &str, old: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| {
                let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    };
    let (new, old) = (parts(new), parts(old));
    for i in 0..new.len().max(old.len()) {
        let (a, b) = (
            new.get(i).copied().unwrap_or(0),
            old.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return a > b;
        }
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Required {
    pub url: String,
    pub sha256: String,
}

/// An installed script, as saved in `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installed {
    pub id: String,
    pub meta: Meta,
    pub enabled: bool,
    /// Where it was installed from, when that was an address.
    pub source_url: Option<String>,
    #[serde(default)]
    pub requires: Vec<Required>,
    /// Hosts the user allowed beyond `@connect`.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

/// What a script needs in order to run.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub installed: Installed,
    pub source: String,
    /// The `@require` files in order.
    pub requires: Vec<String>,
}

/// All installed scripts and their saved values.
pub struct Store {
    dir: PathBuf,
    scripts: Mutex<BTreeMap<String, Installed>>,
    values: Mutex<HashMap<String, BTreeMap<String, Value>>>,
}

fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The hash a `@require` address pins, as in `file.js#sha256=abc`.
fn pinned_sha256(url: &str) -> Option<String> {
    let (_, fragment) = url.split_once('#')?;
    fragment
        .split([',', ';', '&'])
        .find_map(|p| p.strip_prefix("sha256=").or(p.strip_prefix("sha-256=")))
        .map(str::to_ascii_lowercase)
}

impl Store {
    /// Opens the store under `home`. A damaged entry is skipped, so one bad
    /// script cannot stop the browser from starting.
    pub fn open(home: &Path) -> Self {
        let dir = home.join("scripts");
        let mut scripts = BTreeMap::new();
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path().join("meta.json");
            let parsed = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| serde_json::from_str::<Installed>(&t).ok());
            if let Some(item) = parsed {
                scripts.insert(item.id.clone(), item);
            }
        }
        Self {
            dir,
            scripts: Mutex::new(scripts),
            values: Mutex::new(HashMap::new()),
        }
    }

    fn folder(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    fn require_file(&self, id: &str, url: &str) -> PathBuf {
        let name = hex::encode(&Sha256::digest(url.as_bytes())[..12]);
        self.folder(id).join("requires").join(format!("{name}.js"))
    }

    pub fn list(&self) -> Vec<Installed> {
        let scripts = self.scripts.lock().expect("scripts");
        scripts.values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Installed> {
        self.scripts.lock().expect("scripts").get(id).cloned()
    }

    /// Installs a script or replaces its older copy. A `@require` file is
    /// fetched once and refetched only when the script's version changes.
    /// Nothing is saved unless every file arrived.
    pub async fn install(
        &self,
        client: &reqwest::Client,
        source: &str,
        source_url: Option<String>,
    ) -> Result<Installed> {
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let meta = parse(source)?;
        let id = meta.id();
        let old = self.get(&id);
        let same_version = old.as_ref().is_some_and(|o| o.meta.version == meta.version);

        let mut fetched: Vec<(PathBuf, String)> = Vec::new();
        let mut requires = Vec::new();
        for url in &meta.requires {
            let path = self.require_file(&id, url);
            let known = old
                .iter()
                .flat_map(|o| o.requires.iter())
                .find(|r| &r.url == url);
            if let Some(known) = known.filter(|_| same_version && path.is_file()) {
                requires.push(known.clone());
                continue;
            }
            let address = url.split('#').next().unwrap_or(url);
            let text = ensure_success(client.get(address).send().await?)?
                .text()
                .await?;
            let sha256 = sha256_hex(&text);
            if pinned_sha256(url).is_some_and(|want| want != sha256) {
                return Err(Error::Unsupported(format!(
                    "{address} does not match the hash the script gives"
                )));
            }
            requires.push(Required {
                url: url.clone(),
                sha256,
            });
            fetched.push((path, text));
        }

        let folder = self.folder(&id);
        std::fs::create_dir_all(folder.join("requires"))?;
        for (path, text) in &fetched {
            write_atomic(path, text.as_bytes())?;
        }
        write_atomic(&folder.join("script.user.js"), source.as_bytes())?;
        let item = Installed {
            id: id.clone(),
            enabled: old.as_ref().is_none_or(|o| o.enabled),
            source_url: source_url.or_else(|| old.as_ref().and_then(|o| o.source_url.clone())),
            allowed_hosts: old.map(|o| o.allowed_hosts).unwrap_or_default(),
            meta,
            requires,
        };
        self.save(&item)?;
        self.scripts
            .lock()
            .expect("scripts")
            .insert(id, item.clone());
        Ok(item)
    }

    fn save(&self, item: &Installed) -> Result<()> {
        let text = serde_json::to_vec_pretty(item)
            .map_err(|e| Error::Unsupported(format!("could not save the script: {e}")))?;
        write_atomic(&self.folder(&item.id).join("meta.json"), &text)
    }

    fn change(&self, id: &str, f: impl FnOnce(&mut Installed)) -> Result<()> {
        let mut scripts = self.scripts.lock().expect("scripts");
        let item = scripts
            .get_mut(id)
            .ok_or_else(|| Error::Unsupported("that script is not installed".into()))?;
        f(item);
        self.save(item)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.change(id, |s| s.enabled = enabled)
    }

    pub fn allow_host(&self, id: &str, host: &str) -> Result<()> {
        self.change(id, |s| {
            if !s.allowed_hosts.iter().any(|h| h == host) {
                s.allowed_hosts.push(host.to_string());
            }
        })
    }

    /// Removes a script with its saved values.
    pub fn remove(&self, id: &str) -> Result<()> {
        let known = self.scripts.lock().expect("scripts").remove(id).is_some();
        if !known {
            return Err(Error::Unsupported("that script is not installed".into()));
        }
        self.values.lock().expect("values").remove(id);
        std::fs::remove_dir_all(self.folder(id))?;
        Ok(())
    }

    /// Reads a script and its `@require` files. A cached file that no longer
    /// matches its hash is an error, so a changed file never runs.
    pub fn load(&self, id: &str) -> Result<Loaded> {
        let installed = self
            .get(id)
            .ok_or_else(|| Error::Unsupported("that script is not installed".into()))?;
        let source = std::fs::read_to_string(self.folder(id).join("script.user.js"))?;
        let mut requires = Vec::new();
        for r in &installed.requires {
            let text = std::fs::read_to_string(self.require_file(id, &r.url))?;
            if sha256_hex(&text) != r.sha256 {
                return Err(Error::Unsupported(format!(
                    "the saved copy of {} has changed",
                    r.url
                )));
            }
            requires.push(text);
        }
        Ok(Loaded {
            installed,
            source,
            requires,
        })
    }

    fn read_values(&self, id: &str) -> BTreeMap<String, Value> {
        std::fs::read_to_string(self.folder(id).join("values.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// All saved values of a script.
    pub fn values(&self, id: &str) -> BTreeMap<String, Value> {
        let mut all = self.values.lock().expect("values");
        all.entry(id.to_string())
            .or_insert_with(|| self.read_values(id))
            .clone()
    }

    /// Saves one value, or deletes it when `value` is `None`. The lock is
    /// held all the way through the write, so saves that arrive together
    /// cannot undo each other.
    pub fn set_value(&self, id: &str, key: &str, value: Option<Value>) -> Result<()> {
        let mut all = self.values.lock().expect("values");
        let map = all
            .entry(id.to_string())
            .or_insert_with(|| self.read_values(id));
        match value {
            Some(v) => map.insert(key.to_string(), v),
            None => map.remove(key),
        };
        let text = serde_json::to_vec(&*map)
            .map_err(|e| Error::Unsupported(format!("could not save the value: {e}")))?;
        write_atomic(&self.folder(id).join("values.json"), &text)
    }

    /// Fetches the script's update address. Returns the new source when it
    /// is a later version, or `None` when this one is current.
    pub async fn find_update(&self, client: &reqwest::Client, id: &str) -> Result<Option<String>> {
        let item = self
            .get(id)
            .ok_or_else(|| Error::Unsupported("that script is not installed".into()))?;
        let check = item
            .meta
            .update_url
            .clone()
            .or_else(|| item.meta.download_url.clone())
            .or_else(|| item.source_url.clone())
            .ok_or_else(|| Error::Unsupported("this script has no update address".into()))?;
        let text = fetch_text(client, &check).await?;
        let found = parse(&text)?;
        if !is_newer(&found.version, &item.meta.version) {
            return Ok(None);
        }
        // An update address may hold only the header, as `.meta.js` does.
        let full = found
            .download_url
            .or_else(|| item.meta.download_url.clone())
            .filter(|u| u != &check);
        match full {
            Some(url) => Ok(Some(fetch_text(client, &url).await?)),
            None if check.ends_with(".meta.js") => {
                let url = check.replace(".meta.js", ".user.js");
                Ok(Some(fetch_text(client, &url).await?))
            }
            None => Ok(Some(text)),
        }
    }
}

/// Fetches a script from a web address.
pub async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let parsed = Url::parse(url)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Unsupported(
            "a script can only be installed from a web address".into(),
        ));
    }
    Ok(ensure_success(client.get(parsed).send().await?)?
        .text()
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "// ==UserScript==\n// @name Test\n// @name:ja テスト\n// @namespace n\n// @version 1.2\n// @match https://*.example.com/watch/*\n// @match *://plain.test/*\n// @include https://bunkr.*/*\n// @exclude https://bunkr.si/skip*\n// @run-at document-start\n// @noframes\n// @grant GM_setValue\n// @grant GM.xmlHttpRequest\n// @connect api.test\n// ==/UserScript==\nalert(1)\n";

    #[test]
    fn reads_the_header() {
        let m = parse(HEAD).unwrap();
        assert_eq!(m.name, "Test");
        assert_eq!(m.names["ja"], "テスト");
        assert_eq!(m.run_at, RunAt::DocumentStart);
        assert!(m.noframes);
        assert!(!m.runs_in_page());
        assert!(m.has_grant("setvalue") && m.has_grant("xmlhttprequest"));
        assert!(!m.has_grant("download"));
        assert!(parse("alert(1)").is_err());
        assert!(parse("// ==UserScript==\n// @version 1\n// ==/UserScript==").is_err());
        assert_eq!(
            parse("// ==UserScript==\n// @name a\n// ==/UserScript==")
                .unwrap()
                .run_at,
            RunAt::DocumentIdle
        );
    }

    #[test]
    fn matches_addresses() {
        let m = parse(HEAD).unwrap();
        assert!(m.applies("https://example.com/watch/1"));
        assert!(m.applies("https://a.b.example.com/watch/1?x=2#y"));
        assert!(!m.applies("http://example.com/watch/1"));
        assert!(!m.applies("https://example.com/other"));
        assert!(!m.applies("https://notexample.com/watch/1"));
        assert!(m.applies("http://plain.test/"));
        assert!(m.applies("https://plain.test:8443/a"));
        assert!(m.applies("https://bunkr.cr/a/1"));
        assert!(m.applies("https://bunkr.si/a/1"));
        assert!(!m.applies("https://bunkr.si/skip/1"));
    }

    #[test]
    fn connect_rules() {
        let m = parse(HEAD).unwrap();
        assert!(m.connect_allows("api.test", None));
        assert!(m.connect_allows("v2.api.test", None));
        assert!(!m.connect_allows("evilapi.test", None));
        assert!(!m.connect_allows("other.test", Some("page.test")));
        assert!(m.connect_allows("cdn.page.test", Some("page.test")));

        let any = Meta {
            connects: vec!["*".into()],
            ..Default::default()
        };
        assert!(any.connect_allows("anything.test", None));
        assert!(!any.connect_allows("localhost", None));
        assert!(!any.connect_allows("192.168.1.1", None));
        assert!(!any.connect_allows("[::1]", None));
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("1.10", "1.9"));
        assert!(is_newer("2026.4.2.2", "2026.4.2"));
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("1.0", "1.1"));
    }

    /// The test scripts in the repository, with every feature the real
    /// scripts need. These are MIT and checked by everyone.
    #[test]
    fn the_test_scripts_cover_every_feature() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/test/scripts");
        let mut all = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            let meta = parse(&text).unwrap();
            assert!(text.contains("@license      MIT"), "{}", meta.name);
            all.push(meta);
        }
        let granted = |f: &str| all.iter().any(|m| m.has_grant(f));
        for function in [
            "xmlhttprequest",
            "download",
            "setvalue",
            "getvalue",
            "registermenucommand",
            "addstyle",
            "setclipboard",
            "openintab",
        ] {
            assert!(granted(function), "{function} is not covered");
        }
        assert!(all.iter().any(|m| m.run_at == RunAt::DocumentStart));
        assert!(all.iter().any(|m| m.run_at == RunAt::DocumentEnd));
        assert!(all.iter().any(|m| m.noframes));
        assert!(all.iter().any(|m| !m.names.is_empty()));
        assert!(all.iter().any(|m| !m.requires.is_empty()));
        assert!(all.iter().any(|m| !m.connects.is_empty()));
        assert!(all.iter().any(|m| !m.includes.is_empty()));
        assert!(all.iter().any(|m| !m.excludes.is_empty()));
        assert!(all.iter().any(|m| m.grants == ["none"]));
        assert!(all.iter().any(|m| m.grants.is_empty()));
    }

    /// The real scripts stay on Bristo's machine, outside git, so this only
    /// runs where the folder is.
    #[test]
    fn every_sample_script_is_read_and_matches_its_site() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/userscripts");
        if !dir.is_dir() {
            return;
        }
        let want = [
            (
                "bypass-link-redirects",
                "https://www.stremio.com/warning#https%3A%2F%2Fexample.com",
            ),
            (
                "bypass-link-redirects",
                "https://forums.socialmediagirls.com/goto/link-confirmation?url=abc",
            ),
            (
                "deviantart-download-helper",
                "https://www.deviantart.com/someone/art/1",
            ),
            (
                "instagram-download-button",
                "https://www.instagram.com/p/1/",
            ),
            ("media-link-extractor", "https://bunkr.cr/a/1"),
            ("media-link-extractor", "https://www.dropbox.com/s/1"),
            (
                "mediafire-no-adblock-message",
                "https://www.mediafire.com/file/abc/a.zip/file",
            ),
            (
                "mediaset-direct-links",
                "https://mediasetinfinity.mediaset.it/video/a",
            ),
            ("telegram-media-downloader", "https://web.telegram.org/k/"),
            (
                "telegram-media-downloader-improved",
                "https://webz.telegram.org/a",
            ),
            ("x-media-downloader-zip", "https://x.com/a/status/1"),
            ("x-one-click-media-downloader", "https://twitter.com/a"),
        ];
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            let m = parse(&text).unwrap();
            let file = entry.file_name().to_string_lossy().replace(".user.js", "");
            let urls: Vec<_> = want.iter().filter(|(f, _)| *f == file).collect();
            assert!(!urls.is_empty(), "{file} has no test address");
            for (_, url) in urls {
                assert!(m.applies(url), "{file} should run on {url}");
            }
            assert!(!m.applies("https://unrelated.test/"), "{file}");
            for pattern in m.include_patterns().iter().chain(&m.exclude_patterns()) {
                assert!(compile(pattern).is_some(), "{file}: {pattern}");
            }
            seen += 1;
        }
        assert_eq!(seen, 10);
    }

    #[test]
    fn values_saved_together_all_stay() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::open(dir.path()));
        std::fs::create_dir_all(store.folder("s")).unwrap();
        let threads: Vec<_> = (0..16)
            .map(|n| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .set_value("s", &format!("k{n}"), Some(Value::from(n)))
                        .unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(store.values("s").len(), 16);
        assert_eq!(Store::open(dir.path()).values("s").len(), 16);
    }

    #[test]
    fn values_stay_after_reopening_and_scripts_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let store = Store::open(dir.path());
        let item = rt.block_on(store.install(&client, HEAD, None)).unwrap();
        store
            .set_value(&item.id, "k", Some(serde_json::json!({"a": 1})))
            .unwrap();
        store.set_enabled(&item.id, false).unwrap();
        store.allow_host(&item.id, "extra.test").unwrap();

        let store = Store::open(dir.path());
        assert_eq!(store.values(&item.id)["k"], serde_json::json!({"a": 1}));

        let next = HEAD.replace("@version 1.2", "@version 1.3");
        let same = rt.block_on(store.install(&client, &next, None)).unwrap();
        assert_eq!(same.id, item.id);
        assert_eq!(same.meta.version, "1.3");
        assert!(!same.enabled);
        assert_eq!(same.allowed_hosts, ["extra.test"]);
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.values(&item.id)["k"], serde_json::json!({"a": 1}));

        store.remove(&item.id).unwrap();
        assert!(store.list().is_empty());
        assert!(store.load(&item.id).is_err());
    }
}
