//! The level decision (DESIGN §40 §8.3): pure and table-tested. DRM → refuse; override or hint
//! L3 → L3; video outside the receiver profile → L3 (or refuse when `convert = never`);
//! interlaced on AirPlay with `interlaced = auto` → L3 + yadif (Apple HLS rule 1.14); HEVC → L2
//! fMP4; else L2 TS. Audio: AAC ≤ 2 ch copied, everything else AAC stereo 160k; multichannel
//! sound on the Apple TV adds a surround rendition (fMP4 throughout). OWNER: CAST-4.
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
//!   audio: AAC ≤ 2 ch → copy; everything else → AAC stereo 160k (Apple rule 2.3)
//!   surround (Apple TV, video, > 2 ch) .......................... + a surround rendition beside
//!       the stereo one (rules 2.5, 9.6: separate audio streams; fMP4 segments for every
//!       rendition): AC-3, E-AC-3 and AAC ≤ 5.1 copied; anything else AC-3 5.1 448k
//! ```
//! L1 (byte relay) is Phase 2, and only AirPlay receivers have a profile in Phase 1 (Google Cast
//! and DLNA plan as `Refuse(Unsupported)` until their phases add one).

use super::super::{CastErrorKind, CastHints, CastLevel, ReceiverKind};
use crate::config::{CastConvert, CastInterlaced};
use crate::model::cast::{CastFeed, FeedPicture, FeedSound, FeedSurround};
use crate::player::tracks::StreamInfo;

/// What a receiver plays without converting (DESIGN §40 §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// Multichannel sound goes as a surround rendition beside the stereo AAC one (§8.3): the
    /// Apple TV decodes AC-3, E-AC-3 and multichannel AAC (Apple's HLS authoring spec, "Audio
    /// compatibility") and sends them on as multichannel PCM, or Dolby Digital when set so.
    /// Third-party AirPlay TVs stay stereo: Apple's section for them makes the renditions'
    /// segment boundaries a MUST (8.22), which copied video can't promise.
    pub surround: bool,
    /// ffmpeg video codec names played as they are.
    pub video: &'static [&'static str],
    /// The highest H.264 `level_idc` played as is (`None`: no limit within the codec list; the
    /// Apple TV takes ≤ High@5.2, which covers broadcast H.264, Apple rule 1.3b).
    pub h264_max_level: Option<i64>,
    /// Interlaced input must be deinterlaced (AirPlay: Apple HLS rule 1.14 "All interlaced
    /// source content MUST be deinterlaced").
    pub deinterlace: bool,
}

/// The Apple TV: H.264 and HEVC, surround.
pub const APPLE_TV: Profile = Profile { video: &["h264", "hevc"], h264_max_level: None, deinterlace: true, surround: true };
/// Third-party AirPlay TVs (VioletRelay plays the same HLS on the Hisense): as the Apple TV, in
/// stereo.
pub const AIRPLAY_TV: Profile = Profile { surround: false, ..APPLE_TV };

