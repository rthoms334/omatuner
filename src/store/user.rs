//! User data repository: favourites (multiple lists), overrides, progress, history, reminders,
//! orphans. Owner: WP-02. All keyed by `ItemRef` (DESIGN §8).
//!
//! Fingerprints are NEVER supplied by callers: every write that attaches user data to an item
//! computes the item's [`Fingerprint`] inside the writer closure with [`fingerprint_of`] (UI
//! summaries don't carry provider id / tvg-id / url key). `Fingerprint.norm_group` = the
//! primary group's `grp.key` without its `<kind>:` prefix.
//!
//! When the item does not exist (any more) the computed fingerprint is the default one and the
//! stored fingerprint is kept (`fp = COALESCE(?, fp)`), so reattach still has the facts.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::{Error, Result};
use crate::model::{
    FavListId, FavouriteEntry, FavouriteList, Fingerprint, GroupKey, GroupOverride, GroupOverrideRow, GroupRef, HistoryEntry, ItemKey, ItemKind,
    ItemOverride, ItemOverrideRow, ItemRef, Reminder, ReminderId, ReminderMode, ReminderState, SourceId, Timestamp, WatchProgress,
};
use crate::store::sql::{self, atomic, enum_parse, enum_str, fp_from_json, fp_opt, group_norm, kind_from_sql, or_not_found};

/// History rows kept (DESIGN §7.4).
const HISTORY_KEEP: i64 = 500;

// ------------------------------------------------------------------ fingerprints

/// SQL selecting identity facts `(key, provider_id, tvg_id, norm_name, primary group key,
/// url_key)` of the items of one kind in source `?1` (`one` = only key `?2`). Shared by
/// [`fingerprint_of`] and reattach (which loads every item of a source once).
pub(crate) fn facts_sql(kind: ItemKind, one: bool) -> String {
    let key_filter = if one { " AND x.key = ?2" } else { "" };
    match kind {
        ItemKind::Channel => format!(
            "SELECT x.key, x.provider_id, x.tvg_id, x.norm_name,
                    (SELECT g.key FROM channel_group cg JOIN grp g ON g.id = cg.group_id
                     WHERE cg.channel_id = x.id ORDER BY cg.is_primary DESC, g.provider_order, g.id LIMIT 1),
                    x.url_key
             FROM channel x WHERE x.source_id = ?1{key_filter}"
        ),
        ItemKind::Movie => format!(
            "SELECT x.key, x.provider_id, NULL, x.norm_name,
                    (SELECT g.key FROM movie_group mg JOIN grp g ON g.id = mg.group_id
                     WHERE mg.movie_id = x.id ORDER BY g.provider_order, g.id LIMIT 1),
                    x.url_key
             FROM movie x WHERE x.source_id = ?1{key_filter}"
        ),
        ItemKind::Series => format!(
            "SELECT x.key, x.provider_id, NULL, x.norm_name,
                    (SELECT g.key FROM series_group sg JOIN grp g ON g.id = sg.group_id
                     WHERE sg.series_id = x.id ORDER BY g.provider_order, g.id LIMIT 1),
                    NULL
             FROM series x WHERE x.source_id = ?1{key_filter}"
        ),
        // episodes have no name of their own: "<series norm>:s<season>e<episode>"
        ItemKind::Episode => format!(
            "SELECT x.key, x.provider_id, NULL, s.norm_name || ':s' || x.season || 'e' || x.episode, NULL, NULL
             FROM episode x JOIN series s ON s.id = x.series_id WHERE x.source_id = ?1{key_filter}"
        ),
    }
}

/// One row of [`facts_sql`] → (key, fingerprint).
pub(crate) fn facts_from_row(r: &Row<'_>) -> rusqlite::Result<(ItemKey, Fingerprint)> {
    let nonempty = |s: Option<String>| s.filter(|s| !s.is_empty());
    Ok((
        ItemKey(r.get(0)?),
        Fingerprint {
            provider_id: nonempty(r.get(1)?),
            tvg_id: nonempty(r.get(2)?),
            norm_name: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            norm_group: r.get::<_, Option<String>>(4)?.map(|k| group_norm(&k)),
            url_key: nonempty(r.get(5)?),
        },
    ))
}

/// Fingerprint of a catalogue item from its current row (+ primary group key). Items that no
/// longer exist → `Fingerprint::default()` (callers keep the stored one).
pub fn fingerprint_of(conn: &Connection, item: &ItemRef) -> Result<Fingerprint> {
    let mut stmt = conn.prepare_cached(&facts_sql(item.kind, true))?;
    Ok(stmt.query_row(params![item.source, item.key.as_str()], facts_from_row).optional()?.map(|(_, fp)| fp).unwrap_or_default())
}

// ------------------------------------------------------------------ favourite lists

fn fav_list_from_row(r: &Row<'_>) -> rusqlite::Result<FavouriteList> {
    Ok(FavouriteList { id: r.get(0)?, name: r.get(1)?, icon: r.get(2)?, sort_index: r.get(3)?, is_default: r.get(4)? })
}

