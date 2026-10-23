//! New episodes' data (DESIGN §44 §4): why a title is tracked, what arrived, the snapshot that
//! Home, Watchlists, the sidebar, the series page, Settings and agents read, and the note a
//! notification (or its toast) shows. Plain data; `store::new_episodes` reads and writes it.

use serde::{Deserialize, Serialize};

use crate::model::{ArrivalId, ItemRef, SeriesId, Timestamp, WatchlistId};

/// Why a title is tracked (§3.1). Settings ▸ New episodes and the agent's `tracked` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Why {
    /// A series entry of watchlist `list` holds one of its copies; `from`/`to` is the entry's
    /// season range (`None`: the whole series).
    List { list: WatchlistId, name: String, from: Option<u32>, to: Option<u32> },
    /// `finished` of the `known` episodes of `season` are watched (over the binge share).
    Binged { season: u32, finished: u32, known: u32 },
}

impl Why {
    /// The stored form (`newep_arrival.why`): `list:3`, `binge`.
    pub fn code(&self) -> String {
        match self {
            Why::List { list, .. } => format!("list:{}", list.0),
            Why::Binged { .. } => "binge".into(),
        }
    }

    /// What Settings and tiles say: "My list", "My list (S1–S3)", "binged (S4: 7 of 8)".
    pub fn label(&self) -> String {
        match self {
            Why::List { name, from: Some(a), to: Some(b), .. } if a == b => format!("{name} (S{a})"),
            Why::List { name, from: Some(a), to: Some(b), .. } => format!("{name} (S{a}–S{b})"),
            Why::List { name, .. } => name.clone(),
            Why::Binged { season, finished, known } => format!("binged (S{season}: {finished} of {known})"),
        }
    }

    pub fn is_list(&self) -> bool {
        matches!(self, Why::List { .. })
    }
}

/// The seasons a tracked title alerts for (§3.1): a whole-series entry or a binge covers seasons
/// ≥ 1; a range `S a–b` covers `a..=b`, and also every later season when `b` reached the newest
/// season the title had when the entry started tracking it (open-ended; decided once, so a
/// new season keeps alerting after its first episode). Season 0 only when a range names it.
/// Several entries or reasons give the union.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// Seasons ≥ 1.
    pub whole: bool,
    /// Season ranges, inclusive.
    pub ranges: Vec<SeasonRange>,
}

/// A list entry's season range and when the entry was added (`watchlist_item.added_at`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeasonRange {
    pub from: u32,
    pub to: u32,
    pub added_at: Timestamp,
}

impl Coverage {
    /// Add an entry's range (`None`: the whole series), added to its list at `added_at`.
    pub fn add(&mut self, range: Option<(u32, u32)>, added_at: Timestamp) {
        match range {
            None => self.whole = true,
            Some((from, to)) => match self.ranges.iter_mut().find(|r| (r.from, r.to) == (from, to)) {
                // the earlier entry decides: it tracked the title first
                Some(r) => r.added_at = r.added_at.min(added_at),
                None => self.ranges.push(SeasonRange { from, to, added_at }),
            },
        }
    }

    /// Does an arrival in `season` alert? `newest_at(t)`: the newest season the title's ledger
    /// had at `t`, counting its baseline whenever `t` is (`None`: nothing known yet, so the
    /// range is open-ended).
    pub fn covers(&self, season: u32, newest_at: impl Fn(Timestamp) -> Option<u32>) -> bool {
        if self.whole && season >= 1 {
            return true;
        }
        self.ranges.iter().any(|r| (r.from..=r.to).contains(&season) || (season > r.to && newest_at(r.added_at).is_none_or(|n| r.to >= n)))
    }
}

/// One tracked title (§3.1): its visible copies, why it's tracked and what it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Tracked {
    /// The display name of its top copy (never logged at INFO).
    pub title: String,
    /// Its series copies (a stack, or one series); the first is the one a tile shows.
    pub copies: Vec<ItemRef>,
    /// The same copies' row ids (`copies::{episodes, next_episode, episode_progress}`).
    pub series: Vec<SeriesId>,
    pub why: Vec<Why>,
    pub coverage: Coverage,
    /// "Stop alerts for this series" (Q1): still tracked (its ledger keeps up), never alerted.
    pub muted: bool,
}

impl Tracked {
    /// On a watchlist (the sidebar dot and the entry chip are about these only).
    pub fn on_list(&self) -> bool {
        self.why.iter().any(Why::is_list)
    }
}

/// Why an arrival was recorded without a notification (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quiet {
    /// Following already reminded about, switched to or recorded its airing ("aired on TV").
    Following,
    /// The spoiler guard saw it (live, catch-up or marked): recorded as watched.
    Seen,
}

impl Quiet {
    pub fn as_str(self) -> &'static str {
        match self {
            Quiet::Following => "following",
            Quiet::Seen => "seen",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "following" => Some(Quiet::Following),
            "seen" => Some(Quiet::Seen),
            _ => None,
        }
    }
}

/// How an arrival left the Home row (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleared {
    /// Finished in any copy (derived when a snapshot is built, written back by the next check).
    Watched,
    /// Delete on the tile or the menu (Undo for 10 s).
    Dismissed,
    /// Older than `[new_episodes] home_days` (Q3).
    Expired,
}

impl Cleared {
    pub fn as_str(self) -> &'static str {
        match self {
            Cleared::Watched => "watched",
            Cleared::Dismissed => "dismissed",
            Cleared::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "watched" => Some(Cleared::Watched),
            "dismissed" => Some(Cleared::Dismissed),
            "expired" => Some(Cleared::Expired),
            _ => None,
        }
    }
}

