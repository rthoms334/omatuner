//! Guide fixes storage (migration 0009, DESIGN §31): the logged batches that Undo reverts
//! (`guide_fix_log` / `guide_fix_change`), the suggestion cache (`epg_suggestion`) and the
//! rule-row derivation for copies. OWNER: G-core (batch base wrote the contract).
//!
//! Every write is one logged batch (actor, label without titles, n). Manual mapping changes
//! emit `DataScope::Epg` and submit `JobKind::EvaluateFollows { reason: Mapping }` (B7); shift
//! changes call `store::retime::channel_shift` in the same transaction (B8).

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{FixActor, GuideFixLogId, Proposal, Timestamp};

/// Logs kept: 90 days or 200 batches.
pub const LOG_KEEP_S: i64 = 90 * 86_400;
pub const LOG_KEEP_N: usize = 200;
/// Channel changes one batch may hold (Claude: 50 on "yes", more opens the confirm card).
pub const MAX_BATCH: usize = 200;

/// What [`apply`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApplyReport {
    pub log: Option<GuideFixLogId>,
    pub applied: usize,
    /// (channel key, reason) of changes left out (locked, gone…).
    pub skipped: Vec<(String, String)>,
    pub reminders_moved: usize,
}

/// What [`undo`] did: changes made since are skipped and named.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UndoReport {
    pub reverted: usize,
    pub skipped: Vec<(String, String)>,
}

/// Apply a proposal as one logged batch.
pub fn apply(_conn: &Connection, _actor: FixActor, _label: &str, _p: &Proposal, _now: Timestamp) -> Result<ApplyReport> {
    Err(crate::error::Error::other("Guide fixes aren't available in this build yet"))
}

/// Revert a logged batch.
pub fn undo(_conn: &Connection, _log: GuideFixLogId, _now: Timestamp) -> Result<UndoReport> {
    Err(crate::error::Error::other("Guide fixes aren't available in this build yet"))
}

/// Prune logs (`jobs::maintenance::prune_features`).
pub fn prune(_conn: &Connection, _now: Timestamp) -> Result<usize> {
    Ok(0)
}
