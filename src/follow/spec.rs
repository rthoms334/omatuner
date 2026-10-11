//! `FollowSpec`: what a rule matches (`follow_rule.spec`, JSON `{"v":1,…}`). OWNER: F-AB.

use serde::{Deserialize, Serialize};

use crate::model::{FavListId, ItemRef, Quality, SourceId};

/// The template a rule was made from (fills the spec's defaults; `follow_rule.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FollowKind {
    /// Fixtures of a team ("Arsenal: every match").
    Team,
    /// New episodes of a show.
    Show,
    /// A competition or event ("Formula 1 sessions").
    Event,
    #[default]
    Custom,
    /// News flash: a daily / weekday news bulletin that pops up in the pop-out.
    News,
}

/// What a match does (`follow_rule.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FollowMode {
    #[default]
    Notify,
    Switch,
    /// Only listed under Upcoming, no reminder.
    List,
    /// News flash: plays in the pop-out, then closes and restores (see `follow::flash`).
    Flash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Airings {
    #[default]
    LiveOnly,
    LiveOrNew,
    NewOnly,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", tag = "t")]
pub enum Shape {
    /// "Arsenal v …" / "… v Arsenal" (`textmatch::fixture`).
    Fixture,
    /// The title is the term (spin-offs optional: "Taskmaster NZ").
    TitleIs { spin_offs: bool },
    #[default]
    Anywhere,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", tag = "t", content = "v")]
pub enum ChannelScope {
    /// Any channel the user can watch (never explicit, never hidden).
    #[default]
    Any,
    Favourites,
    List(FavListId),
    Playlist(SourceId),
    Channels(Vec<ItemRef>),
}

/// Channel preference beyond the shared ranking (`store::copies::rank_channels`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Prefer {
    pub pin: Option<ItemRef>,
    pub max_quality: Option<Quality>,
}

/// Where the terms are looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fields {
    pub title: bool,
    pub sub_title: bool,
    pub description: bool,
    pub categories: bool,
}

impl Default for Fields {
    fn default() -> Self {
        Self { title: true, sub_title: true, description: false, categories: false }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FollowSpec {
    /// Format version (1).
    pub v: u32,
    /// Any of these (names and aliases).
    pub terms: Vec<String>,
    /// Also needs one of these ("Qualifying", "Race").
    pub with_any: Vec<String>,
    pub without: Vec<String>,
    pub fields: Fields,
    pub shape: Shape,
    /// Guide categories, any of (Sport, Football…); empty = any.
    pub categories: Vec<String>,
    pub airings: Airings,
    pub include_highlights: bool,
    /// Weekday mask (bit 0 = Monday); 0 = every day.
    pub days: u8,
    /// Local minutes after midnight `[from, to)`.
    pub between: Option<(u16, u16)>,
    pub channels: ChannelScope,
    pub prefer: Prefer,
    /// News flash only.
    pub flash: Option<crate::follow::flash::FlashSpec>,
}

impl Default for FollowSpec {
    fn default() -> Self {
        Self {
            v: 1,
            terms: Vec::new(),
            with_any: Vec::new(),
            without: Vec::new(),
            fields: Fields::default(),
            shape: Shape::default(),
            categories: Vec::new(),
            airings: Airings::default(),
            include_highlights: false,
            days: 0,
            between: None,
            channels: ChannelScope::default(),
            prefer: Prefer::default(),
            flash: None,
        }
    }
}
