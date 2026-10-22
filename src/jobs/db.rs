//! Small SQL helpers the job pipelines need and `store::` does not offer (yet). Owner: WP-06.
//!
//! Everything here is a candidate for `store::sources` / `store::epg` (proposed to WP-02 in the
//! WP-06 report); until then it lives next to its only callers. Writes run on the writer
//! connection like every other store write; reads on job reader connections.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::model::{EpgSourceId, Timestamp};

/// `meta` key of the last successful `Maintenance` run.
const LAST_MAINTENANCE: &str = "jobs:last-maintenance";

/// Record a successful EPG download's validators (conditional GET next time). Called after
/// `finish_epg_import`, so a failed import never stores an ETag that would turn the retry into
/// a "not modified" no-op.
pub(super) fn set_epg_validators(conn: &Connection, id: EpgSourceId, etag: Option<&str>, last_modified: Option<&str>) -> Result<()> {
    conn.execute("UPDATE epg_source SET etag = ?2, last_modified = ?3 WHERE id = ?1", params![id, etag, last_modified])?;
    Ok(())
}

/// A 304 for an EPG source: the stored guide is current (`last_refresh`), no error.
pub(super) fn mark_epg_not_modified(conn: &Connection, id: EpgSourceId, now: Timestamp) -> Result<()> {
    conn.execute(
        "UPDATE epg_source SET last_refresh = ?2, last_attempt = ?2, last_error = NULL, failing_since = NULL WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

/// Largest catch-up window (days) of any channel: programmes inside it are kept (DESIGN §7.4).
pub(super) fn max_catchup_days(conn: &Connection) -> Result<u32> {
    let d: i64 =
        conn.query_row("SELECT COALESCE(max(json_extract(catchup, '$.days')), 0) FROM channel WHERE catchup IS NOT NULL", [], |r| r.get(0))?;
    Ok(u32::try_from(d.clamp(0, 3650)).unwrap_or(0))
}

pub(super) fn last_maintenance(conn: &Connection) -> Result<Option<Timestamp>> {
    let v: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [LAST_MAINTENANCE], |r| r.get(0)).optional()?;
    Ok(v.and_then(|v| v.trim().parse().ok()))
}

pub(super) fn set_last_maintenance(conn: &Connection, at: Timestamp) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![LAST_MAINTENANCE, at.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 304 ends a guide's failing streak like a download does (the back-off counts from it).
    #[test]
    fn not_modified_clears_the_failing_streak() {
        let mut c = Connection::open_in_memory().unwrap();
        crate::store::migrations::migrate(&mut c).unwrap();
        c.execute("INSERT INTO epg_source(id, name, kind) VALUES (1, 'g', 'url')", []).unwrap();
        crate::store::sources::mark_epg_failed(&c, EpgSourceId(1), "dns error", 100).unwrap();
        let streak = |c: &Connection| c.query_row("SELECT failing_since FROM epg_source", [], |r| r.get::<_, Option<Timestamp>>(0)).unwrap();
        assert_eq!(streak(&c), Some(100));
        mark_epg_not_modified(&c, EpgSourceId(1), 200).unwrap();
        assert_eq!(streak(&c), None);
    }
}
