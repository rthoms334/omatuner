//! The retime hook (B8, DESIGN §31): a channel's effective time shift changed, so everything
//! scheduled on it moves in the SAME transaction (and again on undo). Called by
//! `store::guide_fix` (G-core owns this file).

use rusqlite::Connection;

use crate::error::Result;
use crate::model::SourceId;

/// How much moved with a shift change (the toast says so).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetimeReport {
    pub reminders: usize,
    pub queue_slots: usize,
    pub follow_rows: usize,
}

/// Move the pending reminders, Up next programme slots and Following ledger rows of one
/// channel by `delta_s`.
pub fn channel_shift(conn: &Connection, source: SourceId, channel_key: &str, delta_s: i64) -> Result<RetimeReport> {
    if delta_s == 0 {
        return Ok(RetimeReport::default());
    }
    Ok(RetimeReport {
        reminders: crate::store::user::retime_reminders(conn, source, channel_key, delta_s)?,
        queue_slots: crate::store::queue::retime(conn, source, channel_key, delta_s)?,
        follow_rows: crate::store::follow::retime(conn, source, channel_key, delta_s)?,
    })
}
