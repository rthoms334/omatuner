//! Up next storage: `queue_item` (migration 0010, DESIGN §32). OWNER: Q-core.
//!
//! Rows are user data (`fp` + `orphaned_since`, `store::user::HOLDERS` "queue"); positions are
//! dense 0..n-1 and rewritten in one transaction on every add, move and removal (≤
//! [`MAX_ROWS`] rows); at most one row is `current` (unique index). The engine
//! (`queue::service`) is the only writer apart from the B8 retime hook, pruning, backup restore
//! and the refresh's reattach / orphan marking; it re-reads the list after each of its writes.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{ItemKey, ItemKind, ItemRef, QueueId, SourceId, Timestamp};
use crate::queue::model::{AddedBy, Insert, QueueEntry, RowState, Slot};
use crate::store::sql::{atomic, fp_to_json, json_ids, kind_from_sql};

/// Rows kept (played ones go first when an add would pass it).
pub const MAX_ROWS: usize = 200;
/// Items one add may queue.
pub const MAX_ADD: usize = 50;
/// Played rows are pruned after this long.
pub const PLAYED_KEEP_S: i64 = 7 * 86_400;

/// A row to add (the engine looked its display fields up).
#[derive(Debug, Clone, PartialEq)]
pub struct NewRow {
    pub item: ItemRef,
    pub any_copy: bool,
    pub programme: Option<Slot>,
    pub title: String,
    pub subtitle: Option<String>,
    pub duration_s: Option<f64>,
    pub from_start: bool,
    pub by: AddedBy,
}

/// What [`add`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Added {
    /// The new rows' ids, in the order given.
    pub ids: Vec<QueueId>,
    /// Rows left out because the queue is full of unplayed rows (the last ones given).
    pub full: usize,
}

const COLS: &str = "id, source_id, kind, item_key, any_copy, prog_start, prog_stop, catchup_id, title, subtitle, duration_s, from_start, state, added_by, orphaned_since, watchlist_item_id, list_context";

fn entry_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEntry> {
    let start: Option<Timestamp> = r.get(5)?;
    let stop: Option<Timestamp> = r.get(6)?;
    let state: String = r.get(12)?;
    let catchup_id: Option<String> = r.get(7)?;
    Ok(QueueEntry {
        id: r.get(0)?,
        item: ItemRef { source: r.get(1)?, kind: kind_from_sql(&r.get::<_, String>(2)?)?, key: ItemKey(r.get(3)?) },
        any_copy: r.get(4)?,
        programme: start.zip(stop).map(|(start, stop)| Slot { start, stop, catchup_id }),
        title: r.get(8)?,
        subtitle: r.get(9)?,
        duration_s: r.get(10)?,
        from_start: r.get(11)?,
        state: RowState::parse(&state).unwrap_or_default(),
        by_agent: r.get::<_, String>(13)? == "agent",
        orphaned: r.get::<_, Option<Timestamp>>(14)?.is_some(),
        watchlist_item_id: r.get(15)?,
        list_context: r.get(16)?,
    })
}

/// Every row in list order.
pub fn load(conn: &Connection) -> Result<Vec<QueueEntry>> {
    // The user-data backup is also read immediately before migration 0015.
    let cols = if crate::store::migrations::has_column(conn, "queue_item", "watchlist_item_id")? {
        COLS.to_owned()
    } else {
        COLS.replace("watchlist_item_id, list_context", "NULL, 0")
    };
    let mut stmt = conn.prepare_cached(&format!("SELECT {cols} FROM queue_item ORDER BY position, id"))?;
    Ok(stmt.query_map([], entry_from_row)?.collect::<rusqlite::Result<_>>()?)
}

/// Mark a row as fed by a watchlist (DESIGN §42 §3), in the add's transaction.
pub fn link_watchlist(conn: &Connection, id: QueueId, entry: crate::model::WatchlistItemId, context: bool) -> Result<()> {
    conn.execute("UPDATE queue_item SET watchlist_item_id = ?2, list_context = ?3 WHERE id = ?1", params![id, entry, context])?;
    Ok(())
}

