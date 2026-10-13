//! The surround renditions' multivariant playlist (DESIGN §40 §8.4 "Surround"). OWNER: CAST-4.
//!
//! A source with more than two channels reaches the Apple TV as Apple's HLS Authoring
//! Specification asks for multichannel sound: demuxed audio (rule 9.6: multichannel MUST be a
//! separate audio stream, declared with `EXT-X-MEDIA`, 8.9) with a stereo AAC rendition beside it
//! (2.3), one group per codec and channel count, and every video variant listed once per group
//! (the spec's appendix "Audio rendition groups and variants"; Apple's bipbop advanced example
//! does exactly this with `aud1` AAC, `aud2` AC-3 and `aud3` E-AC-3):
//!
//! ```text
//! #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="surround",LANGUAGE="en",NAME="Main",AUTOSELECT=YES,DEFAULT=YES,CHANNELS="6",URI="s/index.m3u8"
//! #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="stereo",LANGUAGE="en",NAME="Main",AUTOSELECT=YES,DEFAULT=YES,CHANNELS="2",URI="a/index.m3u8"
//! #EXT-X-STREAM-INF:BANDWIDTH=…,AVERAGE-BANDWIDTH=…,CODECS="hvc1.2.4.L150.B0,ac-3",RESOLUTION=…,FRAME-RATE=…,VIDEO-RANGE=SDR,AUDIO="surround"
//! v/index.m3u8
//! #EXT-X-STREAM-INF:…,CODECS="hvc1.2.4.L150.B0,mp4a.40.2",…,AUDIO="stereo"
//! v/index.m3u8
//! ```
//!
//! * **Which one plays.** AVPlayer picks the highest-bandwidth variant it can play on its audio
//!   route ("The default behavior is to choose the highest bit rate among what's currently
//!   playable"): the surround one, listed first, where the route takes it; the stereo one
//!   otherwise. The group members are identical but for `URI` and `CHANNELS` (RFC 8216
//!   §4.3.4.1.1), so the TV's audio menu shows one track.
//! * **The attributes Apple requires** (9.1 `CODECS`, 9.2 `RESOLUTION`, 9.13/9.14 `BANDWIDTH` and
//!   `AVERAGE-BANDWIDTH`, 9.15 `FRAME-RATE`, 9.16 `VIDEO-RANGE`, 8.10 `LANGUAGE`) come from what
//!   was produced, never guessed: the video codec string and size from the video rendition's
//!   initialization segment (`avcC` / `hvcC`, ISO/IEC 14496-15), the bandwidths measured on the
//!   first ready segments (peak and mean of bits over `EXTINF`), the frame rate and range from
//!   the probe. foxCast saw tvOS 27 fetch and never play a variant claiming 80 Mbit/s with no
//!   `FRAME-RATE` [REF].
//! * **Names.** The renditions live in `v/`, `a/` and `s/` under the generation's directory, as
//!   ffmpeg's `-var_stream_map` writes them (`index.m3u8`, `init_<n>.mp4`, `s<n>.m4s`); the
//!   multivariant playlist names them relatively, so the TV's URLs stay under the token.

use std::fmt::Write as _;
use std::path::Path;

use super::playlist::MediaPlaylist;

/// The rendition directories (and URL components): video, stereo, surround.
pub const VIDEO: &str = "v";
pub const STEREO: &str = "a";
pub const SURROUND: &str = "s";

/// Peak and mean bit rates of a rendition's segments (bits per second).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rate {
    pub peak: u64,
    pub mean: u64,
}

impl std::ops::Add for Rate {
    type Output = Rate;
    fn add(self, o: Rate) -> Rate {
        Rate { peak: self.peak + o.peak, mean: self.mean + o.mean }
    }
}

/// Measure a rendition's listed segments in `dir` (segments under a second count only toward
/// the mean: a short last piece would make a wild peak).
pub fn measure(dir: &Path, pl: &MediaPlaylist) -> Rate {
    let (mut bits, mut secs, mut peak) = (0u64, 0f64, 0u64);
    for s in &pl.segments {
        let Some(name) = s.file() else { continue };
        let Ok(meta) = std::fs::metadata(dir.join(name)) else { continue };
        let b = meta.len() * 8;
        bits += b;
        secs += s.dur;
        if s.dur >= 1.0 {
            peak = peak.max((b as f64 / s.dur).ceil() as u64);
        }
    }
    let mean = if secs > 0.0 { (bits as f64 / secs).ceil() as u64 } else { 0 };
    Rate { peak: peak.max(mean), mean }
}

