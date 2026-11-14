//! yt-dlp as an extractor: for a page it knows, it lists every format with
//! the headers needed to fetch it. BM Osprey only asks it what is there.
//! Downloads still go through BM Osprey's own engine and gateway.
//!
//! A site yt-dlp does not know is a normal answer, not an error. The caller
//! then falls back to its own capture. Formats marked as DRM are left out,
//! since BM Osprey does not touch DRM.
//!
//! Many sites keep picture and sound apart. Each video only format is paired
//! here with the audio that suits it best, and the download joins the two.
//!
//! Arguments go directly to the process, never through a shell. `tools.rs`
//! downloads yt-dlp and Deno.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::detect::MediaKind;
use crate::error::{Error, Result};

const TIMEOUT: Duration = Duration::from_secs(45);
/// yt-dlp's name for its fallback that scans any page for a video.
const GENERIC: &str = "generic";

/// The yt-dlp binary: `OSPREY_YTDLP` if set, then the copy in BM Osprey's tools
/// folder, then whatever is on PATH.
pub fn binary(home: &Path) -> PathBuf {
    if let Some(p) = crate::env_var("YTDLP") {
        return PathBuf::from(p);
    }
    let name = if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    };
    let bundled = home.join("tools").join(name);
    if bundled.is_file() {
        bundled
    } else {
        PathBuf::from(name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    /// Sent through this proxy, normally the gateway, so the tab's route
    /// also covers yt-dlp.
    pub proxy: Option<String>,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    /// Deno, which yt-dlp needs for YouTube.
    pub deno: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Format {
    pub id: String,
    pub url: String,
    pub kind: MediaKind,
    pub ext: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_only: bool,
    pub video_only: bool,
    /// The codec in words, like "H.264" or "AAC".
    pub codec_name: Option<String>,
    pub language: Option<String>,
    /// yt-dlp's own note, like "(original), high".
    pub note: Option<String>,
    /// For a video only format: the id of the audio to join it with.
    pub pairs_with: Option<String>,
    /// Kilobits per second.
    pub bitrate: Option<f64>,
    pub size: Option<u64>,
    /// True when `size` is yt-dlp's guess or worked out from the bitrate.
    pub size_is_estimate: bool,
    /// The headers this format needs. Can hold cookies, so they are not
    /// written out anywhere.
    #[serde(skip)]
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Subtitle {
    pub language: String,
    pub url: String,
    pub ext: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Extracted {
    pub extractor: String,
    pub title: Option<String>,
    /// Seconds.
    pub duration: Option<f64>,
    pub thumbnail: Option<String>,
    pub live: bool,
    pub formats: Vec<Format>,
    pub subtitles: Vec<Subtitle>,
    /// How many formats were left out for being DRM protected.
    pub drm_formats: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Found(Box<Extracted>),
    /// yt-dlp has nothing for this page, with the reason.
    NotKnown(String),
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawInfo {
    extractor: Option<String>,
    title: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    is_live: Option<bool>,
    #[serde(deserialize_with = "or_default")]
    formats: Vec<RawFormat>,
    #[serde(deserialize_with = "or_default")]
    subtitles: BTreeMap<String, Vec<RawSubtitle>>,
    #[serde(deserialize_with = "or_default")]
    entries: Vec<Option<RawInfo>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawFormat {
    format_id: Option<String>,
    url: Option<String>,
    protocol: Option<String>,
    ext: Option<String>,
    width: Option<f64>,
    height: Option<f64>,
    fps: Option<f64>,
    vcodec: Option<String>,
    acodec: Option<String>,
    tbr: Option<f64>,
    abr: Option<f64>,
    language: Option<String>,
    format_note: Option<String>,
    filesize: Option<f64>,
    filesize_approx: Option<f64>,
    has_drm: Option<serde_json::Value>,
    #[serde(deserialize_with = "or_default")]
    http_headers: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawSubtitle {
    url: Option<String>,
    ext: Option<String>,
}

/// yt-dlp writes `null` for lists it has nothing for.
fn or_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn kind_of(protocol: &str) -> Option<MediaKind> {
    match protocol {
        "http" | "https" => Some(MediaKind::Direct),
        "m3u8" | "m3u8_native" => Some(MediaKind::Hls),
        "http_dash_segments" => Some(MediaKind::Dash),
        _ => None,
    }
}

/// yt-dlp writes "none" for a track that is certainly missing, and leaves
/// the field empty when it does not know.
fn codec(value: Option<&str>) -> Option<String> {
    value.filter(|v| *v != "none").map(str::to_string)
}

fn whole(value: Option<f64>) -> Option<u32> {
    value
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v as u32)
}

/// The field is true, false, or the word "maybe". Only a clear no, or no
/// value at all, is safe.
fn is_drm(value: Option<&serde_json::Value>) -> bool {
    !matches!(
        value,
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false))
    )
}

fn codec_name(codec: &str) -> Option<String> {
    let names = [
        ("avc", "H.264"),
        ("hev", "H.265"),
        ("hvc", "H.265"),
        ("av01", "AV1"),
        ("vp9", "VP9"),
        ("vp09", "VP9"),
        ("vp8", "VP8"),
        ("mp4a", "AAC"),
        ("aac", "AAC"),
        ("opus", "Opus"),
        ("vorbis", "Vorbis"),
        ("mp3", "MP3"),
    ];
    names
        .iter()
        .find(|(start, _)| codec.starts_with(start))
        .map(|(_, name)| name.to_string())
}

/// Which audio suits a video, or None when there is no audio to join. The
/// same language comes first, then a container the two can share without
/// converting, then the higher bitrate. yt-dlp lists formats from worst to
/// best, so the later one wins when nothing else tells them apart, which
/// is the case on Dailymotion.
fn best_audio<'a>(formats: &'a [Format], video: &Format) -> Option<&'a Format> {
    formats
        .iter()
        .enumerate()
        .filter(|(_, f)| f.audio_only && f.kind != MediaKind::Dash)
        .max_by_key(|(place, f)| {
            let language = match (&video.language, &f.language) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            let bitrate = f.bitrate.unwrap_or(0.0) as u64;
            (language, same_family(video, f), bitrate, *place)
        })
        .map(|(_, f)| f)
}

fn family(f: &Format) -> &'static str {
    match f.ext.as_deref() {
        Some("mp4" | "m4a" | "m4v" | "mov") => "mp4",
        Some("webm" | "weba") => "webm",
        _ => "other",
    }
}

fn same_family(a: &Format, b: &Format) -> bool {
    family(a) == family(b)
}

/// The file type for a joined video and audio. MP4 cannot hold every
/// codec, and Matroska holds anything.
pub fn joined_ext(video: &Format, audio: &Format) -> &'static str {
    if family(video) == "mp4" && family(audio) == "mp4" {
        "mp4"
    } else {
        "mkv"
    }
}

