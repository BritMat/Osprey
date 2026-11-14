//! A small M3U8 parser covering what real sites use: master playlists with
//! variants and separate audio renditions, media playlists with keys,
//! byte ranges and fMP4 init sections (EXT-X-MAP).

use std::collections::HashMap;

use url::Url;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
    pub audio: Vec<Rendition>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub uri: Url,
    pub bandwidth: u64,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Option<String>,
    pub audio_group: Option<String>,
}

impl Variant {
    pub fn describe(&self) -> String {
        let res = self
            .resolution
            .map(|(w, h)| format!("{w}x{h}"))
            .unwrap_or_else(|| "audio/unknown".into());
        format!("{res} @ {} kbps", self.bandwidth / 1000)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rendition {
    pub group_id: String,
    pub name: Option<String>,
    pub language: Option<String>,
    pub uri: Option<Url>,
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaPlaylist {
    pub segments: Vec<Segment>,
    /// False for a live stream that is still growing.
    pub ended: bool,
    pub target_duration: f64,
}

impl MediaPlaylist {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub uri: Url,
    pub duration: f64,
    pub sequence: u64,
    pub byte_range: Option<ByteRange>,
    pub key: Option<Key>,
    pub map: Option<InitMap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteRange {
    pub length: u64,
    pub offset: u64,
}

impl ByteRange {
    pub fn header_value(&self) -> String {
        format!("bytes={}-{}", self.offset, self.offset + self.length - 1)
    }
}

/// An AES-128 clear key, the standard non-DRM HLS encryption where the
/// key is served to every player from a plain URL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub uri: Url,
    pub iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InitMap {
    pub uri: Url,
    pub byte_range: Option<ByteRange>,
}

pub fn parse(text: &str, base: &Url) -> Result<Playlist> {
    let text = text.trim_start_matches('\u{feff}');
    let first = text.lines().map(str::trim).find(|l| !l.is_empty());
    if !first.is_some_and(|l| l.starts_with("#EXTM3U")) {
        return Err(Error::Parse(
            "missing #EXTM3U header, this is not an HLS playlist".into(),
        ));
    }
    if text.contains("#EXT-X-STREAM-INF") {
        parse_master(text, base).map(Playlist::Master)
    } else {
        parse_media(text, base).map(Playlist::Media)
    }
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty())
}

fn parse_master(text: &str, base: &Url) -> Result<MasterPlaylist> {
    let mut out = MasterPlaylist::default();
    let mut pending: Option<HashMap<String, String>> = None;

    for line in lines(text) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attributes(rest));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = parse_attributes(rest);
            if a.get("TYPE").map(String::as_str) == Some("AUDIO") {
                out.audio.push(Rendition {
                    group_id: a.get("GROUP-ID").cloned().unwrap_or_default(),
                    name: a.get("NAME").cloned(),
                    language: a.get("LANGUAGE").cloned(),
                    uri: a.get("URI").map(|u| base.join(u)).transpose()?,
                    default: a.get("DEFAULT").is_some_and(|v| v == "YES"),
                });
            }
        } else if line.starts_with('#') {
            continue;
        } else if let Some(a) = pending.take() {
            out.variants.push(Variant {
                uri: base.join(line)?,
                bandwidth: a.get("BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0),
                resolution: a.get("RESOLUTION").and_then(|r| {
                    let (w, h) = r.split_once(['x', 'X'])?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                }),
                codecs: a.get("CODECS").cloned(),
                audio_group: a.get("AUDIO").cloned(),
            });
        }
    }

    if out.variants.is_empty() {
        return Err(Error::Parse("master playlist lists no variants".into()));
    }
    Ok(out)
}

fn parse_byte_range(v: &str) -> Option<(u64, Option<u64>)> {
    match v.split_once('@') {
        Some((len, off)) => Some((len.trim().parse().ok()?, Some(off.trim().parse().ok()?))),
        None => Some((v.trim().parse().ok()?, None)),
    }
}

fn parse_iv(v: &str) -> Result<[u8; 16]> {
    let hex_str = v.trim().trim_start_matches("0x").trim_start_matches("0X");
    let padded = format!("{hex_str:0>32}");
    let bytes = hex::decode(&padded).map_err(|_| Error::Parse(format!("bad IV '{v}'")))?;
    bytes
        .try_into()
        .map_err(|_| Error::Parse(format!("IV '{v}' is longer than 128 bits")))
}

fn parse_media(text: &str, base: &Url) -> Result<MediaPlaylist> {
    let mut out = MediaPlaylist::default();
    let mut sequence: u64 = 0;
    let mut key: Option<Key> = None;
    let mut map: Option<InitMap> = None;
    let mut duration: Option<f64> = None;
    let mut range: Option<(u64, Option<u64>)> = None;
    // End of the previous byte range, per resource, for ranges that omit
    // their offset.
    let mut last_end: HashMap<String, u64> = HashMap::new();

    for line in lines(text) {
        if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            out.target_duration = v.trim().parse().unwrap_or(0.0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = v.trim().parse().unwrap_or(0);
        } else if line.starts_with("#EXT-X-ENDLIST") {
            out.ended = true;
        } else if let Some(v) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            if v.trim() == "VOD" {
                out.ended = true;
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = parse_attributes(rest);
            let method = a.get("METHOD").map(String::as_str).unwrap_or("NONE");
            let keyformat = a.get("KEYFORMAT").map(String::as_str).unwrap_or("identity");
            match method {
                "NONE" => key = None,
                "AES-128" if keyformat == "identity" => {
                    let uri = a
                        .get("URI")
                        .ok_or_else(|| Error::Parse("AES-128 key without URI".into()))?;
                    key = Some(Key {
                        uri: base.join(uri)?,
                        iv: a.get("IV").map(|v| parse_iv(v)).transpose()?,
                    });
                }
                other => {
                    return Err(Error::Drm(format!("{other}, key format {keyformat}")));
                }
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attributes(rest);
            let uri = a
                .get("URI")
                .ok_or_else(|| Error::Parse("EXT-X-MAP without URI".into()))?;
            let byte_range =
                a.get("BYTERANGE")
                    .and_then(|v| parse_byte_range(v))
                    .map(|(length, offset)| ByteRange {
                        length,
                        offset: offset.unwrap_or(0),
                    });
            map = Some(InitMap {
                uri: base.join(uri)?,
                byte_range,
            });
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let d = rest.split(',').next().unwrap_or("0");
            duration = Some(d.trim().parse().unwrap_or(0.0));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = parse_byte_range(rest);
        } else if line.starts_with('#') {
            continue;
        } else {
            let uri = base.join(line)?;
            let byte_range = range.take().map(|(length, offset)| {
                let offset =
                    offset.unwrap_or_else(|| last_end.get(uri.as_str()).copied().unwrap_or(0));
                last_end.insert(uri.to_string(), offset + length);
                ByteRange { length, offset }
            });
            out.segments.push(Segment {
                uri,
                duration: duration.take().unwrap_or(0.0),
                sequence,
                byte_range,
                key: key.clone(),
                map: map.clone(),
            });
            sequence += 1;
        }
    }

    if out.segments.is_empty() {
        return Err(Error::Parse("media playlist lists no segments".into()));
    }
    Ok(out)
}

/// Parses `KEY=value,KEY="quoted, value"` attribute lists.
pub(crate) fn parse_attributes(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut rest = s.trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().to_ascii_uppercase();
        rest = &rest[eq + 1..];
        let value;
        if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            value = quoted[..end].to_string();
            rest = quoted.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
        map.insert(key, value);
    }
    map
}

/// Picks a variant. With `max_height` it takes the best one at or below
/// that height (or the smallest if none fit). Without it, the best one.
pub fn pick_variant(master: &MasterPlaylist, max_height: Option<u32>) -> Option<&Variant> {
    let score = |v: &&Variant| {
        let pixels = v
            .resolution
            .map(|(w, h)| u64::from(w) * u64::from(h))
            .unwrap_or(0);
        (pixels, v.bandwidth)
    };
    match max_height {
        None => master.variants.iter().max_by_key(score),
        Some(h) => {
            let fits = master
                .variants
                .iter()
                .filter(|v| v.resolution.is_some_and(|(_, vh)| vh <= h));
            fits.max_by_key(score)
                .or_else(|| master.variants.iter().min_by_key(score))
        }
    }
}

/// Picks the audio rendition for a variant's audio group, preferring the
/// one marked DEFAULT. Renditions without a URI are muxed into the video
/// and need no separate download.
pub fn pick_audio<'a>(master: &'a MasterPlaylist, variant: &Variant) -> Option<&'a Rendition> {
    let group = variant.audio_group.as_deref()?;
    let candidates: Vec<_> = master
        .audio
        .iter()
        .filter(|r| r.group_id == group && r.uri.is_some())
        .collect();
    candidates
        .iter()
        .find(|r| r.default)
        .or(candidates.first())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.test/show/ep1/master.m3u8").unwrap()
    }

    const MASTER: &str = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="audio/en.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="Hindi",LANGUAGE="hi",URI="audio/hi.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2",AUDIO="aud"
