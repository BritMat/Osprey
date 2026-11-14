//! Thin wrapper around the ffmpeg command line tool for remuxing, joining
//! separate audio and video, and converting between formats. Arguments
//! are passed directly to the process, never through a shell.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{Error, Result};

/// The ffmpeg binary. Set `OSPREY_FFMPEG` to point at a bundled copy.
pub fn binary() -> PathBuf {
    crate::env_var("FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

pub async fn is_available() -> bool {
    Command::new(binary())
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn run(args: Vec<OsString>) -> Result<()> {
    let output = Command::new(binary())
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            Error::Ffmpeg(format!(
                "could not start ffmpeg ({e}). Install it or set OSPREY_FFMPEG to its path"
            ))
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<_> = stderr.lines().rev().take(6).collect();
    let tail: Vec<_> = tail.into_iter().rev().collect();
    Err(Error::Ffmpeg(tail.join("\n")))
}

fn os(s: &str) -> OsString {
    s.into()
}

/// Copies the streams into a new container without re-encoding. Fast and
/// lossless. Used to turn stitched .ts segments into a normal .mp4.
pub async fn remux(input: &Path, output: &Path) -> Result<()> {
    // Only audio and video. HLS streams often carry ID3 data tracks that
    // MP4 cannot hold.
    let mut args = vec![
        os("-i"),
        input.into(),
        os("-map"),
        os("0:v?"),
        os("-map"),
        os("0:a?"),
        os("-c"),
        os("copy"),
    ];
    if is_mp4_family(output) {
        args.extend([os("-movflags"), os("+faststart")]);
    }
    args.push(output.into());
    run(args).await
}

/// Joins a video file and a separate audio file into one, without
/// re-encoding.
pub async fn mux(video: &Path, audio: &Path, output: &Path) -> Result<()> {
    let mut args = vec![
        os("-i"),
        video.into(),
        os("-i"),
        audio.into(),
        os("-map"),
        os("0:v:0"),
        os("-map"),
        os("1:a:0"),
        os("-c"),
        os("copy"),
    ];
    if is_mp4_family(output) {
        args.extend([os("-movflags"), os("+faststart")]);
    }
    args.push(output.into());
    run(args).await
}

fn is_mp4_family(p: &Path) -> bool {
    matches!(ext(p).as_str(), "mp4" | "m4v" | "m4a" | "mov")
}

fn ext(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Converts `input` to the format implied by the extension of `output`.
/// With `copy_only` the streams are copied as they are (fast, no quality
/// loss, only works when the target container supports the codecs).
pub async fn convert(input: &Path, output: &Path, copy_only: bool) -> Result<()> {
    if copy_only {
        return remux(input, output).await;
    }
    let preset: &[&str] = match ext(output).as_str() {
        "mp3" => &["-vn", "-c:a", "libmp3lame", "-q:a", "2"],
        "m4a" | "aac" => &["-vn", "-c:a", "aac", "-b:a", "192k"],
        "opus" | "ogg" => &["-vn", "-c:a", "libopus", "-b:a", "128k"],
        "flac" => &["-vn", "-c:a", "flac"],
        "wav" => &["-vn", "-c:a", "pcm_s16le"],
        "mp4" | "m4v" | "mov" => &[
            "-c:v",
            "libx264",
            "-crf",
            "20",
            "-preset",
            "veryfast",
            "-c:a",
            "aac",
            "-b:a",
            "160k",
            "-movflags",
            "+faststart",
        ],
        "webm" => &[
            "-c:v",
            "libvpx-vp9",
            "-crf",
            "32",
            "-b:v",
            "0",
            "-c:a",
            "libopus",
            "-b:a",
            "128k",
        ],
        "mkv" => &["-map", "0", "-c", "copy"],
        "gif" => &["-vf", "fps=12,scale=480:-1:flags=lanczos", "-an"],
        _ => &[],
    };
    let mut args = vec![os("-i"), input.into()];
    args.extend(preset.iter().map(|s| os(s)));
    args.push(output.into());
    run(args).await
}
