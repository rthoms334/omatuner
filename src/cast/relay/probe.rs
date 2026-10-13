//! `ffprobe -of json` on the head buffer (DESIGN §40 §8.1.2): codec, profile, level,
//! `field_order`, size, frame rate, audio codec and channels; skipped when the hints suffice.
//! OWNER: CAST-4.
//!
//! ffprobe reads the loopback proxy's URL (never the provider's), so the bytes it reads are the
//! head buffer that ffmpeg reads again from byte 0: the probe opens no provider connection of its
//! own (§10). Besides `-show_streams -show_format` it lists the packets of the first 3 s
//! (`-read_intervals %+3`, the same window as `-analyzeduration 3M`) for the longest keyframe
//! interval: the "probed max GOP" that sets `hls_time` and the pinned `#EXT-X-TARGETDURATION`
//! (§8.1.4, §8.4). A GOP longer than the window stays unknown and the first segments ffmpeg
//! writes decide instead.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

use super::plan::Facts;
use crate::model::CastHints;
use crate::player::tracks::StreamInfo;

/// ffprobe gives up after this long (a stalled upstream; the load budget is 30 s, §5.5).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest JSON accepted (3 s of packets is a few hundred entries).
const MAX_JSON: usize = 4 << 20;

/// The video stream ffmpeg maps (`0:v:0`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoStream {
    pub codec: String,
    pub profile: Option<String>,
    pub level: Option<i64>,
    /// `progressive`, `tt`, `bb`, `tb`, `bt` (ffprobe's names).
    pub field_order: Option<String>,
    pub pix_fmt: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    /// `color_transfer` (`bt709`, `smpte2084`: PQ, `arib-std-b67`: HLG).
    pub transfer: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioStream {
    /// Its position among the audio streams (`-map 0:a:<index>`).
    pub index: usize,
    pub codec: String,
    pub channels: Option<u32>,
    /// `channel_layout` (`stereo`, `5.1(side)`, `7.1`).
    pub layout: Option<String>,
    /// `profile` (`LC`, `HE-AAC`, `Dolby Digital Plus + Dolby Atmos`).
    pub profile: Option<String>,
    pub sample_rate: Option<i64>,
    /// `tags.language` (ISO 639-2: `eng`).
    pub language: Option<String>,
    pub default: bool,
}

/// ffprobe's view of an input.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probe {
    pub video: Option<VideoStream>,
    pub audio: Vec<AudioStream>,
    /// `format_name` (`mpegts`, `hls`, `mov,mp4,m4a,3gp,3g2,mj2`…).
    pub container: Option<String>,
    /// The longest interval between video keyframes seen in the first 3 s (seconds).
    pub max_gop_s: Option<f64>,
    /// An encrypted track (`encv`/`enca`/`drms`).
    pub encrypted: bool,
}

/// Why the probe gave nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// ffprobe couldn't be started.
    Spawn,
    /// It failed, timed out, or printed no usable JSON (unreadable input, upstream gone).
    Failed,
}

/// The argv after the program: only the loopback URL is variable (DESIGN §20 "never on a command
/// line": no provider URL ever). `hls`: the proxy serves an HLS media playlist, read one segment
/// at a time (`-http_multiple 0`, as ffmpeg's input): ffprobe's HLS demuxer otherwise opens the
/// next segment while it still reads this one, which cuts this one's provider request.
pub fn argv(url: &str, hls: bool) -> Vec<String> {
    let hls: &[&str] = if hls { &["-http_multiple", "0"] } else { &[] };
    [
        "-v",
        "error",
        "-of",
        "json",
        "-show_streams",
        "-show_format",
        "-show_entries",
        "packet=stream_index,pts_time,flags",
        "-read_intervals",
        "%+3",
        "-probesize",
        "3M",
        "-analyzeduration",
        "3M",
    ]
    .iter()
    .chain(hls)
    .chain(&[url])
    .map(|s| (*s).to_owned())
    .collect()
}

