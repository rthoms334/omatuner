//! Identifiers. FROZEN (skeleton).

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! row_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
        }
        impl rusqlite::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> { self.0.to_sql() }
        }
        impl rusqlite::types::FromSql for $name {
            fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                i64::column_result(v).map($name)
            }
        }
    };
}

row_id!(/// `source.id`
    SourceId);
row_id!(/// `grp.id` (channel, movie or series category)
    GroupId);
row_id!(/// `channel.id` — stable while the channel's ItemKey is stable.
    ChannelId);
row_id!(/// `movie.id`
    MovieId);
row_id!(/// `series.id`
    SeriesId);
row_id!(/// `episode.id`
    EpisodeId);
row_id!(/// `epg_source.id`
    EpgSourceId);
row_id!(/// `epg_channel.id` — stable per (epg_source, xmltv_id).
    EpgChannelId);
row_id!(/// `programme.id` (rowid; also the FTS rowid). Changes on every EPG import.
    ProgrammeId);
row_id!(/// `fav_list.id`
    FavListId);
row_id!(/// `reminder.id`
    ReminderId);
row_id!(/// `queue_item.id` (Up next, DESIGN §32; batch base).
    QueueId);
row_id!(/// `watchlist.id` (DESIGN §42).
    WatchlistId);
row_id!(/// `watchlist_item.id` (DESIGN §42).
    WatchlistItemId);
row_id!(/// `follow_rule.id` (Following, DESIGN §33; batch base).
    FollowRuleId);
row_id!(/// `spoiler_rule.id` (spoiler guard, DESIGN §34; batch base).
    SpoilerRuleId);
row_id!(/// `guide_fix_log.id` (Guide fixes, DESIGN §31; one logged batch that Undo reverts).
    GuideFixLogId);

/// A picture of the player (multi-view, DESIGN §35): A is the player that always exists, B the
/// second one multi-view starts lazily. The names never change through swaps and layouts
/// (logs, MCP and the key help use them). Not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TileId(pub u8);

impl TileId {
    pub const A: Self = Self(0);
    pub const B: Self = Self(1);

    /// `'A'`, `'B'`, … (corner letters, log prefixes, MCP `tile`).
    pub fn letter(self) -> char {
        (b'A' + self.0.min(25)) as char
    }

    /// The other picture of a two-picture view.
    pub fn other(self) -> Self {
        if self == Self::A { Self::B } else { Self::A }
    }
}

impl fmt::Display for TileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.letter())
    }
}

/// In-process job id (not persisted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// What kind of catalogue item an [`ItemKey`] identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Channel,
    Movie,
    Series,
    Episode,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Channel => "channel",
            ItemKind::Movie => "movie",
            ItemKind::Series => "series",
            ItemKind::Episode => "episode",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "channel" => ItemKind::Channel,
            "movie" => ItemKind::Movie,
            "series" => ItemKind::Series,
            "episode" => ItemKind::Episode,
            _ => return None,
        })
    }
}

/// Stable, source-scoped identity of a catalogue item. Computed by
/// `sources::identity::item_key` at import time. Formats (DESIGN §8):
///
/// * Xtream: `xt:live:<stream_id>`, `xt:vod:<stream_id>`, `xt:series:<series_id>`, `xt:ep:<id>`
/// * M3U:    `m3u:<hash16>` of (tvg-id, normalized name, normalized group) + `#n` on collision
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemKey(pub String);

impl ItemKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ItemKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Group identity inside a source: `<kind>:<normalized group name>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupKey(pub String);

/// Durable reference to an item across refreshes; what user data is keyed by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemRef {
    pub source: SourceId,
    pub kind: ItemKind,
    pub key: ItemKey,
}

/// Secondary identity facts captured when user data is attached to an item. Used to re-attach
/// user data when an item's primary [`ItemKey`] disappears on refresh (DESIGN §8.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Provider id (Xtream stream_id / series_id / episode id) as text.
    pub provider_id: Option<String>,
    /// `tvg-id` / `epg_channel_id`, as-is.
    pub tvg_id: Option<String>,
    /// `sources::identity::normalize_name(name)`.
    pub norm_name: String,
    /// Normalized primary group name.
    pub norm_group: Option<String>,
    /// URL without query/userinfo/credentials path segments (`identity::url_key`).
    pub url_key: Option<String>,
}
