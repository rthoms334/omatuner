//! Watchlists: durable title entries, derived progress and the favourites move (DESIGN §42).
//! Reads are made on screen open / progress changes, never per frame. The grouped progress
//! query joins the small list's copy references to indexed catalogue and progress rows.

use crate::error::{Error, Result};
use crate::model::{ItemKey, ItemKind, ItemRef, SourceId, Timestamp, VodKind, WatchlistId, WatchlistItemId};
use crate::store::{copies, library::Library, sql, user};
pub use crate::watchlist::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;

pub const MAX_LISTS: usize = 100;
pub const MAX_ENTRIES: usize = 300;
pub const MAX_ADD: usize = 50;
const LIST_COLS: &str = "id,name,note,play_order,sort_index,since,is_default,created_at,created_by,last_played_at";
const ENTRY_COLS: &str = "id,list_id,position,kind,source_id,item_key,any_copy,season_from,season_to,title,year,tmdb_id,skipped,added_at,fp,orphaned_since,series_title,season,episode,match_title";
fn list_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<List> {
    Ok(List {
        id: r.get(0)?,
        name: r.get(1)?,
        note: r.get(2)?,
        play_order: if r.get::<_, String>(3)? == "any" { PlayOrder::Any } else { PlayOrder::Ordered },
        sort_index: r.get(4)?,
        since: r.get(5)?,
        is_default: r.get(6)?,
        created_at: r.get(7)?,
        created_by: r.get(8)?,
        last_played_at: r.get(9)?,
    })
}
fn entry_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let kind = sql::kind_from_sql(&r.get::<_, String>(3)?)?;
    let source: Option<SourceId> = r.get(4)?;
    Ok(Entry {
        id: r.get(0)?,
        list_id: r.get(1)?,
        position: r.get::<_, u32>(2)? as usize,
        kind,
        item: source.map(|source| -> rusqlite::Result<ItemRef> { Ok(ItemRef { source, kind, key: ItemKey(r.get(5)?) }) }).transpose()?,
        any_copy: r.get(6)?,
        season_from: r.get(7)?,
        season_to: r.get(8)?,
        title: r.get(9)?,
        year: r.get(10)?,
        tmdb_id: r.get(11)?,
        skipped: r.get(12)?,
        added_at: r.get(13)?,
        match_title: r.get(19)?,
        fingerprint: sql::fp_from_json(&r.get::<_, String>(14)?),
        orphaned_since: r.get(15)?,
        series_title: r.get(16)?,
        season: r.get(17)?,
        episode: r.get(18)?,
        explicit: serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(14)?)
            .ok()
            .and_then(|v| v["watchlist_explicit"].as_bool())
            .unwrap_or(false),
    })
}
pub fn lists(c: &Connection) -> Result<Vec<List>> {
    Ok(c.prepare_cached(&format!("SELECT {LIST_COLS} FROM watchlist ORDER BY is_default DESC,sort_index,id"))?
        .query_map([], list_row)?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn list(c: &Connection, id: WatchlistId) -> Result<Option<List>> {
    Ok(c.prepare_cached(&format!("SELECT {LIST_COLS} FROM watchlist WHERE id=?1"))?.query_row([id], list_row).optional()?)
}
pub fn entries(c: &Connection, id: WatchlistId) -> Result<Vec<Entry>> {
    Ok(c.prepare_cached(&format!("SELECT {ENTRY_COLS} FROM watchlist_item WHERE list_id=?1 ORDER BY position,id"))?
        .query_map([id], entry_row)?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn entry(c: &Connection, id: WatchlistItemId) -> Result<Option<Entry>> {
    Ok(c.prepare_cached(&format!("SELECT {ENTRY_COLS} FROM watchlist_item WHERE id=?1"))?.query_row([id], entry_row).optional()?)
}
fn required(c: &Connection, id: WatchlistId) -> Result<List> {
    list(c, id)?.ok_or_else(|| Error::other("Watchlist not found"))
}
fn valid_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() { Err(Error::other("Give the watchlist a name")) } else { Ok(name) }
}
// SQLite may reuse the largest deleted INTEGER PRIMARY KEY. Keep a high-water mark so a
// delayed Undo cannot mistake a newer list or entry for the one it originally changed.
fn next_id(c: &Connection, table: &str) -> Result<i64> {
    let key = format!("watchlists:sequence:{table}");
    let id: i64 = c.query_row(
        &format!("SELECT max(COALESCE((SELECT max(id) FROM {table}),0),COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1),0))+1"),
        [&key],
        |r| r.get(0),
    )?;
    c.execute("INSERT INTO meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, id.to_string()])?;
    Ok(id)
}
fn list_identity(c: &Connection, id: WatchlistId) -> Result<i64> {
    let key = format!("watchlist:identity:{id}");
    if let Some(identity) = c.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1", [&key], |r| r.get(0)).optional()? {
        return Ok(identity);
    }
    let identity: i64 =
        c.query_row("SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key='watchlists:sequence:identity'),0)+1", [], |r| r.get(0))?;
    for key in ["watchlists:sequence:identity".to_owned(), key] {
        c.execute(
            "INSERT INTO meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, identity.to_string()],
        )?;
    }
    Ok(identity)
}
pub fn create(c: &Connection, name: &str, note: Option<&str>, order: PlayOrder, by: &str, now: Timestamp) -> Result<WatchlistId> {
    sql::atomic(c, |c| {
        if c.query_row("SELECT count(*) FROM watchlist", [], |r| r.get::<_, i64>(0))? >= MAX_LISTS as i64 {
            return Err(Error::other("You can keep up to 100 watchlists"));
        }
        let id = WatchlistId(next_id(c, "watchlist")?);
        c.execute("INSERT INTO watchlist(id,name,note,play_order,sort_index,created_at,created_by) VALUES (?1,?2,?3,?4,(SELECT COALESCE(max(sort_index),-1)+1 FROM watchlist),?5,?6)",params![id,valid_name(name)?,note,order.as_str(),now,by])?;
        list_identity(c, id)?;
        Ok(id)
    })
}
pub fn default_list(c: &Connection) -> Result<WatchlistId> {
    if let Some(id) = c.query_row("SELECT id FROM watchlist WHERE is_default=1", [], |r| r.get(0)).optional()? {
        return Ok(id);
    }
    let id = create(c, "My list", None, PlayOrder::Any, "app", crate::model::now())?;
    c.execute("UPDATE watchlist SET is_default=1 WHERE id=?1", [id])?;
    Ok(id)
}
pub fn new_entry(c: &Connection, item: &ItemRef) -> Result<NewEntry> {
    if item.kind == ItemKind::Channel {
        return Err(Error::other("Watchlists hold films and series; use Up next or a reminder for live TV"));
    }
    if item.kind == ItemKind::Episode {
        return c.query_row(
            "SELECT e.title,s.clean_name,s.year,NULLIF(CAST(s.tmdb_id AS INTEGER),0),e.season,e.episode FROM episode e JOIN series s ON s.id=e.series_id WHERE e.source_id=?1 AND e.key=?2",
            params![item.source, item.key.as_str()],
            |r| Ok(NewEntry {
                item: Some(item.clone()), kind: item.kind, title: r.get(0)?,
                series_title: r.get(1)?, year: r.get(2)?, tmdb_id: r.get(3)?,
                season: r.get(4)?, episode: r.get(5)?, season_from: None, season_to: None,
            }),
        ).optional()?.ok_or_else(|| Error::other("Title not found"));
    }
    c.query_row(
        &format!("SELECT COALESCE(o.rename,t.clean_name),t.year,NULLIF(CAST(t.tmdb_id AS INTEGER),0) FROM {} t LEFT JOIN item_override o ON o.source_id=t.source_id AND o.kind=?3 AND o.item_key=t.key WHERE t.source_id=?1 AND t.key=?2",item.kind.as_str()),
        params![item.source,item.key.as_str(),item.kind.as_str()],
        |r| Ok(NewEntry {
            item: Some(item.clone()), kind: item.kind, title: r.get(0)?, year: r.get(1)?, tmdb_id: r.get(2)?,
            series_title: None, season: None, episode: None, season_from: None, season_to: None,
        }),
    ).optional()?.ok_or_else(|| Error::other("Title not found"))
}

/// Stack members include all copies for progress, even when the entry pins one for playback.
pub fn title_copies(c: &Connection, lib: &Library, item: &ItemRef) -> Result<Vec<ItemRef>> {
    if item.kind == ItemKind::Episode {
        let parent: Option<(SourceId, String, u32, u32)> = c
            .query_row(
                "SELECT s.source_id,s.key,e.season,e.episode FROM episode e JOIN series s ON s.id=e.series_id WHERE e.source_id=?1 AND e.key=?2",
                params![item.source, item.key.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((source, key, season, episode)) = parent else { return Ok(vec![item.clone()]) };
        let mut out = Vec::new();
        for p in title_copies(c, lib, &ItemRef { source, kind: ItemKind::Series, key: ItemKey(key) })? {
            if let Some(key)=c.query_row("SELECT e.key FROM episode e JOIN series s ON s.id=e.series_id WHERE s.source_id=?1 AND s.key=?2 AND e.season=?3 AND e.episode=?4",params![p.source,p.key.as_str(),season,episode],|r|r.get::<_,String>(0)).optional()? {out.push(ItemRef{source:p.source,kind:ItemKind::Episode,key:ItemKey(key)});}
        }
        return Ok(if out.is_empty() { vec![item.clone()] } else { out });
    }
    let id: Option<i64> = c
        .query_row(&format!("SELECT id FROM {} WHERE source_id=?1 AND key=?2", item.kind.as_str()), params![item.source, item.key.as_str()], |r| {
            r.get(0)
        })
        .optional()?;
    let movies = item.kind == ItemKind::Movie;
    let vk = id.map(|id| if movies { VodKind::Movie(crate::model::MovieId(id)) } else { VodKind::Series(crate::model::SeriesId(id)) });
    let Some(i) = vk.and_then(|v| lib.vod.index_of(v)) else { return Ok(vec![item.clone()]) };
    let rows = if movies { &lib.vod.movies } else { &lib.vod.series };
    Ok(lib.vod.stacks(movies).copies(i).iter().map(|&i| copies::vod_ref(&rows[i as usize])).collect())
}
/// The Xtream series copies behind `items` whose episodes were never loaded. Progress merges
/// every copy of a stacked title, so all of them are named, not only the one an entry pins.
/// Playlist series arrive with their episodes and are never listed.
pub fn series_to_fetch(c: &Connection, lib: &Library, items: &[ItemRef]) -> Result<Vec<crate::model::SeriesId>> {
    let mut ids = Vec::new();
    for item in items.iter().filter(|i| i.kind == ItemKind::Series) {
        for copy in title_copies(c, lib, item)? {
            let id: Option<crate::model::SeriesId> = c
                .query_row("SELECT id FROM series WHERE source_id=?1 AND key=?2", params![copy.source, copy.key.as_str()], |r| r.get(0))
                .optional()?;
            if let Some(id) = id
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
    }
    copies::unloaded_series(c, &ids)
}

/// Play order for one title, using the same pins, progress and provider-trouble rules as Up
/// next. Episode coordinates are mapped back onto that order for series entries.
fn planned_copies(c: &Connection, lib: &Library, item: &ItemRef, unlocked: bool, copy_order: crate::config::CopyOrder) -> Result<Vec<ItemRef>> {
    let parent = if item.kind == ItemKind::Episode {
        c.query_row(
            "SELECT s.source_id,s.key FROM episode e JOIN series s ON s.id=e.series_id WHERE e.source_id=?1 AND e.key=?2",
            params![item.source, item.key.as_str()],
            |r| Ok(ItemRef { source: r.get(0)?, kind: ItemKind::Series, key: ItemKey(r.get(1)?) }),
        )
        .optional()?
    } else {
        Some(item.clone())
    };
    let Some(parent) = parent else { return Ok(Vec::new()) };
    let movies = parent.kind == ItemKind::Movie;
    let id: Option<i64> = c
        .query_row(
            &format!("SELECT id FROM {} WHERE source_id=?1 AND key=?2", parent.kind.as_str()),
            params![parent.source, parent.key.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let vk = id.map(|id| if movies { VodKind::Movie(crate::model::MovieId(id)) } else { VodKind::Series(crate::model::SeriesId(id)) });
    let Some(index) = vk.and_then(|v| lib.vod.index_of(v)) else { return Ok(vec![parent]) };
    let plan = copies::plan(c, &lib.vod, movies, index, unlocked, copy_order, crate::model::now())?;
    let summaries = if movies { &lib.vod.movies } else { &lib.vod.series };
    Ok(plan.order.iter().map(|i| copies::vod_ref(&summaries[*i as usize])).collect())
}

fn same_title(c: &Connection, lib: &Library, a: &Entry, b: &NewEntry) -> Result<bool> {
    if a.kind != b.kind {
        return Ok(false);
    }
    if let (Some(x), Some(y)) = (&a.item, &b.item) {
        return Ok(title_copies(c, lib, x)?.contains(y));
    }
    let (a_title, b_title) = if a.kind == ItemKind::Episode {
        if a.season.zip(a.episode).is_none() || a.season != b.season || a.episode != b.episode {
            return Ok(false);
        }
        let (Some(a), Some(b)) = (&a.series_title, &b.series_title) else { return Ok(false) };
        (a, b)
    } else {
        (&a.title, &b.title)
    };
    if let (Some(x), Some(y)) = (a.tmdb_id.filter(|id| *id > 0), b.tmdb_id.filter(|id| *id > 0)) {
        return Ok(x == y);
    }
    Ok(a.year == b.year && crate::sources::identity::normalize_name(a_title) == crate::sources::identity::normalize_name(b_title))
}
fn valid_range(kind: ItemKind, from: Option<u32>, to: Option<u32>) -> bool {
    (kind == ItemKind::Series || (from.is_none() && to.is_none()))
        && matches!((from, to), (None, None) | (Some(_), Some(_)))
        && from.zip(to).is_none_or(|(f, t)| f <= t)
}
fn overlaps(a: Option<u32>, b: Option<u32>, c: Option<u32>, d: Option<u32>) -> bool {
    a.unwrap_or(0) <= d.unwrap_or(u32::MAX) && c.unwrap_or(0) <= b.unwrap_or(u32::MAX)
}
fn write_entry(c: &Connection, e: &Entry, keep_id: bool) -> Result<WatchlistItemId> {
    let mut fp = serde_json::to_value(&e.fingerprint).map_err(Error::other)?;
    fp["watchlist_explicit"] = e.explicit.into();
    let fp = serde_json::to_string(&fp).map_err(Error::other)?;
    let id = if keep_id { e.id } else { WatchlistItemId(next_id(c, "watchlist_item")?) };
    c.execute("INSERT INTO watchlist_item(id,list_id,position,kind,source_id,item_key,any_copy,season_from,season_to,title,year,tmdb_id,skipped,added_at,fp,orphaned_since,series_title,season,episode,match_title) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",params![id,e.list_id,e.position as i64,e.kind.as_str(),e.item.as_ref().map(|i|i.source),e.item.as_ref().map(|i|i.key.as_str()),e.any_copy,e.season_from,e.season_to,e.title,e.year,e.tmdb_id,e.skipped,e.added_at,fp,e.orphaned_since,e.series_title,e.season,e.episode,e.match_title])?;
    Ok(id)
}
pub fn add(c: &Connection, lib: &Library, id: WatchlistId, targets: &[NewEntry], at: Insert, now: Timestamp) -> Result<AddResult> {
    if targets.len() > MAX_ADD {
        return Err(Error::other("Add up to 50 entries at a time"));
    }
    sql::atomic(c, |c| {
        required(c, id)?;
        let mut have = entries(c, id)?;
        let mut pos = match at {
            Insert::End => have.len(),
            Insert::Index(n) => n.min(have.len()),
            Insert::Next => {
                view(c, lib, id, true, false)?.and_then(|v| v.next).and_then(|n| have.iter().position(|e| e.id == n)).map_or(have.len(), |n| n + 1)
            }
        };
        let mut out = AddResult::default();
        for (i, t) in targets.iter().enumerate() {
            let error = if t.kind == ItemKind::Channel {
                Some("Watchlists hold films and series; use Up next or a reminder for live TV")
            } else if !valid_range(t.kind, t.season_from, t.season_to) {
                Some("invalid_seasons")
            } else if t.kind == ItemKind::Episode
                && (t.series_title.as_deref().is_none_or(|s| s.trim().is_empty()) || t.season.zip(t.episode).is_none())
            {
                Some("episodes need series_title, season and episode")
            } else if have.len() >= MAX_ENTRIES {
                Some("full")
            } else {
                None
            };
            if let Some(error) = error {
                out.refused.push((i, error.into()));
                continue;
            }
            let mut duplicate = None;
            for e in &have {
                if same_title(c, lib, e, t)? && (t.kind != ItemKind::Series || overlaps(e.season_from, e.season_to, t.season_from, t.season_to)) {
                    duplicate = Some(if t.kind == ItemKind::Series { "overlapping_seasons" } else { "duplicate" });
                    break;
                }
            }
            if let Some(error) = duplicate {
                out.refused.push((i, error.into()));
                continue;
            }
            let mut e = Entry {
                id: WatchlistItemId(0),
                list_id: id,
                position: pos,
                kind: t.kind,
                item: t.item.clone(),
                any_copy: true,
                season_from: t.season_from,
                season_to: t.season_to,
                title: t.title.clone(),
                year: t.year,
                tmdb_id: t.tmdb_id,
                series_title: t.series_title.clone(),
                season: t.season,
                episode: t.episode,
                skipped: false,
                added_at: now,
                match_title: t.item.as_ref().map(|item| provider_title(c, item)).transpose()?.flatten(),
                fingerprint: t.item.as_ref().map(|x| user::fingerprint_of(c, x)).transpose()?.unwrap_or_default(),
                orphaned_since: None,
                explicit: t.item.as_ref().map(|i| crate::store::live::item_explicit(c, i)).transpose()?.unwrap_or(false),
            };
            e.id = write_entry(c, &e, false)?;
            out.added.push(e.id);
            have.insert(pos, e);
            pos += 1;
        }
        dense(c, &have)?;
        Ok(out)
    })
}
fn dense(c: &Connection, rows: &[Entry]) -> Result<()> {
    for (i, e) in rows.iter().enumerate() {
        c.execute("UPDATE watchlist_item SET position=?2 WHERE id=?1", params![e.id, i as i64])?;
    }
    Ok(())
}
/// Shared orphan deletion also removes entries; keep their positions dense and drop chips
/// whose source entry was forgotten (DESIGN §42 §4).
pub(crate) fn compact_positions(c: &Connection) -> Result<()> {
    c.execute_batch("WITH ordered AS (SELECT id,row_number() OVER (PARTITION BY list_id ORDER BY position,id)-1 AS pos FROM watchlist_item) UPDATE watchlist_item SET position=(SELECT pos FROM ordered WHERE ordered.id=watchlist_item.id); UPDATE queue_item SET watchlist_item_id=NULL WHERE watchlist_item_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM watchlist_item WHERE id=queue_item.watchlist_item_id);")?;
    Ok(())
}

fn capture(c: &Connection, id: WatchlistId, action: UndoAction) -> Result<Undo> {
    Ok(Undo { list: required(c, id)?, identity: list_identity(c, id)?, action })
}
fn queue_chips(c: &Connection, entries: &[Entry]) -> Result<Vec<UndoChip>> {
    let ids = sql::json_ids(entries.iter().map(|e| e.id.0));
    Ok(c.prepare_cached(
        "SELECT id,watchlist_item_id,source_id,kind,item_key,added_at FROM queue_item WHERE watchlist_item_id IN (SELECT value FROM json_each(?1))",
    )?
    .query_map([ids], |r| {
        Ok(UndoChip {
            id: r.get(0)?,
            entry: r.get(1)?,
            item: ItemRef { source: r.get(2)?, kind: sql::kind_from_sql(&r.get::<_, String>(3)?)?, key: ItemKey(r.get(4)?) },
            added_at: r.get(5)?,
        })
    })?
    .collect::<rusqlite::Result<_>>()?)
}
pub fn move_entries(c: &Connection, id: WatchlistId, ids: &[WatchlistItemId], to: usize) -> Result<()> {
    sql::atomic(c, |c| {
        let all = entries(c, id)?;
        let selected: Vec<_> = all.iter().filter(|e| ids.contains(&e.id)).cloned().collect();
        let mut rest: Vec<_> = all.into_iter().filter(|e| !ids.contains(&e.id)).collect();
        let to = to.min(rest.len());
        rest.splice(to..to, selected);
        dense(c, &rest)
    })
}
pub fn remove(c: &Connection, id: WatchlistId, ids: &[WatchlistItemId]) -> Result<Undo> {
    sql::atomic(c, |c| {
        let removed: Vec<_> = entries(c, id)?.into_iter().filter(|e| ids.contains(&e.id)).collect();
        let chips = queue_chips(c, &removed)?;
        let u = capture(c, id, UndoAction::Removed { entries: removed.clone(), chips })?;
        for e in &removed {
            consume_next(c, id, e.id)?;
            c.execute("DELETE FROM watchlist_item WHERE list_id=?1 AND id=?2", params![id, e.id])?;
            c.execute("UPDATE queue_item SET watchlist_item_id=NULL WHERE watchlist_item_id=?1", [e.id])?;
        }
        dense(c, &entries(c, id)?)?;
        Ok(u)
    })
}
pub fn skip(c: &Connection, id: WatchlistId, ids: &[WatchlistItemId], on: bool) -> Result<()> {
    sql::atomic(c, |c| {
        for e in ids {
            c.execute("UPDATE watchlist_item SET skipped=?3 WHERE list_id=?1 AND id=?2", params![id, e, on])?;
        }
        Ok(())
    })
}
pub fn restart(c: &Connection, id: WatchlistId, now: Timestamp) -> Result<Undo> {
    sql::atomic(c, |c| {
        let since = required(c, id)?.since;
        let skipped = entries(c, id)?.into_iter().filter(|e| e.skipped).map(|e| e.id).collect();
        let u = capture(c, id, UndoAction::Restarted { since, boundary: now, skipped })?;
        c.execute("UPDATE watchlist SET since=?2 WHERE id=?1", params![id, now])?;
        c.execute("UPDATE watchlist_item SET skipped=0 WHERE list_id=?1", [id])?;
        c.execute("DELETE FROM meta WHERE key=?1", [format!("watchlist:next:{id}")])?;
        Ok(u)
    })
}
pub fn rename(c: &Connection, id: WatchlistId, name: &str, note: Option<&str>) -> Result<()> {
    if required(c, id)?.is_default {
        return Err(Error::other("My list cannot be renamed"));
    }
    c.execute("UPDATE watchlist SET name=?2,note=?3 WHERE id=?1", params![id, valid_name(name)?, note])?;
    Ok(())
}
pub fn delete(c: &Connection, id: WatchlistId) -> Result<Undo> {
    sql::atomic(c, |c| {
        let entries = entries(c, id)?;
        let chips = queue_chips(c, &entries)?;
        let u = capture(c, id, UndoAction::Deleted { entries, chips })?;
        if u.list.is_default {
            return Err(Error::other("My list cannot be deleted"));
        }
        c.execute("UPDATE queue_item SET watchlist_item_id=NULL WHERE watchlist_item_id IN (SELECT id FROM watchlist_item WHERE list_id=?1)", [id])?;
        c.execute(
            "DELETE FROM meta WHERE key='queue:context_agent' AND EXISTS(SELECT 1 FROM meta WHERE key='queue:context' AND value=?1)",
            [id.to_string()],
        )?;
        c.execute("DELETE FROM meta WHERE key='queue:context' AND value=?1", [id.to_string()])?;
        c.execute("DELETE FROM meta WHERE key IN (?1,?2)", params![format!("watchlist:next:{id}"), format!("watchlist:show_watched:{id}")])?;
        c.execute("DELETE FROM meta WHERE key LIKE 'watchlist:favourites:%' AND value=?1", [id.to_string()])?;
        c.execute("DELETE FROM watchlist WHERE id=?1", [id])?;
        c.execute("DELETE FROM meta WHERE key=?1", [format!("watchlist:identity:{id}")])?;
        Ok(u)
    })
}
/// Reinsert only the removed rows into the current order, retaining subsequent edits.
fn restore_entries(c: &Connection, id: WatchlistId, removed: &[Entry], chips: &[UndoChip]) -> Result<()> {
    let mut current = entries(c, id)?;
    let mut remap = HashMap::new();
    for old in removed {
        let mut row = old.clone();
        row.list_id = id;
        if let Some(item) = &row.item
            && !c.query_row("SELECT EXISTS(SELECT 1 FROM source WHERE id=?1)", [item.source], |r| r.get::<_, bool>(0))?
        {
            row.item = None;
        }
        row.id = write_entry(c, &row, entry(c, old.id)?.is_none())?;
        remap.insert(old.id, row.id);
        current.insert(row.position.min(current.len()), row);
    }
    dense(c, &current)?;
    for chip in chips {
        if let Some(id) = remap.get(&chip.entry) {
            c.execute("UPDATE queue_item SET watchlist_item_id=?1 WHERE id=?2 AND watchlist_item_id IS NULL AND source_id=?3 AND kind=?4 AND item_key=?5 AND added_at=?6",
                params![id, chip.id, chip.item.source, chip.item.kind.as_str(), chip.item.key.as_str(), chip.added_at])?;
        }
    }
    Ok(())
}
pub fn undo(c: &Connection, u: &Undo) -> Result<()> {
    sql::atomic(c, |c| {
        let l = &u.list;
        if let UndoAction::Deleted { entries, chips } = &u.action {
            // A later list may own either the old ID or name; never overwrite it.
            if c.query_row("SELECT EXISTS(SELECT 1 FROM watchlist WHERE name=?1 COLLATE NOCASE)", [&l.name], |r| r.get::<_, bool>(0))? {
                return Err(Error::other("A watchlist already has this name. Rename it before undoing the deletion"));
            }
            let id = if list(c, l.id)?.is_none() { l.id } else { WatchlistId(next_id(c, "watchlist")?) };
            c.execute("INSERT INTO watchlist(id,name,note,play_order,sort_index,since,is_default,created_at,created_by,last_played_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![id,l.name,l.note,l.play_order.as_str(),l.sort_index,l.since,l.is_default,l.created_at,l.created_by,l.last_played_at])?;
            list_identity(c, id)?;
            restore_entries(c, id, entries, chips)?;
            return Ok(());
        }
        let same_list = list(c, l.id)?.is_some()
            && c.query_row(
                "SELECT CAST(value AS INTEGER)=?2 FROM meta WHERE key=?1",
                params![format!("watchlist:identity:{}", l.id), u.identity],
                |r| r.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        if !same_list {
            return Err(Error::other("This watchlist has since been deleted"));
        }
        match &u.action {
            UndoAction::Added(ids) => {
                for id in ids {
                    consume_next(c, l.id, *id)?;
                    c.execute("UPDATE queue_item SET watchlist_item_id=NULL WHERE watchlist_item_id=?1 AND EXISTS(SELECT 1 FROM watchlist_item WHERE id=?1 AND list_id=?2)",params![id,l.id])?;
                    c.execute("DELETE FROM watchlist_item WHERE id=?1 AND list_id=?2", params![id, l.id])?;
                }
                dense(c, &entries(c, l.id)?)?;
            }
            UndoAction::Removed { entries, chips } => restore_entries(c, l.id, entries, chips)?,
            UndoAction::Restarted { since, boundary, skipped } => {
                // A later Start again owns its own boundary; an older toast must not reset it.
                if c.execute("UPDATE watchlist SET since=?2 WHERE id=?1 AND since=?3", params![l.id, since, boundary])? != 0 {
                    for id in skipped {
                        c.execute("UPDATE watchlist_item SET skipped=1 WHERE id=?1 AND list_id=?2", params![id, l.id])?;
                    }
                }
            }
            UndoAction::Deleted { .. } => unreachable!(),
        }
        Ok(())
    })
}
pub fn set_order(c: &Connection, id: WatchlistId, order: PlayOrder) -> Result<()> {
    c.execute("UPDATE watchlist SET play_order=?2 WHERE id=?1", params![id, order.as_str()])?;
    Ok(())
}
pub fn set_seasons(c: &Connection, lib: &Library, id: WatchlistItemId, from: Option<u32>, to: Option<u32>) -> Result<()> {
    let e = entry(c, id)?.ok_or_else(|| Error::other("Entry not found"))?;
    if !valid_range(e.kind, from, to) {
        return Err(Error::other("invalid_seasons"));
    }
    let t = NewEntry {
        item: e.item.clone(),
        kind: e.kind,
        title: e.title.clone(),
        year: e.year,
        tmdb_id: e.tmdb_id,
        series_title: e.series_title.clone(),
        season: e.season,
        episode: e.episode,
        season_from: from,
        season_to: to,
    };
    for other in entries(c, e.list_id)? {
        if other.id != id && same_title(c, lib, &other, &t)? && overlaps(other.season_from, other.season_to, from, to) {
            return Err(Error::other("overlapping_seasons"));
        }
    }
    c.execute("UPDATE watchlist_item SET season_from=?2,season_to=?3 WHERE id=?1", params![id, from, to])?;
    Ok(())
}
pub fn attach(c: &Connection, id: WatchlistItemId, item: &ItemRef) -> Result<()> {
    let mut e = entry(c, id)?.ok_or_else(|| Error::other("Entry not found"))?;
    if e.kind != item.kind {
        return Err(Error::other("Choose the same kind of title"));
    }
    if item.kind == ItemKind::Episode || (e.year.is_none() && e.title == e.fingerprint.norm_name) {
        let identity = new_entry(c, item)?;
        e.title = identity.title;
        e.series_title = identity.series_title;
        e.year = identity.year;
        e.tmdb_id = identity.tmdb_id;
        e.season = identity.season;
        e.episode = identity.episode;
    }
    let fp = user::fingerprint_of(c, item)?;
    c.execute(
        "UPDATE watchlist_item SET source_id=?2,item_key=?3,fp=?4,orphaned_since=NULL,
         title=?5,series_title=?6,year=?7,tmdb_id=?8,season=?9,episode=?10,match_title=?11 WHERE id=?1",
        params![
            id,
            item.source,
            item.key.as_str(),
            sql::fp_to_json(&fp),
            e.title,
            e.series_title,
            e.year,
            e.tmdb_id,
            e.season,
            e.episode,
            provider_title(c, item)?
        ],
    )?;
    Ok(())
}

pub fn pin(c: &Connection, id: WatchlistItemId, item: &ItemRef) -> Result<()> {
    sql::atomic(c, |c| {
        attach(c, id, item)?;
        c.execute("UPDATE watchlist_item SET any_copy=0 WHERE id=?1", [id])?;
        Ok(())
    })
}
pub fn set_show_watched(c: &Connection, id: WatchlistId, show: bool) -> Result<()> {
    c.execute(
        "INSERT INTO meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![format!("watchlist:show_watched:{id}"), if show { "1" } else { "0" }],
    )?;
    Ok(())
}
pub fn reorder_lists(c: &Connection, ids: &[WatchlistId]) -> Result<()> {
    sql::atomic(c, |c| {
        // Hidden and finished lists keep their slots when a visible group is reordered.
        // My list is pinned, even if a caller includes it in the requested order.
        let all = lists(c)?;
        let mut order = Vec::new();
        for id in ids {
            if all.iter().any(|l| l.id == *id && !l.is_default) && !order.contains(id) {
                order.push(*id);
            }
        }
        let mut moved = order.iter();
        for (i, l) in all.iter().enumerate() {
            let id = if order.contains(&l.id) { *moved.next().expect("one id per selected slot") } else { l.id };
            c.execute("UPDATE watchlist SET sort_index=?2 WHERE id=?1", params![id, i as i64])?;
        }
        Ok(())
    })
}
pub fn duplicate(c: &Connection, id: WatchlistId, name: &str, now: Timestamp) -> Result<WatchlistId> {
    sql::atomic(c, |c| {
        let l = required(c, id)?;
        let new = create(c, name, l.note.as_deref(), l.play_order, "app", now)?;
        c.execute("UPDATE watchlist SET since=?2 WHERE id=?1", params![new, l.since])?;
        for mut e in entries(c, id)? {
            e.list_id = new;
            write_entry(c, &e, false)?;
        }
        Ok(new)
    })
}
pub fn played(c: &Connection, id: WatchlistId, now: Timestamp) -> Result<()> {
    c.execute("UPDATE watchlist SET last_played_at=?2 WHERE id=?1", params![id, now])?;
    Ok(())
}
pub fn memberships(c: &Connection, lib: &Library, item: &ItemRef) -> Result<Vec<(List, usize, usize)>> {
    let target = new_entry(c, item)?;
    let mut out = Vec::new();
    for l in lists(c)? {
        let Some(v) = view(c, lib, l.id, true, false)? else { continue };
        for (i, e) in v.entries.iter().enumerate() {
            if same_title(c, lib, &e.entry, &target)? {
                out.push((l.clone(), i + 1, v.entries.len()));
                break;
            }
        }
    }
    Ok(out)
}
pub fn toggle_my_list(c: &Connection, lib: &Library, item: &ItemRef, now: Timestamp) -> Result<(bool, Undo)> {
    sql::atomic(c, |c| {
        let id = default_list(c)?;
        let t = new_entry(c, item)?;
        let mut ids = Vec::new();
        for e in &entries(c, id)? {
            if same_title(c, lib, e, &t)? {
                ids.push(e.id)
            }
        }
        if ids.is_empty() {
            let result = add(c, lib, id, &[t], Insert::End, now)?;
            if let Some((_, why)) = result.refused.first() {
                return Err(Error::other(why.clone()));
            }
            Ok((true, capture(c, id, UndoAction::Added(result.added))?))
        } else {
            Ok((false, remove(c, id, &ids)?))
        }
    })
}

/// Matching memberships for the add/remove dialog, using title identity across copies.
/// A series range is matched exactly, so a second non-overlapping range remains separate.
pub fn matching_entries(
    c: &Connection,
    lib: &Library,
    list: WatchlistId,
    item: &ItemRef,
    from: Option<u32>,
    to: Option<u32>,
) -> Result<Vec<WatchlistItemId>> {
    let target = new_entry(c, item)?;
    let mut out = Vec::new();
    for e in entries(c, list)? {
        if same_title(c, lib, &e, &target)? && (item.kind != ItemKind::Series || (e.season_from == from && e.season_to == to)) {
            out.push(e.id)
        }
    }
    Ok(out)
}
/// A requested next entry beats an Any order pick. It is consumed when the queue accepts
/// that entry; reordering / watching elsewhere still changes every later choice.
pub fn make_next(c: &Connection, list: WatchlistId, id: WatchlistItemId) -> Result<()> {
    let e = entry(c, id)?.filter(|e| e.list_id == list).ok_or_else(|| Error::other("Entry not found"))?;
    sql::atomic(c, |c| {
        c.execute("UPDATE watchlist_item SET skipped=0 WHERE id=?1", [e.id])?;
        c.execute(
            "INSERT INTO meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![format!("watchlist:next:{list}"), id.to_string()],
        )?;
        Ok(())
    })
}
pub fn consume_next(c: &Connection, list: WatchlistId, id: WatchlistItemId) -> Result<()> {
    c.execute("DELETE FROM meta WHERE key=?1 AND value=?2", params![format!("watchlist:next:{list}"), id.to_string()])?;
    Ok(())
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Unit {
    source: i64,
    key: String,
    season: u32,
    episode: u32,
    finished: bool,
    position: f64,
    duration: Option<f64>,
    updated: i64,
}
/// The small references relation is built from the library's stack indexes. Progress itself
/// is read once per list and grouped by episode coordinates across copies (DESIGN §42 §4).
fn progress_units(c: &Connection, lib: &Library, l: &List, rows: &[Entry]) -> Result<HashMap<WatchlistItemId, Vec<Vec<Unit>>>> {
    let mut refs = Vec::new();
    for e in rows {
        if let Some(item) = &e.item {
            for item in title_copies(c, lib, item)? {
                refs.push(serde_json::json!([e.id.0, item.kind.as_str(), item.source.0, item.key.0, e.season_from, e.season_to]));
            }
        }
    }
    let json = serde_json::to_string(&refs).map_err(Error::other)?;
    let mut stmt = c.prepare_cached(
        "WITH refs AS (
           SELECT value->>0 AS wid,value->>1 AS kind,value->>2 AS source,value->>3 AS key,value->>4 AS sf,value->>5 AS st FROM json_each(?1)
         ), units AS (
           SELECT r.wid,m.source_id AS source,m.key,'movie' AS kind,0 AS season,0 AS episode,m.duration_s AS duration
             FROM refs r JOIN movie m ON r.kind='movie' AND m.source_id=r.source AND m.key=r.key
           UNION ALL
           SELECT r.wid,e.source_id,e.key,'episode',e.season,e.episode,e.duration_s
             FROM refs r JOIN episode e ON r.kind='episode' AND e.source_id=r.source AND e.key=r.key
           UNION ALL
           SELECT r.wid,e.source_id,e.key,'episode',e.season,e.episode,e.duration_s
             FROM refs r JOIN series s ON r.kind='series' AND s.source_id=r.source AND s.key=r.key
             JOIN episode e ON e.series_id=s.id WHERE e.season BETWEEN COALESCE(r.sf,1) AND COALESCE(r.st,2147483647)
         )
         SELECT u.wid,json_group_array(json_object('source',u.source,'key',u.key,'season',u.season,'episode',u.episode,
           'finished',json(CASE WHEN COALESCE(p.finished,0) THEN 'true' ELSE 'false' END),
           'position',COALESCE(p.position_s,0),'duration',COALESCE(p.duration_s,u.duration),'updated',COALESCE(p.updated_at,0)))
         FROM units u LEFT JOIN watch_progress p ON p.source_id=u.source AND p.kind=u.kind AND p.item_key=u.key AND p.updated_at>=?2
         GROUP BY u.wid,u.season,u.episode ORDER BY u.wid,u.season,u.episode",
    )?;
    let mut out: HashMap<WatchlistItemId, Vec<Vec<Unit>>> = HashMap::new();
    for row in stmt.query_map(params![json, l.since], |r| Ok((r.get::<_, WatchlistItemId>(0)?, r.get::<_, String>(1)?)))? {
        let (id, json) = row?;
        out.entry(id).or_default().push(serde_json::from_str(&json).map_err(Error::other)?);
    }
    Ok(out)
}
fn concealed(c: &Connection, lib: &Library, e: &Entry, hides_explicit: bool) -> Result<bool> {
    if lib.built_at == 0 {
        return Ok(true);
    }
    if let Some(item) = &e.item {
        return Ok(lib.conceals(item)
            || (hides_explicit
                && (crate::store::live::item_explicit(c, item)?
                    || (e.orphaned_since.is_some()
                        && (e.explicit || e.fingerprint.norm_group.as_deref().is_some_and(crate::sources::explicit::is_explicit_name))))));
    }
    Ok(hides_explicit && (e.explicit || e.fingerprint.norm_group.as_deref().is_some_and(crate::sources::explicit::is_explicit_name)))
}
fn locked(c: &Connection, lib: &Library, item: &ItemRef) -> Result<bool> {
    let (kind, source, key) = if item.kind == ItemKind::Episode {
        let parent: Option<(SourceId, String)> = c
            .query_row(
                "SELECT s.source_id,s.key FROM episode e JOIN series s ON s.id=e.series_id WHERE e.source_id=?1 AND e.key=?2",
                params![item.source, item.key.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((source, key)) = parent else { return Ok(false) };
        (ItemKind::Series, source, key)
    } else {
        (item.kind, item.source, item.key.0.clone())
    };
    let id: Option<i64> =
        c.query_row(&format!("SELECT id FROM {} WHERE source_id=?1 AND key=?2", kind.as_str()), params![source, key], |r| r.get(0)).optional()?;
    let vk =
        id.map(|id| if kind == ItemKind::Movie { VodKind::Movie(crate::model::MovieId(id)) } else { VodKind::Series(crate::model::SeriesId(id)) });
    Ok(vk.and_then(|v| lib.vod.find(v)).is_some_and(|v| v.locked))
}
/// Visible state for one list. Agent selection always passes locked entries, even in an
/// unlocked app session; the app keeps them and lets Up next request the PIN.
pub fn view(c: &Connection, lib: &Library, id: WatchlistId, unlocked: bool, agent: bool) -> Result<Option<ListView>> {
    view_with_visibility(c, lib, id, unlocked, agent, !lib.visibility.explicit)
}
pub fn view_with_visibility(
    c: &Connection,
    lib: &Library,
    id: WatchlistId,
    unlocked: bool,
    agent: bool,
    hides_explicit: bool,
) -> Result<Option<ListView>> {
    view_with_preferences(c, lib, id, unlocked, agent, hides_explicit, Default::default())
}
/// The UI and engine pass the user's copy preference so the visible copy mark agrees with
/// the copy selected for playback.
pub fn view_with_preferences(
    c: &Connection,
    lib: &Library,
    id: WatchlistId,
    unlocked: bool,
    agent: bool,
    hides_explicit: bool,
    copy_order: crate::config::CopyOrder,
) -> Result<Option<ListView>> {
    view_for_run(c, lib, id, unlocked, agent, hides_explicit, copy_order, &[])
}

#[allow(clippy::too_many_arguments)]
pub fn view_for_run(
    c: &Connection,
    lib: &Library,
    id: WatchlistId,
    unlocked: bool,
    agent: bool,
    hides_explicit: bool,
    copy_order: crate::config::CopyOrder,
    ended: &[ItemRef],
) -> Result<Option<ListView>> {
    let mut excluded = Vec::new();
    for item in ended {
        excluded.extend(title_copies(c, lib, item)?);
    }
    let Some(l) = list(c, id)? else { return Ok(None) };
    let all = entries(c, id)?;
    let mut rows = Vec::new();
    for e in all.iter().cloned() {
        if !concealed(c, lib, &e, hides_explicit)? {
            rows.push(e)
        }
    }
    if !all.is_empty() && rows.is_empty() {
        return Ok(None);
    }
    let mut progress = progress_units(c, lib, &l, &rows)?;
    let mut states = Vec::new();
    for e in rows {
        let groups = progress.remove(&e.id).unwrap_or_default();
        let mut units: Vec<(bool, Unit)> = groups
            .into_iter()
            .filter_map(|copies| {
                let done = copies.iter().any(|p| p.finished);
                copies.into_iter().max_by_key(|p| (p.finished, p.updated)).map(|p| (done, p))
            })
            .collect();
        units.sort_by_key(|(_, p)| (p.season, p.episode));
        let done = units.iter().filter(|(done, _)| *done).count();
        let total = units.len();
        // A resumed episode wins; otherwise carry on after the last finished episode, then
        // wrap to the range's first unfinished one. Specials only enter an explicit range.
        let available: Vec<_> = units
            .iter()
            .filter(|(_, p)| {
                !excluded.iter().any(|item| {
                    item.source.0 == p.source
                        && item.key.as_str() == p.key
                        && item.kind == if e.kind == ItemKind::Movie { ItemKind::Movie } else { ItemKind::Episode }
                })
            })
            .collect();
        let next = available
            .iter()
            .copied()
            .filter(|(done, p)| !done && p.position > 0.0)
            .max_by_key(|(_, p)| p.updated)
            .or_else(|| {
                let last = available.iter().rposition(|(done, _)| *done);
                available.iter().copied().enumerate().find(|(i, (done, _))| !done && last.is_none_or(|last| *i > last)).map(|(_, u)| u)
            })
            .or_else(|| available.iter().copied().find(|(done, _)| !done))
            .or_else(|| available.first().copied().filter(|_| done == total));
        let item = e.item.as_ref();
        let unloaded = if e.kind == ItemKind::Series {
            if let Some(item) = item {
                let mut unloaded = None;
                let copies = if !e.any_copy {
                    vec![item.clone()]
                } else if total == 0 {
                    title_copies(c, lib, item)?
                } else {
                    planned_copies(c, lib, item, unlocked && !agent, copy_order)?.into_iter().take(1).collect()
                };
                for copy in copies {
                    let needs=c.query_row("SELECT s.details_at IS NULL AND src.kind='xtream' AND NOT EXISTS(SELECT 1 FROM episode e WHERE e.series_id=s.id) FROM series s JOIN source src ON src.id=s.source_id WHERE s.source_id=?1 AND s.key=?2",params![copy.source,copy.key.as_str()],|r|r.get::<_,bool>(0)).optional()?.unwrap_or(false);
                    if needs {
                        unloaded.get_or_insert(copy);
                    }
                }
                unloaded
            } else {
                None
            }
        } else {
            None
        };
        let missing = item.is_none() || e.orphaned_since.is_some() || (total == 0 && unloaded.is_none());
        let title_locked = if let Some(item) = item {
            if e.any_copy {
                let mut all_locked = true;
                for copy in title_copies(c, lib, item)? {
                    all_locked &= locked(c, lib, &copy)?;
                }
                all_locked
            } else {
                locked(c, lib, item)?
            }
        } else {
            false
        };
        let is_locked = title_locked && (agent || !unlocked);
        let mut status = if e.skipped {
            EntryStatus::Skipped
        } else if total > 0 && done == total && unloaded.is_none() {
            EntryStatus::Done
        } else if missing {
            EntryStatus::Missing
        } else if is_locked {
            EntryStatus::Locked
        } else {
            EntryStatus::Todo
        };
        let mut playable = if missing || (total > 0 && next.is_none()) {
            None
        } else {
            next.map(|(_, p)| ItemRef {
                source: SourceId(p.source),
                kind: if e.kind == ItemKind::Movie { ItemKind::Movie } else { ItemKind::Episode },
                key: ItemKey(p.key.clone()),
            })
            .or_else(|| item.cloned())
        };
        if e.any_copy
            && unloaded.is_none()
            && !missing
            && !available.is_empty()
            && let Some(item) = item
        {
            let order = planned_copies(c, lib, item, (unlocked && !agent) || title_locked, copy_order)?;
            if e.kind == ItemKind::Movie {
                playable = order.into_iter().next().or(playable);
            } else if let Some((_, unit)) = next {
                for series in order {
                    let key:Option<String>=c.query_row("SELECT e.key FROM episode e JOIN series s ON s.id=e.series_id WHERE s.source_id=?1 AND s.key=?2 AND e.season=?3 AND e.episode=?4 ORDER BY e.id LIMIT 1",params![series.source,series.key.as_str(),unit.season,unit.episode],|r|r.get(0)).optional()?;
                    if let Some(key) = key {
                        playable = Some(ItemRef { source: series.source, kind: ItemKind::Episode, key: ItemKey(key) });
                        break;
                    }
                }
            }
        }
        if let Some(unloaded) = &unloaded {
            playable = Some(unloaded.clone());
        }
        if !e.any_copy && unloaded.is_none() && !available.is_empty() {
            if matches!(e.kind, ItemKind::Movie | ItemKind::Episode) {
                playable = e.item.clone();
            } else if let (Some(pinned), Some((_, unit))) = (&e.item, next) {
                let key:Option<String>=c.query_row("SELECT e.key FROM episode e JOIN series s ON s.id=e.series_id WHERE s.source_id=?1 AND s.key=?2 AND e.season=?3 AND e.episode=?4 ORDER BY e.id LIMIT 1",params![pinned.source,pinned.key.as_str(),unit.season,unit.episode],|r|r.get(0)).optional()?;
                playable = key.map(|key| ItemRef { source: pinned.source, kind: ItemKind::Episode, key: ItemKey(key) });
                if playable.is_none() && !e.skipped {
                    status = EntryStatus::Missing;
                }
            }
        }
        let progress = next.filter(|(done, p)| !done && p.position > 0.0).map(|(_, p)| Progress {
            position_s: p.position,
            duration_s: p.duration,
            updated_at: p.updated,
        });
        let duration_s = if e.kind == ItemKind::Series {
            units.iter().filter(|(done, _)| !done).try_fold(0.0, |sum, (_, p)| p.duration.map(|d| sum + (d - p.position).max(0.0)))
        } else {
            next.and_then(|(_, p)| p.duration)
        };
        let episode = next.filter(|_| e.kind != ItemKind::Movie).map(|(_, p)| (p.season, p.episode)).or(e.season.zip(e.episode));
        let copy_name = playable.as_ref().and_then(|p| lib.source(p.source)).map(|s| s.name.clone());
        let series = if e.kind == ItemKind::Episode {
            e.item.as_ref().map(|item| c.query_row("SELECT COALESCE(o.rename,s.clean_name) FROM episode e JOIN series s ON s.id=e.series_id LEFT JOIN item_override o ON o.source_id=s.source_id AND o.kind='series' AND o.item_key=s.key WHERE e.source_id=?1 AND e.key=?2",params![item.source,item.key.as_str()],|r|r.get::<_,String>(0)).optional()).transpose()?.flatten()
        } else if e.kind == ItemKind::Series {
            Some(e.title.clone())
        } else {
            None
        };
        let series = series.or_else(|| e.series_title.clone());
        states.push(EntryState {
            entry: e,
            status,
            playable,
            episode,
            series,
            progress,
            duration_s,
            done_episodes: done,
            total_episodes: total,
            locked: is_locked,
            copy_name,
        });
    }
    let candidates: Vec<usize> = states
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.done() && e.playable.is_some() && e.status != EntryStatus::Missing && (!agent || !e.locked))
        .map(|(i, _)| i)
        .collect();
    let preferred: Option<WatchlistItemId> =
        c.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1", [format!("watchlist:next:{id}")], |r| r.get(0)).optional()?;
    let preferred = preferred.and_then(|id| candidates.iter().copied().find(|i| states[*i].entry.id == id));
    let next = if preferred.is_some() {
        preferred
    } else if l.play_order == PlayOrder::Ordered {
        candidates.first().copied()
    } else {
        candidates
            .iter()
            .copied()
            .filter(|i| states[*i].progress.is_some())
            .max_by_key(|i| states[*i].progress.as_ref().map(|p| p.updated_at))
            .or_else(|| {
                if candidates.is_empty() {
                    None
                } else {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static PICK: AtomicU64 = AtomicU64::new(0);
                    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
                    let n = time ^ PICK.fetch_add(0x9e3779b97f4a7c15, Ordering::Relaxed);
                    Some(candidates[(n as usize) % candidates.len()])
                }
            })
    };
    let mut passed_over = Vec::new();
    for e in states.iter().take(if l.play_order == PlayOrder::Ordered { next.unwrap_or(states.len()) } else { 0 }) {
        let why = match e.status {
            EntryStatus::Missing => Some("not in your playlists"),
            EntryStatus::Locked if agent => Some("locked: needs your PIN in Tuner"),
            _ => None,
        };
        if let Some(why) = why {
            passed_over.push(PassedOver { id: e.entry.id, title: e.entry.title.clone(), why: why.into() })
        }
    }
    let next = next.map(|i| {
        if states[i].status != EntryStatus::Locked {
            states[i].status = EntryStatus::Next
        }
        states[i].entry.id
    });
    let remaining_s = states.iter().filter(|e| !e.done() && e.status != EntryStatus::Missing).try_fold(0.0, |sum, e| {
        e.duration_s
            .map(|d| sum + if e.entry.kind == ItemKind::Series { d } else { (d - e.progress.as_ref().map_or(0.0, |p| p.position_s)).max(0.0) })
    });
    let show_watched =
        c.query_row("SELECT value='1' FROM meta WHERE key=?1", [format!("watchlist:show_watched:{id}")], |r| r.get(0)).optional()?.unwrap_or(false);
    let mut view = ListView { list: l, entries: states, next, done: 0, total: 0, remaining_s, passed_over, show_watched };
    view.recount();
    Ok(Some(view))
}
pub fn snapshot(c: &Connection, lib: &Library, unlocked: bool, agent: bool) -> Result<WatchlistsSnapshot> {
    snapshot_with_visibility(c, lib, unlocked, agent, !lib.visibility.explicit)
}
pub fn snapshot_with_visibility(c: &Connection, lib: &Library, unlocked: bool, agent: bool, hides_explicit: bool) -> Result<WatchlistsSnapshot> {
    snapshot_with_preferences(c, lib, unlocked, agent, hides_explicit, Default::default())
}
pub fn snapshot_with_preferences(
    c: &Connection,
    lib: &Library,
    unlocked: bool,
    agent: bool,
    hides_explicit: bool,
    copy_order: crate::config::CopyOrder,
) -> Result<WatchlistsSnapshot> {
    let mut out = Vec::new();
    for l in lists(c)? {
        if let Some(v) = view_with_preferences(c, lib, l.id, unlocked, agent, hides_explicit, copy_order)? {
            out.push(v)
        }
    }
    out.sort_by_key(|v| (!v.list.is_default, v.finished(), v.list.sort_index, v.list.id));
    Ok(WatchlistsSnapshot { lists: out })
}

/// Attach placeholders at an import boundary (DESIGN §42 §4). TMDB identity wins; otherwise
/// require the kind, folded title AND year. The lookup is indexed and never scans a catalogue.
pub fn match_placeholders(c: &Connection) -> Result<usize> {
    if !crate::store::migrations::has_column(c, "watchlist_item", "id")? {
        return Ok(0);
    }
    let rows = c
        .prepare_cached(&format!("SELECT {ENTRY_COLS} FROM watchlist_item WHERE source_id IS NULL OR orphaned_since IS NOT NULL"))?
        .query_map([], entry_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut n = 0;
    for e in rows {
        let new = NewEntry {
            item: None,
            kind: e.kind,
            title: e.match_title.clone().unwrap_or_else(|| e.title.clone()),
            year: e.year,
            tmdb_id: e.tmdb_id,
            series_title: e.series_title.clone(),
            season: e.season,
            episode: e.episode,
            season_from: e.season_from,
            season_to: e.season_to,
        };
        let candidates = placeholder_candidates(c, &new)?;
        if let Some(item) = pick_placeholder_copy(c, &candidates)? {
            attach(c, e.id, &item)?;
            n += 1;
        }
    }
    Ok(n)
}

fn placeholder_candidates(c: &Connection, e: &NewEntry) -> Result<Vec<ItemRef>> {
    let mut candidates = Vec::<ItemRef>::new();
    if e.kind == ItemKind::Episode {
        let (Some(title), Some(season), Some(episode)) = (&e.series_title, e.season, e.episode) else { return Ok(candidates) };
        let hash = e.year.and_then(|year| {
            crate::sources::vod_match::parse(title, Some(year), &crate::sources::vod_match::PrefixCodes::default()).stack_hash(ItemKind::Series)
        });
        // Each branch starts with an indexed series identity, then its indexed S/E.
        // A known TMDB series without this episode must not fall back to another series.
        candidates = c
            .prepare_cached(
                "SELECT e.source_id,e.key FROM series s JOIN episode e ON e.series_id=s.id
             WHERE s.tmdb_id=?1 AND e.season=?3 AND e.episode=?4
             UNION ALL
             SELECT e.source_id,e.key FROM series s JOIN episode e ON e.series_id=s.id
             WHERE s.match_hash=?2 AND e.season=?3 AND e.episode=?4
               AND NOT EXISTS(SELECT 1 FROM series WHERE tmdb_id=?1)
             ORDER BY 1,2",
            )?
            .query_map(params![e.tmdb_id.filter(|id| *id > 0).map(|id| id.to_string()), hash, season, episode], |r| {
                Ok(ItemRef { source: r.get(0)?, kind: ItemKind::Episode, key: ItemKey(r.get(1)?) })
            })?
            .collect::<rusqlite::Result<_>>()?;
    } else {
        if let Some(tmdb) = e.tmdb_id.filter(|id| *id > 0) {
            candidates = c
                .prepare_cached(&format!("SELECT source_id,key FROM {} WHERE tmdb_id=?1 ORDER BY source_id,id", e.kind.as_str()))?
                .query_map([tmdb.to_string()], |r| Ok(ItemRef { source: r.get(0)?, kind: e.kind, key: ItemKey(r.get(1)?) }))?
                .collect::<rusqlite::Result<_>>()?;
        }
        if candidates.is_empty() && e.year.is_some() {
            let hash = crate::sources::vod_match::parse(&e.title, e.year, &crate::sources::vod_match::PrefixCodes::default()).stack_hash(e.kind);
            if let Some(hash) = hash {
                candidates = c
                    .prepare_cached(&format!("SELECT source_id,key FROM {} WHERE match_hash=?1 ORDER BY source_id,id", e.kind.as_str()))?
                    .query_map([hash], |r| Ok(ItemRef { source: r.get(0)?, kind: e.kind, key: ItemKey(r.get(1)?) }))?
                    .collect::<rusqlite::Result<_>>()?;
            }
        }
    }
    Ok(candidates)
}
/// Indexed identity lookup used by agent guards before adding a placeholder.
pub fn match_placeholder(c: &Connection, e: &NewEntry) -> Result<Option<ItemRef>> {
    let candidates = placeholder_candidates(c, e)?;
    pick_placeholder_copy(c, &candidates)
}

/// Build only the matched title's tiny index, so placeholder attachment uses the same copy
/// plan as playback without loading the whole catalogue during an import.
fn pick_placeholder_copy(c: &Connection, candidates: &[ItemRef]) -> Result<Option<ItemRef>> {
    use crate::model::{MovieId, SeriesId, VodSummary};
    use crate::store::{
        library::VodIndex,
        stacks::{self, CopyRaw},
    };
    if candidates.len() < 2 {
        return Ok(candidates.first().cloned());
    }
    if candidates[0].kind == ItemKind::Episode {
        let parents = candidates
            .iter()
            .map(|item| {
                c.query_row(
                    "SELECT s.source_id,s.key FROM episode e JOIN series s ON s.id=e.series_id WHERE e.source_id=?1 AND e.key=?2",
                    params![item.source, item.key.as_str()],
                    |r| Ok(ItemRef { source: r.get(0)?, kind: ItemKind::Series, key: ItemKey(r.get(1)?) }),
                )
                .map_err(Error::from)
            })
            .collect::<Result<Vec<_>>>()?;
        let chosen = pick_placeholder_copy(c, &parents)?;
        return Ok(chosen.and_then(|p| parents.iter().position(|s| *s == p)).map(|i| candidates[i].clone()));
    }
    let mut candidates = candidates.to_vec();
    let order: HashMap<SourceId, i64> =
        c.prepare_cached("SELECT id,sort_index FROM source")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    candidates.sort_by_key(|item| (order.get(&item.source).copied().unwrap_or_default(), item.source));
    let mut summaries = Vec::new();
    let mut raw = Vec::new();
    let movies = candidates[0].kind == ItemKind::Movie;
    for item in &candidates {
        let table = item.kind.as_str();
        let (id,name,year,lang,tags,pinned):(i64,String,Option<u16>,Option<String>,i64,bool)=c.query_row(&format!("SELECT t.id,t.clean_name,t.year,t.match_lang,COALESCE(t.match_tags,0),EXISTS(SELECT 1 FROM title_link l WHERE l.source_id=t.source_id AND l.kind=?3 AND l.item_key=t.key AND l.action='pin') FROM {table} t WHERE t.source_id=?1 AND t.key=?2"),params![item.source,item.key.as_str(),table],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        summaries.push(VodSummary {
            kind: if movies { VodKind::Movie(MovieId(id)) } else { VodKind::Series(SeriesId(id)) },
            source_id: item.source,
            key: item.key.clone(),
            name: name.into(),
            year,
            poster: None,
            rating: None,
            group_ids: Vec::new(),
            added_at: None,
            provider_order: 0,
            locked: false,
        });
        raw.push(CopyRaw {
            hash: Some(1),
            lang: lang.map(Into::into),
            tags: crate::sources::vod_match::VersionTags(tags as u16),
            pinned,
            ..Default::default()
        });
    }
    let stacks = stacks::build(&summaries, raw, |_| true, &[]);
    let vod = if movies {
        VodIndex::from_parts(summaries, Vec::new(), Vec::new(), Vec::new(), HashMap::new()).with_stacks(stacks, Default::default())
    } else {
        VodIndex::from_parts(Vec::new(), summaries, Vec::new(), Vec::new(), HashMap::new()).with_stacks(Default::default(), stacks)
    };
    let plan = copies::plan(c, &vod, movies, 0, true, Default::default(), crate::model::now())?;
    Ok(plan.first().and_then(|i| candidates.get(i as usize)).cloned())
}

/// Before catalogue chunks are deleted, preserve the current explicit decision in each
/// entry's fingerprint. A deleted source must not reveal its formerly concealed titles.
pub(crate) fn before_source_delete(c: &Connection, source: SourceId) -> Result<()> {
    if !crate::store::migrations::has_column(c, "watchlist_item", "id")? {
        return Ok(());
    }
    let rows = c
        .prepare_cached(&format!("SELECT {ENTRY_COLS} FROM watchlist_item WHERE source_id=?1"))?
        .query_map([source], entry_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for e in rows {
        if let Some(item) = e.item {
            let exists = c.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {} WHERE source_id=?1 AND key=?2)", item.kind.as_str()),
                params![item.source, item.key.as_str()],
                |r| r.get::<_, bool>(0),
            )?;
            let explicit = if exists { crate::store::live::item_explicit(c, &item)? } else { e.explicit };
            c.execute(
                "UPDATE watchlist_item SET fp=json_set(fp,'$.watchlist_explicit',json(?2)) WHERE id=?1",
                params![e.id, if explicit { "true" } else { "false" }],
            )?;
        }
    }
    Ok(())
}

// Legacy favourites did not store title identities. Compare their preserved fingerprints
// as well as catalogue identity, without building/scanning a catalogue during migration.
fn migration_duplicate(a: &Entry, b: &Entry) -> bool {
    if a.kind != b.kind || a.season_from != b.season_from || a.season_to != b.season_to {
        return false;
    }
    if a.item.is_some() && a.item == b.item {
        return true;
    }
    let fingerprint_matches = !a.fingerprint.norm_name.is_empty() && a.fingerprint.norm_name == b.fingerprint.norm_name;
    if a.kind == ItemKind::Episode && (a.season.zip(a.episode).is_none() || a.season != b.season || a.episode != b.episode) {
        return fingerprint_matches;
    }
    if let (Some(x), Some(y)) = (a.tmdb_id.filter(|id| *id > 0), b.tmdb_id.filter(|id| *id > 0)) {
        return x == y;
    }
    let title = |e: &Entry| {
        if e.kind == ItemKind::Episode {
            e.series_title.clone().unwrap_or_default()
        } else {
            e.match_title.clone().unwrap_or_else(|| e.title.clone())
        }
    };
    a.year == b.year
        && (fingerprint_matches
            || a.year.is_some()
                && !title(a).is_empty()
                && crate::sources::identity::normalize_name(&title(a)) == crate::sources::identity::normalize_name(&title(b)))
}

fn provider_title(c: &Connection, item: &ItemRef) -> Result<Option<String>> {
    if !matches!(item.kind, ItemKind::Movie | ItemKind::Series) {
        return Ok(None);
    }
    Ok(c.query_row(
        &format!("SELECT clean_name FROM {} WHERE source_id=?1 AND key=?2", item.kind.as_str()),
        params![item.source, item.key.as_str()],
        |r| r.get(0),
    )
    .optional()?)
}

/// Repair fallback names from legacy backups before the shared holder overwrites their fp.
pub(crate) fn repair_attached(c: &Connection, from: &ItemRef, to: &ItemRef) -> Result<()> {
    if !crate::store::migrations::has_column(c, "watchlist_item", "id")? {
        return Ok(());
    }
    let rows = c
        .prepare_cached(&format!("SELECT {ENTRY_COLS} FROM watchlist_item WHERE source_id=?1 AND kind=?2 AND item_key=?3"))?
        .query_map(params![from.source, from.kind.as_str(), from.key.as_str()], entry_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(());
    }
    let identity = new_entry(c, to)?;
    for e in rows {
        if e.kind == ItemKind::Episode || (e.year.is_none() && e.title == e.fingerprint.norm_name) {
            c.execute(
                "UPDATE watchlist_item SET title=?2,year=?3,tmdb_id=?4,series_title=?5,season=?6,episode=?7 WHERE id=?1",
                params![e.id, identity.title, identity.year, identity.tmdb_id, identity.series_title, identity.season, identity.episode],
            )?;
        }
        c.execute("UPDATE watchlist_item SET match_title=?2 WHERE id=?1", params![e.id, provider_title(c, to)?])?;
    }
    Ok(())
}

/// Rust migration step, also used when restoring a pre-watchlists backup. The caller owns
/// the transaction; v14 upgrades attempt a user-data backup first (DESIGN §42 §4.1).
pub(crate) fn move_favourites(c: &Connection) -> Result<Vec<String>> {
    let mine = default_list(c)?;
    let mut moved = Vec::new();
    for old in user::fav_lists(c)? {
        let rows: Vec<_> = user::fav_items(c, old.id)?.into_iter().filter(|e| e.item.kind != ItemKind::Channel).collect();
        if rows.is_empty() {
            continue;
        }
        let mapping_key = format!("watchlist:favourites:{}", old.id);
        let previous: Option<(WatchlistId, String)> = c
            .query_row("SELECT w.id,w.name FROM meta m JOIN watchlist w ON w.id=CAST(m.value AS INTEGER) WHERE m.key=?1", [&mapping_key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let (id, name) = if old.is_default {
            (mine, "My list".to_owned())
        } else if let Some(previous) = previous {
            previous
        } else if let Some(existing) = c.query_row("SELECT id,name FROM watchlist WHERE is_default=0 AND (name=?1 COLLATE NOCASE OR name=?2 COLLATE NOCASE) ORDER BY name=?1 COLLATE NOCASE DESC LIMIT 1", params![old.name, format!("{} (favourites)", old.name)], |r| Ok((r.get(0)?, r.get(1)?))).optional()? {
            existing
        } else {
            let mut name = old.name.clone();
            let mut suffix = 0;
            while c.query_row("SELECT EXISTS(SELECT 1 FROM watchlist WHERE name=?1 COLLATE NOCASE)", [&name], |r| r.get::<_, bool>(0))? {
                suffix += 1;
                name = if suffix == 1 { format!("{} (favourites)", old.name) } else { format!("{} (favourites {suffix})", old.name) };
            }
            let id = WatchlistId(next_id(c, "watchlist")?);
            c.execute(
                "INSERT INTO watchlist(id,name,play_order,sort_index,created_at) VALUES (?1,?2,'any',?3,?4)",
                params![id, name, old.sort_index, crate::model::now()],
            )?;
            (id, name)
        };
        let mut have = entries(c, id)?;
        for r in &rows {
            let display = new_entry(c, &r.item).ok();
            let title = display.as_ref().map(|t| t.title.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| r.fingerprint.norm_name.clone());
            let e = Entry {
                id: WatchlistItemId(0),
                list_id: id,
                position: have.len(),
                kind: r.item.kind,
                item: Some(r.item.clone()),
                any_copy: true,
                season_from: None,
                season_to: None,
                title,
                year: display.as_ref().and_then(|t| t.year),
                tmdb_id: display.as_ref().and_then(|t| t.tmdb_id),
                series_title: display.as_ref().and_then(|t| t.series_title.clone()),
                season: display.as_ref().and_then(|t| t.season),
                episode: display.as_ref().and_then(|t| t.episode),
                skipped: false,
                added_at: r.added_at,
                match_title: provider_title(c, &r.item)?,
                fingerprint: r.fingerprint.clone(),
                orphaned_since: r.orphaned_since,
                explicit: crate::store::live::item_explicit(c, &r.item)?,
            };
            if !have.iter().any(|existing| migration_duplicate(existing, &e)) {
                write_entry(c, &e, false)?;
                have.push(e);
            }
        }
        c.execute("DELETE FROM fav_item WHERE list_id=?1 AND kind<>'channel'", [old.id])?;
        if !old.is_default {
            c.execute("DELETE FROM fav_list WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM fav_item WHERE list_id=?1)", [old.id])?;
        }
        let remaining = user::fav_items(c, old.id)?;
        if !old.is_default {
            if remaining.is_empty() {
                c.execute("DELETE FROM meta WHERE key=?1", [&mapping_key])?;
            } else {
                // A mixed list can have another legacy channel reclassified on a later
                // refresh. It belongs in this same watchlist, even after either is renamed.
                c.execute(
                    "INSERT INTO meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    params![mapping_key, id.to_string()],
                )?;
            }
        }
        for (i, e) in remaining.iter().enumerate() {
            c.execute(
                "UPDATE fav_item SET position=?4 WHERE list_id=?1 AND source_id=?2 AND item_key=?3 AND kind='channel'",
                params![old.id, e.item.source, e.item.key.as_str(), i as i64],
            )?;
        }
        moved.push(name);
    }
    if !moved.is_empty() {
        let mut notice = moved_card(c)?;
        for name in &moved {
            if !notice.contains(name) {
                notice.push(name.clone());
            }
        }
        c.execute(
            "INSERT INTO meta(key,value) VALUES ('watchlists:moved',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::to_string(&notice).map_err(Error::other)?],
        )?;
    }
    Ok(moved)
}
/// The once-only launch card. Reading does not acknowledge it; OK / Open Watchlists does.
pub fn moved_card(c: &Connection) -> Result<Vec<String>> {
    let json: Option<String> = c.query_row("SELECT value FROM meta WHERE key='watchlists:moved'", [], |r| r.get(0)).optional()?;
    Ok(json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
}
pub fn dismiss_moved_card(c: &Connection) -> Result<()> {
    c.execute("DELETE FROM meta WHERE key='watchlists:moved'", [])?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BackupLists {
    lists: Vec<List>,
    entries: Vec<Entry>,
    #[serde(default)]
    context: Option<WatchlistId>,
    #[serde(default)]
    show_watched: Vec<WatchlistId>,
    #[serde(default)]
    context_agent: bool,
}
pub fn export(c: &Connection) -> Result<Option<serde_json::Value>> {
    if !crate::store::migrations::has_column(c, "watchlist", "id")? {
        return Ok(None);
    }
    let lists = lists(c)?;
    let mut rows = Vec::new();
    let mut show_watched = Vec::new();
    for l in &lists {
        rows.extend(entries(c, l.id)?);
        if c.query_row("SELECT value='1' FROM meta WHERE key=?1", [format!("watchlist:show_watched:{}", l.id)], |r| r.get::<_, bool>(0))
            .optional()?
            .unwrap_or(false)
        {
            show_watched.push(l.id)
        }
    }
    let context = c.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='queue:context'", [], |r| r.get(0)).optional()?;
    Ok(Some(
        serde_json::to_value(BackupLists {
            lists,
            entries: rows,
            context,
            show_watched,
            context_agent: c.query_row("SELECT value='1' FROM meta WHERE key='queue:context_agent'", [], |r| r.get(0)).optional()?.unwrap_or(false),
        })
        .map_err(Error::other)?,
    ))
}
#[derive(Default)]
pub struct ImportResult {
    pub rows: usize,
    pub entries: HashMap<WatchlistItemId, WatchlistItemId>,
    pub lists: HashMap<WatchlistId, WatchlistId>,
}
pub fn import(c: &Connection, v: &serde_json::Value, remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<ImportResult> {
    import_mode(c, v, remap, true)
}

pub(crate) fn import_mode(c: &Connection, v: &serde_json::Value, remap: &dyn Fn(&ItemRef) -> Option<ItemRef>, merge: bool) -> Result<ImportResult> {
    let saved: BackupLists = serde_json::from_value(v.clone()).map_err(Error::other)?;
    let mut out = ImportResult::default();
    for l in saved.lists {
        let existed = if l.is_default {
            c.query_row("SELECT EXISTS(SELECT 1 FROM watchlist WHERE is_default=1)", [], |r| r.get::<_, bool>(0))?
        } else {
            c.query_row("SELECT EXISTS(SELECT 1 FROM watchlist WHERE name=?1 COLLATE NOCASE)", [&l.name], |r| r.get::<_, bool>(0))?
        };
        let id = if l.is_default {
            default_list(c)?
        } else if let Some(id) = c.query_row("SELECT id FROM watchlist WHERE name=?1 COLLATE NOCASE", [&l.name], |r| r.get(0)).optional()? {
            id
        } else {
            create(c, &l.name, l.note.as_deref(), l.play_order, &l.created_by, l.created_at)?
        };
        if !merge || !existed {
            c.execute(
                "UPDATE watchlist SET note=?2,play_order=?3,since=?4,last_played_at=?5,sort_index=?6,created_at=?7,created_by=?8 WHERE id=?1",
                params![id, l.note, l.play_order.as_str(), l.since, l.last_played_at, l.sort_index, l.created_at, l.created_by],
            )?;
        }
        out.lists.insert(l.id, id);
        let start = entries(c, id)?.len();
        for (i, original) in saved.entries.iter().filter(|e| e.list_id == l.id).enumerate() {
            let mut e = original.clone();
            e.list_id = id;
            e.position = start + i;
            e.item = e.item.as_ref().and_then(remap);
            // Repeated Merge restores do not add the same title/range twice.
            if let Some(existing) = entries(c, id)?.iter().find(|have| {
                have.kind == e.kind
                    && have.season_from == e.season_from
                    && have.season_to == e.season_to
                    && (have.item.is_some() && have.item == e.item
                        || have.item.is_none()
                            && e.item.is_none()
                            && have.title == e.title
                            && have.year == e.year
                            && have.tmdb_id == e.tmdb_id
                            && have.series_title == e.series_title
                            && have.season == e.season
                            && have.episode == e.episode)
            }) {
                out.entries.insert(original.id, existing.id);
                continue;
            }
            let new = write_entry(c, &e, false)?;
            out.entries.insert(original.id, new);
            out.rows += 1;
        }
        dense(c, &entries(c, id)?)?;
        if (!merge || !existed) && saved.show_watched.contains(&l.id) {
            set_show_watched(c, id, true)?;
        }
    }
    if let Some(id) = saved.context.and_then(|id| out.lists.get(&id)) {
        c.execute(
            "INSERT INTO meta(key,value) VALUES ('queue:context_agent',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [if saved.context_agent { "1" } else { "0" }],
        )?;
        c.execute("INSERT INTO meta(key,value) VALUES ('queue:context',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [id.to_string()])?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, library, titles};

    fn with_db(f: impl FnOnce(&mut Connection) -> Result<()> + Send + 'static) {
        let store = Store::open_temp("watchlist-store", None).unwrap();
        store
            .writer()
            .blocking(move |c| {
                fixture(c)?;
                f(c)
            })
            .unwrap();
    }
    fn fixture(c: &Connection) -> Result<()> {
        c.execute_batch(
            "INSERT INTO source(id,name,kind,created_at) VALUES (1,'TV','m3u-url',0),(2,'TV 2','m3u-url',0);
          INSERT INTO movie(id,source_id,key,provider_id,name,clean_name,norm_name,year,stream,provider_order,seen_gen,duration_s) VALUES
           (1,1,'empire','10','Empire (1980)','Empire','empire',1980,'{}',0,1,7200),
           (2,2,'empire2','20','Empire (1980)','Empire','empire',1980,'{}',0,1,7200),
           (3,1,'hope','30','A New Hope (1977)','A New Hope','anewhope',1977,'{}',1,1,7200),
           (4,1,'elf','40','Elf (2003)','Elf','elf',2003,'{}',2,1,6000);
          INSERT INTO series(id,source_id,key,name,clean_name,norm_name,year,provider_order,seen_gen,details_at) VALUES
           (1,1,'andor','Andor (2022)','Andor','andor',2022,0,1,1),
           (2,2,'andor2','Andor (2022)','Andor','andor',2022,0,1,1);",
        )?;
        for source in 1..=2 {
            for season in 0..=3 {
                for episode in 1..=2 {
                    c.execute(
                        "INSERT INTO episode(series_id,source_id,key,season,episode,title,duration_s,stream) VALUES (?1,?1,?2,?3,?4,?5,3000,'{}')",
                        params![source, format!("s{source}-{season}-{episode}"), season, episode, format!("Episode {season}-{episode}")],
                    )?;
                }
            }
        }
        titles::refresh_all(c)?;
        Ok(())
    }
    fn movie(source: i64, key: &str) -> ItemRef {
        ItemRef { source: SourceId(source), kind: ItemKind::Movie, key: ItemKey(key.into()) }
    }
    fn series(source: i64) -> ItemRef {
        ItemRef { source: SourceId(source), kind: ItemKind::Series, key: ItemKey(if source == 1 { "andor" } else { "andor2" }.into()) }
    }
    fn progress(c: &Connection, item: &ItemRef, finished: bool, position: f64, at: i64) -> Result<()> {
        c.execute("INSERT INTO watch_progress(source_id,kind,item_key,position_s,duration_s,finished,updated_at,title) VALUES (?1,?2,?3,?4,7200,?5,?6,'') ON CONFLICT(source_id,kind,item_key) DO UPDATE SET position_s=excluded.position_s,finished=excluded.finished,updated_at=excluded.updated_at",params![item.source,item.kind.as_str(),item.key.as_str(),position,finished,at])?;
        Ok(())
    }
    fn add_refs(c: &Connection, lib: &Library, id: WatchlistId, refs: &[ItemRef]) -> Result<Vec<WatchlistItemId>> {
        let new = refs.iter().map(|r| new_entry(c, r)).collect::<Result<Vec<_>>>()?;
        Ok(add(c, lib, id, &new, Insert::End, 1)?.added)
    }

    /// DESIGN §42 §8.1: any copy counts, a pinned playback copy does not own done state,
    /// and restarting only moves this list's progress boundary. Undo restores skips too.
    #[test]
    fn done_across_copies_restart_and_unwatch() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list = create(c, "Saga", None, PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &lib, list, &[movie(1, "empire"), movie(1, "hope")])?;
            pin(c, ids[0], &movie(1, "empire"))?;
            progress(c, &movie(2, "empire2"), true, 7200.0, 100)?;
            assert_eq!(view(c, &lib, list, false, false)?.unwrap().next, Some(ids[1]));
            skip(c, list, &[ids[1]], true)?;
            let undo_restart = restart(c, list, 101)?;
            let v = view(c, &lib, list, false, false)?.unwrap();
            assert_eq!((v.next, v.done), (Some(ids[0]), 0));
            assert!(user::progress(c, &movie(2, "empire2"))?.unwrap().finished);
            undo(c, &undo_restart)?;
            assert_eq!(view(c, &lib, list, false, false)?.unwrap().done, 2);
            c.execute("DELETE FROM watch_progress WHERE kind='movie'", [])?;
            assert_eq!(view(c, &lib, list, false, false)?.unwrap().next, Some(ids[0]));
            Ok(())
        })
    }

    /// Undo reverses one edit: additions and relative order chosen afterwards survive.
    /// Occupied old IDs are remapped, including chips on rows already in Up next.
    #[test]
    fn undo_remove_keeps_later_additions_order_and_queue_chips() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list_id = create(c, "Before", None, PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &lib, list_id, &[movie(1, "empire"), movie(1, "hope")])?;
            c.execute("INSERT INTO queue_item(position,source_id,kind,item_key,title,added_at,watchlist_item_id) VALUES (0,1,'movie','hope','A New Hope',1,?1)",[ids[1]])?;
            let undo_remove = remove(c, list_id, &[ids[1]])?;
            let added = add_refs(c, &lib, list_id, &[movie(1, "elf")])?[0];
            assert!(added.0 > ids[1].0, "new writes do not reuse deleted IDs");
            // A restored/older database can already have reused the old ID.
            c.execute("UPDATE watchlist_item SET id=?2 WHERE id=?1", params![added, ids[1]])?;
            move_entries(c, list_id, &[ids[1]], 0)?;
            rename(c, list_id, "After", Some("keep this note"))?;
            undo(c, &undo_remove)?;
            let rows = entries(c, list_id)?;
            assert_eq!(rows.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["Elf", "A New Hope", "Empire"]);
            assert_eq!(rows.iter().map(|e| e.position).collect::<Vec<_>>(), [0, 1, 2]);
            assert_eq!(rows[0].id, ids[1], "the later row keeps its identity");
            assert_ne!(rows[1].id, ids[1]);
            assert_eq!(c.query_row("SELECT watchlist_item_id FROM queue_item", [], |r| r.get::<_, WatchlistItemId>(0))?, rows[1].id);
            let list = list(c, list_id)?.unwrap();
            assert_eq!((list.name.as_str(), list.note.as_deref()), ("After", Some("keep this note")));
            Ok(())
        })
    }

    #[test]
    fn undo_restart_keeps_later_name_order_entries_and_skips() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list_id = create(c, "Before", None, PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &lib, list_id, &[movie(1, "empire"), movie(1, "hope")])?;
            skip(c, list_id, &[ids[1]], true)?;
            let undo_restart = restart(c, list_id, 20)?;
            let added = add_refs(c, &lib, list_id, &[movie(1, "elf")])?[0];
            move_entries(c, list_id, &[added], 0)?;
            skip(c, list_id, &[ids[0]], true)?;
            rename(c, list_id, "After", Some("new note"))?;
            set_order(c, list_id, PlayOrder::Any)?;
            undo(c, &undo_restart)?;
            let list = list(c, list_id)?.unwrap();
            assert_eq!((list.name.as_str(), list.note.as_deref(), list.play_order, list.since), ("After", Some("new note"), PlayOrder::Any, 0));
            let rows = entries(c, list_id)?;
            assert_eq!(rows.iter().map(|e| e.id).collect::<Vec<_>>(), [added, ids[0], ids[1]]);
            assert_eq!(rows.iter().map(|e| e.skipped).collect::<Vec<_>>(), [false, true, true]);
            Ok(())
        })
    }

    #[test]
    fn undo_my_list_add_keeps_later_additions_and_readded_title() {
        with_db(|c| {
            let lib = library::load(c)?;
            let (_, undo_add) = toggle_my_list(c, &lib, &movie(1, "empire"), 1)?;
            let list = default_list(c)?;
            let added = add_refs(c, &lib, list, &[movie(1, "hope"), movie(1, "elf")])?;
            undo(c, &undo_add)?;
            assert_eq!(entries(c, list)?.iter().map(|e| e.id).collect::<Vec<_>>(), added);
            let (_, old_add) = toggle_my_list(c, &lib, &movie(1, "empire"), 2)?;
            assert!(!toggle_my_list(c, &lib, &movie(1, "empire"), 3)?.0);
            assert!(toggle_my_list(c, &lib, &movie(1, "empire"), 4)?.0);
            undo(c, &old_add)?;
            assert_eq!(entries(c, list)?.len(), 3, "an old Undo cannot remove a re-added title");
            Ok(())
        })
    }

    #[test]
    fn undo_delete_preserves_reused_ids_and_rejects_name_collision() {
        with_db(|c| {
            let lib = library::load(c)?;
            let old = create(c, "Original", None, PlayOrder::Ordered, "app", 1)?;
            let old_entry = add_refs(c, &lib, old, &[movie(1, "empire")])?[0];
            let undo_delete = delete(c, old)?;
            c.execute("INSERT INTO watchlist(id,name,created_at) VALUES (?1,'Newer',2)", [old])?;
            let new_entry = add_refs(c, &lib, old, &[movie(1, "elf")])?[0];
            c.execute("UPDATE watchlist_item SET id=?2 WHERE id=?1", params![new_entry, old_entry])?;
            undo(c, &undo_delete)?;
            assert_eq!(list(c, old)?.unwrap().name, "Newer");
            assert_eq!(entries(c, old)?[0].title, "Elf");
            let restored = lists(c)?.into_iter().find(|l| l.name == "Original").unwrap();
            assert_ne!(restored.id, old);
            let restored_entry = &entries(c, restored.id)?[0];
            assert_eq!(restored_entry.title, "Empire");
            assert_ne!(restored_entry.id, old_entry);
            let deleted = delete(c, restored.id)?;
            let newer = create(c, "Original", None, PlayOrder::Any, "app", 3)?;
            assert!(undo(c, &deleted).is_err());
            assert_eq!(list(c, newer)?.unwrap().play_order, PlayOrder::Any);
            assert!(entries(c, newer)?.is_empty());
            Ok(())
        })
    }

    /// DESIGN §42 §8.2: distinct ranges of one title advance independently, across copies.
    #[test]
    fn series_ranges_count_every_episode_and_ignore_other_seasons() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list = create(c, "Shows", None, PlayOrder::Ordered, "app", 1)?;
            let mut a = new_entry(c, &series(1))?;
            a.season_from = Some(1);
            a.season_to = Some(2);
            let mut b = a.clone();
            b.season_from = Some(3);
            b.season_to = Some(3);
            let ids = add(c, &lib, list, &[a.clone(), b], Insert::End, 1)?.added;
            assert_eq!(add(c, &lib, list, &[a], Insert::End, 1)?.refused[0].1, "overlapping_seasons");
            for (s, e) in [(1, 1), (1, 2), (2, 1), (3, 1), (3, 2)] {
                progress(c, &ItemRef { source: SourceId(2), kind: ItemKind::Episode, key: ItemKey(format!("s2-{s}-{e}")) }, true, 3000.0, 10)?;
            }
            let v = view(c, &lib, list, false, false)?.unwrap();
            assert_eq!(v.next, Some(ids[0]));
            assert_eq!((v.entries[0].done_episodes, v.entries[0].total_episodes), (3, 4));
            assert!(v.entries[1].done());
            progress(c, &ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey("s1-2-2".into()) }, false, 321.0, 20)?;
            let v = view(c, &lib, list, false, false)?.unwrap();
            assert_eq!(v.entries[0].episode, Some((2, 2)));
            assert_eq!(v.entries[0].progress.as_ref().unwrap().position_s, 321.0);
            progress(c, &ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey("s1-2-2".into()) }, true, 3000.0, 21)?;
            assert!(view(c, &lib, list, false, false)?.unwrap().next.is_none());
            Ok(())
        })
    }

    #[test]
    fn progress_counts_episodes_titles_and_skips_without_changing_entry_positions() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list = create(c, "Progress", None, PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &lib, list, &[series(1)])?;
            progress(c, &ItemRef { source: SourceId(2), kind: ItemKind::Episode, key: ItemKey("s2-1-1".into()) }, true, 3000.0, 10)?;
            let v = view(c, &lib, list, false, false)?.unwrap();
            assert_eq!((v.done, v.total), (1, 6));
            assert_eq!(v.progress_label(), "1 of 6 episodes");
            assert_eq!(v.place(ids[0]), Some((1, 1)));
            assert!(!v.finished());
            skip(c, list, &ids, true)?;
            let v = view(c, &lib, list, false, false)?.unwrap();
            assert_eq!(v.progress_label(), "6 of 6 episodes");
            assert!(v.finished());
            let films = add_refs(c, &lib, list, &[movie(1, "hope"), movie(1, "empire")])?;
            skip(c, list, &[films[0]], true)?;
            let v = view(c, &lib, list, false, false)?.unwrap();
            assert_eq!(v.progress_label(), "7 of 8 watched");
            assert_eq!(v.place(films[1]), Some((3, 3)));
            remove(c, list, &ids)?;
            assert_eq!(view(c, &lib, list, false, false)?.unwrap().progress_label(), "1 of 2 titles");
            Ok(())
        });
    }

    /// DESIGN §42 §8.3–4: the agent passes locked rows even in an unlocked session. Ordered
    /// traversal says why; Any order never chooses those rows and resumes a partial first.
    #[test]
    fn next_filters_and_reports_and_any_order_prefers_resume() {
        with_db(|c| {
            user::set_item_override(c, &movie(1, "hope"), &crate::model::ItemOverride { locked: true, ..Default::default() })?;
            let lib = library::load(c)?;
            let list = create(c, "Picks", None, PlayOrder::Ordered, "app", 1)?;
            let missing = NewEntry {
                item: None,
                kind: ItemKind::Movie,
                title: "Holiday Special".into(),
                year: Some(1978),
                tmdb_id: None,
                season_from: None,
                season_to: None,
                series_title: None,
                season: None,
                episode: None,
            };
            let mut new = vec![missing];
            for item in [movie(1, "hope"), movie(1, "empire"), movie(1, "elf")] {
                new.push(new_entry(c, &item)?)
            }
            let ids = add(c, &lib, list, &new, Insert::End, 1)?.added;
            skip(c, list, &[ids[2]], true)?;
            let v = view(c, &lib, list, true, true)?.unwrap();
            assert_eq!(v.next, Some(ids[3]));
            assert_eq!(
                v.passed_over.iter().map(|p| p.why.as_str()).collect::<Vec<_>>(),
                ["not in your playlists", "locked: needs your PIN in Tuner"]
            );
            assert_eq!(view(c, &lib, list, false, false)?.unwrap().next, Some(ids[1]));
            skip(c, list, &[ids[2]], false)?;
            progress(c, &movie(1, "elf"), false, 123.0, 12)?;
            set_order(c, list, PlayOrder::Any)?;
            for _ in 0..20 {
                let v = view(c, &lib, list, true, true)?.unwrap();
                assert_eq!(v.next, Some(ids[3]));
                assert!(v.passed_over.is_empty());
            }
            progress(c, &movie(2, "empire2"), true, 7200.0, 13)?;
            progress(c, &movie(1, "elf"), true, 7200.0, 13)?;
            assert!(view(c, &lib, list, true, true)?.unwrap().next.is_none());
            Ok(())
        })
    }

    /// DESIGN §42 §8.5: imports attach by either identity, and source deletion keeps the
    /// title in place as a placeholder. A remake with another year does not attach.
    #[test]
    fn placeholders_attach_by_identity_and_survive_source_deletion() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list = create(c, "Missing", None, PlayOrder::Ordered, "app", 1)?;
            let new = |title: &str, year, tmdb_id| NewEntry {
                item: None,
                kind: ItemKind::Movie,
                title: title.into(),
                year,
                tmdb_id,
                season_from: None,
                season_to: None,
                series_title: None,
                season: None,
                episode: None,
            };
            let ids = add(
                c,
                &lib,
                list,
                &[new("Completely different title", Some(2000), Some(88)), new("Rogue One", Some(2016), None), new("Rogue One", Some(2030), None)],
                Insert::End,
                1,
            )?
            .added;
            c.execute_batch("INSERT INTO movie(source_id,key,name,clean_name,norm_name,year,tmdb_id,stream,provider_order,seen_gen) VALUES (1,'tmdb','Actual (2000)','Actual','actual',2000,'88','{}',3,1),(1,'rogue','Rogue One (2016)','Rogue One','rogueone',2016,NULL,'{}',4,1)")?;
            titles::refresh_source(c, SourceId(1))?;
            match_placeholders(c)?;
            assert_eq!(entry(c, ids[0])?.unwrap().item, Some(movie(1, "tmdb")));
            assert_eq!(entry(c, ids[1])?.unwrap().item, Some(movie(1, "rogue")));
            assert!(entry(c, ids[2])?.unwrap().item.is_none());
            c.execute("DELETE FROM source WHERE id=1", [])?;
            let rows = entries(c, list)?;
            assert_eq!(rows.len(), 3);
            assert!(rows.iter().all(|e| e.item.is_none()));
            assert_eq!(rows.iter().map(|e| e.position).collect::<Vec<_>>(), [0, 1, 2]);
            Ok(())
        })
    }

    #[test]
    fn episode_placeholders_match_series_identity_and_coordinates() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list = create(c, "Missing episodes", None, PlayOrder::Ordered, "app", 1)?;
            let new = |series_title: &str, year, tmdb_id, season, episode| NewEntry {
                item: None,
                kind: ItemKind::Episode,
                title: "Episode 1-1".into(),
                series_title: Some(series_title.into()),
                year: Some(year),
                tmdb_id,
                season: Some(season),
                episode: Some(episode),
                season_from: None,
                season_to: None,
            };
            let targets = [
                new("A translated series name", 2000, Some(88), 1, 1),
                new("ÁNDOR", 2022, None, 2, 2),
                new("Andor", 2030, None, 1, 1),
                new("Unrelated series", 2022, None, 1, 1),
                new("Andor", 2022, None, 9, 1),
                new("Andor", 2022, None, 0, 1),
                new("Andor", 2022, None, 1, 2),
            ];
            let ids = add(c, &lib, list, &targets, Insert::End, 1)?.added;
            assert_eq!(ids.len(), targets.len(), "S/E participates in duplicate identity");
            assert_eq!(add(c, &lib, list, &targets[1..2], Insert::End, 1)?.refused[0].1, "duplicate");
            // The episode names are all identical, and their air years differ from the
            // series year. The pinned matched copy wins; one S/E exists only in copy 1.
            c.execute_batch(
                "UPDATE series SET tmdb_id='88'; UPDATE episode SET title='Pilot',air_date='2025-01-01';
                DELETE FROM episode WHERE source_id=2 AND season=2 AND episode=2;
                INSERT INTO title_link(source_id,kind,item_key,action,at) VALUES (2,'series','andor2','pin',1)",
            )?;
            titles::refresh_source(c, SourceId(1))?;
            match_placeholders(c)?;
            let episode = |source, season, number| ItemRef {
                source: SourceId(source),
                kind: ItemKind::Episode,
                key: ItemKey(format!("s{source}-{season}-{number}")),
            };
            assert_eq!(entry(c, ids[0])?.unwrap().item, Some(episode(2, 1, 1)));
            assert_eq!(entry(c, ids[1])?.unwrap().item, Some(episode(1, 2, 2)));
            for id in &ids[2..5] {
                assert!(entry(c, *id)?.unwrap().item.is_none());
            }
            assert_eq!(entry(c, ids[5])?.unwrap().item, Some(episode(2, 0, 1)));
            assert_eq!(entry(c, ids[6])?.unwrap().item, Some(episode(2, 1, 2)));
            // An existing TMDB series with no requested S/E must not fall back to the
            // name of a different series that does have it.
            c.execute_batch(
                "INSERT INTO series(source_id,key,name,clean_name,norm_name,year,tmdb_id,provider_order,seen_gen)
                VALUES (1,'other','Other','Other','other',2022,'99',1,1)",
            )?;
            assert!(match_placeholder(c, &new("Andor", 2022, Some(99), 1, 1))?.is_none());
            // With no TMDB match, the folded series title and year are the fallback.
            assert_eq!(match_placeholder(c, &new("Andor", 2022, Some(100), 1, 1))?, Some(episode(2, 1, 1)));
            Ok(())
        })
    }

    #[test]
    fn episode_identity_survives_deletion_undo_and_backup() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list = create(c, "Episodes", None, PlayOrder::Ordered, "app", 1)?;
            c.execute("UPDATE series SET tmdb_id='88'", [])?;
            let refs = [1, 2].map(|n| ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey(format!("s1-1-{n}")) });
            let ids = add_refs(c, &lib, list, &refs)?;
            let removed = remove(c, list, &ids[1..])?;
            c.execute("DELETE FROM source WHERE id=1", [])?;
            undo(c, &removed)?;
            let original = entries(c, list)?;
            assert_eq!(original.len(), 2);
            for e in &original {
                assert!(e.item.is_none());
                assert_eq!((e.series_title.as_deref(), e.year, e.tmdb_id, e.season), (Some("Andor"), Some(2022), Some(88), Some(1)));
            }
            let saved = export(c)?.unwrap();
            delete(c, list)?;
            let restored = import(c, &saved, &|r| Some(r.clone()))?;
            let restored_list = restored.lists[&list];
            assert_eq!(entries(c, restored_list)?.len(), 2);
            assert_eq!(import(c, &saved, &|r| Some(r.clone()))?.rows, 0);
            titles::refresh_source(c, SourceId(2))?;
            match_placeholders(c)?;
            for (i, e) in entries(c, restored_list)?.iter().enumerate() {
                assert_eq!(e.item, Some(ItemRef { source: SourceId(2), kind: ItemKind::Episode, key: ItemKey(format!("s2-1-{}", i + 1)) }));
            }
            Ok(())
        })
    }

    #[test]
    fn episode_placeholder_attaches_when_lazy_details_bring_its_number() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list = create(c, "Later episode", None, PlayOrder::Ordered, "app", 1)?;
            let target = NewEntry {
                item: None,
                kind: ItemKind::Episode,
                title: "Andor".into(),
                series_title: Some("Andor".into()),
                year: Some(2022),
                tmdb_id: None,
                season: Some(4),
                episode: Some(1),
                season_from: None,
                season_to: None,
            };
            let id = add(c, &lib, list, &[target], Insert::End, 1)?.added[0];
            assert_eq!(match_placeholders(c)?, 0);
            let d = crate::store::import::SeriesDetails {
                episodes: vec![crate::store::import::ImportEpisode {
                    key: ItemKey("later".into()),
                    provider_id: None,
                    season: 4,
                    episode: 1,
                    title: "A different title".into(),
                    plot: None,
                    duration_s: None,
                    still: None,
                    air_date: None,
                    rating: None,
                    stream: crate::model::StreamLocator::Url { url: "https://example.test/episode".into() },
                }],
                tmdb_id: Some(88),
                ..Default::default()
            };
            crate::store::import::store_series_details(c, crate::model::SeriesId(2), &d, 2)?;
            let e = entry(c, id)?.unwrap();
            assert_eq!(e.item, Some(ItemRef { source: SourceId(2), kind: ItemKind::Episode, key: ItemKey("later".into()) }));
            assert_eq!(e.tmdb_id, Some(88));
            Ok(())
        })
    }

    #[test]
    fn lists_keep_manual_order_with_default_first_and_finished_last() {
        with_db(|c| {
            let lib = library::load(c)?;
            let mine = default_list(c)?;
            let a = create(c, "First", None, PlayOrder::Ordered, "app", 1)?;
            let done = create(c, "Finished", None, PlayOrder::Ordered, "app", 1)?;
            let b = create(c, "Last", None, PlayOrder::Ordered, "app", 1)?;
            add_refs(c, &lib, a, &[movie(1, "hope")])?;
            add_refs(c, &lib, b, &[movie(1, "elf")])?;
            add_refs(c, &lib, done, &[movie(1, "empire")])?;
            progress(c, &movie(1, "empire"), true, 7200.0, 10)?;
            played(c, b, 20)?;
            played(c, done, 30)?;
            let order = |c: &Connection| -> Result<Vec<_>> { Ok(snapshot(c, &lib, false, false)?.lists.iter().map(|l| l.list.id).collect()) };
            assert_eq!(lists(c)?.iter().map(|l| l.id).collect::<Vec<_>>(), [mine, a, done, b]);
            assert_eq!(order(c)?, [mine, a, b, done]);
            reorder_lists(c, &[b, a, mine])?;
            assert_eq!(order(c)?, [mine, b, a, done]);
            assert_eq!(lists(c)?.iter().map(|l| l.sort_index).collect::<Vec<_>>(), [0, 1, 2, 3]);
            played(c, a, 40)?;
            assert_eq!(order(c)?, [mine, b, a, done]);
            progress(c, &movie(1, "empire"), false, 0.0, 50)?;
            assert_eq!(order(c)?, [mine, b, done, a]);
            // Missing entries are unfinished, even without a next playable entry.
            let target = NewEntry {
                item: None,
                kind: ItemKind::Movie,
                title: "Missing".into(),
                year: Some(2000),
                tmdb_id: None,
                series_title: None,
                season: None,
                episode: None,
                season_from: None,
                season_to: None,
            };
            add(c, &lib, done, &[target], Insert::End, 1)?;
            progress(c, &movie(1, "empire"), true, 7200.0, 60)?;
            assert_eq!(order(c)?, [mine, b, done, a]);
            assert!(!view(c, &lib, done, false, false)?.unwrap().finished());
            Ok(())
        })
    }

    /// DESIGN §42 §8.6: the shared reattach pipeline follows provider re-keys. Move/remove
    /// stay dense, Undo puts the original order back, and backup keeps list state and refs.
    #[test]
    fn holders_dense_order_and_backup_round_trip() {
        with_db(|c| {
            let lib = library::load(c)?;
            let list = create(c, "Order", Some("story order"), PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &lib, list, &[movie(1, "empire"), movie(1, "hope"), movie(1, "elf")])?;
            c.execute("UPDATE movie SET key='changed' WHERE source_id=1 AND key='empire'", [])?;
            crate::store::import::reattach_user_data(c, SourceId(1), 10)?;
            assert_eq!(entry(c, ids[0])?.unwrap().item, Some(movie(1, "changed")));
            move_entries(c, list, &[ids[2]], 0)?;
            let undo_remove = remove(c, list, &[ids[1]])?;
            let rows = entries(c, list)?;
            assert_eq!(rows.iter().map(|e| e.id).collect::<Vec<_>>(), [ids[2], ids[0]]);
            assert_eq!(rows.iter().map(|e| e.position).collect::<Vec<_>>(), [0, 1]);
            undo(c, &undo_remove)?;
            let saved = export(c)?.unwrap();
            let before = entries(c, list)?;
            delete(c, list)?;
            let result = import(c, &saved, &|r| Some(r.clone()))?;
            let new = result.lists[&list];
            let after = entries(c, new)?;
            assert_eq!(
                after.iter().map(|e| (&e.title, e.position, e.season_from, e.skipped)).collect::<Vec<_>>(),
                before.iter().map(|e| (&e.title, e.position, e.season_from, e.skipped)).collect::<Vec<_>>()
            );
            crate::store::import::reattach_user_data(c, SourceId(1), 11)?;
            assert!(entries(c, new)?.iter().all(|e| e.orphaned_since.is_none()));
            Ok(())
        })
    }

    /// DESIGN §42 §8.12: concealing a title removes it from every count and the active rail,
    /// including a fresh explicit gate with a stale library; showing it restores its place.
    #[test]
    fn concealed_entries_leave_no_counts_and_return_in_place() {
        with_db(|c| {
            c.execute("UPDATE movie SET explicit=1 WHERE id=1", [])?;
            let shown = library::load_visible(c, library::Visibility { explicit: true })?;
            let list = create(c, "Private", None, PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &shown, list, &[movie(1, "empire")])?;
            let hidden = snapshot_with_visibility(c, &shown, false, true, true)?;
            assert!(hidden.list(list).is_none());
            assert!(hidden.active().all(|l| l.list.id != list));
            let visible = snapshot_with_visibility(c, &shown, true, false, false)?;
            let v = visible.list(list).unwrap();
            assert_eq!((v.total, v.next), (1, Some(ids[0])));
            assert!(!format!("{v:?}").contains("http"));
            assert_eq!(entries(c, list)?[0].position, 0);
            Ok(())
        })
    }

    /// A legacy M3U channel can become a film on refresh. Its saved row leaves Favourites,
    /// and later reclassifications from the same mixed list reuse its watchlist.
    #[test]
    fn reclassified_channel_favourites_move_once_to_the_same_watchlist() {
        with_db(|c| {
            let favourites = user::create_fav_list(c, "Weekend", "")?;
            let channel = |key: &str| ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(key.into()) };
            for key in ["empire", "hope", "elf"] {
                user::set_favourite(c, favourites, &channel(key), true, 1)?;
            }
            let reclassify = |key: &str| crate::store::import::KeyMove { from: channel(key), to: movie(1, key) };
            crate::store::import::apply_key_moves(c, &[reclassify("empire")])?;
            let weekend = lists(c)?.into_iter().find(|l| l.name == "Weekend").unwrap();
            assert_eq!(entries(c, weekend.id)?[0].item, Some(movie(1, "empire")));
            assert!(user::fav_items(c, favourites)?.iter().all(|e| e.item.kind == ItemKind::Channel));
            dismiss_moved_card(c)?;
            assert_eq!(crate::store::import::apply_key_moves(c, &[reclassify("empire")])?, 0);
            assert!(moved_card(c)?.is_empty(), "an unchanged refresh does not repeat the notice");
            user::rename_fav_list(c, favourites, "Weekend channels", "")?;
            crate::store::import::apply_key_moves(c, &[reclassify("hope")])?;
            assert_eq!(lists(c)?.len(), 2, "the mixed list's existing watchlist is reused");
            assert_eq!(
                entries(c, weekend.id)?.iter().map(|e| (e.position, e.item.as_ref().unwrap().key.as_str())).collect::<Vec<_>>(),
                [(0, "empire"), (1, "hope")]
            );
            Ok(())
        })
    }
    #[test]
    fn watchlist_matching_uses_indexes_ignores_bad_tmdb_and_preserves_provider_title() {
        with_db(|c| {
            for table in ["movie", "series"] {
                let plans = c
                    .prepare(&format!("EXPLAIN QUERY PLAN SELECT source_id,key FROM {table} WHERE match_hash=?1 ORDER BY source_id,id"))?
                    .query_map([1], |r| r.get::<_, String>(3))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                assert!(plans.iter().any(|p| p.contains(&format!("watchlist_{table}_hash"))), "{plans:?}");
                assert!(!plans.iter().any(|p| p.contains(&format!("SCAN {table}"))), "{plans:?}");
            }
            c.execute_batch("UPDATE movie SET tmdb_id='0' WHERE id=1; UPDATE movie SET tmdb_id='unknown' WHERE id=3;")?;
            assert_eq!(new_entry(c, &movie(1, "empire"))?.tmdb_id, None);
            assert_eq!(new_entry(c, &movie(1, "hope"))?.tmdb_id, None);
            user::set_item_override(
                c,
                &movie(1, "hope"),
                &crate::model::ItemOverride { rename: Some("My renamed film".into()), ..Default::default() },
            )?;
            let lib = library::load(c)?;
            let list = create(c, "Identity", None, PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &lib, list, &[movie(1, "hope")])?;
            let mut missing = new_entry(c, &movie(1, "empire"))?;
            missing.item = None;
            missing.tmdb_id = Some(0);
            missing.title = "Unrelated".into();
            assert!(match_placeholder(c, &missing)?.is_none());
            assert_eq!(add(c, &lib, list, &[missing], Insert::End, 1)?.added.len(), 1);
            c.execute("UPDATE watchlist_item SET source_id=NULL,item_key=NULL WHERE id=?1", [ids[0]])?;
            assert_eq!(match_placeholders(c)?, 1);
            let entry = entry(c, ids[0])?.unwrap();
            assert_eq!(entry.item, Some(movie(1, "hope")));
            assert_eq!(entry.title, "My renamed film");
            Ok(())
        });
    }

    #[test]
    fn watchlist_legacy_move_deduplicates_and_repairs_attached_fallbacks() {
        with_db(|c| {
            c.execute_batch(
                "INSERT INTO fav_list(id,name,icon,sort_index,is_default) VALUES(20,'Trending','',1,0);
                INSERT INTO fav_item(list_id,source_id,kind,item_key,position,added_at,fp) VALUES
                (1,1,'movie','empire',0,1,'{}'),(1,2,'movie','empire2',1,1,'{}'),(20,1,'movie','hope',0,1,'{}');",
            )?;
            move_favourites(c)?;
            let mine = default_list(c)?;
            assert_eq!(entries(c, mine)?.len(), 1);
            let trending = lists(c)?.into_iter().find(|l| l.name == "Trending").unwrap().id;
            c.execute_batch(
                "INSERT INTO fav_list(id,name,icon,sort_index,is_default) VALUES(30,'Trending','',1,0);
                INSERT INTO fav_item(list_id,source_id,kind,item_key,position,added_at,fp) VALUES
                (30,1,'movie','hope',0,1,'{}'),(30,1,'movie','elf',1,1,'{}');",
            )?;
            move_favourites(c)?;
            assert_eq!(lists(c)?.len(), 2);
            assert_eq!(entries(c, trending)?.iter().map(|e| e.position).collect::<Vec<_>>(), [0, 1]);
            let id = entries(c, trending)?[0].id;
            c.execute(
                "UPDATE watchlist_item SET title='anewhope',year=NULL,tmdb_id=NULL,fp='{\"norm_name\":\"anewhope\"}',orphaned_since=1 WHERE id=?1",
                [id],
            )?;
            crate::store::import::reattach_user_data(c, SourceId(1), 10)?;
            let e = entry(c, id)?.unwrap();
            assert_eq!((e.title.as_str(), e.year, e.orphaned_since), ("A New Hope", Some(1977), None));
            c.execute("UPDATE watchlist_item SET orphaned_since=11 WHERE id=?1", [id])?;
            user::forget_orphan(c, &movie(1, "hope"))?;
            assert!(entry(c, id)?.unwrap().item.is_none(), "Forget retains the title as a placeholder");
            assert_eq!(match_placeholders(c)?, 1);
            Ok(())
        });
    }

    #[test]
    fn watchlist_watched_marks_undo_restores_progress_on_every_copy() {
        with_db(|c| {
            let items = vec![(movie(1, "empire"), "Empire".into()), (movie(2, "empire2"), "Empire".into())];
            progress(c, &items[0].0, false, 123.0, 10)?;
            let before = copies::watched_items(c, &items, true, 20)?;
            assert_eq!(c.query_row("SELECT count(*) FROM watch_progress WHERE finished=1", [], |r| r.get::<_, i64>(0))?, 2);
            copies::undo(c, &before)?;
            assert_eq!(c.query_row("SELECT count(*) FROM watch_progress", [], |r| r.get::<_, i64>(0))?, 1);
            assert_eq!(
                c.query_row("SELECT position_s,finished FROM watch_progress", [], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, bool>(1)?)))?,
                (123.0, false)
            );
            Ok(())
        });
    }

    #[test]
    fn watchlist_merge_preserves_settings_and_dense_positions_and_duplicate_since() {
        with_db(|c| {
            let lib = library::load(c)?;
            let id = create(c, "Saga", Some("saved"), PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &lib, id, &[movie(1, "empire"), movie(1, "hope"), movie(1, "elf")])?;
            restart(c, id, 50)?;
            skip(c, id, &[ids[1]], true)?;
            let copied = duplicate(c, id, "Copy", 60)?;
            assert_eq!(list(c, copied)?.unwrap().since, 50);
            assert!(entries(c, copied)?[1].skipped);
            let saved = export(c)?.unwrap();
            remove(c, id, &ids[1..])?;
            c.execute("UPDATE watchlist SET since=100,play_order='any',note='local',sort_index=7,last_played_at=101 WHERE id=?1", [id])?;
            import(c, &saved, &|r| Some(r.clone()))?;
            let l = list(c, id)?.unwrap();
            assert_eq!(
                (l.since, l.play_order, l.note.as_deref(), l.sort_index, l.last_played_at),
                (100, PlayOrder::Any, Some("local"), 7, Some(101))
            );
            assert_eq!(entries(c, id)?.iter().map(|e| e.position).collect::<Vec<_>>(), [0, 1, 2]);
            assert_eq!(import(c, &saved, &|r| Some(r.clone()))?.rows, 0);
            Ok(())
        });
    }

    #[test]
    fn watchlist_ended_refs_pass_only_that_episode_without_marking_watched() {
        with_db(|c| {
            let lib = library::load(c)?;
            let id = create(c, "Episodes", None, PlayOrder::Ordered, "app", 1)?;
            add_refs(c, &lib, id, &[series(1)])?;
            let ended = ItemRef { source: SourceId(2), kind: ItemKind::Episode, key: ItemKey("s2-1-1".into()) };
            progress(c, &ended, false, 1.0, 20)?;
            let v = view_for_run(c, &lib, id, true, false, false, Default::default(), std::slice::from_ref(&ended))?.unwrap();
            assert_eq!(v.entries[0].episode, Some((1, 2)));
            assert_eq!(v.entries[0].done_episodes, 0);
            assert!(!user::progress(c, &ended)?.unwrap().finished);
            for season in 1..=3 {
                for episode in 1..=2 {
                    if (season, episode) != (1, 1) {
                        progress(
                            c,
                            &ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey(format!("s1-{season}-{episode}")) },
                            true,
                            3000.0,
                            21,
                        )?;
                    }
                }
            }
            assert!(
                view_for_run(c, &lib, id, true, false, false, Default::default(), std::slice::from_ref(&ended))?.unwrap().entries[0]
                    .playable
                    .is_none(),
                "the excluded unfinished episode must not replay an already watched episode"
            );
            assert_eq!(view(c, &lib, id, true, false)?.unwrap().entries[0].episode, Some((1, 1)), "new run can resume the unfinished episode");
            Ok(())
        });
    }

    #[test]
    fn watchlist_loaded_series_counts_done_with_an_unloaded_other_copy() {
        with_db(|c| {
            c.execute_batch("UPDATE source SET kind='xtream' WHERE id=2; DELETE FROM episode WHERE source_id=2; UPDATE series SET details_at=NULL WHERE source_id=2;
                INSERT INTO title_link(source_id,kind,item_key,action,at) VALUES (1,'series','andor','pin',1);")?;
            let lib = library::load(c)?;
            let id = create(c, "Series", None, PlayOrder::Ordered, "app", 1)?;
            let ids = add_refs(c, &lib, id, &[series(1)])?;
            assert_eq!(view(c, &lib, id, true, false)?.unwrap().entries[0].playable.as_ref().unwrap().kind, ItemKind::Episode);
            for season in 1..=3 {
                for episode in 1..=2 {
                    progress(
                        c,
                        &ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey(format!("s1-{season}-{episode}")) },
                        true,
                        3000.0,
                        20,
                    )?;
                }
            }
            assert!(view(c, &lib, id, true, false)?.unwrap().finished());
            pin(c, ids[0], &series(2))?;
            assert_eq!(
                view(c, &lib, id, true, false)?.unwrap().entries[0].playable,
                Some(series(2)),
                "explicit unloaded pin still needs its details"
            );
            let plan = copies::move_plan(c, SourceId(2))?;
            assert!(plan.cannot.iter().any(|entry| entry.why.contains("pinned")));
            Ok(())
        });
    }
}
