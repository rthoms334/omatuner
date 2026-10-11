//! Spoiler-guard storage: `spoiler_rule` + `spoiler_seen` (migration 0012, DESIGN §34). OWNER:
//! S1 (batch base wrote the contract; bodies are safe no-ops until S1).
//!
//! `spoiler_seen` is keyed by `(textmatch::event_key, first_start)` (B3), the same key as
//! Following's ledger, so "Mark seen" on a followed match lifts the guard and Following's
//! Upcoming shows "seen".

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{ItemRef, Timestamp};

/// Seen rows are pruned this long after their window closes.
pub const SEEN_KEEP_S: i64 = 30 * 86_400;

/// Prune seen rows (`jobs::maintenance::prune_features`).
pub fn prune(_conn: &Connection, _now: Timestamp) -> Result<usize> {
    Ok(0)
}

/// Rules and seen rows for a backup.
pub fn export(_conn: &Connection) -> Result<Option<serde_json::Value>> {
    Ok(None)
}

pub fn import(_conn: &Connection, _v: &serde_json::Value, _remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<usize> {
    Ok(0)
}