/// Run ffprobe on `url` (environment cleared but `LC_ALL=C`, killed on drop and when Tuner dies,
/// like ffmpeg: [`super::ffmpeg::command`]).
pub async fn probe(ffprobe: &Path, url: &str, hls: bool) -> Result<Probe, ProbeError> {
    let mut cmd = super::ffmpeg::command(ffprobe);
    cmd.args(argv(url, hls)).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| {
        tracing::warn!("cast: ffprobe didn't start: {e}");
        ProbeError::Spawn
    })?;
    let out = match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::debug!("cast: ffprobe: {e}");
            return Err(ProbeError::Failed);
        }
        Err(_) => {
            tracing::debug!("cast: ffprobe timed out");
            return Err(ProbeError::Failed);
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        tracing::debug!("cast: ffprobe failed ({}): {}", out.status, crate::redact::redact(err.lines().next().unwrap_or_default()));
        return Err(ProbeError::Failed);
    }
    if out.stdout.len() > MAX_JSON {
        return Err(ProbeError::Failed);
    }
    parse(&String::from_utf8_lossy(&out.stdout)).ok_or(ProbeError::Failed)
}

/// Parse ffprobe's JSON; `None` when it holds no stream at all.
pub fn parse(json: &str) -> Option<Probe> {
    let v: Value = serde_json::from_str(json).ok()?;
    let streams = v.get("streams")?.as_array()?;
    let s = |st: &Value, k: &str| st.get(k).and_then(Value::as_str).map(str::to_owned).filter(|s| !s.is_empty() && s != "unknown");
    let u = |st: &Value, k: &str| st.get(k).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
    let mut out = Probe { container: v.get("format").and_then(|f| s(f, "format_name")), ..Probe::default() };
    let mut video_index = None;
    for st in streams {
        let tag = st.get("codec_tag_string").and_then(Value::as_str).unwrap_or_default();
        out.encrypted |= matches!(tag, "encv" | "enca" | "drms");
        match st.get("codec_type").and_then(Value::as_str) {
            Some("video") if out.video.is_none() => {
                // cover art in a file is a "video" stream of one picture
                if st.get("disposition").and_then(|d| d.get("attached_pic")).and_then(Value::as_i64) == Some(1) {
                    continue;
                }
                let Some(codec) = s(st, "codec_name") else { continue };
                video_index = st.get("index").and_then(Value::as_i64);
                out.video = Some(VideoStream {
                    codec,
                    profile: s(st, "profile"),
                    level: st.get("level").and_then(Value::as_i64).filter(|l| *l > 0),
                    field_order: s(st, "field_order"),
                    pix_fmt: s(st, "pix_fmt"),
                    width: u(st, "width"),
                    height: u(st, "height"),
                    fps: rate(st, "avg_frame_rate").or_else(|| rate(st, "r_frame_rate")),
                    transfer: s(st, "color_transfer"),
                });
            }
            Some("audio") => {
                let index = out.audio.len();
                out.audio.push(AudioStream {
                    index,
                    codec: s(st, "codec_name").unwrap_or_else(|| "unknown".into()),
                    channels: u(st, "channels"),
                    layout: s(st, "channel_layout"),
                    profile: s(st, "profile"),
                    sample_rate: st.get("sample_rate").and_then(Value::as_str).and_then(|r| r.parse().ok()),
                    language: st.get("tags").and_then(|t| s(t, "language")).filter(|l| l != "und"),
                    default: st.get("disposition").and_then(|d| d.get("default")).and_then(Value::as_i64) == Some(1),
                });
            }
            _ => {}
        }
    }
    if out.video.is_none() && out.audio.is_empty() {
        return None;
    }
    if let (Some(vi), Some(packets)) = (video_index, v.get("packets").and_then(Value::as_array)) {
        let mut keys: Vec<f64> = packets
            .iter()
            .filter(|p| p.get("stream_index").and_then(Value::as_i64) == Some(vi))
            .filter(|p| p.get("flags").and_then(Value::as_str).is_some_and(|f| f.starts_with('K')))
            .filter_map(|p| p.get("pts_time").and_then(Value::as_str)?.parse::<f64>().ok())
            .collect();
        keys.sort_by(f64::total_cmp);
        out.max_gop_s = keys.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0.0).max_by(f64::total_cmp);
    }
    Some(out)
}

