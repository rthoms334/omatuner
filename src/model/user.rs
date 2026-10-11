//! User data: favourites, overrides, progress, history, reminders. FROZEN (skeleton).
//! All keyed by [`ItemRef`] so it survives playlist refreshes (DESIGN §8).

use serde::{Deserialize, Serialize};

use super::{FavListId, Fingerprint, GroupKey, ItemKind, ItemRef, ReminderId, SourceId, Timestamp};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FavouriteList {
    pub id: FavListId,
    pub name: String,
    /// Nerd Font glyph.
    pub icon: String,
    pub sort_index: i32,
    /// The default list ("Favourites") cannot be deleted; `Ctrl+F` toggles membership in it.
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FavouriteEntry {
    pub list: FavListId,
    pub item: ItemRef,
    pub position: i32,
    pub added_at: Timestamp,
    /// Captured at add-time for reattach.
    pub fingerprint: Fingerprint,
    /// Set when the item vanished on refresh and could not be reattached.
    pub orphaned_since: Option<Timestamp>,
}

/// Per-item user customisation. Absent fields = no override.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemOverride {
    pub rename: Option<String>,
    pub hidden: bool,
    pub locked: bool,
    /// Manual order inside its group (reorder).
    pub user_order: Option<i32>,
    /// Per-channel catch-up offset in seconds.
    pub catchup_offset_s: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemOverrideRow {
    pub item: ItemRef,
    pub ov: ItemOverride,
    pub fingerprint: Fingerprint,
    pub orphaned_since: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupOverride {
    pub rename: Option<String>,
    pub hidden: bool,
    pub locked: bool,
    pub user_order: Option<i32>,
    /// Pinned to Home as a section.
    pub pinned_home: bool,
    /// Shown although the source's category filter hides its tag (DESIGN §28.3). Cleared by
    /// hiding the group.
    pub shown: bool,
    /// The user's word on explicit content (DESIGN §20.3): `Some(true)` "Mark as explicit",
    /// `Some(false)` "Not explicit", `None` = as detected.
    pub explicit: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupOverrideRow {
    pub source: SourceId,
    pub kind: ItemKind,
    pub key: GroupKey,
    pub ov: GroupOverride,
}

/// A group by its stable identity (bulk hide, Settings ▸ Hidden).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupRef {
    pub source: SourceId,
    pub kind: ItemKind,
    pub key: GroupKey,
}

impl From<&super::Group> for GroupRef {
    fn from(g: &super::Group) -> Self {
        Self { source: g.source_id, kind: g.kind, key: g.key.clone() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchProgress {
    pub item: ItemRef,
    pub position_s: f64,
    pub duration_s: Option<f64>,
    pub updated_at: Timestamp,
    pub finished: bool,
    /// Display title captured at write time (Continue Watching works even if the item vanished).
    pub title: String,
    pub fingerprint: Fingerprint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub item: ItemRef,
    pub at: Timestamp,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReminderState {
    Pending,
    Fired,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reminder {
    pub id: ReminderId,
    pub channel: ItemRef,
    pub channel_name: String,
    pub programme_title: String,
    pub start: Timestamp,
    pub stop: Timestamp,
    /// Seconds before `start` to notify.
    pub lead_s: i64,
    pub state: ReminderState,
}

/// Reattach outcome for one orphan candidate (reported in import job results).
#[derive(Debug, Clone, PartialEq)]
pub enum ReattachOutcome {
    /// Primary key still present.
    Unchanged,
    /// Moved to a new key via the named fingerprint step.
    Reattached { new_key: super::ItemKey, via: &'static str },
    /// No unique candidate; user data kept as orphan.
    Orphaned,
}
