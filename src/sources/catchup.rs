//! Catch-up URL building for every flavour. Owner: WP-05a. DESIGN §10 (semantics follow Kodi
//! pvr.iptvsimple / UniPlayer; re-implemented, not copied — Kodi is GPL-2).
//!
//! `CatchupRequest.start/stop` are the DISPLAYED times (`ProgrammeSlim::shifted` by the
//! channel's tvg-shift); the archive is indexed in EPG time, hence:
//! Effective programme start = `start − correction_s − tvg_shift_min·60 + offset_s(user)`;
//! duration = `stop − start` (3600 if unknown), capped so `start + duration <= now`.
//! Granularity: 1 s if the template uses `{utc}`, `${start}`, `{S}` or `{offset:1}`, else
//! 60 s (start floored to the minute).
//!
//! | kind              | URL |
//! |-------------------|-----|
//! | Default           | expand(`template`) (no template → like Append) |
//! | Append            | live URL + expand(`template`) |
//! | Shift             | live URL + (`?`/`&`) `utc={utc}&lutc={lutc}` |
//! | Flussonic(-Hls/-Ts)| rewrite `…/mpegts` → `…/timeshift_abs-{utc}.ts`, `…/index.m3u8` → `…/timeshift_rel-{offset:1}.m3u8`, `…/mono.m3u8` → `…/mono-timeshift_rel-{offset:1}.m3u8` |
//! | Xc                | `{host}/timeshift/{u}/{p}/{duration:60}/{Y}-{m}-{d}:{H}-{M}/{id}.{ext}` (server-local) |
//! | Vod               | expand(`template`) or `{catchup-id}` |
//!
//! Placeholders: `{utc}` `${start}` `{utcend}` `${end}` `{lutc}` `${now}` `${timestamp}`
//! `{duration}` `${duration}` `{duration:X}` `{offset:X}` `${offset}` `{Y}{m}{d}{H}{M}{S}`
//! (local = server tz), `{utc:FMT}` `${start:FMT}` `{utcend:FMT}` `${end:FMT}` `{lutc:FMT}`
//! `${now:FMT}` (FMT letters Y m d H M S → strftime), `{catchup-id}`.

use crate::error::Result;
use crate::model::{CatchupKind, CatchupSpec, Timestamp};

/// Everything needed to build one catch-up URL.
#[derive(Debug, Clone)]
pub struct CatchupRequest<'a> {
    pub live_url: &'a str,
    pub spec: &'a CatchupSpec,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub now: Timestamp,
    pub tvg_shift_min: i32,
    /// Per-channel user offset (seconds, may be negative).
    pub user_offset_s: i64,
    pub catchup_id: Option<&'a str>,
    /// Timezone for `{Y}{m}{d}{H}{M}{S}` (server tz for Xtream, local tz otherwise).
    pub tz: jiff::tz::TimeZone,
}

/// Build the URL. Errors: `NotFound` when outside the archive window, `Parse` when a
/// Flussonic/XC rewrite regex doesn't match the live URL.
pub fn build_url(req: &CatchupRequest<'_>) -> Result<String> { todo!("WP-05a") }

/// Expand placeholders in `template` (tokenizer over `{…}` and `${…}`; unknown tokens left
/// verbatim).
pub fn expand(template: &str, req: &CatchupRequest<'_>) -> String { todo!("WP-05a") }

/// Is a programme starting at `start` inside the archive window at `now`?
/// (`now - days·86400 <= start < now`.)
pub fn in_window(spec: &CatchupSpec, start: Timestamp, now: Timestamp) -> bool {
    start < now && start >= now - spec.days as i64 * 86_400
}

/// Parse an M3U `catchup=` value (aliases: `catchup-type`; `xtream` → Xc, `timeshift` → Shift,
/// `fs` → Flussonic). Implemented in the skeleton (used by WP-03's M3U parser).
pub fn parse_kind(s: &str) -> Option<CatchupKind> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "default" => CatchupKind::Default,
        "append" => CatchupKind::Append,
        "shift" | "timeshift" => CatchupKind::Shift,
        "flussonic" | "fs" => CatchupKind::Flussonic,
        "flussonic-hls" | "fs-hls" => CatchupKind::FlussonicHls,
        "flussonic-ts" | "fs-ts" => CatchupKind::FlussonicTs,
        "xc" | "xtream" => CatchupKind::Xc,
        "vod" => CatchupKind::Vod,
        _ => return None,
    })
}
