//! The level decision (DESIGN §40 §8.3): pure and table-tested. DRM → refuse; override or hint
//! L3 → L3; video outside the receiver profile → L3 (or refuse when `convert = never`);
//! interlaced on AirPlay with `interlaced = auto` → L3 + yadif (Apple HLS rule 1.14); HEVC → L2
//! fMP4; else L2 TS. Audio: AAC ≤ 2 ch copied, everything else AAC stereo 160k. OWNER: CAST-4.
//!
//! ```text
//! plan(receiver, facts, options) →
//!   DRM / encrypted ............................................ Refuse(Unsupported)
//!   user override or cast_hint says L3 ........................ L3
//!   video not in receiver profile (MPEG-2, VC-1, VP9 on Apple…)  L3 (convert = auto, else Refuse(Unsupported))
//!   H.264 above receiver level (Cast gen 1/2, Phase 2) ......... L3
//!   interlaced && interlaced = auto && profile.deinterlace .... L3 + yadif   (Apple HLS rule 1.14)
//!   HEVC ...................................................... L2 with fMP4 segments (Apple rule 1.5)
//!   otherwise ................................................. L2 (TS segments)
//!   audio: AAC ≤ 2 ch → copy; everything else → AAC stereo 160k (Apple rule 2.3; surround is Phase 2)
//! ```
//! L1 (byte relay) is Phase 2, and only AirPlay receivers have a profile in Phase 1 (Google Cast
//! and DLNA plan as `Refuse(Unsupported)` until their phases add one).

use super::super::{CastErrorKind, CastHints, CastLevel, ReceiverKind};
use crate::config::{CastConvert, CastInterlaced};

/// What a receiver plays without converting (DESIGN §40 §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// ffmpeg video codec names played as they are.
    pub video: &'static [&'static str],
    /// The highest H.264 `level_idc` played as is (`None`: no limit within the codec list; the
    /// Apple TV takes ≤ High@5.2, which covers broadcast H.264, Apple rule 1.3b).
    pub h264_max_level: Option<i64>,
    /// Interlaced input must be deinterlaced (AirPlay: Apple HLS rule 1.14 "All interlaced
    /// source content MUST be deinterlaced").
    pub deinterlace: bool,
}

/// The Apple TV and the experimental third-party AirPlay TVs (VioletRelay plays the same HLS on
/// the Hisense): H.264 and HEVC.
pub const AIRPLAY: Profile = Profile { video: &["h264", "hevc"], h264_max_level: None, deinterlace: true };

/// The receiver's profile; `None` before its phase (Google Cast: 2, DLNA: 3).
pub fn profile(receiver: ReceiverKind) -> Option<Profile> {
    match receiver {
        ReceiverKind::AppleTv | ReceiverKind::AirPlayTv => Some(AIRPLAY),
        ReceiverKind::Chromecast | ReceiverKind::Dlna => None,
    }
}

/// What is known about the input (DESIGN §40 §8.2: the hints, else the probe).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// ffmpeg video codec name; `None`: no video stream (a radio channel).
    pub vcodec: Option<String>,
    /// H.264 / HEVC `level` as ffprobe reports it (`level_idc`).
    pub level: Option<i64>,
    pub interlaced: Option<bool>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    /// ffmpeg audio codec name of the stream that plays; `None`: no audio.
    pub acodec: Option<String>,
    pub channels: Option<u32>,
    /// `-map 0:a:<n>`: the audio stream that plays (mpv's choice, else the default, else the
    /// first; §8.3). `None`: the first, if any (`0:a:0?`).
    pub audio_index: Option<usize>,
    /// DRM or encrypted (HLS `SAMPLE-AES`, an `encv`/`enca` track).
    pub encrypted: bool,
}

impl Facts {
    /// From the hints alone (mpv's view of the moving item, or a cached `cast_hint`).
    pub fn from_hints(h: &CastHints) -> Self {
        Self {
            vcodec: h.vcodec.clone(),
            interlaced: h.interlaced,
            height: h.height,
            acodec: h.acodec.clone(),
            channels: h.channels,
            ..Self::default()
        }
    }
}

/// The hints decide on their own (the probe is skipped, §8.1.2): both codecs, the channel count
/// and the scan type are known.
pub fn hints_suffice(h: &CastHints) -> bool {
    h.vcodec.is_some() && h.acodec.is_some() && h.channels.is_some() && h.interlaced.is_some()
}

