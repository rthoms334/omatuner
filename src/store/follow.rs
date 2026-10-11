//! Following storage: `follow_rule` + the `follow_match` ledger (migration 0011, DESIGN §33).
//! OWNER: F-AB (batch base wrote the contract; bodies are safe no-ops until F-AB).
//!
//! Hooks other owners call (signatures fixed by the base):
//! * [`on_reminder_removed`] from `store::user::remove_reminder` (the only delete path, B6), in
//!   the same transaction: a removed rule reminder's ledger row becomes `skipped`.
//! * [`on_reminder_edited`] when the user edits a rule reminder's lead or mode: the reminder
//!   detaches (`rule_id = NULL`, ledger row `adopted`).
//! * [`retime`] from `store::retime::channel_shift` (B8).

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{ItemRef, ReminderId, RemoveWhy, SourceId, Timestamp};

/// Ledger rows of show rules are kept this long (new-only remembers episodes).
pub const SHOW_KEEP_S: i64 = 2 * 365 * 86_400;
/// Other ledger rows are kept this long after their stop.
pub const KEEP_S: i64 = 30 * 86_400;

/// A reminder is being removed (B6). `why == User | Agent` on a rule's reminder → its ledger
/// row becomes `skipped` (never re-created). Must not fail when the ledger is empty.
pub fn on_reminder_removed(_conn: &Connection, _id: ReminderId, _why: RemoveWhy) -> Result<()> {
    Ok(())
}

/// The user edited a rule reminder's lead or mode: detach it from its rule (B6 point 5).
pub fn on_reminder_edited(_conn: &Connection, _id: ReminderId) -> Result<()> {
    Ok(())
}

/// Move ledger rows of one channel by `delta_s` (B8). Returns how many moved.
pub fn retime(_conn: &Connection, _source: SourceId, _channel_key: &str, _delta_s: i64) -> Result<usize> {
    Ok(0)
}

/// Mark past rows `aired` and prune (`jobs::maintenance::prune_features`).
pub fn prune(_conn: &Connection, _now: Timestamp) -> Result<usize> {
    Ok(0)
}

/// Rules for a backup (not the ledger).
pub fn export(_conn: &Connection) -> Result<Option<serde_json::Value>> {
    Ok(None)
}

/// Restore rules (merged by name + spec).
pub fn import(_conn: &Connection, _v: &serde_json::Value, _remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<usize> {
    Ok(0)
}
