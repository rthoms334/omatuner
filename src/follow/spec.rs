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

/// Weekday mask of Monday–Friday (`FollowSpec::days`).
pub const WEEKDAYS: u8 = 0b001_1111;

/// What a team rule leaves out by default (the design's template: the senior men's side, no
/// highlights).
pub const TEAM_WITHOUT: [&str; 11] = ["Women", "Ladies", "U18", "U19", "U21", "U23", "Legends", "Fan TV", "Highlights", "Academy", "Reserves"];

impl FollowSpec {
    /// The template of `kind` (DESIGN §33 §4): team — fixtures in title, episode title and
    /// description, live only, the senior men's side; show — the title is the show, new
    /// episodes only; event — anywhere in title or episode title, live only; news flash — the
    /// title is the bulletin, every airing, headlines only (`headlines_min`); custom —
    /// anywhere in title or episode title, every airing.
    pub fn template(kind: FollowKind, terms: Vec<String>, headlines_min: u16) -> Self {
        let base = FollowSpec { terms, ..FollowSpec::default() };
        let title_only = Fields { title: true, sub_title: false, description: false, categories: false };
        match kind {
            FollowKind::Team => FollowSpec {
                without: TEAM_WITHOUT.iter().map(|s| (*s).to_owned()).collect(),
                fields: Fields { title: true, sub_title: true, description: true, categories: false },
                shape: Shape::Fixture,
                airings: Airings::LiveOnly,
                ..base
            },
            FollowKind::Show => FollowSpec { fields: title_only, shape: Shape::TitleIs { spin_offs: false }, airings: Airings::NewOnly, ..base },
            FollowKind::Event => FollowSpec { shape: Shape::Anywhere, airings: Airings::LiveOnly, ..base },
            FollowKind::News => FollowSpec {
                fields: title_only,
                shape: Shape::TitleIs { spin_offs: false },
                airings: Airings::Any,
                flash: Some(crate::follow::flash::FlashSpec {
                    length: crate::follow::flash::FlashLength::Headlines(headlines_min.max(1)),
                    volume: crate::follow::flash::FlashVolume::Normal,
                }),
                ..base
            },
            FollowKind::Custom => FollowSpec { airings: Airings::Any, ..base },
        }
    }

    /// Why this spec can't be saved (no name to look for, no field to look in).
    pub fn problem(&self) -> Option<&'static str> {
        let mut buf = String::new();
        let has_term = self.terms.iter().any(|t| {
            crate::textmatch::fold::fold(t, &mut buf);
            !buf.is_empty()
        });
        if !has_term {
            return Some("a rule needs at least one name to look for");
        }
        if !(self.fields.title || self.fields.sub_title || self.fields.description || self.fields.categories) {
            return Some("a rule must look in at least one field");
        }
        None
    }

    /// Runs on local weekday `wd` (0 = Monday) at local minute `minute` (`days`, `between`).
    pub fn admits_time(&self, wd: u8, minute: u16) -> bool {
        let day_ok = self.days == 0 || self.days & (1 << (wd % 7)) != 0;
        let time_ok = match self.between {
            None => true,
            Some((from, to)) if from <= to => (from..to).contains(&minute),
            Some((from, to)) => minute >= from || minute < to,
        };
        day_ok && time_ok
    }
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