fn pair_audio(formats: &mut [Format]) {
    let pairs: Vec<Option<String>> = formats
        .iter()
        .map(|f| {
            f.video_only
                .then(|| best_audio(formats, f).map(|a| a.id.clone()))
                .flatten()
        })
        .collect();
    for (format, pair) in formats.iter_mut().zip(pairs) {
        format.pairs_with = pair;
    }
}

fn format_from(raw: RawFormat, duration: Option<f64>) -> Option<Format> {
    let kind = kind_of(raw.protocol.as_deref()?)?;
    let url = raw.url?;
    let (video, audio) = (raw.vcodec.as_deref(), raw.acodec.as_deref());
    // Picture strips for the seek bar have neither track.
    if video == Some("none") && audio == Some("none") {
        return None;
    }
    let exact = raw.filesize.filter(|s| *s > 0.0).map(|s| s as u64);
    let guess = raw
        .filesize_approx
        .filter(|s| *s > 0.0)
        .map(|s| s as u64)
        .or_else(|| {
            let bytes = raw.tbr? * 1000.0 / 8.0 * duration?;
            (bytes > 0.0).then_some(bytes as u64)
        });
    Some(Format {
        id: raw.format_id.unwrap_or_default(),
        url,
        kind,
        ext: raw.ext,
        width: whole(raw.width),
        height: whole(raw.height),
        fps: raw.fps,
        video_codec: codec(video),
        audio_codec: codec(audio),
        audio_only: video == Some("none"),
        video_only: audio == Some("none"),
        codec_name: codec(video)
            .or(codec(audio))
            .as_deref()
            .and_then(codec_name),
        language: raw.language,
        note: raw.format_note,
        pairs_with: None,
        bitrate: raw.tbr.or(raw.abr),
        size: exact.or(guess),
        size_is_estimate: exact.is_none() && guess.is_some(),
        headers: raw.http_headers.into_iter().collect(),
    })
}