pub fn fav_lists(conn: &Connection) -> Result<Vec<FavouriteList>> {
    let mut stmt = conn.prepare_cached("SELECT id, name, icon, sort_index, is_default FROM fav_list ORDER BY sort_index, id")?;
    let rows = stmt.query_map([], fav_list_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn create_fav_list(conn: &Connection, name: &str, icon: &str) -> Result<FavListId> {
    conn.execute(
        "INSERT INTO fav_list(name, icon, sort_index, is_default)
         VALUES (?1, ?2, (SELECT COALESCE(max(sort_index), -1) + 1 FROM fav_list), 0)",
        params![name, icon],
    )?;
    Ok(FavListId(conn.last_insert_rowid()))
}

pub fn rename_fav_list(conn: &Connection, id: FavListId, name: &str, icon: &str) -> Result<()> {
    if conn.execute("UPDATE fav_list SET name = ?2, icon = ?3 WHERE id = ?1", params![id, name, icon])? == 0 {
        return Err(sql::not_found("favourites list", id));
    }
    Ok(())
}

/// The default list cannot be deleted (returns `Error::Other`).
pub fn delete_fav_list(conn: &Connection, id: FavListId) -> Result<()> {
    let is_default: bool = or_not_found(conn.query_row("SELECT is_default FROM fav_list WHERE id = ?1", [id], |r| r.get(0)), "favourites list", id)?;
    if is_default {
        return Err(Error::Other("the default favourites list cannot be deleted".into()));
    }
    conn.execute("DELETE FROM fav_list WHERE id = ?1", [id])?;
    conn.execute("DELETE FROM meta WHERE key = ?1", [format!("watchlist:favourites:{id}")])?;
    Ok(())
}

pub fn reorder_fav_lists(conn: &mut Connection, order: &[FavListId]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("UPDATE fav_list SET sort_index = ?2 WHERE id = ?1")?;
        for (i, id) in order.iter().enumerate() {
            stmt.execute(params![id, i as i64])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn default_fav_list(conn: &Connection) -> Result<FavListId> {
    let id = conn.query_row("SELECT id FROM fav_list WHERE is_default = 1 ORDER BY id LIMIT 1", [], |r| r.get(0)).optional()?;
    match id {
        Some(id) => Ok(id),
        // the migration creates it; recreate defensively if a restore/replace lost it
        None => {
            conn.execute("INSERT INTO fav_list(name, icon, sort_index, is_default) VALUES ('Favourites', '', 0, 1)", [])?;
            Ok(FavListId(conn.last_insert_rowid()))
        }
    }
}

pub(crate) const FAV_ITEM_COLS: &str = "list_id, source_id, kind, item_key, position, added_at, fp, orphaned_since";

pub(crate) fn fav_entry_from_row(r: &Row<'_>) -> rusqlite::Result<FavouriteEntry> {
    Ok(FavouriteEntry {
        list: r.get(0)?,
        item: ItemRef { source: r.get(1)?, kind: kind_from_sql(&r.get::<_, String>(2)?)?, key: ItemKey(r.get(3)?) },
        position: r.get(4)?,
        added_at: r.get(5)?,
        fingerprint: fp_from_json(&r.get::<_, String>(6)?),
        orphaned_since: r.get(7)?,
    })
}

/// Entries of a list in user order (orphans included, flagged).
pub fn fav_items(conn: &Connection, list: FavListId) -> Result<Vec<FavouriteEntry>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {FAV_ITEM_COLS} FROM fav_item WHERE list_id = ?1 ORDER BY position, added_at"))?;
    let rows = stmt.query_map([list], fav_entry_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Add (appended at the end; fingerprint computed inside) or remove. Idempotent.
pub fn set_favourite(conn: &Connection, list: FavListId, item: &ItemRef, on: bool, now: Timestamp) -> Result<()> {
    if item.kind != ItemKind::Channel {
        return Err(crate::error::Error::other("use_watchlist: films and series belong in Watchlists"));
    }
    if !on {
        conn.execute(
            "DELETE FROM fav_item WHERE list_id = ?1 AND source_id = ?2 AND kind = ?3 AND item_key = ?4",
            params![list, item.source, item.kind.as_str(), item.key.as_str()],
        )?;
        return Ok(());
    }
    let fp = fingerprint_of(conn, item)?;
    conn.execute(
        "INSERT INTO fav_item(list_id, source_id, kind, item_key, position, added_at, fp)
         VALUES (?1, ?2, ?3, ?4, (SELECT COALESCE(max(position), -1) + 1 FROM fav_item WHERE list_id = ?1), ?5, COALESCE(?6, '{}'))
         ON CONFLICT(list_id, source_id, kind, item_key) DO NOTHING",
        params![list, item.source, item.kind.as_str(), item.key.as_str(), now, fp_opt(&fp)],
    )?;
    Ok(())
}

/// Move within a list to `position` (0-based), renumbering others.
pub fn move_favourite(conn: &mut Connection, list: FavListId, item: &ItemRef, position: usize) -> Result<()> {
    let tx = conn.transaction()?;
    let mut items: Vec<ItemRef> = fav_items(&tx, list)?.into_iter().map(|e| e.item).collect();
    let Some(from) = items.iter().position(|i| i == item) else {
        return Err(Error::NotFound("favourite".into()));
    };
    let it = items.remove(from);
    items.insert(position.min(items.len()), it);
    {
        let mut stmt =
            tx.prepare_cached("UPDATE fav_item SET position = ?5 WHERE list_id = ?1 AND source_id = ?2 AND kind = ?3 AND item_key = ?4")?;
        for (i, r) in items.iter().enumerate() {
            stmt.execute(params![list, r.source, r.kind.as_str(), r.key.as_str(), i as i64])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn lists_containing(conn: &Connection, item: &ItemRef) -> Result<Vec<FavListId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.list_id FROM fav_item f JOIN fav_list l ON l.id = f.list_id
         WHERE f.source_id = ?1 AND f.kind = ?2 AND f.item_key = ?3 ORDER BY l.sort_index, l.id",
    )?;
    let rows = stmt.query_map(params![item.source, item.kind.as_str(), item.key.as_str()], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ------------------------------------------------------------------ overrides

fn item_override_from_row(r: &Row<'_>, off: usize) -> rusqlite::Result<ItemOverride> {
    Ok(ItemOverride {
        rename: r.get(off)?,
        hidden: r.get(off + 1)?,
        locked: r.get(off + 2)?,
        user_order: r.get(off + 3)?,
        catchup_offset_s: r.get(off + 4)?,
    })
}

pub fn item_override(conn: &Connection, item: &ItemRef) -> Result<Option<ItemOverride>> {
    let mut stmt = conn.prepare_cached(
        "SELECT rename, hidden, locked, user_order, catchup_offset_s FROM item_override
         WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3",
    )?;
    Ok(stmt.query_row(params![item.source, item.kind.as_str(), item.key.as_str()], |r| item_override_from_row(r, 0)).optional()?)
}

/// Upsert (fingerprint computed inside); deletes the row when `ov` equals
/// `ItemOverride::default()`.
pub fn set_item_override(conn: &Connection, item: &ItemRef, ov: &ItemOverride) -> Result<()> {
    if *ov == ItemOverride::default() {
        // the row may still carry Guide fixes' columns (a time shift, dismissed issues), which
        // `ItemOverride` doesn't model: clear ours and drop the row only when nothing is left
        let key = params![item.source, item.kind.as_str(), item.key.as_str()];
        conn.execute(
            "UPDATE item_override SET rename = NULL, hidden = 0, locked = 0, user_order = NULL, catchup_offset_s = NULL
             WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3",
            key,
        )?;
        conn.execute(
            "DELETE FROM item_override WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3
               AND shift_min IS NULL AND guide_ok = 0",
            key,
        )?;
        return Ok(());
    }
    let fp = fingerprint_of(conn, item)?;
    conn.prepare_cached(
        "INSERT INTO item_override(source_id, kind, item_key, rename, hidden, locked, user_order, catchup_offset_s, fp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, '{}'))
         ON CONFLICT(source_id, kind, item_key) DO UPDATE SET
           rename = excluded.rename, hidden = excluded.hidden, locked = excluded.locked, user_order = excluded.user_order,
           catchup_offset_s = excluded.catchup_offset_s, fp = COALESCE(?9, item_override.fp)",
    )?
    .execute(params![
        item.source,
        item.kind.as_str(),
        item.key.as_str(),
        ov.rename,
        ov.hidden,
        ov.locked,
        ov.user_order,
        ov.catchup_offset_s,
        fp_opt(&fp)
    ])?;
    Ok(())
}

/// Batch reorder inside a group: `order` = item keys in the new order (user_order = index).
/// v1 limitation: `user_order` is per ITEM, not per (group, item) — reordering in one group
/// also changes the item's relative position in its other `;` groups (documented, DESIGN §13.2).
pub fn reorder_items(conn: &mut Connection, source: SourceId, kind: ItemKind, order: &[crate::model::ItemKey]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare_cached(
            "INSERT INTO item_override(source_id, kind, item_key, user_order, fp) VALUES (?1, ?2, ?3, ?4, COALESCE(?5, '{}'))
             ON CONFLICT(source_id, kind, item_key) DO UPDATE SET user_order = excluded.user_order,
               fp = COALESCE(?5, item_override.fp)",
        )?;
        for (i, key) in order.iter().enumerate() {
            let fp = fingerprint_of(&tx, &ItemRef { source, kind, key: key.clone() })?;
            up.execute(params![source, kind.as_str(), key.as_str(), i as i64, fp_opt(&fp)])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn item_overrides(conn: &Connection, source: Option<SourceId>) -> Result<Vec<ItemOverrideRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT source_id, kind, item_key, rename, hidden, locked, user_order, catchup_offset_s, fp, orphaned_since
         FROM item_override WHERE ?1 IS NULL OR source_id = ?1 ORDER BY source_id, kind, item_key",
    )?;
    let rows = stmt.query_map([source], |r| {
        Ok(ItemOverrideRow {
            item: ItemRef { source: r.get(0)?, kind: kind_from_sql(&r.get::<_, String>(1)?)?, key: ItemKey(r.get(2)?) },
            ov: item_override_from_row(r, 3)?,
            fingerprint: fp_from_json(&r.get::<_, String>(8)?),
            orphaned_since: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn set_group_override(conn: &Connection, source: SourceId, kind: ItemKind, key: &GroupKey, ov: &GroupOverride) -> Result<()> {
    if *ov == GroupOverride::default() {
        conn.execute("DELETE FROM group_override WHERE source_id = ?1 AND kind = ?2 AND group_key = ?3", params![source, kind.as_str(), key.0])?;
        return Ok(());
    }
    conn.execute(
        "INSERT INTO group_override(source_id, kind, group_key, rename, hidden, locked, user_order, pinned_home, shown, explicit)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(source_id, kind, group_key) DO UPDATE SET rename = excluded.rename, hidden = excluded.hidden,
           locked = excluded.locked, user_order = excluded.user_order, pinned_home = excluded.pinned_home, shown = excluded.shown,
           explicit = excluded.explicit",
        params![source, kind.as_str(), key.0, ov.rename, ov.hidden, ov.locked, ov.user_order, ov.pinned_home, ov.shown, ov.explicit],
    )?;
    Ok(())
}

/// One group's override (default when it has none).
fn group_override_of(conn: &Connection, g: &GroupRef) -> Result<GroupOverride> {
    Ok(conn
        .prepare_cached(
            "SELECT rename, hidden, locked, user_order, pinned_home, shown, explicit FROM group_override
             WHERE source_id = ?1 AND kind = ?2 AND group_key = ?3",
        )?
        .query_row(params![g.source, g.kind.as_str(), g.key.0], |r| {
            Ok(GroupOverride {
                rename: r.get(0)?,
                hidden: r.get(1)?,
                locked: r.get(2)?,
                user_order: r.get(3)?,
                pinned_home: r.get(4)?,
                shown: r.get(5)?,
                explicit: r.get(6)?,
            })
        })
        .optional()?
        .unwrap_or_default())
}

/// The user's word on one category's explicit content (DESIGN §20.3): `Some(true)` "Mark as
/// explicit", `Some(false)` "Not explicit", `None` = back to what the import detected. Keeps
/// the rest of the override.
pub fn set_group_explicit(conn: &Connection, g: &GroupRef, explicit: Option<bool>) -> Result<()> {
    let ov = GroupOverride { explicit, ..group_override_of(conn, g)? };
    set_group_override(conn, g.source, g.kind, &g.key, &ov)
}

/// Hide groups in one transaction (Ctrl+H on one or several marked groups, "Hide these N
/// groups"; DESIGN §28.7): sets `hidden`, clears `shown`, keeps the rest of each override.
pub fn hide_groups(conn: &Connection, groups: &[GroupRef]) -> Result<()> {
    atomic(conn, |c| {
        for g in groups {
            let ov = GroupOverride { hidden: true, shown: false, ..group_override_of(c, g)? };
            set_group_override(c, g.source, g.kind, &g.key, &ov)?;
        }
        Ok(())
    })
}

/// Show one group again (Settings ▸ Hidden; DESIGN §28.5): clears `hidden` and sets `shown`
/// to `exempt` — true when the source's category filter hides the group (`GroupFilter::Hidden`
/// or `Exempt`), so the user's choice beats the rule.
pub fn show_group(conn: &Connection, g: &GroupRef, exempt: bool) -> Result<()> {
    let ov = GroupOverride { hidden: false, shown: exempt, ..group_override_of(conn, g)? };
    set_group_override(conn, g.source, g.kind, &g.key, &ov)
}

pub fn group_overrides(conn: &Connection) -> Result<Vec<GroupOverrideRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT source_id, kind, group_key, rename, hidden, locked, user_order, pinned_home, shown, explicit FROM group_override
         ORDER BY source_id, kind, group_key",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(GroupOverrideRow {
            source: r.get(0)?,
            kind: kind_from_sql(&r.get::<_, String>(1)?)?,
            key: GroupKey(r.get(2)?),
            ov: GroupOverride {
                rename: r.get(3)?,
                hidden: r.get(4)?,
                locked: r.get(5)?,
                user_order: r.get(6)?,
                pinned_home: r.get(7)?,
                shown: r.get(8)?,
                explicit: r.get(9)?,
            },
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ------------------------------------------------------------------ progress & history

pub(crate) const PROGRESS_COLS: &str = "source_id, kind, item_key, position_s, duration_s, finished, updated_at, title, fp";

pub(crate) fn progress_from_row(r: &Row<'_>) -> rusqlite::Result<WatchProgress> {
    progress_from_offset(r, 0)
}

/// [`progress_from_row`] for a row whose [`PROGRESS_COLS`] start at column `at`.
pub(crate) fn progress_from_offset(r: &Row<'_>, at: usize) -> rusqlite::Result<WatchProgress> {
    Ok(WatchProgress {
        item: ItemRef { source: r.get(at)?, kind: kind_from_sql(&r.get::<_, String>(at + 1)?)?, key: ItemKey(r.get(at + 2)?) },
        position_s: r.get(at + 3)?,
        duration_s: r.get(at + 4)?,
        finished: r.get(at + 5)?,
        updated_at: r.get(at + 6)?,
        title: r.get(at + 7)?,
        fingerprint: fp_from_json(&r.get::<_, String>(at + 8)?),
    })
}

/// Upsert progress. Called by the player controller every 10 s and on stop/switch/end (off the
/// UI thread). An empty `p.fingerprint` is ignored: the fingerprint is computed inside.
///
/// A non-empty `p.fingerprint` is only used for a new row of an item that does not exist in
/// the catalogue (the computed fingerprint always wins).
pub fn save_progress(conn: &Connection, p: &WatchProgress) -> Result<()> {
    let fp = fingerprint_of(conn, &p.item)?;
    conn.prepare_cached(
        "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, finished, updated_at, title, fp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, ?10, '{}'))
         ON CONFLICT(source_id, kind, item_key) DO UPDATE SET
           position_s = excluded.position_s, duration_s = COALESCE(excluded.duration_s, watch_progress.duration_s),
           finished = excluded.finished, updated_at = excluded.updated_at, title = excluded.title,
           fp = COALESCE(?9, watch_progress.fp),
           orphaned_since = CASE WHEN ?9 IS NULL THEN watch_progress.orphaned_since END",
    )?
    .execute(params![
        p.item.source,
        p.item.kind.as_str(),
        p.item.key.as_str(),
        p.position_s,
        p.duration_s,
        p.finished,
        p.updated_at,
        p.title,
        fp_opt(&fp),
        fp_opt(&p.fingerprint),
    ])?;
    Ok(())
}

pub fn progress(conn: &Connection, item: &ItemRef) -> Result<Option<WatchProgress>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {PROGRESS_COLS} FROM watch_progress WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3"))?;
    Ok(stmt.query_row(params![item.source, item.kind.as_str(), item.key.as_str()], progress_from_row).optional()?)
}

/// Unfinished movies/episodes, most recent first (Home "Continue watching").
///
/// Orphaned rows (item vanished, not re-attached) are left out: they cannot be played.
pub fn continue_watching(conn: &Connection, limit: usize) -> Result<Vec<WatchProgress>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PROGRESS_COLS} FROM watch_progress
         WHERE finished = 0 AND kind IN ('movie', 'episode') AND orphaned_since IS NULL AND position_s > 0
         ORDER BY updated_at DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map([limit as i64], progress_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Bulk mark watched/unwatched (season "mark all watched").
/// `items` = (item, display title).
///
/// Watched keeps any stored position and sets `finished`; unwatched forgets the progress row
/// (the item then reads as never started).
pub fn mark_watched(conn: &mut Connection, items: &[(ItemRef, String)], finished: bool, now: Timestamp) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut del = tx.prepare_cached("DELETE FROM watch_progress WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3")?;
        let mut up = tx.prepare_cached(
            "INSERT INTO watch_progress(source_id, kind, item_key, position_s, finished, updated_at, title, fp)
             VALUES (?1, ?2, ?3, 0, 1, ?4, ?5, COALESCE(?6, '{}'))
             ON CONFLICT(source_id, kind, item_key) DO UPDATE SET finished = 1, updated_at = excluded.updated_at,
               title = excluded.title, fp = COALESCE(?6, watch_progress.fp)",
        )?;
        for (item, title) in items {
            if finished {
                let fp = fingerprint_of(&tx, item)?;
                up.execute(params![item.source, item.kind.as_str(), item.key.as_str(), now, title, fp_opt(&fp)])?;
            } else {
                del.execute(params![item.source, item.kind.as_str(), item.key.as_str()])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Append a history row (de-duplicates consecutive same item; keeps the newest 500 rows).
pub fn push_history(conn: &Connection, e: &HistoryEntry) -> Result<()> {
    atomic(conn, |c| {
        let last: Option<(i64, SourceId, String, ItemKey)> = c
            .prepare_cached("SELECT id, source_id, kind, item_key FROM history ORDER BY at DESC, id DESC LIMIT 1")?
            .query_row([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, ItemKey(r.get(3)?))))
            .optional()?;
        match last {
            Some((id, s, k, key)) if s == e.item.source && k == e.item.kind.as_str() && key == e.item.key => {
                c.execute("UPDATE history SET at = max(at, ?2), title = ?3 WHERE id = ?1", params![id, e.at, e.title])?;
            }
            _ => {
                c.prepare_cached("INSERT INTO history(source_id, kind, item_key, title, at) VALUES (?1, ?2, ?3, ?4, ?5)")?.execute(params![
                    e.item.source,
                    e.item.kind.as_str(),
                    e.item.key.as_str(),
                    e.title,
                    e.at
                ])?;
                c.prepare_cached("DELETE FROM history WHERE id IN (SELECT id FROM history ORDER BY at DESC, id DESC LIMIT -1 OFFSET ?1)")?
                    .execute([HISTORY_KEEP])?;
            }
        }
        Ok(())
    })
}

/// Forget every history row of `item` (Delete on a "Recently watched" tile); returns the rows
/// removed.
pub fn remove_history(conn: &Connection, item: &ItemRef) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM history WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3",
        params![item.source, item.kind.as_str(), item.key.as_str()],
    )?)
}

/// Distinct channels by most recent watch (channel history overlay, last-channel recall).
pub fn recent_channels(conn: &Connection, limit: usize) -> Result<Vec<HistoryEntry>> {
    // SQLite: bare columns of an aggregate query come from the row holding max(at)
    let mut stmt = conn.prepare_cached(
        "SELECT source_id, item_key, title, max(at) AS last FROM history WHERE kind = 'channel'
         GROUP BY source_id, item_key ORDER BY last DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(HistoryEntry { item: ItemRef { source: r.get(0)?, kind: ItemKind::Channel, key: ItemKey(r.get(1)?) }, title: r.get(2)?, at: r.get(3)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every history row, newest first (backup).
pub(crate) fn all_history(conn: &Connection) -> Result<Vec<HistoryEntry>> {
    let mut stmt = conn.prepare_cached("SELECT source_id, kind, item_key, title, at FROM history ORDER BY at DESC, id DESC")?;
    let rows = stmt.query_map([], |r| {
        Ok(HistoryEntry {
            item: ItemRef { source: r.get(0)?, kind: kind_from_sql(&r.get::<_, String>(1)?)?, key: ItemKey(r.get(2)?) },
            title: r.get(3)?,
            at: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ------------------------------------------------------------------ reminders

pub(crate) const REMINDER_COLS: &str =
    "id, source_id, channel_key, channel_name, title, start, stop, lead_s, state, mode, offset_s, open_in, rule_id";

pub(crate) fn reminder_from_row(r: &Row<'_>) -> rusqlite::Result<Reminder> {
    Ok(Reminder {
        id: r.get(0)?,
        channel: ItemRef { source: r.get(1)?, kind: ItemKind::Channel, key: ItemKey(r.get(2)?) },
        channel_name: r.get(3)?,
        programme_title: r.get(4)?,
        start: r.get(5)?,
        stop: r.get(6)?,
        lead_s: r.get(7)?,
        state: enum_parse(&r.get::<_, String>(8)?).unwrap_or(ReminderState::Pending),
        mode: enum_parse(&r.get::<_, String>(9)?).unwrap_or_default(),
        offset_s: r.get(10)?,
        open_in: r.get::<_, Option<String>>(11)?.and_then(|s| enum_parse(&s)),
        rule_id: r.get(12)?,
    })
}

fn reminders_where(conn: &Connection, cond: &str, args: impl rusqlite::Params) -> Result<Vec<Reminder>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {REMINDER_COLS} FROM reminder WHERE {cond}"))?;
    let rows = stmt.query_map(args, reminder_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Longest lead of a notification (a day).
pub const MAX_LEAD_S: i64 = 86_400;
/// A start offset goes at most this far before the start…
pub const MIN_OFFSET_S: i64 = -3600;
/// …and this far after it.
pub const MAX_OFFSET_S: i64 = 12 * 3600;

fn check_reminder_times(lead_s: i64, offset_s: i64) -> Result<()> {
    if lead_s != Reminder::SWITCH_LEAD && !(0..=MAX_LEAD_S).contains(&lead_s) {
        return Err(Error::Other("a reminder's lead is 0 to 24 hours".into()));
    }
    if !(MIN_OFFSET_S..=MAX_OFFSET_S).contains(&offset_s) {
        return Err(Error::Other("a reminder's start offset is −1 to +12 hours".into()));
    }
    Ok(())
}

/// A reminder's target (start + offset) must fall before the programme ends: one after it would
/// be dismissed unseen ("30 min in" on a 15-minute bulletin).
fn check_reminder_target(r: &Reminder, offset_s: i64) -> Result<()> {
    let r = Reminder { offset_s, ..r.clone() };
    if crate::reminders::target_at(&r) >= crate::reminders::effective_stop(&r) {
        return Err(Error::Other("a reminder's start offset must fall before the programme ends".into()));
    }
    Ok(())
}

/// A row's `(lead_s, mode)` as stored: a switch always carries both markers (`mode = 'switch'`
/// and `lead_s = Reminder::SWITCH_LEAD`), a notification neither (a switch made a note again
/// gets the default lead), so every reader — `Reminder::switches`, SQL on either column — agrees.
fn stored_mode(lead_s: i64, mode: ReminderMode) -> (i64, ReminderMode) {
    match (mode, lead_s) {
        (ReminderMode::Switch, _) | (_, Reminder::SWITCH_LEAD) => (Reminder::SWITCH_LEAD, ReminderMode::Switch),
        (ReminderMode::Notify, l) => (l, ReminderMode::Notify),
    }
}

/// Insert (unique per channel+start; returns existing id if present; fingerprint computed
/// inside). A switch is stored with both markers ([`stored_mode`]).
pub fn add_reminder(conn: &Connection, r: &Reminder) -> Result<ReminderId> {
    if r.channel.kind != ItemKind::Channel {
        return Err(Error::Other("reminders are for channels only".into()));
    }
    check_reminder_times(r.lead_s, r.offset_s)?;
    // Following's offset is the rule's ("skip the build-up"): its rows are never refused here,
    // or one short airing would fail the whole evaluation
    if r.rule_id.is_none() {
        check_reminder_target(r, r.offset_s)?;
    }
    let (lead_s, mode) = stored_mode(r.lead_s, r.mode);
    let fp = fingerprint_of(conn, &r.channel)?;
    conn.execute(
        "INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop, lead_s, state, fp, mode, offset_s, open_in, rule_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, '{}'), ?10, ?11, ?12, ?13)
         ON CONFLICT(source_id, channel_key, start) DO NOTHING",
        params![
            r.channel.source,
            r.channel.key.as_str(),
            r.channel_name,
            r.programme_title,
            r.start,
            r.stop,
            lead_s,
            enum_str(&r.state),
            fp_opt(&fp),
            enum_str(&mode),
            r.offset_s,
            r.open_in.as_ref().map(enum_str),
            r.rule_id
        ],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM reminder WHERE source_id = ?1 AND channel_key = ?2 AND start = ?3",
        params![r.channel.source, r.channel.key.as_str(), r.start],
        |row| row.get(0),
    )?)
}

/// Remove a reminder — the ONLY delete path (B6): Settings, the guide's `r` toggle, Cancel
/// toasts, MCP and maintenance all come here, so Following learns in the same transaction that
/// the user skipped a match it made (`store::follow::on_reminder_removed`).
pub fn remove_reminder(conn: &Connection, id: ReminderId, why: crate::model::RemoveWhy) -> Result<()> {
    crate::store::follow::on_reminder_removed(conn, id, why)?;
    conn.execute("DELETE FROM reminder WHERE id = ?1", [id])?;
    Ok(())
}

/// Move the pending reminders of one channel by `delta_s` (a time-shift change, B8; called by
/// `store::retime::channel_shift` in the same transaction). Returns how many moved.
pub fn retime_reminders(conn: &Connection, source: SourceId, channel_key: &str, delta_s: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE reminder SET start = start + ?3, stop = stop + ?3 WHERE source_id = ?1 AND channel_key = ?2 AND state = 'pending'",
        params![source, channel_key, delta_s],
    )?)
}

pub fn set_reminder_state(conn: &Connection, id: ReminderId, state: ReminderState) -> Result<()> {
    if conn.execute("UPDATE reminder SET state = ?2 WHERE id = ?1", params![id, enum_str(&state)])? == 0 {
        return Err(sql::not_found("reminder", id));
    }
    Ok(())
}

/// Pending reminders with `start - lead_s >= since`, ordered by fire time.
pub fn pending_reminders(conn: &Connection, since: Timestamp) -> Result<Vec<Reminder>> {
    reminders_where(conn, "state = 'pending' AND start - lead_s >= ?1 ORDER BY start - lead_s, id", [since])
}

/// Every reminder of a channel (any state), by start.
pub fn reminders_for_channel(conn: &Connection, channel: &ItemRef) -> Result<Vec<Reminder>> {
    reminders_where(conn, "source_id = ?1 AND channel_key = ?2 ORDER BY start, id", params![channel.source, channel.key.as_str()])
}

/// Every pending reminder (Settings ▸ Reminders), soonest first.
pub fn all_reminders(conn: &Connection) -> Result<Vec<Reminder>> {
    reminders_where(conn, "state = 'pending' ORDER BY start, id", [])
}

/// Make reminder `id` an automatic switch (DESIGN §30.8): `mode = 'switch'`, pending again. An
/// agent's `schedule_switch` on a programme that already had a notification reminder (rows are
/// unique per channel + start). A rule's reminder detaches from its rule (B6 point 5).
pub fn set_reminder_switch(conn: &Connection, id: ReminderId) -> Result<()> {
    edit_reminder(conn, id, &ReminderEdit { mode: Some(ReminderMode::Switch), ..Default::default() })?;
    set_reminder_state(conn, id, ReminderState::Pending)
}

/// A change to one reminder (`None` = unchanged).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReminderEdit {
    pub lead_s: Option<i64>,
    pub mode: Option<ReminderMode>,
    pub offset_s: Option<i64>,
    /// `Some(None)` = back to the setting.
    pub open_in: Option<Option<crate::model::OpenIn>>,
}

/// Edit reminder `id` (Settings ▸ Reminders, MCP `reminders`); returns the row as it is now.
/// Changing a rule reminder's lead or mode makes it the user's: it detaches from its rule first
/// (`store::follow::on_reminder_edited`, B6 point 5), so the next evaluation leaves it alone.
/// A fired reminder whose new time is still ahead is pending again.
pub fn edit_reminder(conn: &Connection, id: ReminderId, e: &ReminderEdit) -> Result<Reminder> {
    let Some(old) = reminder(conn, id)? else { return Err(sql::not_found("reminder", id)) };
    let (old_lead, old_mode) = stored_mode(old.lead_s, old.mode);
    let mode = e.mode.unwrap_or(old_mode);
    // a switch made a note again starts from the default lead
    let lead_s = e.lead_s.unwrap_or(if old_lead == Reminder::SWITCH_LEAD { 0 } else { old_lead });
    let offset_s = e.offset_s.unwrap_or(old.offset_s);
    check_reminder_times(lead_s, offset_s)?;
    if e.offset_s.is_some_and(|o| o != old.offset_s) {
        check_reminder_target(&old, offset_s)?;
    }
    let (lead_s, mode) = stored_mode(lead_s, mode);
    let open_in = e.open_in.unwrap_or(old.open_in);
    if old.rule_id.is_some() && (lead_s != old_lead || mode != old_mode || offset_s != old.offset_s) {
        crate::store::follow::on_reminder_edited(conn, id)?;
    }
    conn.execute(
        "UPDATE reminder SET lead_s = ?2, mode = ?3, offset_s = ?4, open_in = ?5 WHERE id = ?1",
        params![id, lead_s, enum_str(&mode), offset_s, open_in.as_ref().map(enum_str)],
    )?;
    let Some(now_row) = reminder(conn, id)? else { return Err(sql::not_found("reminder", id)) };
    if now_row.state == ReminderState::Fired && crate::reminders::arming::fire_at(&now_row, 0) > crate::model::now() {
        set_reminder_state(conn, id, ReminderState::Pending)?;
        return Ok(Reminder { state: ReminderState::Pending, ..now_row });
    }
    Ok(now_row)
}

/// Pending automatic switches (`Reminder::switches`), soonest first.
pub fn pending_switches(conn: &Connection) -> Result<Vec<Reminder>> {
    reminders_where(conn, "state = 'pending' AND (mode = 'switch' OR lead_s = ?1) ORDER BY start, id", [Reminder::SWITCH_LEAD])
}

/// Pending reminders whose programme overlaps `[from, to)` (clash checks before adding one:
/// `reminders::clashes_with`).
pub fn reminders_overlapping(conn: &Connection, from: Timestamp, to: Timestamp) -> Result<Vec<Reminder>> {
    reminders_where(
        conn,
        "state = 'pending' AND start + offset_s < ?2 AND (CASE WHEN stop > start THEN stop ELSE start + ?3 END) > ?1 ORDER BY start, id",
        params![from, to, crate::reminders::DEFAULT_DURATION_S],
    )
}

/// The channel is PIN-locked, by itself or through one of its groups (a Following switch to it
/// becomes a "needs your PIN" note while the session is locked).
pub fn channel_locked(conn: &Connection, channel: &ItemRef) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM item_override WHERE source_id = ?1 AND kind = 'channel' AND item_key = ?2 AND locked = 1)
                 OR EXISTS(SELECT 1 FROM channel c
                           JOIN channel_group cg ON cg.channel_id = c.id
                           JOIN grp g ON g.id = cg.group_id
                           JOIN group_override go ON go.source_id = g.source_id AND go.kind = g.kind AND go.group_key = g.key AND go.locked = 1
                           WHERE c.source_id = ?1 AND c.key = ?2)",
        )?
        .query_row(params![channel.source, channel.key.as_str()], |r| r.get(0))?)
}

/// One reminder by id (any state), if it exists.
pub fn reminder(conn: &Connection, id: ReminderId) -> Result<Option<Reminder>> {
    Ok(reminders_where(conn, "id = ?1", [id])?.into_iter().next())
}

/// Every reminder in any state (backup).
pub(crate) fn every_reminder(conn: &Connection) -> Result<Vec<Reminder>> {
    reminders_where(conn, "1 ORDER BY start, id", [])
}

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
    /// Its category (`fingerprint.norm_group`) is explicit content (DESIGN §20.3): the
    /// category as stored now and the user's word on it, or — the category gone too — its
    /// name. Settings ▸ Orphans leaves these out while explicit content is hidden.
    pub explicit: bool,
}

/// The user-data tables carrying `fp` + `orphaned_since`: (holder name, table, kind column
/// expression, key column, extra condition).
const HOLDERS: &[(&str, &str, &str, &str, &str)] = &[
    ("favourite", "fav_item", "kind", "item_key", ""),
    // Watchlists survive re-keys; placeholders have no ItemRef (DESIGN §42 §4).
    ("watchlist", "watchlist_item", "kind", "item_key", " AND source_id IS NOT NULL"),
    ("override", "item_override", "kind", "item_key", ""),
    ("progress", "watch_progress", "kind", "item_key", ""),
    ("reminder", "reminder", "'channel'", "channel_key", ""),
    ("epg", "channel_epg_map", "'channel'", "channel_key", " AND method = 'manual'"),
    // "Not the same title" / "Always play from" (DESIGN §29.11)
    ("split, pin or join", "title_link", "kind", "item_key", ""),
    // Up next rows still to play (DESIGN §32): re-keyed items reattach, vanished ones list
    // under Orphans (played, skipped and failed rows are pruned by age instead)
    ("queue", "queue_item", "kind", "item_key", " AND state IN ('pending', 'current')"),
];

fn holder_name(s: &str) -> &'static str {
    HOLDERS.iter().find(|h| h.0 == s).map(|h| h.0).unwrap_or("other")
}

pub fn orphans(conn: &Connection) -> Result<Vec<Orphan>> {
    let sql = HOLDERS
        .iter()
        .map(|(name, table, kind, key, extra)| {
            format!("SELECT source_id, {kind}, {key}, fp, orphaned_since, '{name}' FROM {table} WHERE orphaned_since IS NOT NULL{extra}")
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            ItemRef { source: r.get(0)?, kind: kind_from_sql(&r.get::<_, String>(1)?)?, key: ItemKey(r.get(2)?) },
            fp_from_json(&r.get::<_, String>(3)?),
            r.get::<_, Timestamp>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    let mut out: Vec<Orphan> = Vec::new();
    let mut index: HashMap<ItemRef, usize> = HashMap::new();
    for row in rows {
        let (item, fp, since, holder) = row?;
        let holder = holder_name(&holder);
        match index.get(&item) {
            Some(&i) => {
                let o = &mut out[i];
                o.since = o.since.min(since);
                if o.fingerprint == Fingerprint::default() {
                    o.fingerprint = fp;
                }
                if !o.holders.contains(&holder) {
                    o.holders.push(holder);
                }
            }
            None => {
                index.insert(item.clone(), out.len());
                out.push(Orphan { item, fingerprint: fp, since, holders: vec![holder], explicit: false });
            }
        }
    }
    for o in &mut out {
        o.explicit = orphan_explicit(conn, o)?;
    }
    out.sort_by(|a, b| b.since.cmp(&a.since).then_with(|| a.item.cmp(&b.item)));
    Ok(out)
}

/// An orphan's category is explicit content ([`Orphan::explicit`]).
fn orphan_explicit(conn: &Connection, o: &Orphan) -> Result<bool> {
    let Some(group) = o.fingerprint.norm_group.as_deref() else { return Ok(false) };
    let kind = if o.item.kind == ItemKind::Episode { ItemKind::Series } else { o.item.kind };
    let key = format!("{}:{group}", kind.as_str());
    let stored: Option<bool> = conn
        .prepare_cached(
            "SELECT COALESCE((SELECT o.explicit FROM group_override o WHERE o.source_id = ?1 AND o.kind = ?2 AND o.group_key = ?3),
                             (SELECT g.explicit > 0 FROM grp g WHERE g.source_id = ?1 AND g.kind = ?2 AND g.key = ?3))",
        )?
        .query_row(params![o.item.source, kind.as_str(), key], |r| r.get(0))?;
    Ok(stored.unwrap_or_else(|| crate::sources::explicit::is_explicit_name(group)))
}

/// `(table, key column, extra condition)` of the holders storing data for items of `kind`.
pub(crate) fn holders_for(kind: ItemKind) -> impl Iterator<Item = (&'static str, &'static str, String)> {
    HOLDERS.iter().filter_map(move |(_, table, kcol, key, extra)| {
        if *kcol == "kind" {
            Some((*table, *key, format!(" AND kind = '{}'{extra}", kind.as_str())))
        } else if kind == ItemKind::Channel {
            Some((*table, *key, extra.to_string()))
        } else {
            None
        }
    })
}

/// Delete all user data for an orphan.
pub fn forget_orphan(conn: &mut Connection, item: &ItemRef) -> Result<()> {
    let tx = conn.transaction()?;
    for (table, key, extra) in holders_for(item.kind) {
        if table == "watchlist_item" {
            tx.execute(&format!("UPDATE {table} SET source_id=NULL,item_key=NULL,orphaned_since=NULL WHERE source_id=?1 AND {key}=?2 AND orphaned_since IS NOT NULL{extra}"), params![item.source, item.key.as_str()])?;
            continue;
        }
        tx.execute(
            &format!("DELETE FROM {table} WHERE source_id = ?1 AND {key} = ?2 AND orphaned_since IS NOT NULL{extra}"),
            params![item.source, item.key.as_str()],
        )?;
    }
    crate::store::watchlist::compact_positions(&tx)?;
    tx.commit()?;
    Ok(())
}

/// Mark every user row of `item` orphaned (keeps the earliest `orphaned_since`). Returns true
/// when at least one row was not orphaned before.
pub(crate) fn mark_orphaned(conn: &Connection, item: &ItemRef, now: Timestamp) -> Result<bool> {
    let mut newly = false;
    for (table, key, extra) in holders_for(item.kind) {
        let n = conn.execute(
            &format!("UPDATE {table} SET orphaned_since = ?3 WHERE source_id = ?1 AND {key} = ?2 AND orphaned_since IS NULL{extra}"),
            params![item.source, item.key.as_str(), now],
        )?;
        newly |= n > 0;
    }
    Ok(newly)
}

/// The item exists: clear `orphaned_since` and refresh the stored fingerprint. Returns true if
/// any row was orphaned before.
pub(crate) fn mark_attached(conn: &Connection, item: &ItemRef, fp: &Fingerprint) -> Result<bool> {
    crate::store::watchlist::repair_attached(conn, item, item)?;
    let fp = fp_opt(fp);
    let mut was_orphan = false;
    for (table, key, extra) in holders_for(item.kind) {
        was_orphan |= conn.execute(
            &format!("UPDATE {table} SET orphaned_since = NULL WHERE source_id = ?1 AND {key} = ?2 AND orphaned_since IS NOT NULL{extra}"),
            params![item.source, item.key.as_str()],
        )? > 0;
        if fp.is_some() {
            conn.execute(
                &format!("UPDATE {table} SET fp = ?3 WHERE source_id = ?1 AND {key} = ?2 AND fp IS NOT ?3{extra}"),
                params![item.source, item.key.as_str(), fp],
            )?;
        }
    }
    Ok(was_orphan)
}

/// Re-point all user rows of `from` to `to` (same kind), un-orphaned, with `to`'s fingerprint.
/// Where `to` already has a row in a table, the existing row wins and `from`'s is dropped.
pub(crate) fn move_user_data(conn: &Connection, from: &ItemRef, to: &ItemRef) -> Result<()> {
    if from.kind != to.kind {
        return Err(Error::Other(format!("cannot attach a {} to a {}", from.kind.as_str(), to.kind.as_str())));
    }
    if from == to {
        return Ok(());
    }
    crate::store::watchlist::repair_attached(conn, from, to)?;
    let fp = fp_opt(&fingerprint_of(conn, to)?);
    if from.kind == ItemKind::Channel {
        // `channel_epg_map` is keyed per (channel, guide) whatever the method, so the generic
        // move below would lose `from`'s manual row to an automatic row of `to` on the same
        // guide. Manual mappings are never overwritten: drop those automatic rows first. A
        // channel has at most one manual mapping, so if `to` already has one it wins.
        let to_has_manual: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM channel_epg_map WHERE source_id = ?1 AND channel_key = ?2 AND method = 'manual')",
            params![to.source, to.key.as_str()],
            |r| r.get(0),
        )?;
        if to_has_manual {
            conn.execute(
                "DELETE FROM channel_epg_map WHERE source_id = ?1 AND channel_key = ?2 AND method = 'manual'",
                params![from.source, from.key.as_str()],
            )?;
        } else {
            conn.execute(
                "DELETE FROM channel_epg_map WHERE source_id = ?3 AND channel_key = ?4 AND method != 'manual'
                   AND epg_source_id IN (SELECT epg_source_id FROM channel_epg_map
                                         WHERE source_id = ?1 AND channel_key = ?2 AND method = 'manual')",
                params![from.source, from.key.as_str(), to.source, to.key.as_str()],
            )?;
        }
    }
    for (table, key, extra) in holders_for(from.kind) {
        conn.execute(
            &format!(
                "UPDATE OR IGNORE {table} SET source_id = ?3, {key} = ?4, fp = COALESCE(?5, fp), orphaned_since = NULL
                 WHERE source_id = ?1 AND {key} = ?2{extra}"
            ),
            params![from.source, from.key.as_str(), to.source, to.key.as_str(), fp],
        )?;
        conn.execute(&format!("DELETE FROM {table} WHERE source_id = ?1 AND {key} = ?2{extra}"), params![from.source, from.key.as_str()])?;
    }
    // history follows the item too (no fp: plain re-key); the failure cache is per stream
    conn.execute(
        "UPDATE history SET source_id = ?3, item_key = ?4 WHERE source_id = ?1 AND kind = ?5 AND item_key = ?2",
        params![from.source, from.key.as_str(), to.source, to.key.as_str(), from.kind.as_str()],
    )?;
    if from.kind == ItemKind::Channel {
        conn.execute("DELETE FROM stream_failure WHERE source_id = ?1 AND item_key = ?2", params![from.source, from.key.as_str()])?;
        crate::store::epg::resolve_channel(conn, to)?;
    }
    Ok(())
}

/// Manually re-point all user data of `from` to `to` (Orphans screen "Attach to…"); `to`'s
/// fingerprint is computed inside.
pub fn reassign(conn: &mut Connection, from: &ItemRef, to: &ItemRef) -> Result<()> {
    let tx = conn.transaction()?;
    move_user_data(&tx, from, to)?;
    tx.commit()?;
    Ok(())
}
