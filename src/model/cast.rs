//! Casting value types (DESIGN §40 §5.4). FROZEN (skeleton CAST-0): shared by the player
//! (`player::remote`), the cast service (`crate::cast`), the UI and the agent. Plain data: no
//! I/O, no addresses. A device is named by its [`DeviceId`], never by where it is.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A receiver's stable identity: `blake3(protocol ‖ 0 ‖ stable id)[..16]` (AirPlay: the TXT
/// `pi`, else `deviceid`). `Display` / `FromStr` / serde use 32 lower-case hex digits (the
/// agent's `device:<hex>` ref, `cast.default_device`). Never derived from an address: those
/// change and are re-resolved every session.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId([u8; 16]);

impl DeviceId {
    /// The id of the receiver `stable_id` speaks `protocol` (`"airplay"`, `"googlecast"`, …).
    pub fn derive(protocol: &str, stable_id: &str) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(protocol.as_bytes());
        h.update(&[0]);
        h.update(stable_id.as_bytes());
        let mut id = [0u8; 16];
        id.copy_from_slice(&h.finalize().as_bytes()[..16]);
        Self(id)
    }

    pub const fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The first 4 hex digits: how logs name a device ("cast device 3fa1").
    pub fn short(&self) -> String {
        format!("{:02x}{:02x}", self.0[0], self.0[1])
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

/// Not 32 hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadDeviceId;

impl fmt::Display for BadDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a device id")
    }
}

impl std::error::Error for BadDeviceId {}

impl FromStr for DeviceId {
    type Err = BadDeviceId;
    fn from_str(s: &str) -> Result<Self, BadDeviceId> {
        let s = s.as_bytes();
        if s.len() != 32 {
            return Err(BadDeviceId);
        }
        let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8).ok_or(BadDeviceId);
        let mut id = [0u8; 16];
        for (i, pair) in s.as_chunks::<2>().0.iter().enumerate() {
            id[i] = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        Ok(Self(id))
    }
}

impl Serialize for DeviceId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DeviceId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// What kind of receiver a device is (the agent's `kind`, the picker's second line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiverKind {
    /// An Apple TV (AirPlay 2 video, `/command` play queue). Phase 1.
    AppleTv,
    /// A third-party AirPlay TV (Hisense, Samsung, LG): the third-party profile (DESIGN §40 §7.7).
    AirPlayTv,
    /// A Mac's AirPlay Receiver (macOS 12+): the Apple TV's play queue after transient pairing
    /// (no PIN; DESIGN §40 Appendix E).
    Mac,
    /// A Chromecast, Google TV or TV with Cast built in (Google Cast, the Default Media Receiver).
    Chromecast,
    /// A DLNA / UPnP AV MediaRenderer (one continuous MPEG-TS stream).
    Dlna,
}

/// Where a cast session is (the casting card's phase line, `status.casting.phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CastPhase {
    /// The session is starting (pair-verify, SETUP).
    Connecting,
    /// Told to play, before the first picture (also while a zap tunes).
    Starting,
    Playing,
    Paused,
    Buffering,
    /// A seek's replace is under way.
    Seeking,
    /// The session ended without the user asking (the error is in the player's snapshot).
    Lost,
}

/// How the TV is fed (DESIGN §40 §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CastLevel {
    /// L1: the bytes as the provider sends them (Phase 2+).
    Original,
    /// L2: remuxed to HLS; video copied, audio copied or converted to AAC stereo.
    Repackaged,
    /// L3: video re-encoded (libx264, deinterlaced when interlaced), then HLS.
    Converted,
}

/// Why casting failed (DESIGN §40 §3.4): each kind has one fixed sentence and its actions; the
/// copy never contains device or provider text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CastErrorKind {
    /// The TV doesn't answer (off, another network, refused or timed out).
    Unreachable,
    /// Not paired yet.
    PairingRequired,
    /// Pair-verify failed: the TV forgot Tuner (reset, update, Allow Access changed).
    PairingLost,
    /// The TV refuses pairing (HTTP 470, "Allow Access" set to the Home only).
    PairingRefused,
    /// The TV couldn't fetch from Tuner and the firewall blocks (or may block) the port.
    Firewall,
    /// The TV can't play this format and converting is off (or the item is DRM).
    Unsupported,
    /// The `ffmpeg` CLI (or its encoders) is missing.
    NeedsFfmpeg,
    /// Converting runs slower than real time.
    CpuTooSlow,
    /// Stopped from the TV's remote, or another device took over: casting ends quietly.
    StoppedOnTv,
    /// The TV fetched but couldn't play it (after one escalation).
    ReceiverFailed,
}

impl CastErrorKind {
    /// The agent's typed reason (`status.casting.error`, `play` outcomes): `cast-firewall`, …
    pub fn reason(self) -> &'static str {
        match self {
            CastErrorKind::Unreachable => "cast-unreachable",
            CastErrorKind::PairingRequired => "cast-pairing-required",
            CastErrorKind::PairingLost => "cast-pairing-lost",
            CastErrorKind::PairingRefused => "cast-pairing-refused",
            CastErrorKind::Firewall => "cast-firewall",
            CastErrorKind::Unsupported => "cast-unsupported",
            CastErrorKind::NeedsFfmpeg => "cast-needs-ffmpeg",
            CastErrorKind::CpuTooSlow => "cast-cpu-too-slow",
            CastErrorKind::StoppedOnTv => "cast-stopped-on-tv",
            CastErrorKind::ReceiverFailed => "cast-receiver-failed",
        }
    }
}

