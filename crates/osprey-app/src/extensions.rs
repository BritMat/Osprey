//! Real browser extensions on Windows, through WebView2. Each one is an
//! unpacked extension in `<BM Osprey home>/extensions/<name>`. WebView2 loads
//! them when a web view is made, so a change needs a restart of the browser.
//! Private tabs load none, since an extension could keep what it sees.

use std::path::{Path, PathBuf};

pub fn folder(home: &Path) -> PathBuf {
    home.join("extensions")
}

/// The installed extensions, by folder name.
pub fn list(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("manifest.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn copy_folder(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_folder(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Copies an unpacked extension into BM Osprey's folder.
pub fn add(home: &Path, source: &Path) -> Result<String, String> {
    if !source.join("manifest.json").is_file() {
        return Err("that folder has no manifest.json, so it is not an unpacked extension".into());
    }
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .ok_or("that folder has no name")?;
    let target = folder(home).join(osprey_core::sanitize_filename(&name));
    copy_folder(source, &target).map_err(|e| e.to_string())?;
    Ok(name)
}

pub fn remove(home: &Path, name: &str) -> Result<(), String> {
    if !list(home).iter().any(|n| n == name) {
        return Err("that extension is not installed".into());
    }
    std::fs::remove_dir_all(folder(home).join(name)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_lists_and_removes() {
        let home = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        assert!(add(home.path(), source.path()).is_err());

        let ext = source.path().join("my-ext");
        std::fs::create_dir_all(ext.join("js")).unwrap();
        std::fs::write(ext.join("manifest.json"), "{}").unwrap();
        std::fs::write(ext.join("js/a.js"), "1").unwrap();
        assert_eq!(add(home.path(), &ext).unwrap(), "my-ext");
        assert_eq!(list(home.path()), ["my-ext"]);
        assert!(folder(home.path()).join("my-ext/js/a.js").is_file());

        assert!(remove(home.path(), "../other").is_err());
        remove(home.path(), "my-ext").unwrap();
        assert!(list(home.path()).is_empty());
    }
}