/// The settings and state the decision depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub convert: CastConvert,
    pub interlaced: CastInterlaced,
    /// "Convert this channel", a cached `cast_hint` level, or the session's one-step escalation.
    /// Only `Converted` changes the ladder (a cached L2 doesn't skip deinterlacing).
    pub force: Option<CastLevel>,
    /// `libx264` and `aac` are there ([`super::check_ffmpeg`]): without them L3 is off.
    pub can_convert: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Video {
    /// No video stream.
    None,
    Copy,
    /// libx264 (DESIGN §40 §8.3 L3 settings); `deinterlace`: `yadif=deint=interlaced` first.
    Encode {
        deinterlace: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audio {
    None,
    Copy,
    /// AAC stereo 160k (Apple rule 2.3).
    Aac,
}

/// HLS segment container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segments {
    /// MPEG-TS (`s<n>.ts`): H.264.
    Ts,
    /// fMP4 (`init.mp4` + `s<n>.m4s`) with the `hvc1` tag: HEVC (Apple rule 1.5; never HEVC in
    /// TS).
    Fmp4,
}

impl Segments {
    pub fn ext(self) -> &'static str {
        match self {
            Segments::Ts => "ts",
            Segments::Fmp4 => "m4s",
        }
    }
}

/// How one generation is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub level: CastLevel,
    pub video: Video,
    pub audio: Audio,
    pub segments: Segments,
}

/// The ladder (module docs). `Err` is the refusal: `Unsupported` (DRM, no profile, converting
/// off) or `NeedsFfmpeg` (converting is needed and allowed but an encoder is missing).
pub fn plan(receiver: ReceiverKind, facts: &Facts, opt: Options) -> Result<Plan, CastErrorKind> {
    if facts.encrypted {
        return Err(CastErrorKind::Unsupported);
    }
    let profile = profile(receiver).ok_or(CastErrorKind::Unsupported)?;
    let audio = audio(facts);
    let convert = |deinterlace: bool| -> Result<Plan, CastErrorKind> {
        if !opt.can_convert {
            return Err(CastErrorKind::NeedsFfmpeg);
        }
        Ok(Plan { level: CastLevel::Converted, video: Video::Encode { deinterlace }, audio, segments: Segments::Ts })
    };
    let Some(vcodec) = facts.vcodec.as_deref() else {
        // radio: the sound alone, repackaged
        return Ok(Plan { level: CastLevel::Repackaged, video: Video::None, audio, segments: Segments::Ts });
    };
    // yadif with `deint=interlaced` leaves progressive frames alone, so a converted picture is
    // always run through it for a receiver that needs deinterlacing (the scan type of a channel
    // can change mid-stream, and the hints may not know it)
    let deinterlace = profile.deinterlace && facts.interlaced != Some(false);
    if opt.force == Some(CastLevel::Converted) {
        return convert(deinterlace);
    }
    if !profile.video.contains(&vcodec) {
        return match opt.convert {
            CastConvert::Auto => convert(deinterlace),
            CastConvert::Never => Err(CastErrorKind::Unsupported),
        };
    }
    if vcodec == "h264"
        && let (Some(max), Some(level)) = (profile.h264_max_level, facts.level)
        && level > max
    {
        return match opt.convert {
            CastConvert::Auto => convert(deinterlace),
            CastConvert::Never => Err(CastErrorKind::Unsupported),
        };
    }
    // Apple 1.14 is an authoring MUST, not a playback limit: AVPlayer shows interlaced H.264. So
    // when converting is off (or impossible) the picture goes as is rather than not at all.
    if facts.interlaced == Some(true)
        && opt.interlaced == CastInterlaced::Auto
        && profile.deinterlace
        && opt.convert == CastConvert::Auto
        && opt.can_convert
    {
        return convert(true);
    }
    let segments = if vcodec == "hevc" { Segments::Fmp4 } else { Segments::Ts };
    Ok(Plan { level: CastLevel::Repackaged, video: Video::Copy, audio, segments })
}