/// One `newep_arrival` row: a title episode found new.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Arrival {
    pub id: ArrivalId,
    /// The series copy it arrived in first; `None` once that playlist is deleted.
    pub series: Option<ItemRef>,
    pub season: u32,
    pub episode: u32,
    /// Display only (the title's name when it arrived).
    pub series_title: String,
    /// [`Why::code`]s when it arrived.
    pub why: Vec<String>,
    pub arrived_at: Timestamp,
    pub notified_at: Option<Timestamp>,
    pub quiet: Option<Quiet>,
    pub opened_at: Option<Timestamp>,
    pub cleared_at: Option<Timestamp>,
    pub cleared_how: Option<Cleared>,
}

/// One new episode of a [`SeriesNews`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewEpisode {
    pub arrival: ArrivalId,
    pub season: u32,
    pub episode: u32,
    /// The episode to play or download: in the copy it arrived in, else in any copy.
    pub episode_ref: Option<ItemRef>,
    pub arrived_at: Timestamp,
    pub quiet: Option<Quiet>,
    /// Its series page or watchlist entry was opened since (the chip went).
    pub opened: bool,
    /// Started, not finished: the share played (the tile's progress bar).
    pub progress: Option<f32>,
}

/// One title with new episodes not yet watched or dismissed: a Home tile, an agent's `list`
/// entry. Locked, hidden and explicit titles never get one.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesNews {
    pub title: String,
    /// Its copies; the first is the tile's.
    pub copies: Vec<ItemRef>,
    pub series: Vec<SeriesId>,
    /// Why it's tracked now (empty: no longer tracked, the arrivals stay until cleared).
    pub why: Vec<Why>,
    /// In (season, episode) order.
    pub new: Vec<NewEpisode>,
    /// The newest arrival (the row's order: newest first).
    pub newest_at: Timestamp,
}

impl SeriesNews {
    /// On a watchlist now.
    pub fn on_list(&self) -> bool {
        self.why.iter().any(Why::is_list)
    }

    /// The Watchlists chip ("● New · S2E9–E10"): an arrival not opened yet.
    pub fn unopened(&self) -> bool {
        self.new.iter().any(|e| !e.opened)
    }

    /// The tile's second line: "S2E9–E10", "S2E9 · S3E1".
    pub fn span(&self) -> String {
        span(&self.new.iter().map(|e| (e.season, e.episode)).collect::<Vec<_>>())
    }
}

/// What Home, Watchlists, the sidebar, the series page, Settings and agents read (rebuilt on
/// `DataScope::{NewEpisodes, Progress, Watchlists, Vod, VodDetails}` and on a library swap,
/// never per frame).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewEpisodesSnapshot {
    /// Newest arrival first.
    pub series: Vec<SeriesNews>,
    /// The sidebar's dot on Watchlists: a list series has an arrival newer than the last time
    /// the Watchlists screen was opened.
    pub list_dot: bool,
    /// When the last check finished.
    pub checked_at: Option<Timestamp>,
}

impl NewEpisodesSnapshot {
    /// The news of the title holding copy `item` (a series page, a watchlist entry).
    pub fn of(&self, item: &ItemRef) -> Option<&SeriesNews> {
        self.series.iter().find(|s| s.copies.contains(item))
    }

    /// New episodes in all (the agent's `status`).
    pub fn episodes(&self) -> usize {
        self.series.iter().map(|s| s.new.len()).sum()
    }
}

/// "S2E9–E10", "S2E8, E10", "S2E9 · S3E1": per season, a run of numbers as a span.
pub fn span(eps: &[(u32, u32)]) -> String {
    let mut eps = eps.to_vec();
    eps.sort_unstable();
    eps.dedup();
    let mut parts = Vec::new();
    for season in eps.iter().map(|e| e.0).collect::<std::collections::BTreeSet<_>>() {
        let nums: Vec<u32> = eps.iter().filter(|e| e.0 == season).map(|e| e.1).collect();
        let (first, last) = (nums[0], nums[nums.len() - 1]);
        parts.push(match nums.len() {
            1 => format!("S{season}E{first}"),
            n if (last - first) as usize + 1 == n => format!("S{season}E{first}–E{last}"),
            _ => format!("S{season}E{}", nums.iter().map(u32::to_string).collect::<Vec<_>>().join(", E")),
        });
    }
    parts.join(" · ")
}

/// A notification about new episodes, or its in-app toast when no notification server answers
/// (§2.2). Its text went through the spoiler guard already.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// "Landman: 2 new episodes", "New episodes in 6 series".
    pub summary: String,
    /// "S2E9, S2E10 · on TV", "Landman, Slow Horses, The Bear and 3 more".
    pub body: String,
    /// What the body click and the first button do.
    pub action: NoteAction,
    /// The episodes **Download** queues (any copy); empty: no Download button.
    pub download: Vec<ItemRef>,
}

impl Note {
    /// One line for the toast.
    pub fn toast_text(&self) -> String {
        if self.body.is_empty() { self.summary.clone() } else { format!("{} — {}", self.summary, self.body) }
    }
}

/// The first action of a [`Note`].
#[derive(Debug, Clone, PartialEq)]
pub enum NoteAction {
    /// "Play S2E9": the series' next unwatched episode (Q4), through `series`' copy plan.
    Play { label: String, series: ItemRef, episode: ItemRef },
    /// The summary's Open: Home with the New episodes row.
    Open,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_as_runs_per_season() {
        assert_eq!(span(&[(2, 10), (2, 9)]), "S2E9–E10");
        assert_eq!(span(&[(2, 9), (3, 1)]), "S2E9 · S3E1");
        assert_eq!(span(&[(2, 8), (2, 10)]), "S2E8, E10");
        assert_eq!(span(&[(5, 1)]), "S5E1");
    }
}
