//! Stored matching data of movies and series and the user's corrections (DESIGN §29.10–§29.11,
//! P2): the `match_*` columns that the library build stacks on, and `title_link` (split /
//! pin).
//!
//! * The match columns are computed from the provider's RAW `name` and `year` by
//!   [`sources::vod_match::parse`](crate::sources::vod_match::parse), per playlist (its
//!   frequent prefixes are part of the rules), by [`refresh_source`]: at the end of every
//!   catalogue import (`import::finish_catalogue_with`) and once for every playlist by
//!   migration 0005. Only rows not computed yet (`match_tags IS NULL`: new, or the import saw
//!   another name or year) are parsed — unless the rules version or the playlist's prefixes
//!   changed (`meta` `vod_match:<id>`), then all of them. Only changed rows are written.
//! * A split or a pin changes stacks: write it with `DataScope::Overrides` (the library
//!   rebuilds).

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::model::{ItemKey, ItemKind, ItemRef, SourceId, Timestamp};
use crate::sources::vod_match::{self, PrefixCodes, RULES_VERSION};

/// The `meta` key of a playlist's match stamp.
fn stamp_key(source: SourceId) -> String {
    format!("vod_match:{}", source.0)
}

/// Recompute the match columns of one playlist's movies and series where needed (module
/// docs). Returns the rows written.
pub fn refresh_source(conn: &Connection, source: SourceId) -> Result<u32> {
    let mut names: Vec<String> = Vec::new();
    for table in ["movie", "series"] {
        let mut stmt = conn.prepare_cached(&format!("SELECT name FROM {table} WHERE source_id = ?1"))?;
        for n in stmt.query_map([source], |r| r.get::<_, String>(0))? {
            names.push(n?);
        }
    }
    if names.is_empty() {
        conn.execute("DELETE FROM meta WHERE key = ?1", [stamp_key(source)])?;
        return Ok(0);
    }
    let codes = PrefixCodes::from_names(names.iter().map(String::as_str));
    drop(names);
    let stamp = format!("{RULES_VERSION}:{:016x}", codes.fingerprint());
    let old: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [stamp_key(source)], |r| r.get(0)).optional()?;
    let all = old.as_deref() != Some(stamp.as_str());
    let mut written = 0;
    for (table, kind) in [("movie", ItemKind::Movie), ("series", ItemKind::Series)] {
        written += refresh_table(conn, source, table, kind, &codes, all)?;
    }
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![stamp_key(source), stamp],
    )?;
    Ok(written)
}

type Stored = (Option<String>, Option<i64>, Option<String>, Option<i64>, Option<String>);

fn refresh_table(conn: &Connection, source: SourceId, table: &str, kind: ItemKind, codes: &PrefixCodes, all: bool) -> Result<u32> {
    let filter = if all { "" } else { " AND match_tags IS NULL" };
    let mut read = conn.prepare(&format!(
        "SELECT id, name, year, match_key, match_hash, match_lang, match_tags, match_origin FROM {table} WHERE source_id = ?1{filter}"
    ))?;
    let mut write = conn.prepare_cached(&format!(
        "UPDATE {table} SET match_key = ?2, match_hash = ?3, match_lang = ?4, match_tags = ?5, match_origin = ?6 WHERE id = ?1"
    ))?;
    let mut rows = read.query([source])?;
    let mut written = 0;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        let info = vod_match::parse(&name, r.get(2)?, codes);
        let new: Stored = (
            Some(info.key.clone()).filter(|k| !k.is_empty()),
            info.stack_hash(kind),
            info.lang_text(),
            Some(i64::from(info.tags.0)),
            info.origin_text(),
        );
        let old: Stored = (r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?);
        if new != old {
            write.execute(params![r.get::<_, i64>(0)?, new.0, new.1, new.2, new.3, new.4])?;
            written += 1;
        }
    }
    Ok(written)
}

/// [`refresh_source`] for every playlist (migration 0005).
pub fn refresh_all(conn: &Connection) -> Result<u32> {
    let ids: Vec<SourceId> = conn.prepare("SELECT id FROM source ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    for id in ids {
        n += refresh_source(conn, id)?;
    }
    Ok(n)
}

