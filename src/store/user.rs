//! User data repository: favourites (multiple lists), overrides, progress, history, reminders,
//! orphans. Owner: WP-02. All keyed by `ItemRef` (DESIGN §8).
//!
//! Fingerprints are NEVER supplied by callers: every write that attaches user data to an item
//! computes the item's [`Fingerprint`] inside the writer closure with [`fingerprint_of`] (UI
//! summaries don't carry provider id / tvg-id / url key). `Fingerprint.norm_group` = the
//! primary group's `grp.key` without its `<kind>:` prefix.

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{
    FavListId, FavouriteEntry, FavouriteList, Fingerprint, GroupKey, GroupOverride, GroupOverrideRow, HistoryEntry,
    ItemKind, ItemOverride, ItemOverrideRow, ItemRef, Reminder, ReminderId, ReminderState, SourceId, Timestamp,
    WatchProgress,
};

// ------------------------------------------------------------------ fingerprints

/// Fingerprint of a catalogue item from its current row (+ primary group key). Items that no
/// longer exist → `Fingerprint::default()` (callers keep the stored one).
pub fn fingerprint_of(conn: &Connection, item: &ItemRef) -> Result<Fingerprint> { todo!("WP-02") }

// ------------------------------------------------------------------ favourite lists

pub fn fav_lists(conn: &Connection) -> Result<Vec<FavouriteList>> { todo!("WP-02") }
pub fn create_fav_list(conn: &Connection, name: &str, icon: &str) -> Result<FavListId> { todo!("WP-02") }
pub fn rename_fav_list(conn: &Connection, id: FavListId, name: &str, icon: &str) -> Result<()> { todo!("WP-02") }
/// The default list cannot be deleted (returns `Error::Other`).
pub fn delete_fav_list(conn: &Connection, id: FavListId) -> Result<()> { todo!("WP-02") }
pub fn reorder_fav_lists(conn: &mut Connection, order: &[FavListId]) -> Result<()> { todo!("WP-02") }
pub fn default_fav_list(conn: &Connection) -> Result<FavListId> { todo!("WP-02") }

/// Entries of a list in user order (orphans included, flagged).
pub fn fav_items(conn: &Connection, list: FavListId) -> Result<Vec<FavouriteEntry>> { todo!("WP-02") }

/// Add (appended at the end; fingerprint computed inside) or remove. Idempotent.
pub fn set_favourite(conn: &Connection, list: FavListId, item: &ItemRef, on: bool, now: Timestamp) -> Result<()> {
    todo!("WP-02")
}

/// Move within a list to `position` (0-based), renumbering others.
pub fn move_favourite(conn: &mut Connection, list: FavListId, item: &ItemRef, position: usize) -> Result<()> {
    todo!("WP-02")
}

pub fn lists_containing(conn: &Connection, item: &ItemRef) -> Result<Vec<FavListId>> { todo!("WP-02") }

// ------------------------------------------------------------------ overrides

pub fn item_override(conn: &Connection, item: &ItemRef) -> Result<Option<ItemOverride>> { todo!("WP-02") }

/// Upsert (fingerprint computed inside); deletes the row when `ov` equals
/// `ItemOverride::default()`.
pub fn set_item_override(conn: &Connection, item: &ItemRef, ov: &ItemOverride) -> Result<()> {
    todo!("WP-02")
}

/// Batch reorder inside a group: `order` = item keys in the new order (user_order = index).
/// v1 limitation: `user_order` is per ITEM, not per (group, item) — reordering in one group
/// also changes the item's relative position in its other `;` groups (documented, DESIGN §13.2).
pub fn reorder_items(conn: &mut Connection, source: SourceId, kind: ItemKind, order: &[crate::model::ItemKey]) -> Result<()> {
    todo!("WP-02")
}

pub fn item_overrides(conn: &Connection, source: Option<SourceId>) -> Result<Vec<ItemOverrideRow>> { todo!("WP-02") }

pub fn set_group_override(conn: &Connection, source: SourceId, kind: ItemKind, key: &GroupKey, ov: &GroupOverride) -> Result<()> {
    todo!("WP-02")
}

pub fn group_overrides(conn: &Connection) -> Result<Vec<GroupOverrideRow>> { todo!("WP-02") }

// ------------------------------------------------------------------ progress & history

/// Upsert progress. Called by the player controller every 10 s and on stop/switch/end (off the
/// UI thread). An empty `p.fingerprint` is ignored: the fingerprint is computed inside.
pub fn save_progress(conn: &Connection, p: &WatchProgress) -> Result<()> { todo!("WP-02") }

pub fn progress(conn: &Connection, item: &ItemRef) -> Result<Option<WatchProgress>> { todo!("WP-02") }

/// Unfinished movies/episodes, most recent first (Home "Continue watching").
pub fn continue_watching(conn: &Connection, limit: usize) -> Result<Vec<WatchProgress>> { todo!("WP-02") }

/// Bulk mark watched/unwatched (season "mark all watched").
/// `items` = (item, display title).
pub fn mark_watched(conn: &mut Connection, items: &[(ItemRef, String)], finished: bool, now: Timestamp) -> Result<()> {
    todo!("WP-02")
}

/// Append a history row (de-duplicates consecutive same item; keeps the newest 500 rows).
pub fn push_history(conn: &Connection, e: &HistoryEntry) -> Result<()> { todo!("WP-02") }

/// Distinct channels by most recent watch (channel history overlay, last-channel recall).
pub fn recent_channels(conn: &Connection, limit: usize) -> Result<Vec<HistoryEntry>> { todo!("WP-02") }

// ------------------------------------------------------------------ reminders

/// Insert (unique per channel+start; returns existing id if present; fingerprint computed
/// inside).
pub fn add_reminder(conn: &Connection, r: &Reminder) -> Result<ReminderId> { todo!("WP-02") }
pub fn remove_reminder(conn: &Connection, id: ReminderId) -> Result<()> { todo!("WP-02") }
pub fn set_reminder_state(conn: &Connection, id: ReminderId, state: ReminderState) -> Result<()> { todo!("WP-02") }
/// Pending reminders with `start - lead_s >= since`, ordered by fire time.
pub fn pending_reminders(conn: &Connection, since: Timestamp) -> Result<Vec<Reminder>> { todo!("WP-02") }
pub fn reminders_for_channel(conn: &Connection, channel: &ItemRef) -> Result<Vec<Reminder>> { todo!("WP-02") }
/// Every pending reminder (Settings ▸ Reminders), soonest first.
pub fn all_reminders(conn: &Connection) -> Result<Vec<Reminder>> { todo!("WP-02") }

// ------------------------------------------------------------------ orphans

/// User data rows whose item vanished and could not be reattached (Settings ▸ Orphans).
#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    pub item: ItemRef,
    pub fingerprint: Fingerprint,
    pub since: Timestamp,
    /// Which tables hold data for it: "favourite", "override", "progress", "reminder", "epg"
    /// (every one of them has `fp` + `orphaned_since` columns).
    pub holders: Vec<&'static str>,
}

pub fn orphans(conn: &Connection) -> Result<Vec<Orphan>> { todo!("WP-02") }

/// Delete all user data for an orphan.
pub fn forget_orphan(conn: &mut Connection, item: &ItemRef) -> Result<()> { todo!("WP-02") }

/// Manually re-point all user data of `from` to `to` (Orphans screen "Attach to…"); `to`'s
/// fingerprint is computed inside.
pub fn reassign(conn: &mut Connection, from: &ItemRef, to: &ItemRef) -> Result<()> { todo!("WP-02") }
