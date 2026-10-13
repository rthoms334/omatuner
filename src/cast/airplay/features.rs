// Ported from pyatv pyatv/protocols/airplay/utils.py @ b277a4c8 (MIT, © 2020 Pierre Ståhl).
//! What a receiver is, from its mDNS TXT record or `GET /info` (DESIGN §40 §6, §7.6). OWNER:
//! CAST-1. `features`/`ft` as `lo,hi` 32-bit words; `flags`/`sf` (TXT 0x18644 and `/info`
//! statusFlags 0x118644 both decode). Video-capable: bit 0 or bit 49 (pyatv's gate). Pairing
//! required: `flags & (0x200 | 0x8)` (never 0x8 alone). A Mac (`act=2`, `^Mac\d+,\d+$`): listed,
//! greyed. `acl=1`: "Same Home only". A non-`AppleTV` model with bit 33: a third-party TV. Versions
//! (`osvers`, `srcvers`, `osBuildVersion`, `sourceVersion`) are for logs, `diagnose` and Settings
//! only. Every string is device data: sanitized by the caller (`cast::sanitize_name`).
//!
//! Parsing follows pyatv `protocols/airplay/utils.py` (`parse_features`, `_get_flags`,
//! `get_pairing_requirement`, `UNSUPPORTED_MODELS`) @ b277a4c8 (MIT): `features` is
//! `0xLO[,0xHI]` (1–8 hex digits each); flags are hex with or without `0x`. TXT keys match
//! case-insensitively. `/info` carries `features` as an integer (or pyatv's string form) and
//! `statusFlags` as an integer. Unparseable values read as 0, so a receiver with garbage
//! features is simply not a video receiver (pyatv raises instead).
//!
//! The order of [`classify`]: not video-capable → not listed; a Mac (`act=2` or a `Mac` model)
//! → greyed "Can't cast to Macs"; an Apple TV (`model` starting `AppleTV`) → usable; any other
//! model → a third-party TV (`ReceiverKind::AirPlayTv`, cast with the third-party profile),
//! listed only with features bit 33 (the play queue). `acl=1` greys an otherwise usable receiver
//! as "Same Home only".

use super::bplist;
use crate::cast::{DeviceId, ReceiverKind, Usability};

/// Bit 0: AirPlay video v1 (pyatv `SupportsAirPlayVideoV1`).
pub const VIDEO_V1: u64 = 1 << 0;
/// Bit 33: the video play queue (pyatv `SupportsAirPlayVideoPlayQueue`), what a third-party
/// TV needs for `/command` (VioletRelay).
pub const VIDEO_PLAY_QUEUE: u64 = 1 << 33;
/// Bit 49: AirPlay video v2 (pyatv `SupportsAirPlayVideoV2`; the Hisense has only this one).
pub const VIDEO_V2: u64 = 1 << 49;
/// Status flag: a PIN is required (pyatv `PIN_REQUIRED`).
pub const FLAG_PIN_REQUIRED: u64 = 0x8;
/// Status flag: pairing is required (pyatv `LEGACY_PAIRING_BIT`).
pub const FLAG_PAIRING: u64 = 0x200;

/// The fields Tuner reads (raw device strings; sanitize before showing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AirPlayInfo {
    pub name: Option<String>,
    pub model: Option<String>,
    /// `deviceid` (a MAC-like id).
    pub device_id: Option<String>,
    /// `pi`: the pairing identity (the stable id when present).
    pub pi: Option<String>,
    /// `psi`: the `channelID` prefix (an Apple TV's `/info`; a third-party TV's TXT record).
    pub psi: Option<String>,
    /// `gid`: the group id (a third-party TV keys `SETPEERSX`'s `ClockPorts` by it, §7.7).
    pub gid: Option<String>,
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

/// A receiver's identifiers as VioletRelay reads them from its TXT record (DESIGN §40 §7.7,
/// `airplay_modern.py` L281–283, facts only): `psi`, else `pi`, is its `receiver_id` — the
/// type-130 `channelID` prefix — and `gid`, else that id, keys `SETPEERSX`'s `ClockPorts`.
/// Discovery keeps them with the device (crate only, like its address); a session falls back to
/// `/info`'s only when the TXT record had none (a TV added by address).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiverIds {
    pub psi: Option<String>,
    pub pi: Option<String>,
    pub gid: Option<String>,
}

impl ReceiverIds {
    pub fn is_empty(&self) -> bool {
        self.psi.is_none() && self.pi.is_none() && self.gid.is_none()
    }

    /// VioletRelay's `receiver_id`: `psi`, else `pi`.
    pub fn receiver_id(&self) -> Option<&str> {
        self.psi.as_deref().or(self.pi.as_deref())
    }

    /// `SETPEERSX`'s `ClockPorts` key: `gid`, else [`ReceiverIds::receiver_id`].
    pub fn clock_key(&self) -> Option<&str> {
        self.gid.as_deref().or_else(|| self.receiver_id())
    }
}

