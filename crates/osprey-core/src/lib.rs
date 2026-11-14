//! osprey-core is the engine behind BM Osprey. It has no UI of its own, so the
//! CLI, a future desktop app and BM Aegis can all share it.
//!
//! The usual entry point is [`fetch`], which probes a URL, picks the right
//! strategy (multi connection download or HLS assembly) and hands the
//! pieces to ffmpeg for the final file.

pub mod bypass;
pub mod client;
pub mod detect;
pub mod direct;
pub mod error;
pub mod ffmpeg;
pub mod filter;
pub mod gateway;
pub mod hls;
pub mod inspect;
pub mod limit;
pub mod privacy;
pub mod probe;
pub mod progress;
pub mod settings;
pub mod textmode;
pub mod tools;
pub mod userscript;
pub mod ytdlp;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Reads the setting `OSPREY_<name>` from the environment. The app was
/// called BM Fetch before, so the old `BMF_<name>` still works for now.
pub fn env_var(name: &str) -> Option<OsString> {
    std::env::var_os(format!("OSPREY_{name}")).or_else(|| std::env::var_os(format!("BMF_{name}")))
}

/// When `new` is missing but `old` (the name BM Fetch used) exists, renames
/// old to new, so unfinished work from before the rename is picked up.
/// Does nothing if the rename fails, the work then starts over.
pub(crate) fn adopt_old_name(old: &Path, new: &Path) {
    if !new.exists() && old.exists() {
        let _ = std::fs::rename(old, new);
    }
}

/// The folder where a stream's pieces are kept until they are joined.
fn work_dir(parent: &Path, url: &str) -> PathBuf {
    let id = fnv1a(url);
    let work = parent.join(format!(".osprey-{id:016x}"));
    adopt_old_name(&parent.join(format!(".bmf-{id:016x}")), &work);
    work
}

pub use bypass::{bypass, Bypassed};
pub use client::{build_client, RequestOptions};
pub use detect::{classify, scan_text, FoundMedia, MediaKind};
pub use direct::DirectOptions;
pub use error::{Error, Result};
pub use filter::{Filters, MediaRules, RequestKind};
pub use hls::HlsOptions;
pub use privacy::{DnsMode, Privacy, Route};
pub use probe::{probe, ProbeInfo};
pub use progress::{Progress, ProgressFn, Unit};

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub request: RequestOptions,
    pub direct: DirectOptions,
    pub hls: HlsOptions,
    /// A file path, or an existing folder to save into. Defaults to the
    /// current folder with a name taken from the URL.
    pub output: Option<PathBuf>,
    /// When the sound is a separate address, it is fetched too and joined
    /// to the video in one file.
    pub audio: Option<SeparateAudio>,
}

/// The sound for a video that has none of its own.
#[derive(Debug, Clone, PartialEq)]
pub struct SeparateAudio {
    pub url: String,
    pub kind: MediaKind,
    /// The type of the joined file, like "mp4".
    pub ext: String,
}

/// Downloads whatever media lives at `url` and returns the path of the
/// finished file.
pub async fn fetch(url: &str, opts: &FetchOptions, progress: ProgressFn) -> Result<PathBuf> {
    if let Some(audio) = &opts.audio {
        return fetch_joined(url, audio, opts, progress).await;
    }
    let clients = client::build_clients(&opts.request)?;
    let client = clients.first().clone();
    let info = probe(&client, url).await?;

    match info.kind {
        MediaKind::Direct => {
            let name = info.filename.clone().unwrap_or_else(|| "download".into());
            let dest = resolve_output(opts.output.as_deref(), &sanitize_filename(&name)).await?;
            direct::download(&clients, &info, &dest, &opts.direct, progress).await?;
            Ok(dest)
        }
        MediaKind::Hls => {
            let stem = sanitize_filename(&stream_name(&info.final_url));
            let dest = resolve_output(opts.output.as_deref(), &format!("{stem}.mp4")).await?;
            let parent = dest.parent().map(Path::to_path_buf).unwrap_or_default();
            let work = work_dir(&parent, url);

            let out =
                hls::download(&clients, &info.final_url, &work, &opts.hls, progress.clone()).await?;
            finish_stream(&out, &dest, &work, progress).await
        }
        MediaKind::Page => Err(Error::Unsupported(format!(
            "{url} is a web page, not media. Try `osprey sniff` to list the media on it or `osprey bypass` if it is a short link"
        ))),
        MediaKind::Dash => Err(Error::Unsupported(
            "DASH (.mpd) streams are on the roadmap but not supported yet".into(),
        )),
    }
}