/// What the video rendition's initialization segment says: the RFC 6381 codec string and the
/// coded size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoInit {
    pub codecs: String,
    pub width: u32,
    pub height: u32,
}

/// The multivariant playlist's inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct Master {
    pub video: VideoInit,
    pub fps: Option<f64>,
    /// `SDR`, `PQ` or `HLG` ([`video_range`]).
    pub range: &'static str,
    pub video_rate: Rate,
    pub stereo_rate: Rate,
    pub surround_rate: Rate,
    /// `ac-3`, `ec-3`, `mp4a.40.2`…
    pub surround_codecs: String,
    /// `6`, `8`, `16/JOC`.
    pub surround_channels: String,
    /// RFC 5646 (`en`, `und`).
    pub language: String,
}

/// The multivariant playlist (module docs).
pub fn render(m: &Master) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    let media = |group: &str, channels: &str, dir: &str| {
        format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{group}\",LANGUAGE=\"{}\",NAME=\"Main\",AUTOSELECT=YES,DEFAULT=YES,CHANNELS=\"{channels}\",URI=\"{dir}/index.m3u8\"\n",
            m.language
        )
    };
    out.push_str(&media("surround", &m.surround_channels, SURROUND));
    out.push_str(&media("stereo", "2", STEREO));
    for (group, audio, rate) in [("surround", m.surround_codecs.as_str(), m.surround_rate), ("stereo", "mp4a.40.2", m.stereo_rate)] {
        let r = m.video_rate + rate;
        let _ = write!(
            out,
            "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={},CODECS=\"{},{audio}\",RESOLUTION={}x{}",
            r.peak, r.mean, m.video.codecs, m.video.width, m.video.height
        );
        if let Some(fps) = m.fps.filter(|f| *f > 0.0 && *f < 1000.0) {
            let _ = write!(out, ",FRAME-RATE={fps:.3}");
        }
        let _ = writeln!(out, ",VIDEO-RANGE={},AUDIO=\"{group}\"\n{VIDEO}/index.m3u8", m.range);
    }
    out
}

/// `VIDEO-RANGE` from the probe's `color_transfer`.
pub fn video_range(transfer: Option<&str>) -> &'static str {
    match transfer {
        Some("smpte2084") => "PQ",
        Some("arib-std-b67") => "HLG",
        _ => "SDR",
    }
}

/// The surround rendition's `CODECS` entry for what ffmpeg wrote into it: the copied codec
/// (`ac3`, `eac3`, `aac` by its profile) or the AC-3 it made.
pub fn audio_codecs(codec: &str, profile: Option<&str>) -> &'static str {
    match (codec, profile) {
        ("eac3", _) => "ec-3",
        ("aac", Some(p)) if p.contains("HE-AACv2") => "mp4a.40.29",
        ("aac", Some(p)) if p.contains("HE-AAC") => "mp4a.40.5",
        ("aac", _) => "mp4a.40.2",
        _ => "ac-3",
    }
}

/// `CHANNELS` for the surround rendition: Dolby Atmos in E-AC-3 is `16/JOC` (the spec's
/// "Audio compatibility"), anything else its channel count.
pub fn audio_channels(codec: &str, profile: Option<&str>, channels: u32) -> String {
    if codec == "eac3" && profile.is_some_and(|p| p.contains("Atmos")) { "16/JOC".into() } else { channels.to_string() }
}

/// An ISO 639-2 tag (`eng`, as ffprobe reads it) as RFC 5646 wants it (`en`): the two-letter
/// code where there is one for the usual broadcast languages, a three-letter one as it is
/// (valid where no two-letter code exists), `und` when unknown.
pub fn language(tag: Option<&str>) -> String {
    const TWO: &[(&str, &str)] = &[
        ("eng", "en"),
        ("deu", "de"),
        ("ger", "de"),
        ("fra", "fr"),
        ("fre", "fr"),
        ("spa", "es"),
        ("ita", "it"),
        ("nld", "nl"),
        ("dut", "nl"),
        ("por", "pt"),
        ("swe", "sv"),
        ("nor", "no"),
        ("dan", "da"),
        ("fin", "fi"),
        ("pol", "pl"),
        ("rus", "ru"),
        ("tur", "tr"),
        ("ara", "ar"),
        ("hin", "hi"),
        ("jpn", "ja"),
        ("kor", "ko"),
        ("zho", "zh"),
        ("chi", "zh"),
        ("ell", "el"),
        ("gre", "el"),
        ("heb", "he"),
        ("hun", "hu"),
        ("ces", "cs"),
        ("cze", "cs"),
        ("ron", "ro"),
        ("rum", "ro"),
        ("ukr", "uk"),
        ("cym", "cy"),
        ("wel", "cy"),
        ("gle", "ga"),
    ];
    let Some(t) = tag.map(str::to_ascii_lowercase).filter(|t| t.len() >= 2 && t.len() <= 3 && t.bytes().all(|b| b.is_ascii_lowercase())) else {
        return "und".into();
    };
    TWO.iter().find(|(three, _)| *three == t).map_or(t, |(_, two)| (*two).to_owned())
}