impl AirPlayInfo {
    pub fn ids(&self) -> ReceiverIds {
        ReceiverIds { psi: self.psi.clone(), pi: self.pi.clone(), gid: self.gid.clone() }
    }
}

/// How the picker and the first-run card treat a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    pub kind: ReceiverKind,
    pub pairing_required: bool,
    pub usability: Usability,
}

/// pyatv `parse_features`: `0xLO` or `0xLO,0xHI` → `HI << 32 | LO`; anything else → 0.
fn parse_features(s: &str) -> u64 {
    let word = |w: &str| {
        let hex = w.trim().strip_prefix("0x").or_else(|| w.trim().strip_prefix("0X"))?;
        (1..=8).contains(&hex.len()).then(|| u32::from_str_radix(hex, 16).ok()).flatten()
    };
    match s.split_once(',') {
        None => word(s).map_or(0, u64::from),
        Some((lo, hi)) => match (word(lo), word(hi)) {
            (Some(lo), Some(hi)) => u64::from(hi) << 32 | u64::from(lo),
            _ => 0,
        },
    }
}

/// pyatv `_get_flags`: hex, with or without `0x`; anything else → 0.
fn parse_flags(s: &str) -> u64 {
    let s = s.trim();
    u64::from_str_radix(s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s), 16).unwrap_or(0)
}

/// From TXT key/value pairs.
pub fn from_txt(txt: &[(String, String)]) -> AirPlayInfo {
    let get = |key: &str| txt.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    let num = |key: &str| get(key).and_then(|v| v.trim().parse().ok());
    AirPlayInfo {
        // the name is the mDNS instance name, not a TXT key (the caller fills it)
        name: None,
        model: get("model"),
        device_id: get("deviceid"),
        pi: get("pi"),
        psi: get("psi"),
        gid: get("gid"),
        features: get("features").or_else(|| get("ft")).map_or(0, |f| parse_features(&f)),
        flags: get("flags").or_else(|| get("sf")).map_or(0, |f| parse_flags(&f)),
        act: num("act"),
        acl: num("acl"),
        os_version: get("osvers"),
        os_build: None,
        source_version: get("srcvers"),
    }
}

/// From the `/info` plist.
pub fn from_info(plist: &bplist::Value) -> AirPlayInfo {
    use bplist::Value;
    let string = |key: &str| match plist.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let int = |key: &str| match plist.get(key) {
        Some(Value::Int(i)) => Some(*i as u64),
        Some(Value::String(s)) => Some(parse_flags(s)),
        _ => None,
    };
    let features = match plist.get("features") {
        Some(Value::Int(i)) => *i as u64,
        Some(Value::String(s)) => parse_features(s),
        _ => 0,
    };
    AirPlayInfo {
        name: string("name"),
        model: string("model"),
        device_id: string("deviceID"),
        pi: string("pi"),
        psi: string("psi"),
        gid: string("gid"),
        features,
        flags: int("statusFlags").or_else(|| int("flags")).unwrap_or(0),
        act: int("act").and_then(|v| u32::try_from(v).ok()),
        acl: int("acl").and_then(|v| u32::try_from(v).ok()),
        // `/info` has no marketing version (only the build); TXT `osvers` carries it
        os_version: None,
        os_build: string("osBuildVersion"),
        source_version: string("sourceVersion"),
    }
}

/// pyatv `UNSUPPORTED_MODELS`: `^Mac\d+,\d+$`.
fn is_mac_model(model: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    model.strip_prefix("Mac").and_then(|rest| rest.split_once(',')).is_some_and(|(major, minor)| digits(major) && digits(minor))
}

/// `None`: not a video receiver (not listed).
pub fn classify(info: &AirPlayInfo) -> Option<Classified> {
    if info.features & (VIDEO_V1 | VIDEO_V2) == 0 {
        return None;
    }
    let pairing_required = info.flags & (FLAG_PAIRING | FLAG_PIN_REQUIRED) != 0;
    let model = info.model.as_deref().unwrap_or_default();
    let mac = info.act == Some(2) || is_mac_model(model);
    let kind = if mac || model.starts_with("AppleTV") {
        ReceiverKind::AppleTv
    } else if info.features & VIDEO_PLAY_QUEUE != 0 {
        ReceiverKind::AirPlayTv
    } else {
        return None;
    };
    let usability = if mac {
        Usability::Mac
    } else if info.acl == Some(1) {
        Usability::HomeOnly
    } else {
        Usability::Usable
    };
    Some(Classified { kind, pairing_required, usability })
}