/// Fetches a video and its separate sound, each as a file or a stream, and
/// joins them. Unfinished work stays in the work folder, so a retry or a
/// resume carries on from there.
async fn fetch_joined(
    url: &str,
    audio: &SeparateAudio,
    opts: &FetchOptions,
    progress: ProgressFn,
) -> Result<PathBuf> {
    let clients = client::build_clients(&opts.request)?;
    let stem = sanitize_filename(&stream_name(url));
    let default_name = format!("{stem}.{}", audio.ext);
    let dest = resolve_output(opts.output.as_deref(), &default_name).await?;
    let parent = dest.parent().map(Path::to_path_buf).unwrap_or_default();
    let work = work_dir(&parent, url);
    let video = fetch_track(&clients, url, &work.join("video"), opts, &progress).await?;
    let audio = fetch_track(&clients, &audio.url, &work.join("audio"), opts, &progress).await?;
    let out = hls::HlsOutput {
        video,
        audio: Some(audio),
    };
    finish_stream(&out, &dest, &work, progress).await
}

/// Saves one track in `dir`, and returns where it landed.
async fn fetch_track(
    clients: &client::Clients,
    url: &str,
    dir: &Path,
    opts: &FetchOptions,
    progress: &ProgressFn,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let info = probe(clients.first(), url).await?;
    match info.kind {
        MediaKind::Hls => {
            let out = hls::download(
                clients.clone(),
                &info.final_url,
                dir,
                &opts.hls,
                progress.clone(),
            )
            .await?;
            Ok(out.video)
        }
        MediaKind::Direct => {
            let file = dir.join("track");
            direct::download(
                clients.clone(),
                &info,
                &file,
                &opts.direct,
                progress.clone(),
            )
            .await?;
            Ok(file)
        }
        other => Err(Error::Unsupported(format!(
            "cannot fetch a {other:?} track yet"
        ))),
    }
}

/// Turns finished tracks into the final file.
async fn finish_stream(
    out: &hls::HlsOutput,
    dest: &Path,
    work: &Path,
    progress: ProgressFn,
) -> Result<PathBuf> {
    if ffmpeg::is_available().await {
        progress(Progress::Status("joining the pieces with ffmpeg".into()));
        let merged = match &out.audio {
            Some(audio) => ffmpeg::mux(&out.video, audio, dest).await,
            None => ffmpeg::remux(&out.video, dest).await,
        };
        match merged {
            Ok(()) => {
                let _ = tokio::fs::remove_dir_all(work).await;
                Ok(dest.to_path_buf())
            }
            Err(e) => {
                // Like 1DM's fallback: keep the joined raw stream,
                // which most players open, rather than lose it.
                let _ = tokio::fs::remove_file(dest).await;
                let saved = keep_raw(out, dest, work).await?;
                progress(Progress::Status(format!(
                    "ffmpeg could not make an MP4 ({e}), so the raw stream was kept"
                )));
                Ok(saved)
            }
        }
    } else {
        let saved = keep_raw(out, dest, work).await?;
        progress(Progress::Status(if out.audio.is_some() {
            "ffmpeg not found, so video and audio were saved as separate files".into()
        } else {
            "ffmpeg not found, saved the raw stream".into()
        }));
        Ok(saved)
    }
}