/// AAC with at most two channels is copied; anything else (MP2, AC-3, E-AC-3, AAC-LATM, AAC 5.1,
/// an unknown channel count) becomes AAC stereo.
fn audio(f: &Facts) -> Audio {
    match (f.acodec.as_deref(), f.channels) {
        (None, _) => Audio::None,
        (Some("aac"), Some(1..=2)) => Audio::Copy,
        _ => Audio::Aac,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(v: Option<&str>, a: Option<&str>, ch: u32, interlaced: bool) -> Facts {
        Facts { vcodec: v.map(str::to_owned), acodec: a.map(str::to_owned), channels: Some(ch), interlaced: Some(interlaced), ..Facts::default() }
    }

    const AUTO: Options = Options { convert: CastConvert::Auto, interlaced: CastInterlaced::Auto, force: None, can_convert: true };
    const L2_TS: fn(Audio) -> Plan = |audio| Plan { level: CastLevel::Repackaged, video: Video::Copy, audio, segments: Segments::Ts };
    const L3: fn(bool, Audio) -> Plan =
        |deinterlace, audio| Plan { level: CastLevel::Converted, video: Video::Encode { deinterlace }, audio, segments: Segments::Ts };

    /// Every Apple TV row of §4.2 and every rung of §8.3.
    #[test]
    fn the_ladder_for_airplay() {
        use CastErrorKind::*;
        let never = Options { convert: CastConvert::Never, ..AUTO };
        let as_is = Options { interlaced: CastInterlaced::AsIs, ..AUTO };
        let no_enc = Options { can_convert: false, ..AUTO };
        let forced = Options { force: Some(CastLevel::Converted), ..AUTO };
        let cached_l2 = Options { force: Some(CastLevel::Repackaged), ..AUTO };
        let encrypted = Facts { encrypted: true, ..facts(Some("h264"), Some("aac"), 2, false) };
        let rows: &[(&str, Facts, Options, Result<Plan, CastErrorKind>)] = &[
            ("progressive H.264 + AAC stereo", facts(Some("h264"), Some("aac"), 2, false), AUTO, Ok(L2_TS(Audio::Copy))),
            ("MP2 sound", facts(Some("h264"), Some("mp2"), 2, false), AUTO, Ok(L2_TS(Audio::Aac))),
            ("AAC-LATM sound", facts(Some("h264"), Some("aac_latm"), 2, false), AUTO, Ok(L2_TS(Audio::Aac))),
            ("AC-3 5.1", facts(Some("h264"), Some("ac3"), 6, false), AUTO, Ok(L2_TS(Audio::Aac))),
            ("AAC 5.1", facts(Some("h264"), Some("aac"), 6, false), AUTO, Ok(L2_TS(Audio::Aac))),
            ("interlaced → L3 + yadif (Apple 1.14)", facts(Some("h264"), Some("aac"), 2, true), AUTO, Ok(L3(true, Audio::Copy))),
            ("interlaced, send as is", facts(Some("h264"), Some("mp2"), 2, true), as_is, Ok(L2_TS(Audio::Aac))),
            ("interlaced, converting off", facts(Some("h264"), Some("aac"), 2, true), never, Ok(L2_TS(Audio::Copy))),
            ("interlaced, no encoder", facts(Some("h264"), Some("aac"), 2, true), no_enc, Ok(L2_TS(Audio::Copy))),
            (
                "HEVC → fMP4",
                facts(Some("hevc"), Some("eac3"), 6, false),
                AUTO,
                Ok(Plan { level: CastLevel::Repackaged, video: Video::Copy, audio: Audio::Aac, segments: Segments::Fmp4 }),
            ),
            ("MPEG-2 → L3", facts(Some("mpeg2video"), Some("mp2"), 2, false), AUTO, Ok(L3(false, Audio::Aac))),
            ("576i MPEG-2 → L3 + yadif", facts(Some("mpeg2video"), Some("mp2"), 2, true), AUTO, Ok(L3(true, Audio::Aac))),
            ("VC-1, converting off", facts(Some("vc1"), Some("aac"), 2, false), never, Err(Unsupported)),
            ("VP9, no encoder", facts(Some("vp9"), Some("opus"), 2, false), no_enc, Err(NeedsFfmpeg)),
            ("DRM", encrypted, AUTO, Err(Unsupported)),
            ("Convert this channel", facts(Some("h264"), Some("aac"), 2, false), forced, Ok(L3(false, Audio::Copy))),
            ("a cached L2 still deinterlaces", facts(Some("h264"), Some("aac"), 2, true), cached_l2, Ok(L3(true, Audio::Copy))),
            ("forced, no encoder", facts(Some("h264"), Some("aac"), 2, false), Options { can_convert: false, ..forced }, Err(NeedsFfmpeg)),
            (
                "radio",
                facts(None, Some("mp2"), 2, false),
                AUTO,
                Ok(Plan { level: CastLevel::Repackaged, video: Video::None, audio: Audio::Aac, segments: Segments::Ts }),
            ),
            ("silent", facts(Some("h264"), None, 0, false), AUTO, Ok(L2_TS(Audio::None))),
        ];
        for (what, f, opt, want) in rows {
            assert_eq!(plan(ReceiverKind::AppleTv, f, *opt), *want, "{what}");
            assert_eq!(plan(ReceiverKind::AirPlayTv, f, *opt), *want, "{what} (third-party AirPlay)");
        }
        let unknown_scan = Facts { interlaced: None, ..facts(Some("mpeg2video"), Some("aac"), 2, false) };
        assert_eq!(plan(ReceiverKind::AppleTv, &unknown_scan, AUTO), Ok(L3(true, Audio::Copy)), "unknown scan: yadif decides per frame");
    }

    #[test]
    fn later_phases_have_no_profile_yet() {
        let f = facts(Some("h264"), Some("aac"), 2, false);
        assert_eq!(plan(ReceiverKind::Chromecast, &f, AUTO), Err(CastErrorKind::Unsupported));
        assert_eq!(plan(ReceiverKind::Dlna, &f, AUTO), Err(CastErrorKind::Unsupported));
    }

    #[test]
    fn hints_suffice_only_when_complete() {
        let full = CastHints { vcodec: Some("h264".into()), acodec: Some("aac".into()), channels: Some(2), interlaced: Some(false), height: None };
        assert!(hints_suffice(&full));
        assert!(!hints_suffice(&CastHints { interlaced: None, ..full.clone() }), "mpv knows interlace only per frame");
        assert!(!hints_suffice(&CastHints::default()));
    }
}
