//! Track list, stream info, aspect/zoom. Owner: WP-07.

use libmpv2::Mpv;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Sub,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: i64,
    pub kind: TrackKind,
    pub title: Option<String>,
    pub lang: Option<String>,
    pub codec: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub selected: bool,
    pub external: bool,
    pub channels: Option<i64>,
}

impl Track {
    /// "English · AC-3 5.1" style label for menus.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = &self.title {
            parts.push(t.clone());
        }
        if let Some(l) = &self.lang {
            parts.push(l.clone());
        }
        if let Some(c) = &self.codec {
            parts.push(c.to_uppercase());
        }
        if let Some(ch) = self.channels {
            parts.push(format!("{ch}ch"));
        }
        if self.forced {
            parts.push("forced".into());
        }
        if parts.is_empty() { format!("Track {}", self.id) } else { parts.join(" · ") }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tracks {
    pub video: Vec<Track>,
    pub audio: Vec<Track>,
    pub subs: Vec<Track>,
}

impl Tracks {
    pub fn selected_audio(&self) -> Option<&Track> {
        self.audio.iter().find(|t| t.selected)
    }
    pub fn selected_sub(&self) -> Option<&Track> {
        self.subs.iter().find(|t| t.selected)
    }
}

/// Parse mpv's `track-list` (read as STRING ⇒ JSON array).
pub fn parse_track_list(json: &str) -> Tracks {
    let mut out = Tracks::default();
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else { return out };
    for it in items {
        let kind = match it.get("type").and_then(Value::as_str) {
            Some("video") => TrackKind::Video,
            Some("audio") => TrackKind::Audio,
            Some("sub") => TrackKind::Sub,
            _ => continue,
        };
        let s = |k: &str| it.get(k).and_then(Value::as_str).map(str::to_owned);
        let b = |k: &str| it.get(k).and_then(Value::as_bool).unwrap_or(false);
        let t = Track {
            id: it.get("id").and_then(Value::as_i64).unwrap_or(0),
            kind,
            title: s("title"),
            lang: s("lang"),
            codec: s("codec"),
            default: b("default"),
            forced: b("forced"),
            selected: b("selected"),
            external: b("external"),
            channels: it.get("demux-channel-count").and_then(Value::as_i64),
        };
        match kind {
            TrackKind::Video => out.video.push(t),
            TrackKind::Audio => out.audio.push(t),
            TrackKind::Sub => out.subs.push(t),
        }
    }
    out
}

/// Stream info panel (`I`): read on demand, refreshed every 2 s while visible.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamInfo {
    pub video_codec: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub fps: Option<f64>,
    pub pixel_format: Option<String>,
    /// `video-params/gamma`: "pq"/"hlg" ⇒ HDR badge.
    pub gamma: Option<String>,
    pub colormatrix: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<String>,
    pub samplerate: Option<i64>,
    pub video_bitrate: Option<f64>,
    pub audio_bitrate: Option<f64>,
    pub hwdec: Option<String>,
    pub dropped_frames: Option<i64>,
    pub cache_secs: Option<f64>,
    pub container: Option<String>,
}

impl StreamInfo {
    pub fn is_hdr(&self) -> bool {
        matches!(self.gamma.as_deref(), Some("pq") | Some("hlg"))
    }
}

/// Query all stream-info properties (each missing one → None). Cheap (~20 property reads).
pub fn query_stream_info(mpv: &Mpv) -> StreamInfo {
    let s = |n: &str| mpv.get_property::<String>(n).ok().filter(|v| !v.is_empty());
    let i = |n: &str| mpv.get_property::<i64>(n).ok();
    let f = |n: &str| mpv.get_property::<f64>(n).ok();
    StreamInfo {
        video_codec: s("video-codec"),
        width: i("video-params/w"),
        height: i("video-params/h"),
        fps: f("estimated-vf-fps").or_else(|| f("container-fps")),
        pixel_format: s("video-params/pixelformat"),
        gamma: s("video-params/gamma"),
        colormatrix: s("video-params/colormatrix"),
        audio_codec: s("audio-codec-name"),
        audio_channels: s("audio-params/channels"),
        samplerate: i("audio-params/samplerate"),
        video_bitrate: f("video-bitrate"),
        audio_bitrate: f("audio-bitrate"),
        hwdec: s("hwdec-current"),
        dropped_frames: i("frame-drop-count"),
        cache_secs: f("demuxer-cache-duration"),
        container: s("file-format"),
    }
}

/// Aspect override (`v` cycles). Maps to mpv `video-aspect-override` / `panscan` / `keepaspect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AspectMode {
    #[default]
    Auto,
    R16x9,
    R4x3,
    R21x9,
    /// Crop to fill (panscan=1).
    Fill,
    /// Ignore aspect (keepaspect=no).
    Stretch,
}

impl AspectMode {
    pub const CYCLE: [AspectMode; 6] =
        [AspectMode::Auto, AspectMode::R16x9, AspectMode::R4x3, AspectMode::R21x9, AspectMode::Fill, AspectMode::Stretch];

    pub fn next(self) -> Self {
        let i = Self::CYCLE.iter().position(|m| *m == self).unwrap_or(0);
        Self::CYCLE[(i + 1) % Self::CYCLE.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            AspectMode::Auto => "Auto",
            AspectMode::R16x9 => "16:9",
            AspectMode::R4x3 => "4:3",
            AspectMode::R21x9 => "21:9",
            AspectMode::Fill => "Fill",
            AspectMode::Stretch => "Stretch",
        }
    }

    /// (video-aspect-override, panscan, keepaspect) — note: with the render API the app also
    /// sizes the FBO; `keepaspect` applies inside the FBO.
    pub fn mpv_values(self) -> (&'static str, f64, bool) {
        match self {
            AspectMode::Auto => ("no", 0.0, true),
            AspectMode::R16x9 => ("16:9", 0.0, true),
            AspectMode::R4x3 => ("4:3", 0.0, true),
            AspectMode::R21x9 => ("2.33", 0.0, true),
            AspectMode::Fill => ("no", 1.0, true),
            AspectMode::Stretch => ("no", 0.0, false),
        }
    }
}
