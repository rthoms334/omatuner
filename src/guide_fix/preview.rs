//! Before / after now-next of proposed changes (batched `store::epg::now_next`, < 20 ms for
//! 200 pairs). OWNER: G-core.

use crate::model::{FixChange, NowNext, Timestamp};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PreviewRow {
    pub before: NowNext,
    pub after: NowNext,
    pub warnings: Vec<String>,
}

/// Preview `changes` at `now`. Placeholder until G-core: empty rows.
pub fn preview(_conn: &rusqlite::Connection, changes: &[FixChange], _now: Timestamp) -> crate::error::Result<Vec<PreviewRow>> {
    Ok(changes.iter().map(|_| PreviewRow::default()).collect())
}