/// The receiver's profile; `None` before its phase (Google Cast: 2, DLNA: 3).
pub fn profile(receiver: ReceiverKind) -> Option<Profile> {
    match receiver {
        ReceiverKind::AppleTv => Some(APPLE_TV),
        ReceiverKind::AirPlayTv => Some(AIRPLAY_TV),
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
    /// What the probe adds about that stream: its layout (`5.1(side)`), profile (`LC`,
    /// `HE-AAC`, `Dolby Digital Plus + Dolby Atmos`), sample rate and language tag (`eng`).
    pub layout: Option<String>,
    pub aprofile: Option<String>,
    pub sample_rate: Option<i64>,
    pub language: Option<String>,
    /// The picture's width and transfer characteristic (`smpte2084`, `arib-std-b67`): the
    /// multivariant playlist's `RESOLUTION` and `VIDEO-RANGE`.
    pub width: Option<u32>,
    pub transfer: Option<String>,
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
/// and the scan type are known, and the sound isn't surround for a receiver that takes it (the
/// surround rendition needs what only the probe tells: layout, profile, language, frame rate).
pub fn hints_suffice(h: &CastHints, receiver: ReceiverKind) -> bool {
    let surround = profile(receiver).is_some_and(|p| p.surround) && h.channels.is_some_and(|c| c > 2);
    h.vcodec.is_some() && h.acodec.is_some() && h.channels.is_some() && h.interlaced.is_some() && !surround
}

impl Facts {
    /// The sound for a log line: `aac 5.1(side) 48 kHz`, `ac3 6 ch`, `no audio`.
    pub fn sound(&self) -> String {
        let Some(codec) = self.acodec.as_deref() else { return "no audio".into() };
        let layout = self.layout.clone().or_else(|| self.channels.map(|c| format!("{c} ch")));
        let rate = self.sample_rate.map(|r| format!("{} kHz", r as f64 / 1000.0));
        [Some(codec.to_owned()), layout, rate].into_iter().flatten().collect::<Vec<_>>().join(" ")
    }
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
    /// Stereo only, whatever the source: the TV didn't play the surround renditions, or ffmpeg
    /// couldn't make them (the session's and the relay's one fallback each).
    pub stereo: bool,
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

/// The surround rendition beside the stereo AAC one (§8.3; Apple HLS authoring 2.3 stereo MUST,
/// 2.5 multichannel formats, 9.6 multichannel MUST be a separate audio stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surround {
    /// The source's own sound, copied: AC-3 (`ac-3`), E-AC-3 (`ec-3`), or AAC up to 5.1
    /// (`mp4a.40.2`, which MPEG-TS carries as ADTS: `aac` says so).
    Copy { aac: bool },
    /// Converted to AC-3 5.1 at 448 kbit/s (DTS, TrueHD, multichannel MP2 or PCM, AAC 7.1…):
    /// what every Dolby receiver decodes, and needing no Dolby Digital companion (rule 2.6 asks
    /// for one beside E-AC-3 for devices without it).
    Ac3,
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
    /// The stereo sound (with a surround rendition: always AAC stereo, made from the source).
    pub audio: Audio,
    pub segments: Segments,
    /// A surround rendition beside the stereo one (then every rendition is fMP4).
    pub surround: Option<Surround>,
}

/// The ladder (module docs). `Err` is the refusal: `Unsupported` (DRM, no profile, converting
/// off) or `NeedsFfmpeg` (converting is needed and allowed but an encoder is missing).
pub fn plan(receiver: ReceiverKind, facts: &Facts, opt: Options) -> Result<Plan, CastErrorKind> {
    if facts.encrypted {
        return Err(CastErrorKind::Unsupported);
    }
    let profile = profile(receiver).ok_or(CastErrorKind::Unsupported)?;
    let surround = surround(profile, facts, opt);
    let audio = if surround.is_some() { Audio::Aac } else { audio(facts) };
    // the renditions are demuxed, and an audio rendition is fMP4 (rule 2.1): so is the video
    let renditions = if surround.is_some() { Segments::Fmp4 } else { Segments::Ts };
    let convert = |deinterlace: bool| -> Result<Plan, CastErrorKind> {
        if !opt.can_convert {
            return Err(CastErrorKind::NeedsFfmpeg);
        }
        Ok(Plan { level: CastLevel::Converted, video: Video::Encode { deinterlace }, audio, segments: renditions, surround })
    };
    let Some(vcodec) = facts.vcodec.as_deref() else {
        // radio: the sound alone, repackaged
        return Ok(Plan { level: CastLevel::Repackaged, video: Video::None, audio, segments: Segments::Ts, surround: None });
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
    let segments = if vcodec == "hevc" { Segments::Fmp4 } else { renditions };
    Ok(Plan { level: CastLevel::Repackaged, video: Video::Copy, audio, segments, surround })
}

/// The surround rendition, when the receiver takes one and there is a picture (tvOS lists no
/// audio-only variant, rule 9.20: a radio channel stays a single stereo playlist) and more than
/// two channels.
fn surround(profile: Profile, f: &Facts, opt: Options) -> Option<Surround> {
    if !profile.surround || opt.stereo || f.vcodec.is_none() {
        return None;
    }
    let channels = f.channels.filter(|c| *c > 2)?;
    Some(match f.acodec.as_deref()? {
        "ac3" | "eac3" => Surround::Copy { aac: false },
        "aac" if channels <= 6 => Surround::Copy { aac: true },
        _ => Surround::Ac3,
    })
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

/// What `plan` feeds the TV (the info panel while casting, DESIGN §40 §11.3): its level and
/// each stream's way there, with the source facts it planned on (`probed`: ffprobe's, else the
/// hints'); `target_s` once the item's target duration is pinned (§8.1.4).
pub fn feed(plan: &Plan, facts: &Facts, probed: bool, target_s: Option<u32>) -> CastFeed {
    CastFeed {
        probed,
        level: plan.level,
        picture: match plan.video {
            Video::None => FeedPicture::None,
            Video::Copy => FeedPicture::Copied,
            Video::Encode { deinterlace } => FeedPicture::Converted { deinterlaced: deinterlace },
        },
        sound: match plan.audio {
            Audio::None => FeedSound::None,
            Audio::Copy => FeedSound::Copied,
            Audio::Aac => FeedSound::Stereo,
        },
        surround: plan.surround.map(|s| match s {
            Surround::Copy { .. } => FeedSurround::Copied,
            Surround::Ac3 => FeedSurround::Ac3,
        }),
        fmp4: plan.segments == Segments::Fmp4,
        target_s,
        interlaced: facts.interlaced,
        layout: facts.layout.clone(),
    }
}

/// The info panel's view of a launch (§11.3): ffprobe's view of the source (`probed`) or, when
/// the hints sufficed and the probe was skipped (§8.1.2), the source as the facts know it — the
/// codec names stay ffmpeg's, as the session copies them into the item's hints — with
/// [`feed`]. The relay sends it once the target is pinned ([`pinned`]).
pub fn fed_info(probed: Option<StreamInfo>, facts: &Facts, plan: &Plan) -> StreamInfo {
    let was_probed = probed.is_some();
    let mut info = probed.unwrap_or_else(|| StreamInfo {
        video_codec: facts.vcodec.clone(),
        width: facts.width.map(i64::from),
        height: facts.height.map(i64::from),
        fps: facts.fps,
        audio_codec: facts.acodec.clone(),
        audio_channels: facts.channels.map(|c| c.to_string()),
        samplerate: facts.sample_rate,
        ..StreamInfo::default()
    });
    info.cast = Some(feed(plan, facts, was_probed, None));
    info
}

/// [`fed_info`] with the target duration the launch pinned.
pub fn pinned(mut info: StreamInfo, target_s: u32) -> StreamInfo {
    if let Some(f) = info.cast.as_mut() {
        f.target_s = Some(target_s);
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(v: Option<&str>, a: Option<&str>, ch: u32, interlaced: bool) -> Facts {
        Facts { vcodec: v.map(str::to_owned), acodec: a.map(str::to_owned), channels: Some(ch), interlaced: Some(interlaced), ..Facts::default() }
    }

    const AUTO: Options = Options { convert: CastConvert::Auto, interlaced: CastInterlaced::Auto, force: None, can_convert: true, stereo: false };
    const L2_TS: fn(Audio) -> Plan = |audio| Plan { level: CastLevel::Repackaged, video: Video::Copy, audio, segments: Segments::Ts, surround: None };
    const L3: fn(bool, Audio) -> Plan = |deinterlace, audio| Plan {
        level: CastLevel::Converted,
        video: Video::Encode { deinterlace },
        audio,
        segments: Segments::Ts,
        surround: None,
    };

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
            ("AC-3 stereo", facts(Some("h264"), Some("ac3"), 2, false), AUTO, Ok(L2_TS(Audio::Aac))),
            ("interlaced → L3 + yadif (Apple 1.14)", facts(Some("h264"), Some("aac"), 2, true), AUTO, Ok(L3(true, Audio::Copy))),
            ("interlaced, send as is", facts(Some("h264"), Some("mp2"), 2, true), as_is, Ok(L2_TS(Audio::Aac))),
            ("interlaced, converting off", facts(Some("h264"), Some("aac"), 2, true), never, Ok(L2_TS(Audio::Copy))),
            ("interlaced, no encoder", facts(Some("h264"), Some("aac"), 2, true), no_enc, Ok(L2_TS(Audio::Copy))),
            (
                "HEVC → fMP4",
                facts(Some("hevc"), Some("eac3"), 2, false),
                AUTO,
                Ok(Plan { level: CastLevel::Repackaged, video: Video::Copy, audio: Audio::Aac, segments: Segments::Fmp4, surround: None }),
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
                Ok(Plan { level: CastLevel::Repackaged, video: Video::None, audio: Audio::Aac, segments: Segments::Ts, surround: None }),
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

    /// Multichannel sound on the Apple TV (§8.3): a surround rendition beside the stereo AAC one,
    /// every rendition fMP4; the source's Dolby or AAC ≤ 5.1 copied, anything else AC-3 5.1. A
    /// third-party TV, a radio channel, stereo sound or the stereo fallback: one stereo playlist,
    /// as before.
    #[test]
    fn surround_renditions_per_source_layout() {
        use Surround::*;
        let l2 = |segments, surround| Ok(Plan { level: CastLevel::Repackaged, video: Video::Copy, audio: Audio::Aac, segments, surround });
        let fmp4 = Segments::Fmp4;
        let rows: &[(&str, Facts, Options, Result<Plan, CastErrorKind>)] = &[
            ("H.264 + AC-3 5.1", facts(Some("h264"), Some("ac3"), 6, false), AUTO, l2(fmp4, Some(Copy { aac: false }))),
            ("HEVC + E-AC-3 5.1", facts(Some("hevc"), Some("eac3"), 6, false), AUTO, l2(fmp4, Some(Copy { aac: false }))),
            ("HEVC + E-AC-3 7.1", facts(Some("hevc"), Some("eac3"), 8, false), AUTO, l2(fmp4, Some(Copy { aac: false }))),
            ("HEVC + AAC 5.1 (the 2026-10-13 film)", facts(Some("hevc"), Some("aac"), 6, false), AUTO, l2(fmp4, Some(Copy { aac: true }))),
            ("AAC 7.1", facts(Some("h264"), Some("aac"), 8, false), AUTO, l2(fmp4, Some(Ac3))),
            ("DTS 5.1", facts(Some("hevc"), Some("dts"), 6, false), AUTO, l2(fmp4, Some(Ac3))),
            ("TrueHD 7.1", facts(Some("hevc"), Some("truehd"), 8, false), AUTO, l2(fmp4, Some(Ac3))),
            ("MP2 5.1", facts(Some("h264"), Some("mp2"), 6, false), AUTO, l2(fmp4, Some(Ac3))),
            (
                "interlaced + AC-3 5.1: converted, still surround",
                facts(Some("h264"), Some("ac3"), 6, true),
                AUTO,
                Ok(Plan {
                    level: CastLevel::Converted,
                    video: Video::Encode { deinterlace: true },
                    audio: Audio::Aac,
                    segments: fmp4,
                    surround: Some(Copy { aac: false }),
                }),
            ),
            ("stereo fallback", facts(Some("h264"), Some("ac3"), 6, false), Options { stereo: true, ..AUTO }, Ok(L2_TS(Audio::Aac))),
            ("AAC stereo", facts(Some("h264"), Some("aac"), 2, false), AUTO, Ok(L2_TS(Audio::Copy))),
            (
                "radio 5.1: no audio-only variant (rule 9.20)",
                facts(None, Some("ac3"), 6, false),
                AUTO,
                Ok(Plan { level: CastLevel::Repackaged, video: Video::None, audio: Audio::Aac, segments: Segments::Ts, surround: None }),
            ),
        ];
        for (what, f, opt, want) in rows {
            assert_eq!(plan(ReceiverKind::AppleTv, f, *opt), *want, "{what}");
        }
        // a third-party TV stays stereo
        assert_eq!(plan(ReceiverKind::AirPlayTv, &facts(Some("h264"), Some("ac3"), 6, false), AUTO), Ok(L2_TS(Audio::Aac)));
        assert_eq!(plan(ReceiverKind::AirPlayTv, &facts(Some("hevc"), Some("aac"), 6, false), AUTO), l2(fmp4, None));
        // the hints alone don't decide surround: the probe tells layout, language and frame rate
        let h = CastHints { vcodec: Some("hevc".into()), acodec: Some("aac".into()), channels: Some(6), interlaced: Some(false), height: None };
        assert!(!hints_suffice(&h, ReceiverKind::AppleTv));
        assert!(hints_suffice(&h, ReceiverKind::AirPlayTv));
        let f = Facts { layout: Some("5.1(side)".into()), sample_rate: Some(48000), ..facts(Some("hevc"), Some("aac"), 6, false) };
        assert_eq!(f.sound(), "aac 5.1(side) 48 kHz");
        assert_eq!(facts(Some("h264"), Some("ac3"), 6, false).sound(), "ac3 6 ch");
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
        assert!(hints_suffice(&full, ReceiverKind::AppleTv));
        assert!(!hints_suffice(&CastHints { interlaced: None, ..full.clone() }, ReceiverKind::AppleTv), "mpv knows interlace only per frame");
        assert!(!hints_suffice(&CastHints::default(), ReceiverKind::AppleTv));
    }

    /// What the info panel says the TV gets (§11.3), per rung of the ladder: the level, each
    /// stream's way, the segments; a launch from the hints alone keeps their facts and says it
    /// wasn't probed, one from ffprobe keeps the probe's view; the pinned target comes last.
    #[test]
    fn the_feed_says_how_each_stream_reaches_the_tv() {
        use FeedPicture as P;
        let fed = |f: &Facts| feed(&plan(ReceiverKind::AppleTv, f, AUTO).unwrap(), f, true, None);
        let surround = Facts { layout: Some("5.1(side)".into()), ..facts(Some("h264"), Some("ac3"), 6, false) };
        // level, picture, stereo sound, surround, fMP4
        type Way = (CastLevel, FeedPicture, FeedSound, Option<FeedSurround>, bool);
        let rows: &[(&str, Facts, Way)] = &[
            ("as it comes", facts(Some("h264"), Some("aac"), 2, false), (CastLevel::Repackaged, P::Copied, FeedSound::Copied, None, false)),
            ("MP2 to AAC", facts(Some("h264"), Some("mp2"), 2, false), (CastLevel::Repackaged, P::Copied, FeedSound::Stereo, None, false)),
            (
                "576i MPEG-2",
                facts(Some("mpeg2video"), Some("mp2"), 2, true),
                (CastLevel::Converted, P::Converted { deinterlaced: true }, FeedSound::Stereo, None, false),
            ),
            ("HEVC", facts(Some("hevc"), Some("aac"), 2, false), (CastLevel::Repackaged, P::Copied, FeedSound::Copied, None, true)),
            ("AC-3 5.1", surround.clone(), (CastLevel::Repackaged, P::Copied, FeedSound::Stereo, Some(FeedSurround::Copied), true)),
            (
                "DTS 5.1",
                facts(Some("h264"), Some("dts"), 6, false),
                (CastLevel::Repackaged, P::Copied, FeedSound::Stereo, Some(FeedSurround::Ac3), true),
            ),
            ("radio", facts(None, Some("mp3"), 2, false), (CastLevel::Repackaged, P::None, FeedSound::Stereo, None, false)),
        ];
        for (what, f, want) in rows {
            let got = fed(f);
            assert_eq!((got.level, got.picture, got.sound, got.surround, got.fmp4), *want, "{what}");
        }
        assert_eq!(fed(&surround).layout.as_deref(), Some("5.1(side)"));

        // hints alone: their facts, not probed; the probe's own view wins when it ran
        let f = Facts { height: Some(1080), width: Some(1920), ..facts(Some("h264"), Some("ac3"), 2, true) };
        let p = plan(ReceiverKind::AppleTv, &f, AUTO).unwrap();
        let info = fed_info(None, &f, &p);
        assert_eq!((info.video_codec.as_deref(), info.height, info.audio_channels.as_deref()), (Some("h264"), Some(1080), Some("2")));
        let cast = info.cast.clone().unwrap();
        assert!(!cast.probed && cast.interlaced == Some(true) && cast.target_s.is_none(), "{cast:?}");
        let probed = StreamInfo { video_codec: Some("h264 (High@4.0)".into()), container: Some("mpegts".into()), ..StreamInfo::default() };
        let info = fed_info(Some(probed), &f, &p);
        assert_eq!((info.video_codec.as_deref(), info.container.as_deref()), (Some("h264 (High@4.0)"), Some("mpegts")));
        assert!(info.cast.as_ref().is_some_and(|c| c.probed));
        assert_eq!(pinned(info, 6).cast.and_then(|c| c.target_s), Some(6));
    }
}
