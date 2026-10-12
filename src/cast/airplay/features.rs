//! What a receiver is, from its mDNS TXT record or `GET /info` (DESIGN §40 §6, §7.6). OWNER:
//! CAST-1. `features`/`ft` as `lo,hi` 32-bit words; `flags`/`sf` (TXT 0x18644 and `/info`
//! statusFlags 0x118644 both decode). Video-capable: bit 0 or bit 49 (pyatv's gate). Pairing
//! required: `flags & (0x200 | 0x8)` (never 0x8 alone). A Mac (`act=2`, `^Mac\d+,\d+$`): listed,
//! greyed. `acl=1`: "Same Home only". A non-`AppleTV` model with bit 33: experimental. Versions
//! (`osvers`, `srcvers`, `osBuildVersion`, `sourceVersion`) are for logs, `diagnose` and Settings
//! only. Every string is device data: sanitized by the caller (`cast::sanitize_name`).

use super::bplist;
use crate::cast::{DeviceId, ReceiverKind, Usability};

/// The fields Tuner reads (raw device strings; sanitize before showing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AirPlayInfo {
    pub name: Option<String>,
    pub model: Option<String>,
    /// `deviceid` (a MAC-like id).
    pub device_id: Option<String>,
    /// `pi`: the pairing identity (the stable id when present).
    pub pi: Option<String>,
    /// `psi` (`/info`): the `channelID` prefix.
    pub psi: Option<String>,
    pub features: u64,
    pub flags: u64,
    pub act: Option<u32>,
    pub acl: Option<u32>,
    /// TXT `osvers` ("26.6").
    pub os_version: Option<String>,
    /// `/info` `osBuildVersion` ("23L773").
    pub os_build: Option<String>,
    /// TXT `srcvers` / `/info` `sourceVersion` ("960.13.1").
    pub source_version: Option<String>,
}

/// How the picker and the first-run card treat a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    pub kind: ReceiverKind,
    pub pairing_required: bool,
    pub usability: Usability,
    pub experimental: bool,
}

/// From TXT key/value pairs.
pub fn from_txt(_txt: &[(String, String)]) -> AirPlayInfo {
    // CAST-1
    AirPlayInfo::default()
}

/// From the `/info` plist.
pub fn from_info(_plist: &bplist::Value) -> AirPlayInfo {
    // CAST-1
    AirPlayInfo::default()
}

/// `None`: not a video receiver (not listed).
pub fn classify(_info: &AirPlayInfo) -> Option<Classified> {
    // CAST-1
    None
}

/// `DeviceId::derive("airplay", pi-or-deviceid)`; `None` without either.
pub fn device_id(info: &AirPlayInfo) -> Option<DeviceId> {
    info.pi.as_deref().or(info.device_id.as_deref()).map(|id| DeviceId::derive(crate::cast::Protocol::AirPlay.as_str(), id))
}