360p/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2",AUDIO="aud"
720p/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,AUDIO="aud"
https://other.test/1080p/index.m3u8
"#;

    #[test]
    fn parses_master_and_picks_quality() {
        let Playlist::Master(m) = parse(MASTER, &base()).unwrap() else {
            panic!("expected master")
        };
        assert_eq!(m.variants.len(), 3);
        assert_eq!(
            m.variants[1].uri.as_str(),
            "https://cdn.test/show/ep1/720p/index.m3u8"
        );
        assert_eq!(
            m.variants[0].codecs.as_deref(),
            Some("avc1.4d401e,mp4a.40.2")
        );
        assert_eq!(
            pick_variant(&m, None).unwrap().resolution,
            Some((1920, 1080))
        );
        assert_eq!(
            pick_variant(&m, Some(720)).unwrap().resolution,
            Some((1280, 720))
        );
        assert_eq!(
            pick_variant(&m, Some(240)).unwrap().resolution,
            Some((640, 360))
        );

        let v = pick_variant(&m, None).unwrap();
        let a = pick_audio(&m, v).unwrap();
        assert_eq!(a.language.as_deref(), Some("en"));
        assert_eq!(
            a.uri.as_ref().unwrap().as_str(),
            "https://cdn.test/show/ep1/audio/en.m3u8"
        );
    }

    #[test]
    fn parses_media_with_keys_ranges_and_map() {
        let text = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-MAP:URI="init.mp4",BYTERANGE="720@0"
#EXTINF:6.0,
#EXT-X-BYTERANGE:1000@720
all.m4s
#EXT-X-KEY:METHOD=AES-128,URI="https://keys.test/k1",IV=0x000102030405060708090a0b0c0d0e0f
#EXTINF:6.0,
#EXT-X-BYTERANGE:500
all.m4s
#EXT-X-KEY:METHOD=AES-128,URI="k2"
#EXTINF:4.5,title
seg3.m4s
#EXT-X-KEY:METHOD=NONE
#EXTINF:2,
seg4.m4s
#EXT-X-ENDLIST
"#;
        let Playlist::Media(p) = parse(text, &base()).unwrap() else {
            panic!("expected media")
        };
        assert!(p.ended);
        assert_eq!(p.segments.len(), 4);
        assert_eq!(p.duration(), 18.5);
        assert_eq!(p.segments[0].sequence, 100);
        assert_eq!(p.segments[3].sequence, 103);
        assert_eq!(
            p.segments[0].byte_range,
            Some(ByteRange {
                length: 1000,
                offset: 720
            })
        );
        assert_eq!(
            p.segments[1].byte_range,
            Some(ByteRange {
                length: 500,
                offset: 1720
            })
        );
        assert!(p.segments[0].key.is_none());
        let k = p.segments[1].key.as_ref().unwrap();
        assert_eq!(k.iv.unwrap()[15], 0x0f);
        assert_eq!(
            p.segments[2].key.as_ref().unwrap().uri.as_str(),
            "https://cdn.test/show/ep1/k2"
        );
        assert!(p.segments[2].key.as_ref().unwrap().iv.is_none());
        assert!(p.segments[3].key.is_none());
        let map = p.segments[0].map.as_ref().unwrap();
        assert_eq!(
            map.byte_range,
            Some(ByteRange {
                length: 720,
                offset: 0
            })
        );
    }

    #[test]
    fn refuses_drm() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://abc\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:6,\na.ts\n";
        assert!(matches!(parse(text, &base()), Err(Error::Drm(_))));
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:x\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\"\n#EXTINF:6,\na.ts\n";
        assert!(matches!(parse(text, &base()), Err(Error::Drm(_))));
    }

    #[test]
    fn rejects_non_playlists() {
        assert!(matches!(parse("<html>", &base()), Err(Error::Parse(_))));
    }

    #[test]
    fn attribute_lists_with_quoted_commas() {
        let a = parse_attributes(r#"BANDWIDTH=1,CODECS="a,b",NAME="x y", AUTOSELECT=YES"#);
        assert_eq!(a["CODECS"], "a,b");
        assert_eq!(a["NAME"], "x y");
        assert_eq!(a["AUTOSELECT"], "YES");
        assert_eq!(a["BANDWIDTH"], "1");
    }
}