/// The video track of an initialization segment: `moov/trak/mdia/minf/stbl/stsd` → its
/// `avc1`/`avc3`/`hvc1`/`hev1` sample entry (width and height at bytes 24–28 of the
/// VisualSampleEntry, ISO/IEC 14496-12 §12.1.3) → `avcC` / `hvcC`.
pub fn video_init(data: &[u8]) -> Option<VideoInit> {
    let moov = child(data, b"moov")?;
    for (kind, trak) in boxes(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(stbl) = path(trak, &[b"mdia", b"minf", b"stbl"]) else { continue };
        let stsd = child(stbl, b"stsd")?;
        let entries = stsd.get(8..)?;
        for (fourcc, entry) in boxes(entries) {
            let (width, height) = (u16::from_be_bytes([*entry.get(24)?, *entry.get(25)?]), u16::from_be_bytes([*entry.get(26)?, *entry.get(27)?]));
            let children = entry.get(78..)?;
            let tag = std::str::from_utf8(&fourcc).ok()?;
            let codecs = match &fourcc {
                b"avc1" | b"avc3" => avc(tag, child(children, b"avcC")?)?,
                b"hvc1" | b"hev1" => hevc(tag, child(children, b"hvcC")?)?,
                _ => continue,
            };
            return Some(VideoInit { codecs, width: u32::from(width), height: u32::from(height) });
        }
    }
    None
}

/// `avc1.PPCCLL` (RFC 6381 §3.3): profile, constraint flags, level from the `avcC` record.
fn avc(tag: &str, avcc: &[u8]) -> Option<String> {
    Some(format!("{tag}.{:02x}{:02x}{:02x}", avcc.get(1)?, avcc.get(2)?, avcc.get(3)?))
}

/// `hvc1.<space><profile>.<compatibility>.<tier><level>.<constraints>` (ISO/IEC 14496-15
/// Annex E.3): the compatibility flags bit-reversed in hex, trailing zero constraint bytes left
/// out (`hvc1.2.4.L150.B0` for Main 10 at level 5).
fn hevc(tag: &str, hvcc: &[u8]) -> Option<String> {
    let b = hvcc.get(1)?;
    let space = ["", "A", "B", "C"][usize::from(b >> 6)];
    let tier = if b & 0x20 != 0 { 'H' } else { 'L' };
    let profile = b & 0x1f;
    let compat = u32::from_be_bytes(hvcc.get(2..6)?.try_into().ok()?).reverse_bits();
    let constraints = hvcc.get(6..12)?;
    let level = hvcc.get(12)?;
    let mut s = format!("{tag}.{space}{profile}.{compat:X}.{tier}{level}");
    let kept = constraints.iter().rposition(|c| *c != 0).map_or(0, |i| i + 1);
    for c in &constraints[..kept] {
        let _ = write!(s, ".{c:X}");
    }
    Some(s)
}