/// `"30000/1001"` → 29.97; `0/0` and nonsense → `None`.
fn rate(st: &Value, key: &str) -> Option<f64> {
    let (n, d) = st.get(key)?.as_str()?.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    let r = n / d;
    (r.is_finite() && r > 0.0 && r < 1000.0).then_some(r)
}

impl Probe {
    /// The audio stream that plays: the default one, else the first (§8.3; mpv's choice isn't
    /// known to the probe, and a moving item's hints skip it).
    pub fn chosen_audio(&self) -> Option<&AudioStream> {
        self.audio.iter().find(|a| a.default).or_else(|| self.audio.first())
    }

    /// `Some(true)` for field-coded video (`tt`/`bb`/`tb`/`bt`), `Some(false)` for `progressive`.
    pub fn interlaced(&self) -> Option<bool> {
        match self.video.as_ref()?.field_order.as_deref()? {
            "progressive" => Some(false),
            "tt" | "bb" | "tb" | "bt" => Some(true),
            _ => None,
        }
    }

    /// The decision's input. Where the probe doesn't know the scan type, the hints' answer (mpv
    /// saw the frames) stands.
    pub fn facts(&self, hints: &CastHints) -> Facts {
        let audio = self.chosen_audio();
        Facts {
            vcodec: self.video.as_ref().map(|v| v.codec.clone()),
            level: self.video.as_ref().and_then(|v| v.level),
            interlaced: self.interlaced().or(hints.interlaced),
            height: self.video.as_ref().and_then(|v| v.height),
            fps: self.video.as_ref().and_then(|v| v.fps),
            acodec: audio.map(|a| a.codec.clone()),
            channels: audio.and_then(|a| a.channels),
            layout: audio.and_then(|a| a.layout.clone()),
            aprofile: audio.and_then(|a| a.profile.clone()),
            sample_rate: audio.and_then(|a| a.sample_rate),
            language: audio.and_then(|a| a.language.clone()),
            width: self.video.as_ref().and_then(|v| v.width),
            transfer: self.video.as_ref().and_then(|v| v.transfer.clone()),
            audio_index: audio.map(|a| a.index),
            encrypted: self.encrypted,
        }
    }

    /// What `cast_hint` caches for the next cast of this item.
    pub fn hints(&self) -> CastHints {
        let audio = self.chosen_audio();
        CastHints {
            vcodec: self.video.as_ref().map(|v| v.codec.clone()),
            acodec: audio.map(|a| a.codec.clone()),
            channels: audio.and_then(|a| a.channels),
            interlaced: self.interlaced(),
            height: self.video.as_ref().and_then(|v| v.height),
        }
    }