/// Moves the joined raw stream (and separate audio, if any) next to
/// `dest` and removes the work folder. Returns the video file's path.
async fn keep_raw(out: &hls::HlsOutput, dest: &Path, work: &Path) -> Result<PathBuf> {
    let raw_ext = out
        .video
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("ts");
    let video_dest = if out.audio.is_some() {
        dest.with_extension(format!("video.{raw_ext}"))
    } else {
        dest.with_extension(raw_ext)
    };
    tokio::fs::rename(&out.video, &video_dest).await?;
    if let Some(audio) = &out.audio {
        let a_ext = audio.extension().and_then(|e| e.to_str()).unwrap_or("ts");
        tokio::fs::rename(audio, dest.with_extension(format!("audio.{a_ext}"))).await?;
    }
    let _ = tokio::fs::remove_dir_all(work).await;
    Ok(video_dest)
}

/// Picks a readable name for a stream. Playlists are usually called
/// master.m3u8 or index.m3u8, so the folder name is often more useful.
fn stream_name(url: &str) -> String {
    const GENERIC: &[&str] = &[
        "master",
        "index",
        "playlist",
        "prog_index",
        "chunklist",
        "manifest",
        "main",
        "video",
    ];
    let Ok(parsed) = url::Url::parse(url) else {
        return "video".into();
    };
    let segs: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    for seg in segs.iter().rev() {
        let stem = seg.rsplit_once('.').map(|(s, _)| s).unwrap_or(seg);
        if !stem.is_empty() && !GENERIC.contains(&stem.to_ascii_lowercase().as_str()) {
            return stem.to_string();
        }
    }
    "video".into()
}

/// Makes a name safe on Windows, macOS and Linux.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "LPT1", "LPT2", "LPT3",
    ];
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if cleaned.is_empty() || reserved.contains(&stem.as_str()) {
        format!("_{cleaned}")
    } else {
        cleaned.chars().take(180).collect()
    }
}

async fn resolve_output(output: Option<&Path>, default_name: &str) -> Result<PathBuf> {
    let path = match output {
        Some(p)
            if tokio::fs::metadata(p)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false) =>
        {
            p.join(default_name)
        }
        // "videos/" means a folder, even when it does not exist yet.
        Some(p) if p.as_os_str().to_string_lossy().ends_with(['/', '\\']) => {
            tokio::fs::create_dir_all(p).await?;
            p.join(default_name)
        }
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?.join(default_name),
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    Ok(unique_path(path).await)
}

/// Adds " (1)", " (2)" and so on instead of overwriting a finished file.
async fn unique_path(path: PathBuf) -> PathBuf {
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return path;
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    for n in 1.. {
        let candidate = path.with_file_name(format!("{stem} ({n}){ext}"));
        if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            return candidate;
        }
    }
    unreachable!()
}

/// Stable hash used to name the resume folder for a URL.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_streams_sensibly() {
        assert_eq!(
            stream_name("https://cdn.test/shows/my-episode/master.m3u8"),
            "my-episode"
        );
        assert_eq!(stream_name("https://cdn.test/v/clip42.m3u8?x=1"), "clip42");
        assert_eq!(stream_name("https://cdn.test/index.m3u8"), "video");
    }

    #[tokio::test]
    async fn output_folders_that_do_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let wanted = dir.path().join("new folder").display().to_string() + "/";
        let p = resolve_output(Some(Path::new(&wanted)), "clip.mp4")
            .await
            .unwrap();
        assert_eq!(p, dir.path().join("new folder").join("clip.mp4"));
        assert!(dir.path().join("new folder").is_dir());

        let file = dir.path().join("named.mp4");
        assert_eq!(resolve_output(Some(&file), "x.mp4").await.unwrap(), file);
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_filename("a/b:c?.mp4"), "a_b_c_.mp4");
        assert_eq!(sanitize_filename("CON.mp4"), "_CON.mp4");
        assert_eq!(sanitize_filename("  "), "_");
        assert_eq!(sanitize_filename("clip. "), "clip");
    }
}
