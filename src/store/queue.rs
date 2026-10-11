//! Up next storage: `queue_item` (migration 0010, DESIGN §32). OWNER: Q-core (batch base wrote
//! the contract; bodies are safe no-ops until Q-core).
//!
//! Rows are user data (`fp` + `orphaned_since`, `store::user::HOLDERS` "queue"); positions are
//! dense 0..n-1 and rewritten in one transaction on a move (≤ [`MAX_ROWS`] rows); at most one
//! row is `current` (unique index).

use rusqlite::Connection;

use crate::error::Result;
use crate::model::ItemRef;

/// Rows kept (played ones go first when an add would pass it).
pub const MAX_ROWS: usize = 200;
/// Items one add may queue.
pub const MAX_ADD: usize = 50;
/// Played rows are pruned after this long.
pub const PLAYED_KEEP_S: i64 = 7 * 86_400;

/// Move the programme slots of one channel by `delta_s` (B8: a time-shift change, same
/// transaction). Returns how many rows moved.
pub fn retime(_conn: &Connection, _source: crate::model::SourceId, _channel_key: &str, _delta_s: i64) -> Result<usize> {
    Ok(0)
}

/// Prune played rows older than [`PLAYED_KEEP_S`] (`jobs::maintenance::prune_features`).
pub fn prune(_conn: &Connection, _now: crate::model::Timestamp) -> Result<usize> {
    Ok(0)
}

/// The queue for a backup (`None`: nothing to keep).
pub fn export(_conn: &Connection) -> Result<Option<serde_json::Value>> {
    Ok(None)
}

/// Restore [`export`]'s value; `remap` maps a backup's refs to this install's (None = gone).
pub fn import(_conn: &Connection, _v: &serde_json::Value, _remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<usize> {
    Ok(0)
}