/// ISO BMFF boxes of `data`: `(type, body)` (64-bit sizes too; a box running past the end is
/// cut at it).
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let h = data.get(at..at + 8)?;
        let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let kind = [h[4], h[5], h[6], h[7]];
        let (hdr, size) = match size {
            1 => (16, usize::try_from(u64::from_be_bytes(data.get(at + 8..at + 16)?.try_into().ok()?)).ok()?),
            0 => (8, data.len() - at),
            n => (8, n),
        };
        if size < hdr {
            return None;
        }
        let body = &data[(at + hdr).min(data.len())..(at + size).min(data.len())];
        at += size;
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn path<'a>(mut data: &'a [u8], kinds: &[&[u8; 4]]) -> Option<&'a [u8]> {
    for k in kinds {
        data = child(data, k)?;
    }
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    /// An init segment with one video sample entry `fourcc` of `w`×`h` holding `config`.
    fn init(fourcc: &[u8; 4], config: (&[u8; 4], &[u8]), w: u16, h: u16) -> Vec<u8> {
        let mut entry = vec![0u8; 78];
        entry[24..26].copy_from_slice(&w.to_be_bytes());
        entry[26..28].copy_from_slice(&h.to_be_bytes());
        entry.extend(bx(config.0, config.1));
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(bx(fourcc, &entry));
        let stbl = bx(b"stbl", &bx(b"stsd", &stsd));
        let trak = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
        let mut out = bx(b"ftyp", b"iso5\0\0\0\x01");
        out.extend(bx(b"moov", &[bx(b"mvhd", &[0; 100]), trak].concat()));
        out
    }

    /// The codec strings Apple's own examples carry: `avc1.64002a` (High@4.2) and a Main 10 HEVC
    /// at level 5 (`hvc1.2.4.L150.B0`).
    #[test]
    fn the_codec_string_and_size_come_from_the_init_segment() {
        let avcc = [1u8, 0x64, 0x00, 0x2a, 0xff];
        let v = video_init(&init(b"avc1", (b"avcC", &avcc), 1920, 1080)).unwrap();
        assert_eq!(v, VideoInit { codecs: "avc1.64002a".into(), width: 1920, height: 1080 });
        // hvcC: version 1, space 0 tier Main profile 2, compatibility 0x20000000 (profile 2),
        // constraint bytes B0 00 00 00 00 00, level 150
        let mut hvcc = vec![1u8, 0x02, 0x20, 0, 0, 0, 0xb0, 0, 0, 0, 0, 0, 150];
        hvcc.extend([0xf0, 0, 0xfc, 0xfd, 0xfa, 0xfa, 0, 0, 0x0f]);
        let v = video_init(&init(b"hvc1", (b"hvcC", &hvcc), 3840, 2160)).unwrap();
        assert_eq!(v, VideoInit { codecs: "hvc1.2.4.L150.B0".into(), width: 3840, height: 2160 });
        assert_eq!(video_init(b"not an mp4"), None);
    }

    /// The structure Apple's spec asks for (module docs): two groups differing only in URI and
    /// CHANNELS, the surround variant first, every attribute present, relative names only.
    #[test]
    fn the_multivariant_playlist_lists_the_video_once_per_audio_group() {
        let m = Master {
            video: VideoInit { codecs: "hvc1.2.4.L150.B0".into(), width: 1920, height: 800 },
            fps: Some(24000.0 / 1001.0),
            range: video_range(Some("bt709")),
            video_rate: Rate { peak: 9_000_000, mean: 6_000_000 },
            stereo_rate: Rate { peak: 170_000, mean: 160_000 },
            surround_rate: Rate { peak: 400_000, mean: 384_000 },
            surround_codecs: audio_codecs("aac", Some("LC")).into(),
            surround_channels: audio_channels("aac", Some("LC"), 6),
            language: language(Some("eng")),
        };
        let text = render(&m);
        assert_eq!(
            text,
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"surround\",LANGUAGE=\"en\",NAME=\"Main\",AUTOSELECT=YES,DEFAULT=YES,CHANNELS=\"6\",URI=\"s/index.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"stereo\",LANGUAGE=\"en\",NAME=\"Main\",AUTOSELECT=YES,DEFAULT=YES,CHANNELS=\"2\",URI=\"a/index.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=9400000,AVERAGE-BANDWIDTH=6384000,CODECS=\"hvc1.2.4.L150.B0,mp4a.40.2\",RESOLUTION=1920x800,FRAME-RATE=23.976,VIDEO-RANGE=SDR,AUDIO=\"surround\"\n\
             v/index.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=9170000,AVERAGE-BANDWIDTH=6160000,CODECS=\"hvc1.2.4.L150.B0,mp4a.40.2\",RESOLUTION=1920x800,FRAME-RATE=23.976,VIDEO-RANGE=SDR,AUDIO=\"stereo\"\n\
             v/index.m3u8\n"
        );
        assert!(!text.contains("://"));
        assert_eq!(audio_codecs("ac3", None), "ac-3");
        assert_eq!(audio_codecs("eac3", None), "ec-3");
        assert_eq!(audio_codecs("dts", None), "ac-3", "converted to AC-3");
        assert_eq!(audio_channels("eac3", Some("Dolby Digital Plus + Dolby Atmos"), 8), "16/JOC");
        assert_eq!((video_range(Some("smpte2084")), video_range(Some("arib-std-b67")), video_range(None)), ("PQ", "HLG", "SDR"));
        assert_eq!(
            (language(Some("ger")), language(Some("tlh")), language(None), language(Some("e1"))),
            ("de".into(), "tlh".into(), "und".into(), "und".into())
        );
    }
}