/// The cast session as the player's snapshot shows it (`PlayerSnapshot::cast`; `None` = the
/// picture is local).
#[derive(Debug, Clone, PartialEq)]
pub struct CastView {
    pub device: DeviceId,
    /// Sanitized (`cast::sanitize_name`): no control characters, ≤ 64 chars, never a URL.
    pub name: String,
    pub kind: ReceiverKind,
    pub phase: CastPhase,
    /// How the TV is fed, once the relay decided.
    pub level: Option<CastLevel>,
    /// The TV is fed L3 only because it was forced (Convert this channel, an L2 failure
    /// remembered for this item on this TV, the session's escalation), not by the format: the
    /// player menu offers **Send as is** instead of a greyed Convert this channel (§3.3, §8.3).
    pub as_is: bool,
    /// Live: roughly how far the TV runs behind the live point.
    pub behind_live_s: Option<u16>,
    /// The receiver can seek (by replace; DESIGN §40 §11.3).
    pub can_seek: bool,
    /// The receiver takes a volume (AirPlay: no — "Use the TV remote for volume").
    pub volume: bool,
}

/// What the TV is fed (DESIGN §40 §8.3, §8.4): the relay's plan for the generation that plays,
/// sent with every launch (`StreamInfo::cast`, the info panel while casting, §11.3
/// `RefreshStreamInfo`). The source itself is the rest of that `StreamInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastFeed {
    /// The source's facts are ffprobe's; `false`: the hints alone (mpv's view of the item that
    /// moved to the TV, or a cached `cast_hint`) said enough and the probe was skipped (§8.1.2).
    pub probed: bool,
    pub level: CastLevel,
    pub picture: FeedPicture,
    /// The stereo sound (beside a surround rendition always AAC stereo, made from the source).
    pub sound: FeedSound,
    /// The surround rendition beside the stereo one (§8.4), if any.
    pub surround: Option<FeedSurround>,
    /// fMP4 segments (HEVC, or every rendition beside a surround one); else MPEG-TS.
    pub fmp4: bool,
    /// The item's pinned `#EXT-X-TARGETDURATION` (§8.1.4), once its first segments are ready.
    pub target_s: Option<u32>,
    /// A full-length item (§8.5): the TV has the whole VOD playlist, this many segments, each
    /// made when the TV asks for it. `None`: a growing (EVENT) or live playlist.
    pub full_length: Option<u64>,
    /// The source's scan type as planned on (`None`: unknown).
    pub interlaced: Option<bool>,
    /// The source sound's channel layout as ffprobe names it (`5.1(side)`); probe only.
    pub layout: Option<String>,
}

/// The picture's way to the TV (§8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedPicture {
    /// No picture (a radio channel).
    None,
    /// Copied as the provider sends it (L2 repackaged, L1 untouched).
    Copied,
    /// Re-encoded on this computer (L3); `deinterlaced`: through yadif first (Apple rule 1.14).
    Converted { deinterlaced: bool },
}

/// The stereo sound's way to the TV (§8.3: AAC up to two channels is copied).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedSound {
    None,
    Copied,
    /// Converted to AAC stereo (Apple rule 2.3).
    Stereo,
}

/// The surround rendition (§8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedSurround {
    /// The source's own AC-3, E-AC-3 or AAC ≤ 5.1, copied.
    Copied,
    /// Converted to AC-3 5.1.
    Ac3,
}

/// Where the TV's positions come from (DESIGN §40 §7.3; VOD and catch-up only, live has
/// none): its `playbackInfo` answers, or, once one went unanswered, the estimate from
/// `timeJumped` and its fetches (a little behind). The info panel says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSource {
    Reported,
    Estimated,
}

/// What is known about an item's streams before the relay plans (DESIGN §40 §8.2): mpv's view
/// when moving what plays, a cached `cast_hint`, or nothing (the relay probes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CastHints {
    /// ffmpeg codec names (`h264`, `hevc`, `mpeg2video`…).
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub channels: Option<u32>,
    pub interlaced: Option<bool>,
    pub height: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_are_stable_hex_and_parse_back() {
        let id = DeviceId::derive("airplay", "6C:4A:85:00:00:01");
        assert_eq!(id, DeviceId::derive("airplay", "6C:4A:85:00:00:01"));
        assert_ne!(id, DeviceId::derive("googlecast", "6C:4A:85:00:00:01"));
        let hex = id.to_string();
        assert_eq!(hex.len(), 32);
        assert_eq!(hex.parse::<DeviceId>(), Ok(id));
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{hex}\""));
        assert!("xyz".parse::<DeviceId>().is_err() && hex[..30].parse::<DeviceId>().is_err());
    }
}