/// A user correction of one copy (`title_link.action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkAction {
    /// "Not the same title": the copy never joins a stack.
    Split,
    /// "Always play from …": the copy plays first in its stack.
    Pin,
}

impl LinkAction {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkAction::Split => "split",
            LinkAction::Pin => "pin",
        }
    }
}

/// One stored correction, for Settings ▸ Hidden ▸ "Titles you split or combined".
#[derive(Debug, Clone, PartialEq)]
pub struct TitleLink {
    pub item: ItemRef,
    pub action: LinkAction,
    pub at: Timestamp,
    /// The copy's display name, when it still exists.
    pub name: Option<String>,
}

fn vod_kind_str(kind: ItemKind) -> Option<&'static str> {
    match kind {
        ItemKind::Movie => Some("movie"),
        ItemKind::Series => Some("series"),
        _ => None,
    }
}

/// Split `item` off its stack ("Not the same title") or undo it. Write with
/// `DataScope::Overrides`.
pub fn set_split(conn: &Connection, item: &ItemRef, split: bool, now: Timestamp) -> Result<()> {
    set_link(conn, item, LinkAction::Split, split, now)
}

/// Pin `item` ("Always play from …"), clearing the pins of `others` (the other copies of its
/// stack), or unpin it.
pub fn set_pin(conn: &Connection, item: &ItemRef, others: &[ItemRef], pin: bool, now: Timestamp) -> Result<()> {
    if pin {
        for o in others.iter().filter(|o| *o != item) {
            set_link(conn, o, LinkAction::Pin, false, now)?;
        }
    }
    set_link(conn, item, LinkAction::Pin, pin, now)
}

fn set_link(conn: &Connection, item: &ItemRef, action: LinkAction, on: bool, now: Timestamp) -> Result<()> {
    let Some(kind) = vod_kind_str(item.kind) else { return Ok(()) };
    if on {
        conn.execute(
            "INSERT INTO title_link(source_id, kind, item_key, action, at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source_id, kind, item_key, action) DO UPDATE SET at = excluded.at",
            params![item.source, kind, item.key.as_str(), action.as_str(), now],
        )?;
    } else {
        conn.execute(
            "DELETE FROM title_link WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3 AND action = ?4",
            params![item.source, kind, item.key.as_str(), action.as_str()],
        )?;
    }
    Ok(())
}

/// Every correction, newest first, with the copy's display name.
pub fn links(conn: &Connection) -> Result<Vec<TitleLink>> {
    let mut stmt = conn.prepare(
        "SELECT t.source_id, t.kind, t.item_key, t.action, t.at,
                COALESCE((SELECT COALESCE(o.rename, m.clean_name) FROM movie m
                            LEFT JOIN item_override o ON o.source_id = m.source_id AND o.kind = 'movie' AND o.item_key = m.key
                           WHERE t.kind = 'movie' AND m.source_id = t.source_id AND m.key = t.item_key),
                         (SELECT COALESCE(o.rename, s.clean_name) FROM series s
                            LEFT JOIN item_override o ON o.source_id = s.source_id AND o.kind = 'series' AND o.item_key = s.key
                           WHERE t.kind = 'series' AND s.source_id = t.source_id AND s.key = t.item_key))
         FROM title_link t ORDER BY t.at DESC, t.source_id, t.item_key",
    )?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(1)?;
        let action: String = r.get(3)?;
        Ok(TitleLink {
            item: ItemRef { source: r.get(0)?, kind: if kind == "movie" { ItemKind::Movie } else { ItemKind::Series }, key: ItemKey(r.get(2)?) },
            action: if action == "split" { LinkAction::Split } else { LinkAction::Pin },
            at: r.get(4)?,
            name: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Row ids of the copies of `kind` with `action` (the library build).
pub fn linked_ids(conn: &Connection, kind: ItemKind, action: LinkAction) -> Result<HashSet<i64>> {
    let Some(k) = vod_kind_str(kind) else { return Ok(HashSet::new()) };
    let table = k;
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT x.id FROM title_link t JOIN {table} x ON x.source_id = t.source_id AND x.key = t.item_key WHERE t.kind = ?1 AND t.action = ?2"
    ))?;
    let ids = stmt.query_map(params![k, action.as_str()], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}