/// `DeviceId::derive("airplay", pi-or-deviceid)`; `None` without either.
pub fn device_id(info: &AirPlayInfo) -> Option<DeviceId> {
    info.pi.as_deref().or(info.device_id.as_deref()).map(|id| DeviceId::derive(crate::cast::Protocol::AirPlay.as_str(), id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt(fixture: &str) -> Vec<(String, String)> {
        fixture.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
    }

    fn info(fixture: &[u8]) -> AirPlayInfo {
        from_info(&bplist::decode(fixture).unwrap())
    }

    #[test]
    fn the_apple_tv_is_a_paired_video_receiver_from_either_source() {
        let t = from_txt(&txt(include_str!("../../../tests/fixtures/cast/txt_appletv_27_0.txt")));
        assert_eq!(t.features, 0x3C175FDE_4A7FDFD5);
        assert_eq!(t.flags, 0x18644);
        assert_eq!((t.os_version.as_deref(), t.source_version.as_deref()), (Some("27.0"), Some("980.77.2")));
        let i = info(include_bytes!("../../../tests/fixtures/cast/info_appletv_26_6.plist"));
        assert_eq!(i.flags, 0x118644, "/info statusFlags has bit 20 set, TXT flags don't");
        assert_eq!((i.os_build.as_deref(), i.source_version.as_deref()), (Some("23L773"), Some("960.13.1")));
        assert_eq!(i.psi.as_deref(), Some("00000000-0000-4000-8000-000000000105"));
        let expected = Classified { kind: ReceiverKind::AppleTv, pairing_required: true, usability: Usability::Usable };
        assert_eq!(classify(&t), Some(expected));
        assert_eq!(classify(&i), Some(expected));
        assert_eq!(device_id(&t), device_id(&i), "the same TV whichever way it was found");
    }

    /// The Hisense's TXT `features=0x7F8AD0,0x38BCF46` is the low word, then bits 32–63: bits 49
    /// and 33 without bit 0. `flags=0x244` has the pairing bit, `acl=0` lets anyone cast.
    #[test]
    fn the_hisense_is_a_usable_third_party_tv_with_bits_49_and_33_only() {
        let t = from_txt(&txt(include_str!("../../../tests/fixtures/cast/txt_hisense.txt")));
        let i = info(include_bytes!("../../../tests/fixtures/cast/info_hisense.plist"));
        assert_eq!((t.features, t.flags, t.acl), (0x038BCF46_007F8AD0, 0x244, Some(0)));
        for r in [&t, &i] {
            assert_eq!(r.features & VIDEO_V1, 0);
            assert_ne!(r.features & VIDEO_V2, 0);
            assert_ne!(r.features & VIDEO_PLAY_QUEUE, 0);
            let c = classify(r).unwrap();
            assert_eq!((c.kind, c.pairing_required, c.usability), (ReceiverKind::AirPlayTv, true, Usability::Usable));
        }
        assert_eq!(i.psi, None, "a third-party TV's /info has no psi");
        assert_eq!(device_id(&t), device_id(&i));
        // VioletRelay's identifiers are the TXT record's (`/info` has only `pi`)
        let psi = "00000000-0000-0000-0000-020000000070";
        assert_eq!((t.ids().receiver_id(), t.ids().clock_key()), (Some(psi), Some(psi)));
        assert_eq!(i.ids().receiver_id(), Some("02:00:00:00:00:70"));
    }

    #[test]
    fn macs_home_only_and_non_video_receivers() {
        let base = AirPlayInfo { model: Some("AppleTV14,1".into()), features: VIDEO_V1, flags: 0x8, ..Default::default() };
        // 0x8 alone still means pairing; neither bit: not needed
        assert!(classify(&base).unwrap().pairing_required);
        assert!(!classify(&AirPlayInfo { flags: 0x4, ..base.clone() }).unwrap().pairing_required);
        let mac = AirPlayInfo { model: Some("Mac14,7".into()), ..base.clone() };
        assert_eq!(classify(&mac).unwrap().usability, Usability::Mac);
        let act2 = AirPlayInfo { act: Some(2), ..base.clone() };
        assert_eq!(classify(&act2).unwrap().usability, Usability::Mac);
        assert_eq!(classify(&AirPlayInfo { acl: Some(1), ..base.clone() }).unwrap().usability, Usability::HomeOnly);
        assert!(!is_mac_model("MacBookPro18,1") && !is_mac_model("Mac14,") && is_mac_model("Mac15,12"));
        // a HomePod (audio only) and a third-party TV without the play queue: not listed
        assert_eq!(classify(&AirPlayInfo { model: Some("AudioAccessory5,1".into()), features: 1 << 9, ..Default::default() }), None);
        assert_eq!(classify(&AirPlayInfo { model: Some("43A5FE".into()), features: VIDEO_V2, ..Default::default() }), None);
    }

    #[test]
    fn feature_and_flag_strings_parse_like_pyatv() {
        assert_eq!(parse_features("0x5A7FFFF7,0x1E"), 0x1E_5A7FFFF7);
        assert_eq!(parse_features("0x4A7FCA00"), 0x4A7FCA00);
        assert_eq!(parse_features("0x123456789"), 0, "nine digits");
        assert_eq!(parse_features("5A7FFFF7"), 0, "no 0x");
        assert_eq!((parse_flags("0x244"), parse_flags("244"), parse_flags("junk")), (0x244, 0x244, 0));
        let pyatv_fake = bplist::Value::Dict(vec![("features".into(), bplist::Value::String("0x4A7FCA00,0x3C356BD0".into()))]);
        assert_eq!(from_info(&pyatv_fake).features, 0x3C356BD0_4A7FCA00);
    }
}
