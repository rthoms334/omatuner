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