/// Reads what `yt-dlp -J` printed. It prints `null` when it failed.
pub fn parse(json: &str) -> Result<Outcome> {
    let Some(mut info) = serde_json::from_str::<Option<RawInfo>>(json)? else {
        return Ok(Outcome::NotKnown("yt-dlp found nothing".into()));
    };
    // A page with several videos comes as a list. For now the first one
    // that has formats. The link grabber (item 9) will want them all.
    if info.formats.is_empty() {
        let extractor = info.extractor.take();
        let first = info
            .entries
            .into_iter()
            .flatten()
            .find(|e| !e.formats.is_empty());
        match first {
            Some(entry) => {
                info = entry;
                info.extractor = info.extractor.or(extractor);
            }
            None => return Ok(Outcome::NotKnown("yt-dlp listed no formats".into())),
        }
    }
    let extractor = info.extractor.unwrap_or_default();
    if extractor == GENERIC {
        return Ok(Outcome::NotKnown(format!(
            "yt-dlp has no extractor for this site, only its {GENERIC} scan"
        )));
    }
    let total = info.formats.len();
    let (drm, clear): (Vec<_>, Vec<_>) = info
        .formats
        .into_iter()
        .partition(|f| is_drm(f.has_drm.as_ref()));
    let duration = info.duration;
    let mut formats: Vec<Format> = clear
        .into_iter()
        .filter_map(|f| format_from(f, duration))
        .collect();
    pair_audio(&mut formats);
    if formats.is_empty() {
        return Ok(Outcome::NotKnown(format!(
            "none of the {total} formats can be used"
        )));
    }
    let subtitles = info
        .subtitles
        .into_iter()
        .flat_map(|(language, list)| {
            list.into_iter().filter_map(move |s| {
                Some(Subtitle {
                    language: language.clone(),
                    url: s.url?,
                    ext: s.ext,
                })
            })
        })
        .collect();
    Ok(Outcome::Found(Box::new(Extracted {
        extractor,
        title: info.title,
        duration,
        thumbnail: info.thumbnail,
        live: info.is_live.unwrap_or(false),
        formats,
        subtitles,
        drm_formats: drm.len(),
    })))
}

fn arguments(url: &str, opts: &ExtractOptions) -> Vec<String> {
    let mut args: Vec<String> = [
        "--dump-single-json",
        "--no-playlist",
        "--no-warnings",
        // Only what is set here counts, not a config file or plugin.
        "--ignore-config",
        "--no-plugin-dirs",
        "--no-cache-dir",
        // Its scan of any page would fetch the page a second time, and
        // BM Osprey has its own capture for those.
        "--ies",
        "default,-generic",
        "--socket-timeout",
        "15",
    ]
    .map(str::to_string)
    .to_vec();
    if let Some(p) = &opts.proxy {
        args.extend(["--proxy".to_string(), p.clone()]);
    }
    if let Some(u) = &opts.user_agent {
        args.extend(["--user-agent".to_string(), u.clone()]);
    }
    if let Some(r) = &opts.referer {
        args.extend(["--referer".to_string(), r.clone()]);
    }
    if let Some(d) = &opts.deno {
        args.extend(["--js-runtimes".to_string(), format!("deno:{}", d.display())]);
    }
    // Nothing after this can be taken for an option.
    args.extend(["--".to_string(), url.to_string()]);
    args
}