/// (id, state) of every row in list order.
fn order(conn: &Connection) -> Result<Vec<(QueueId, RowState)>> {
    let mut stmt = conn.prepare_cached("SELECT id, state FROM queue_item ORDER BY position, id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, QueueId>(0)?, RowState::parse(&r.get::<_, String>(1)?).unwrap_or_default())))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Write positions 0..n-1 in `ids`' order.
fn write_order(conn: &Connection, ids: &[QueueId]) -> Result<()> {
    let mut stmt = conn.prepare_cached("UPDATE queue_item SET position = ?2 WHERE id = ?1 AND position != ?2")?;
    for (i, id) in ids.iter().enumerate() {
        stmt.execute(params![id, i as i64])?;
    }
    Ok(())
}

/// Reuse an interrupted/context-fed row with the freshly selected copy and resume state.
pub(crate) fn refresh_list_row(c: &Connection, id: QueueId, row: &NewRow) -> Result<()> {
    let fp = crate::store::user::fingerprint_of(c, &row.item)?;
    c.execute("UPDATE queue_item SET source_id=?2,kind=?3,item_key=?4,any_copy=?5,title=?6,subtitle=?7,duration_s=?8,from_start=?9,added_by=?10,fp=?11,orphaned_since=NULL WHERE id=?1",
        params![id, row.item.source, row.item.kind.as_str(), row.item.key.as_str(), row.any_copy, row.title, row.subtitle, row.duration_s, row.from_start, if row.by == AddedBy::Agent { "agent" } else { "app" }, fp_to_json(&fp)])?;
    Ok(())
}

/// Where `at` lands in `rows` (list order): `Next` = before the first pending row.
pub fn insert_index(rows: &[(QueueId, RowState)], at: Insert) -> usize {
    match at {
        Insert::End => rows.len(),
        Insert::Next => rows.iter().position(|(_, s)| *s == RowState::Pending).unwrap_or(rows.len()),
        Insert::Index(i) => i.min(rows.len()),
    }
}

/// Add `rows` at `at` (one transaction). Past [`MAX_ROWS`] the oldest done rows make room;
/// when only unplayed rows are left, the rest isn't added ([`Added::full`]).
pub fn add(conn: &Connection, rows: &[NewRow], at: Insert, now: Timestamp) -> Result<Added> {
    atomic(conn, |c| {
        let mut list = order(c)?;
        let over = (list.len() + rows.len()).saturating_sub(MAX_ROWS);
        if over > 0 {
            let evict: Vec<QueueId> = c
                .prepare_cached(
                    "SELECT id FROM queue_item WHERE state IN ('played', 'skipped', 'failed') ORDER BY coalesce(done_at, 0), position LIMIT ?1",
                )?
                .query_map([over as i64], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            c.execute("DELETE FROM queue_item WHERE id IN (SELECT value FROM json_each(?1))", [json_ids(evict.iter().map(|i| i.0))])?;
            list.retain(|(id, _)| !evict.contains(id));
        }
        let room = MAX_ROWS.saturating_sub(list.len());
        let take = rows.len().min(room);
        let mut ins = c.prepare_cached(
            "INSERT INTO queue_item(position, source_id, kind, item_key, any_copy, prog_start, prog_stop, catchup_id, title, subtitle,
               duration_s, from_start, state, added_at, added_by, fp)
             VALUES (-1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', ?12, ?13, ?14)",
        )?;
        let mut ids = Vec::with_capacity(take);
        for r in &rows[..take] {
            let fp = crate::store::user::fingerprint_of(c, &r.item)?;
            ins.execute(params![
                r.item.source,
                r.item.kind.as_str(),
                r.item.key.as_str(),
                r.any_copy,
                r.programme.as_ref().map(|s| s.start),
                r.programme.as_ref().map(|s| s.stop),
                r.programme.as_ref().and_then(|s| s.catchup_id.as_deref()),
                r.title,
                r.subtitle,
                r.duration_s,
                r.from_start,
                now,
                if r.by == AddedBy::Agent { "agent" } else { "app" },
                fp_to_json(&fp),
            ])?;
            ids.push(QueueId(c.last_insert_rowid()));
        }
        let at = insert_index(&list, at);
        let mut all: Vec<QueueId> = list.iter().map(|(id, _)| *id).collect();
        all.splice(at..at, ids.iter().copied());
        write_order(c, &all)?;
        Ok(Added { ids, full: rows.len() - take })
    })
}

/// Move `ids` (keeping their order) to `to` among the other rows. Unknown ids are ignored.
pub fn move_rows(conn: &Connection, ids: &[QueueId], to: Insert) -> Result<()> {
    atomic(conn, |c| {
        let list = order(c)?;
        let moving: Vec<QueueId> = list.iter().map(|(id, _)| *id).filter(|id| ids.contains(id)).collect();
        let rest: Vec<(QueueId, RowState)> = list.into_iter().filter(|(id, _)| !ids.contains(id)).collect();
        let at = insert_index(&rest, to);
        let mut all: Vec<QueueId> = rest.iter().map(|(id, _)| *id).collect();
        all.splice(at..at, moving);
        write_order(c, &all)
    })
}

/// Put `ids` in this order into the places they hold now (every other row keeps its place;
/// unknown ids are ignored): "Shuffle the rest" in one transaction.
pub fn reorder(conn: &Connection, ids: &[QueueId]) -> Result<()> {
    atomic(conn, |c| {
        let mut all: Vec<QueueId> = order(c)?.into_iter().map(|(id, _)| id).collect();
        let mut seen = std::collections::HashSet::new();
        let new: Vec<QueueId> = ids.iter().copied().filter(|id| all.contains(id) && seen.insert(*id)).collect();
        let mut new = new.into_iter();
        for slot in all.iter_mut() {
            if ids.contains(slot)
                && let Some(id) = new.next()
            {
                *slot = id;
            }
        }
        write_order(c, &all)
    })
}

/// Remove rows; positions stay dense.
pub fn remove(conn: &Connection, ids: &[QueueId]) -> Result<usize> {
    atomic(conn, |c| {
        let n = c.execute("DELETE FROM queue_item WHERE id IN (SELECT value FROM json_each(?1))", [json_ids(ids.iter().map(|i| i.0))])?;
        renumber(c)?;
        Ok(n)
    })
}

/// "Remove played" (`played_only`: played, skipped and failed rows) or "Clear Up next…".
pub fn clear(conn: &Connection, played_only: bool) -> Result<usize> {
    atomic(conn, |c| {
        let n = if played_only {
            c.execute("DELETE FROM queue_item WHERE state IN ('played', 'skipped', 'failed')", [])?
        } else {
            c.execute("DELETE FROM queue_item", [])?
        };
        renumber(c)?;
        Ok(n)
    })
}

fn renumber(c: &Connection) -> Result<()> {
    let ids: Vec<QueueId> = order(c)?.into_iter().map(|(id, _)| id).collect();
    write_order(c, &ids)
}

/// Set a row's state; a new `current` row demotes the old one to pending (at most one).
pub fn set_state(conn: &Connection, id: QueueId, state: RowState, now: Timestamp) -> Result<()> {
    atomic(conn, |c| {
        if state == RowState::Current {
            c.execute("UPDATE queue_item SET state = 'pending' WHERE state = 'current' AND id != ?1", [id])?;
        }
        c.execute("UPDATE queue_item SET state = ?2, done_at = ?3 WHERE id = ?1", params![id, state.as_str(), state.is_done().then_some(now)])?;
        Ok(())
    })
}

/// "Play from the start" on a row.
pub fn set_from_start(conn: &Connection, id: QueueId, on: bool) -> Result<()> {
    conn.execute("UPDATE queue_item SET from_start = ?2 WHERE id = ?1", params![id, on])?;
    Ok(())
}

/// "Use another copy…": the row plays `item` (then falls back as usual).
pub fn pin_copy(conn: &Connection, id: QueueId, item: &ItemRef) -> Result<()> {
    let fp = crate::store::user::fingerprint_of(conn, item)?;
    conn.execute(
        "UPDATE queue_item SET source_id = ?2, kind = ?3, item_key = ?4, any_copy = 0, fp = ?5, orphaned_since = NULL WHERE id = ?1",
        params![id, item.source, item.kind.as_str(), item.key.as_str(), fp_to_json(&fp)],
    )?;
    Ok(())
}

/// Move the programme slots of one channel by `delta_s` (B8: a time-shift change, same
/// transaction). Played rows keep their times. Returns how many rows moved.
pub fn retime(conn: &Connection, source: SourceId, channel_key: &str, delta_s: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE queue_item SET prog_start = prog_start + ?3, prog_stop = prog_stop + ?3
         WHERE kind = 'channel' AND source_id = ?1 AND item_key = ?2 AND state IN ('pending', 'current')",
        params![source, channel_key, delta_s],
    )?)
}

/// Prune played rows older than [`PLAYED_KEEP_S`] (`jobs::maintenance::prune_features`).
pub fn prune(conn: &Connection, now: Timestamp) -> Result<usize> {
    atomic(conn, |c| {
        let n =
            c.execute("DELETE FROM queue_item WHERE state IN ('played', 'skipped', 'failed') AND coalesce(done_at, 0) < ?1", [now - PLAYED_KEEP_S])?;
        if n > 0 {
            renumber(c)?;
        }
        Ok(n)
    })
}

/// One row in a backup (unplayed rows only: the queue still to watch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct BackupRow {
    item: ItemRef,
    any_copy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stop: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    catchup_id: Option<String>,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_s: Option<f64>,
    from_start: bool,
    by_agent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watchlist_item_id: Option<crate::model::WatchlistItemId>,
    #[serde(default)]
    list_context: bool,
}

