//! Live TV: groups, channels, stream locators, catch-up specs. FROZEN (skeleton).

use serde::{Deserialize, Serialize};

use super::{ChannelId, EpgChannelId, FavListId, GroupId, GroupKey, ItemKey, ItemKind, SourceId, Timestamp};

/// A category of channels, movies or series inside one source (M3U `group-title`, Xtream
/// category). User overrides (rename/hide/lock/order) are merged in by the store.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub id: GroupId,
    pub source_id: SourceId,
    /// Channel, Movie or Series.
    pub kind: ItemKind,
    pub key: GroupKey,
    /// Provider name, as imported (after smart cleanup if enabled).
    pub name: String,
    /// User rename, if any.
    pub rename: Option<String>,
    /// Position in the provider's ordering.
    pub provider_order: i32,
    /// User ordering (lower first); `None` = provider order.
    pub user_order: Option<i32>,
    /// Effective: the user's hide, or the source's category filter (see `filter`).
    pub hidden: bool,
    /// The user's own hide (`GroupOverride::hidden`), whatever the rule does.
    pub user_hidden: bool,
    /// PIN-locked (parental).
    pub locked: bool,
    pub item_count: u32,
    /// Xtream category id, if any.
    pub provider_id: Option<String>,
    /// Country / language tag of the provider's raw category name (DESIGN §28.2).
    pub tag: Option<super::CategoryTag>,
    /// What the source's category filter does to this group (DESIGN §28.3).
    pub filter: super::GroupFilter,
}

impl Group {
    pub fn display_name(&self) -> &str {
        self.rename.as_deref().unwrap_or(&self.name)
    }
}

/// Extra HTTP request properties for a stream (M3U `#EXTVLCOPT`, `#KODIPROP`, pipe headers,
/// source User-Agent). Passed to mpv as per-file options.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpHeaders {
    pub user_agent: Option<String>,
    pub referrer: Option<String>,
    pub origin: Option<String>,
    /// Additional `Name: value` headers (cookies, auth tokens...). Values are secrets for logs.
    pub extra: Vec<(String, String)>,
}

impl HttpHeaders {
    pub fn is_empty(&self) -> bool {
        self.user_agent.is_none() && self.referrer.is_none() && self.origin.is_none() && self.extra.is_empty()
    }
}

/// Xtream stream family, for URL building.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XtreamStreamKind {
    Live,
    Movie,
    Series,
}

/// Where a stream lives. Xtream URLs are built at play time from the source credentials so
/// passwords are never duplicated into catalogue rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum StreamLocator {
    /// Literal URL from an M3U (may embed tokens — redact when displaying).
    Url { url: String },
    /// Xtream stream; `ext` = container extension for movie/series, ignored for live.
    Xtream { kind: XtreamStreamKind, stream_id: i64, ext: Option<String> },
}

/// Catch-up flavour (M3U `catchup=` / Xtream `tv_archive`). See DESIGN §10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CatchupKind {
    /// `catchup-source` is the full URL template (or behaves like Append when missing).
    Default,
    /// `catchup-source` is appended to the live URL.
    Append,
    /// `?utc={utc}&lutc={lutc}` appended (alias `timeshift`).
    Shift,
    /// Flussonic URL rewrite (`flussonic`, `fs`).
    Flussonic,
    FlussonicHls,
    FlussonicTs,
    /// Xtream `timeshift` path (alias `xtream`); also used for native Xtream sources.
    Xc,
    /// `catchup-source` or `{catchup-id}` is a VOD URL.
    Vod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupSpec {
    pub kind: CatchupKind,
    /// `catchup-source` template, if any.
    pub template: Option<String>,
    /// Archive window in days (catchup-days → timeshift → tvg-rec → catchup-time/86400;
    /// Xtream `tv_archive_duration`).
    pub days: u32,
    /// `catchup-correction` in seconds (subtracted from programme start).
    pub correction_s: i64,
}

/// Why a channel is known not to be playable by mpv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unsupported {
    /// Widevine/PlayReady (`#KODIPROP inputstream.adaptive.license_type`).
    Drm,
}

/// A live channel, fully loaded (detail views, playback). Lists use [`ChannelSummary`].
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub id: ChannelId,
    pub source_id: SourceId,
    pub key: ItemKey,
    /// Xtream stream_id (text) or M3U `tvg-id`-less → None.
    pub provider_id: Option<String>,
    /// Raw provider name.
    pub name: String,
    /// Smart-cleaned name (== name when cleanup disabled).
    pub clean_name: String,
    /// User rename.
    pub rename: Option<String>,
    /// `tvg-chno` / Xtream `num`.
    pub number: Option<u32>,
    pub logo: Option<String>,
    /// All groups (M3U `;`-lists / XUI `category_ids`), primary first.
    pub group_ids: Vec<GroupId>,
    pub tvg_id: Option<String>,
    pub tvg_name: Option<String>,
    /// `tvg-shift` in minutes (applied to EPG times for this channel).
    pub tvg_shift_min: i32,
    pub stream: StreamLocator,
    pub headers: HttpHeaders,
    pub catchup: Option<CatchupSpec>,
    /// Per-channel user catch-up offset in seconds (UHF 1.100.1), applied on top of correction.
    pub catchup_offset_s: i64,
    pub radio: bool,
    pub unsupported: Option<Unsupported>,
    pub provider_order: i32,
    pub user_order: Option<i32>,
    pub hidden: bool,
    pub locked: bool,
    pub added_at: Option<Timestamp>,
    /// Resolved EPG channel (manual mapping > automatic), if any.
    pub epg_channel_id: Option<EpgChannelId>,
}

impl Channel {
    pub fn display_name(&self) -> &str {
        self.rename.as_deref().unwrap_or(&self.clean_name)
    }
}

/// Slim, `Clone`-cheap row kept in the in-memory library index for every visible channel.
/// Everything a list row / grid header / search result needs without touching SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelSummary {
    pub id: ChannelId,
    pub source_id: SourceId,
    pub key: ItemKey,
    /// Display name (rename > clean name).
    pub name: std::sync::Arc<str>,
    pub number: Option<u32>,
    pub logo: Option<std::sync::Arc<str>>,
    pub group_ids: Vec<GroupId>,
    pub epg_channel_id: Option<EpgChannelId>,
    pub tvg_shift_min: i32,
    pub has_catchup: bool,
    pub catchup_days: u32,
    pub radio: bool,
    pub locked: bool,
    pub unsupported: bool,
    pub provider_order: i32,
    pub user_order: Option<i32>,
    pub added_at: Option<Timestamp>,
}

/// Ordering choices for lists (UHF users asked for date added / year / rating).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SortMode {
    /// Provider order, with user reordering applied.
    #[default]
    Provider,
    Number,
    NameAsc,
    NameDesc,
    RecentlyAdded,
    /// VOD only.
    Rating,
    /// VOD only.
    Year,
}

/// The ordered set of channels the user is browsing; defines zapping order (UHF: up/down zaps
/// *within the collection you opened the channel from*).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Collection {
    AllChannels,
    Group(GroupId),
    Favourites(FavListId),
    /// Recently watched channels, most recent first.
    History,
    /// Ad-hoc (search results).
    Adhoc(Vec<ChannelId>),
}