    /// The stream info panel while casting (§11.3 `RefreshStreamInfo`).
    pub fn stream_info(&self) -> StreamInfo {
        let v = self.video.as_ref();
        let a = self.chosen_audio();
        StreamInfo {
            video_codec: v.map(|v| match (&v.profile, v.level) {
                (Some(p), Some(l)) if v.codec == "h264" => format!("{} ({p}@{:.1})", v.codec, l as f64 / 10.0),
                (Some(p), _) => format!("{} ({p})", v.codec),
                _ => v.codec.clone(),
            }),
            width: v.and_then(|v| v.width).map(i64::from),
            height: v.and_then(|v| v.height).map(i64::from),
            fps: v.and_then(|v| v.fps),
            pixel_format: v.and_then(|v| v.pix_fmt.clone()),
            audio_codec: a.map(|a| a.codec.clone()),
            audio_channels: a.and_then(|a| a.channels).map(|c| c.to_string()),
            samplerate: a.and_then(|a| a.sample_rate),
            container: self.container.clone(),
            ..StreamInfo::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape ffprobe 9 prints for a 576i DVB channel: MPEG-2 video, MP2 and AC-3 audio (the
    /// second flagged default), 0.5 s GOPs.
    const DVB: &str = r#"{
      "packets": [
        {"stream_index": 0, "pts_time": "1.400000", "flags": "K__"},
        {"stream_index": 1, "pts_time": "1.410000", "flags": "K__"},
        {"stream_index": 0, "pts_time": "1.440000", "flags": "___"},
        {"stream_index": 0, "pts_time": "1.900000", "flags": "K__"},
        {"stream_index": 0, "pts_time": "2.400000", "flags": "K__"}
      ],
      "streams": [
        {"index": 0, "codec_name": "mpeg2video", "codec_type": "video", "profile": "Main", "level": 8,
         "width": 720, "height": 576, "pix_fmt": "yuv420p", "field_order": "tt", "avg_frame_rate": "25/1",
         "codec_tag_string": "[2][0][0][0]", "disposition": {"default": 0, "attached_pic": 0}},
        {"index": 1, "codec_name": "mp2", "codec_type": "audio", "channels": 2, "sample_rate": "48000",
         "codec_tag_string": "[3][0][0][0]", "disposition": {"default": 0}},
        {"index": 2, "codec_name": "ac3", "codec_type": "audio", "channels": 6, "channel_layout": "5.1(side)", "sample_rate": "48000",
         "codec_tag_string": "[129][0][0][0]", "disposition": {"default": 1}, "tags": {"language": "eng"}},
        {"index": 3, "codec_name": "dvb_teletext", "codec_type": "subtitle"}
      ],
      "format": {"format_name": "mpegts", "nb_streams": 4}
    }"#;

    #[test]
    fn a_dvb_probe_becomes_facts_hints_and_stream_info() {
        let p = parse(DVB).unwrap();
        assert_eq!(p.container.as_deref(), Some("mpegts"));
        assert_eq!(p.interlaced(), Some(true));
        assert_eq!(p.max_gop_s, Some(0.5));
        let a = p.chosen_audio().unwrap();
        assert_eq!((a.index, a.codec.as_str(), a.channels), (1, "ac3", Some(6)), "the default track, mapped as 0:a:1");
        let f = p.facts(&CastHints::default());
        assert_eq!((f.vcodec.as_deref(), f.interlaced, f.audio_index, f.fps), (Some("mpeg2video"), Some(true), Some(1), Some(25.0)));
        assert_eq!(p.hints().height, Some(576));
        assert_eq!((f.layout.as_deref(), f.language.as_deref(), f.width), (Some("5.1(side)"), Some("eng"), Some(720)));
        assert_eq!(f.sound(), "ac3 5.1(side) 48 kHz");
        let info = p.stream_info();
        assert_eq!((info.video_codec.as_deref(), info.audio_channels.as_deref()), (Some("mpeg2video (Main)"), Some("6")));
    }

    #[test]
    fn unknown_scan_type_defers_to_the_hints_and_encryption_is_seen() {
        let json = r#"{"streams": [
            {"index": 0, "codec_name": "h264", "codec_type": "video", "codec_tag_string": "encv", "avg_frame_rate": "0/0",
             "r_frame_rate": "30000/1001", "level": 40, "profile": "High"},
            {"index": 1, "codec_name": "aac", "codec_type": "audio", "channels": 2}]}"#;
        let p = parse(json).unwrap();
        assert!(p.encrypted);
        assert_eq!(p.interlaced(), None);
        assert_eq!(p.facts(&CastHints { interlaced: Some(false), ..CastHints::default() }).interlaced, Some(false));
        assert!((p.video.as_ref().unwrap().fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(p.max_gop_s, None, "no packets listed");
        assert_eq!(p.stream_info().video_codec.as_deref(), Some("h264 (High@4.0)"));
        assert!(parse(r#"{"streams": []}"#).is_none() && parse("Cannot read").is_none());
    }

    #[test]
    fn the_argv_holds_only_the_given_url() {
        for hls in [false, true] {
            let a = argv("http://127.0.0.1:4711/u/tok", hls);
            assert_eq!(a.last().map(String::as_str), Some("http://127.0.0.1:4711/u/tok"));
            assert_eq!(a.iter().filter(|s| s.contains("://")).count(), 1);
            // an option the input doesn't take is an error to ffprobe: HLS only
            assert_eq!(a.join(" ").contains("-http_multiple 0 "), hls, "{a:?}");
        }
    }
}
