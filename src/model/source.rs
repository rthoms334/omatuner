//! Playlist sources (M3U / Xtream). FROZEN (skeleton).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{SourceId, Timestamp};
use crate::redact::Secret;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// M3U/M3U8 downloaded from a URL (may be an Xtream `get.php`; see onboarding auto-detect).
    M3uUrl,
    /// Local M3U file (re-read on refresh).
    M3uFile,
    /// Xtream Codes `player_api.php` (live + VOD + series + provider EPG).
    Xtream,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::M3uUrl => "m3u-url",
            SourceKind::M3uFile => "m3u-file",
            SourceKind::Xtream => "xtream",
        }
    }
}

/// Preferred container for Xtream live streams (UHF "TS vs M3U8").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LiveFormat {
    /// m3u8 if `allowed_output_formats` contains it, else ts.
    #[default]
    Auto,
    Ts,
    M3u8,
}

/// Theme colour role used to tint a source's badge (resolved against the live palette so it
/// follows the Omarchy theme).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SourceColor {
    #[default]
    Accent,
    Red,
    Yellow,
    Orange,
    Green,
    Cyan,
    Blue,
    Magenta,
    Brown,
    Muted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XtreamCredentials {
    /// Base URL as typed by the user: scheme://host[:port] (never `server_info.url`).
    pub server: String,
    pub username: String,
    pub password: Secret<String>,
}

/// Per-source settings (stored as JSON in `source.settings`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SourceSettings {
    /// HTTP User-Agent for API calls, playlist download and streams (falls back to config).
    pub user_agent: Option<String>,
    pub live_format: LiveFormat,
    /// Hours between automatic refreshes; `None` = config default.
    pub refresh_h: Option<u32>,
    /// IANA tz overriding `server_info.timezone` for catch-up URL times.
    pub server_tz: Option<String>,
    /// Use the provider EPG (Xtream `xmltv.php` / M3U `url-tvg`).
    pub use_provider_epg: Option<bool>,
    /// Override of `config.general.smart_names` for this source.
    pub smart_names: Option<bool>,
    /// Import VOD/series for Xtream (users with live-only interest can disable).
    pub import_vod: Option<bool>,
}

/// Account information from Xtream `user_info`/`server_info` (UHF "server info").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AccountInfo {
    pub status: AccountStatus,
    pub exp_date: Option<Timestamp>,
    pub is_trial: bool,
    pub active_cons: Option<u32>,
    pub max_connections: Option<u32>,
    pub created_at: Option<Timestamp>,
    pub allowed_formats: Vec<String>,
    /// `server_info.timezone` (IANA), as reported.
    pub server_tz: Option<String>,
    /// server `timestamp_now` − local now, seconds (clock skew diagnostics).
    pub clock_skew_s: Option<i64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AccountStatus {
    #[default]
    Unknown,
    Active,
    Expired,
    Banned,
    Disabled,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceCounts {
    pub channels: u32,
    pub movies: u32,
    pub series: u32,
    pub groups: u32,
}

/// A configured playlist source.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub id: SourceId,
    pub name: String,
    pub kind: SourceKind,
    /// Nerd Font glyph chosen by the user (UHF "playlist icon").
    pub icon: String,
    pub color: SourceColor,
    /// Playlist URL for `M3uUrl`. May contain credentials → always display via `redact_url`.
    pub url: Option<Secret<String>>,
    pub path: Option<PathBuf>,
    pub xtream: Option<XtreamCredentials>,
    pub settings: SourceSettings,
    pub enabled: bool,
    pub sort_index: i32,
    pub created_at: Timestamp,
    pub status: SourceStatus,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourceStatus {
    pub last_refresh: Option<Timestamp>,
    pub last_attempt: Option<Timestamp>,
    /// Redacted error message of the last failed refresh.
    pub last_error: Option<String>,
    pub counts: SourceCounts,
    pub account: Option<AccountInfo>,
    /// ETag / Last-Modified of the last playlist download (conditional refresh).
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// What the onboarding wizard / source editor produces. Validated by `jobs::probe_source`.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceDraft {
    pub name: String,
    pub kind: SourceKind,
    pub icon: String,
    pub color: SourceColor,
    pub url: Option<String>,
    pub path: Option<PathBuf>,
    pub xtream: Option<XtreamCredentials>,
    pub settings: SourceSettings,
}