/// The queue for a backup (`None`: nothing to keep). Unplayed rows in list order.
pub fn export(conn: &Connection) -> Result<Option<serde_json::Value>> {
    let rows: Vec<BackupRow> = load(conn)?
        .into_iter()
        .filter(|e| !e.state.is_done())
        .map(|e| BackupRow {
            item: e.item,
            any_copy: e.any_copy,
            start: e.programme.as_ref().map(|s| s.start),
            stop: e.programme.as_ref().map(|s| s.stop),
            catchup_id: e.programme.and_then(|s| s.catchup_id),
            title: e.title,
            subtitle: e.subtitle,
            duration_s: e.duration_s,
            from_start: e.from_start,
            by_agent: e.by_agent,
            watchlist_item_id: e.watchlist_item_id,
            list_context: e.list_context,
        })
        .collect();
    if rows.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({ "v": 1, "rows": rows })))
}

/// Restore [`export`]'s value; `remap` maps a backup's refs to this install's (None = gone).
/// Rows are appended as pending; ones already pending here (same item and slot) are skipped,
/// and so is everything past [`MAX_ROWS`]. Returns the rows added.
pub fn import(conn: &Connection, v: &serde_json::Value, remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<usize> {
    import_with_watchlists(conn, v, remap, &std::collections::HashMap::new())
}

/// Watchlist row ids are local to the database; a restore remaps chips with their entries.
pub fn import_with_watchlists(
    conn: &Connection,
    v: &serde_json::Value,
    remap: &dyn Fn(&ItemRef) -> Option<ItemRef>,
    watchlist_entries: &std::collections::HashMap<crate::model::WatchlistItemId, crate::model::WatchlistItemId>,
) -> Result<usize> {
    let rows: Vec<BackupRow> = match v.get("rows").cloned().map(serde_json::from_value) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            tracing::warn!("backup: Up next rows unreadable: {e}");
            return Ok(0);
        }
        None => return Ok(0),
    };
    let now = crate::model::now();
    let mut new = Vec::new();
    let mut links = Vec::new();
    for r in rows {
        let Some(item) = remap(&r.item) else { continue };
        if (item.kind == ItemKind::Channel) != (r.start.is_some() && r.stop.is_some()) {
            continue;
        }
        let pending: Option<i64> = conn
            .prepare_cached(
                "SELECT 1 FROM queue_item WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3 AND prog_start IS ?4
                   AND state IN ('pending', 'current')",
            )?
            .query_row(params![item.source, item.kind.as_str(), item.key.as_str(), r.start], |row| row.get(0))
            .optional()?;
        if pending.is_some() || new.iter().any(|n: &NewRow| n.item == item && n.programme.as_ref().map(|s| s.start) == r.start) {
            continue;
        }
        new.push(NewRow {
            item,
            any_copy: r.any_copy,
            programme: r.start.zip(r.stop).map(|(start, stop)| Slot { start, stop, catchup_id: r.catchup_id }),
            title: r.title,
            subtitle: r.subtitle,
            duration_s: r.duration_s,
            from_start: r.from_start,
            by: if r.by_agent { AddedBy::Agent } else { AddedBy::App },
        });
        links.push((r.watchlist_item_id.and_then(|id| watchlist_entries.get(&id).copied()), r.list_context));
    }
    atomic(conn, |c| {
        let added = add(c, &new, Insert::End, now)?;
        for (id, (entry, context)) in added.ids.iter().zip(links) {
            if let Some(entry) = entry {
                link_watchlist(c, *id, entry, context)?;
            }
        }
        Ok(added.ids.len())
    })
}
