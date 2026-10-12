//! Spoiler-guard rows (`spoiler_rule`, `spoiler_seen`; DESIGN §34 §3). OWNER: S1.

use serde::{Deserialize, Serialize};

use super::{SpoilerRuleId, Timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpoilerRuleKind {
    Topic,
    Series,
    /// "I'll watch it later": a topic plus an end time; also shields the live event.
    Blackout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SpoilerMode {
    #[default]
    UntilSeen,
    Always,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpoilerRule {
    pub id: SpoilerRuleId,
    pub kind: SpoilerRuleKind,
    pub label: String,
    /// Folded phrases incl. aliases (topic / blackout).
    pub terms: Vec<String>,
    pub series_norm: Option<String>,
    /// Guide-category scope; empty = any.
    pub categories: Vec<String>,
    pub mode: SpoilerMode,
    pub window_h: u32,
    pub until_at: Option<Timestamp>,
    pub sport_descr: bool,
    pub shield: bool,
    /// `false` on a series row = "don't guard" (the user's exception).
    pub enabled: bool,
    pub by_agent: bool,
    pub created_at: Timestamp,
}

/// How an event was seen (`spoiler_seen.how`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeenHow {
    Live,
    Catchup,
    Vod,
    Marked,
    Agent,
}

/// One `spoiler_seen` row: a guarded event (or series airing) the user has seen. Keyed by
/// `(event_key, first_start)` (B3, shared with Following's ledger); an airing whose key is equal
/// and whose start is within [`SEEN_SPAN_S`] of `first_start` counts as the same event, so one
/// "seen" covers repeats, +1 channels and both playlists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenRow {
    pub event_key: String,
    pub first_start: Timestamp,
    /// The programme's title as the guide wrote it (Settings ▸ "Seen recently", Undo toasts).
    pub title: String,
    /// `normalize_name` of the series when the airing carried S/E (lifts series boundaries).
    pub series_norm: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub seen_at: Timestamp,
    pub how: SeenHow,
}

/// Airings of one event key this close to a seen row's `first_start` are the same event.
pub const SEEN_SPAN_S: i64 = 7 * 86_400;
