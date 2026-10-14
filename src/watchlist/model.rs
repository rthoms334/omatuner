//! Watchlists' stored rows and visible, derived state (DESIGN §42 §3–§5).

use crate::model::{Fingerprint, ItemKind, ItemRef, Timestamp, WatchlistId, WatchlistItemId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PlayOrder {
    #[default]
    #[serde(rename = "ordered")]
    Ordered,
    #[serde(rename = "any")]
    Any,
}
impl PlayOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ordered => "ordered",
            Self::Any => "any",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct List {
    pub id: WatchlistId,
    pub name: String,
    pub note: Option<String>,
    pub play_order: PlayOrder,
    pub sort_index: i64,
    pub since: Timestamp,
    pub is_default: bool,
    pub created_at: Timestamp,
    pub created_by: String,
    pub last_played_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: WatchlistItemId,
    pub list_id: WatchlistId,
    pub position: usize,
    pub kind: ItemKind,
    pub item: Option<ItemRef>,
    pub any_copy: bool,
    pub season_from: Option<u32>,
    pub season_to: Option<u32>,
    pub title: String,
    pub year: Option<u16>,
    /// For episodes, year and TMDB id identify the parent series.
    pub tmdb_id: Option<i64>,
    #[serde(default)]
    pub series_title: Option<String>,
    #[serde(default)]
    pub season: Option<u32>,
    #[serde(default)]
    pub episode: Option<u32>,
    pub skipped: bool,
    pub added_at: Timestamp,
    pub fingerprint: Fingerprint,
    pub orphaned_since: Option<Timestamp>,
    /// Last known explicit status, kept when a deleted playlist leaves a placeholder.
    #[serde(default)]
    pub explicit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryStatus {
    Done,
    Next,
    Todo,
    Skipped,
    Missing,
    Locked,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Progress {
    pub position_s: f64,
    pub duration_s: Option<f64>,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntryState {
    pub entry: Entry,
    pub status: EntryStatus,
    /// The next film or episode. A series with unloaded details retains its series ref.
    pub playable: Option<ItemRef>,
    pub episode: Option<(u32, u32)>,
    /// Parent title for the episode spoiler guard, never a plot.
    pub series: Option<String>,
    pub progress: Option<Progress>,
    pub duration_s: Option<f64>,
    pub done_episodes: usize,
    pub total_episodes: usize,
    pub locked: bool,
    pub copy_name: Option<String>,
}
impl EntryState {
    pub fn done(&self) -> bool {
        matches!(self.status, EntryStatus::Done | EntryStatus::Skipped)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PassedOver {
    pub id: WatchlistItemId,
    pub title: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListView {
    pub list: List,
    pub entries: Vec<EntryState>,
    pub next: Option<WatchlistItemId>,
    pub done: usize,
    pub total: usize,
    pub remaining_s: Option<f64>,
    pub passed_over: Vec<PassedOver>,
    pub show_watched: bool,
}
impl ListView {
    pub fn finished(&self) -> bool {
        self.total > 0 && self.done == self.total
    }
    pub fn next_entry(&self) -> Option<&EntryState> {
        self.next.and_then(|id| self.entries.iter().find(|e| e.entry.id == id))
    }
    pub fn place(&self, id: WatchlistItemId) -> Option<(usize, usize)> {
        self.entries.iter().position(|e| e.entry.id == id).map(|n| (n + 1, self.total))
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WatchlistsSnapshot {
    pub lists: Vec<ListView>,
}
impl WatchlistsSnapshot {
    pub fn list(&self, id: WatchlistId) -> Option<&ListView> {
        self.lists.iter().find(|l| l.list.id == id)
    }
    pub fn active(&self) -> impl Iterator<Item = &ListView> {
        self.lists.iter().filter(|l| l.next.is_some())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEntry {
    pub item: Option<ItemRef>,
    pub kind: ItemKind,
    pub title: String,
    pub year: Option<u16>,
    /// For episodes, year and TMDB id identify the parent series.
    pub tmdb_id: Option<i64>,
    pub series_title: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub season_from: Option<u32>,
    pub season_to: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Insert {
    #[default]
    End,
    Next,
    Index(usize),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddResult {
    pub added: Vec<WatchlistItemId>,
    pub refused: Vec<(usize, String)>,
}

/// The inverse of one edit. Later edits to the list are deliberately kept (DESIGN §42 §5).
#[derive(Debug, Clone)]
pub struct Undo {
    pub(crate) list: List,
    pub(crate) identity: i64,
    pub(crate) action: UndoAction,
}
#[derive(Debug, Clone)]
pub(crate) enum UndoAction {
    Added(Vec<WatchlistItemId>),
    Removed { entries: Vec<Entry>, chips: Vec<UndoChip> },
    Restarted { since: Timestamp, boundary: Timestamp, skipped: Vec<WatchlistItemId> },
    Deleted { entries: Vec<Entry>, chips: Vec<UndoChip> },
}
#[derive(Debug, Clone)]
pub(crate) struct UndoChip {
    pub id: i64,
    pub entry: WatchlistItemId,
    pub item: ItemRef,
    pub added_at: Timestamp,
}