/// The last line yt-dlp wrote as an error.
fn reason(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .map(|l| l.trim_start_matches("ERROR:").trim().to_string())
        .unwrap_or_else(|| "yt-dlp stopped without saying why".to_string())
}

/// Asks yt-dlp about a page. An error here means yt-dlp could not be run
/// at all. A page it cannot read is [`Outcome::NotKnown`].
pub async fn extract(binary: &Path, url: &str, opts: &ExtractOptions) -> Result<Outcome> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Error::Unsupported(format!(
            "yt-dlp only gets web addresses, not {url}"
        )));
    }
    let mut command = Command::new(binary);
    command
        .args(arguments(url, opts))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    // No console window flashing up behind the browser.
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let output = match tokio::time::timeout(TIMEOUT, command.output()).await {
        Ok(done) => done.map_err(|e| {
            Error::Unsupported(format!(
                "could not start yt-dlp ({e}). Install it or set OSPREY_YTDLP to its path"
            ))
        })?,
        Err(_) => {
            return Ok(Outcome::NotKnown(format!(
                "yt-dlp gave no answer in {} seconds",
                TIMEOUT.as_secs()
            )))
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(Outcome::NotKnown(reason(&stderr)));
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(json: &str) -> Extracted {
        match parse(json).unwrap() {
            Outcome::Found(e) => *e,
            other => panic!("expected formats, got {other:?}"),
        }
    }

    #[test]
    fn reads_formats_of_every_kind() {
        let e = found(
            r#"{"extractor":"Odnoklassniki","title":"Ep 12","duration":1000.0,
            "thumbnail":"https://t.test/a.jpg","is_live":false,
            "formats":[
              {"format_id":"hd","url":"https://v.test/a.mp4","protocol":"https","ext":"mp4",
               "width":1280,"height":720,"filesize":5000,
               "http_headers":{"User-Agent":"x","Referer":"https://p.test/"}},
              {"format_id":"hls-2083","url":"https://v.test/a.m3u8","protocol":"m3u8_native",
               "height":720.0,"tbr":2000.0},
              {"format_id":"d","url":"https://v.test/a.mpd","protocol":"http_dash_segments",
               "vcodec":"avc1","acodec":"none","filesize_approx":777.9},
              {"format_id":"a","url":"https://v.test/a.m4a","protocol":"https",
               "vcodec":"none","acodec":"mp4a.40.2"}
            ],
            "subtitles":{"en":[{"url":"https://s.test/en.vtt","ext":"vtt"},{"ext":"srt"}]}}"#,
        );
        assert_eq!(e.extractor, "Odnoklassniki");
        assert_eq!(e.duration, Some(1000.0));
        assert_eq!(e.drm_formats, 0);
        let [hd, hls, dash, audio] = &e.formats[..] else {
            panic!("four formats");
        };
        assert_eq!(hd.kind, MediaKind::Direct);
        assert_eq!(hls.kind, MediaKind::Hls);
        assert_eq!(dash.kind, MediaKind::Dash);

        assert_eq!((hd.size, hd.size_is_estimate), (Some(5000), false));
        assert_eq!(hd.height, Some(720));
        assert!(hd
            .headers
            .contains(&("Referer".into(), "https://p.test/".into())));
        assert_eq!(
            (hls.size, hls.size_is_estimate),
            (Some(250_000_000), true),
            "2000 kbit/s for 1000 s"
        );
        assert_eq!((dash.size, dash.size_is_estimate), (Some(777), true));
        assert!(dash.video_only && !dash.audio_only);
        assert_eq!(dash.audio_codec, None);
        assert!(audio.audio_only && !audio.video_only);

        assert_eq!(e.subtitles.len(), 1, "an entry with no address is left out");
        assert_eq!(e.subtitles[0].language, "en");
    }

    /// The shape Dailymotion really gives: separate tracks, no audio
    /// bitrate, and two formats at the top height.
    const SPLIT: &str = r#"{"extractor":"dailymotion","duration":934.0,"formats":[
      {"format_id":"a1","url":"https://v.test/q1.m3u8","protocol":"m3u8_native","ext":"mp4",
       "vcodec":"none","format_note":"(original), low"},
      {"format_id":"a2","url":"https://v.test/q2.m3u8","protocol":"m3u8_native","ext":"mp4",
       "vcodec":"none","format_note":"(original), high"},
      {"format_id":"v0","url":"https://v.test/h264.m3u8","protocol":"m3u8_native","ext":"mp4",
       "height":1080,"vcodec":"avc1.640028","acodec":"none","tbr":6221.6},
      {"format_id":"v1","url":"https://v.test/av1.m3u8","protocol":"m3u8_native","ext":"mp4",
       "height":1080,"vcodec":"av01.0.08M.08","acodec":"none","tbr":3191.6}
    ]}"#;

    #[test]
    fn video_only_formats_are_paired_with_the_best_audio() {
        let e = found(SPLIT);
        let pair = |id: &str| {
            let f = e.formats.iter().find(|f| f.id == id).unwrap();
            f.pairs_with.as_deref()
        };
        assert_eq!(pair("v0"), Some("a2"), "the last one is the best");
        assert_eq!(pair("v1"), Some("a2"));
        assert_eq!(pair("a2"), None, "audio only stays its own choice");
        let v0 = &e.formats[2];
        assert_eq!(v0.codec_name.as_deref(), Some("H.264"));
        assert_eq!(e.formats[3].codec_name.as_deref(), Some("AV1"));
        assert_eq!(e.formats[1].note.as_deref(), Some("(original), high"));
    }

    #[test]
    fn audio_is_chosen_by_language_then_container_then_bitrate() {
        let e = found(
            r#"{"extractor":"x","formats":[
              {"format_id":"en-hi","url":"https://v.test/1","protocol":"https","ext":"m4a",
               "vcodec":"none","language":"en","abr":320},
              {"format_id":"ja-lo","url":"https://v.test/2","protocol":"https","ext":"m4a",
               "vcodec":"none","language":"ja","abr":64},
              {"format_id":"ja-opus","url":"https://v.test/3","protocol":"https","ext":"webm",
               "vcodec":"none","language":"ja","abr":160},
              {"format_id":"ja-v","url":"https://v.test/4","protocol":"https","ext":"mp4",
               "vcodec":"avc1","acodec":"none","language":"ja"},
              {"format_id":"web-v","url":"https://v.test/5","protocol":"https","ext":"webm",
               "vcodec":"vp9","acodec":"none"}
            ]}"#,
        );
        let id = |want: &str| e.formats.iter().find(|f| f.id == want).unwrap();
        assert_eq!(id("ja-v").pairs_with.as_deref(), Some("ja-lo"));
        assert_eq!(id("web-v").pairs_with.as_deref(), Some("ja-opus"));
        assert_eq!(joined_ext(id("ja-v"), id("ja-lo")), "mp4");
        assert_eq!(joined_ext(id("web-v"), id("ja-opus")), "mkv");
        assert_eq!(joined_ext(id("web-v"), id("ja-lo")), "mkv");
    }

    #[test]
    fn with_no_audio_at_all_nothing_is_paired() {
        let e = found(
            r#"{"extractor":"x","formats":[
              {"format_id":"v","url":"https://v.test/1","protocol":"https",
               "vcodec":"avc1","acodec":"none"},
              {"format_id":"d","url":"https://v.test/2.mpd","protocol":"http_dash_segments",
               "vcodec":"none","acodec":"opus"}
            ]}"#,
        );
        assert_eq!(e.formats[0].pairs_with, None, "DASH cannot be fetched yet");
    }

    #[test]
    fn empty_fields_written_as_null_are_fine() {
        let e = found(
            r#"{"extractor":"x","subtitles":null,"entries":null,"formats":[
              {"url":"https://v.test/a.mp4","protocol":"https","http_headers":null}]}"#,
        );
        assert_eq!(e.formats.len(), 1);
    }

    #[test]
    fn unusable_formats_are_left_out() {
        let e = found(
            r#"{"extractor":"x","formats":[
              {"url":"https://v.test/ok.mp4","protocol":"https"},
              {"url":"https://v.test/sb","protocol":"mhtml"},
              {"url":"https://v.test/sb2","protocol":"https","vcodec":"none","acodec":"none"},
              {"protocol":"https"}
            ]}"#,
        );
        assert_eq!(e.formats.len(), 1);
        assert_eq!(e.formats[0].url, "https://v.test/ok.mp4");
    }

    #[test]
    fn drm_is_never_listed() {
        let e = found(
            r#"{"extractor":"x","formats":[
              {"url":"https://v.test/1.mp4","protocol":"https","has_drm":false},
              {"url":"https://v.test/2.mp4","protocol":"https","has_drm":null},
              {"url":"https://v.test/3.mpd","protocol":"http_dash_segments","has_drm":true},
              {"url":"https://v.test/4.m3u8","protocol":"m3u8","has_drm":"maybe"}
            ]}"#,
        );
        assert_eq!(e.formats.len(), 2);
        assert_eq!(e.drm_formats, 2);

        let only_drm = r#"{"extractor":"x","formats":[
            {"url":"https://v.test/3.mpd","protocol":"http_dash_segments","has_drm":true}]}"#;
        assert!(matches!(parse(only_drm).unwrap(), Outcome::NotKnown(_)));
    }

    #[test]
    fn unknown_sites_fall_back() {
        assert!(matches!(parse("null").unwrap(), Outcome::NotKnown(_)));
        assert!(matches!(
            parse(r#"{"extractor":"x","formats":[]}"#).unwrap(),
            Outcome::NotKnown(_)
        ));
        let generic = r#"{"extractor":"generic","formats":[
            {"url":"https://v.test/a.mp4","protocol":"https"}]}"#;
        assert!(matches!(parse(generic).unwrap(), Outcome::NotKnown(_)));
        assert!(parse("not json").is_err());
    }

    #[test]
    fn a_list_uses_its_first_video() {
        let e = found(
            r#"{"_type":"playlist","extractor":"list","entries":[null,{"entries":[]},
              {"title":"one","formats":[{"url":"https://v.test/1.mp4","protocol":"https"}]}]}"#,
        );
        assert_eq!(e.title.as_deref(), Some("one"));
        assert_eq!(
            e.extractor, "list",
            "the list's name when the entry has none"
        );
    }

    #[test]
    fn a_web_address_cannot_become_an_option() {
        let args = arguments(
            "https://a.test/",
            &ExtractOptions {
                proxy: Some("socks5h://127.0.0.1:9".into()),
                referer: Some("https://p.test/".into()),
                deno: Some(PathBuf::from("/tools/deno")),
                ..Default::default()
            },
        );
        assert_eq!(args[args.len() - 2..], ["--", "https://a.test/"]);
        assert!(args
            .windows(2)
            .any(|w| w == ["--proxy", "socks5h://127.0.0.1:9"]));
        assert!(args.contains(&"--ignore-config".to_string()));
        assert!(args
            .windows(2)
            .any(|w| w == ["--js-runtimes", "deno:/tools/deno"]));
    }

    #[tokio::test]
    async fn only_web_addresses_are_passed_on() {
        let bad = extract(
            Path::new("yt-dlp"),
            "--exec=calc",
            &ExtractOptions::default(),
        )
        .await;
        assert!(bad.is_err());
    }

    #[test]
    fn errors_are_read_from_the_output() {
        assert_eq!(
            reason("[x] working\nERROR: [dailymotion] x9: Access forbidden.\n"),
            "[dailymotion] x9: Access forbidden."
        );
        assert_eq!(reason(""), "yt-dlp stopped without saying why");
    }
}
