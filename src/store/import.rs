//! Catalogue (live/VOD/series) import sink: batched upserts with mark-and-sweep generations and
//! user-data reattach. Owner: WP-02. Driven by `jobs::import_source` (WP-06).
//!
//! Protocol (DESIGN §9.2):
//! ```text
//! let imp = begin_catalogue(conn, source)?;                // generation = source.generation + 1
//! upsert_groups(conn, &imp, &groups)?;                     // small, one tx
//! for batch in channels.chunks(2000) { write_channels(conn, &imp, batch)?; }   // one tx each
//! ... movies / series likewise ...
//! let summary = finish_catalogue(conn, &imp, &completed_kinds, now)?;   // one tx: sweep +
//!                                     // reattach + counts + generation bump + resolve EPG
//! ```
//! Rows are upserted on `(source_id, key)` so unchanged items keep their row id. A kind that did
//! not complete (network error mid-way) is NOT swept, so a failed refresh never empties a list.
//! If the job is cancelled, nothing is swept and `source.generation` is not bumped; rows already
//! upserted simply carry the newer data.
//!
//! Phase 5 (DESIGN §27.4–27.5): M3U series arrive whole through [`write_series_episodes`];
//! [`finish_catalogue_with`] can switch the source's kind and move user data between keys
//! (also across kinds) in its transaction, before reattach.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{
    AccountInfo, CatchupSpec, Fingerprint, GroupId, GroupKey, HttpHeaders, ItemKey, ItemKind, ItemRef, Quality, ReattachOutcome, SeriesId, SourceId,
    SourceKind, StreamLocator, Timestamp, Unsupported, XtreamCredentials, XtreamStreamKind,
};
use crate::sources::identity::xtream_key;
use crate::sources::xtream::detect::{parse_stream_url, short_live_id};
use crate::store::sql::{self, atomic, enum_str, fp_from_json, fp_opt, from_json_opt, kind_from_sql, or_not_found, u32_of};

/// An in-progress catalogue import.
#[derive(Debug, Clone)]
pub struct CatalogueImport {
    pub source: SourceId,
    pub generation: i64,
    pub started_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportGroup {
    pub kind: ItemKind,
    pub key: GroupKey,
    /// Display name (smart-cleaned when enabled).
    pub name: String,
    /// The provider's category name as received (`grp.raw_name`, tag detection; DESIGN §28.2).
    /// `None` for groups made up by the importer ("Uncategorized", unseen keys): stored as
    /// `name`, so a NULL `grp.raw_name` only ever means "imported before P6".
    pub raw_name: Option<String>,
    pub provider_id: Option<String>,
    pub provider_order: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportChannel {
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub name: String,
    pub clean_name: String,
    pub norm_name: String,
    pub number: Option<u32>,
    pub logo: Option<String>,
    pub tvg_id: Option<String>,
    pub tvg_name: Option<String>,
    pub tvg_shift_min: i32,
    pub stream: StreamLocator,
    pub url_key: Option<String>,
    pub headers: Option<HttpHeaders>,
    pub catchup: Option<CatchupSpec>,
    pub radio: bool,
    pub unsupported: Option<Unsupported>,
    pub provider_order: i32,
    pub added_at: Option<Timestamp>,
    /// Group keys, primary first (must have been passed to `upsert_groups`).
    pub groups: Vec<GroupKey>,
    /// The quality word of the provider name (`smart_names`), whether or not cleanup is on.
    pub quality: Option<NameQuality>,
}

/// The quality word of a provider name (`channel.quality`) and the family of duplicates the
/// channel belongs to (`channel.family`).
#[derive(Debug, Clone, PartialEq)]
pub struct NameQuality {
    pub class: Quality,
    /// `normalize_name` of the smart-cleaned name: "BBC One SD" and "UK: BBC One 2160p" share
    /// it (identity's `norm_name` keeps the quality words it doesn't know).
    pub family: String,
}

impl NameQuality {
    pub fn of(cleaned: &crate::sources::smart_names::CleanName) -> Option<Self> {
        cleaned.quality.map(|class| Self { class, family: crate::sources::identity::normalize_name(&cleaned.name) })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportMovie {
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub name: String,
    pub clean_name: String,
    pub norm_name: String,
    pub year: Option<u16>,
    pub poster: Option<String>,
    pub rating: Option<f32>,
    pub stream: StreamLocator,
    pub url_key: Option<String>,
    pub provider_order: i32,
    pub added_at: Option<Timestamp>,
    pub groups: Vec<GroupKey>,
    /// The provider's TMDB id from the list (`tmdb`), when it sends one (DESIGN §29.10).
    pub tmdb: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportSeries {
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub name: String,
    pub clean_name: String,
    pub norm_name: String,
    pub year: Option<u16>,
    pub cover: Option<String>,
    pub backdrop: Option<String>,
    pub rating: Option<f32>,
    pub plot: Option<String>,
    pub genre: Option<String>,
    pub cast: Option<String>,
    pub director: Option<String>,
    pub release_date: Option<String>,
    pub last_modified: Option<Timestamp>,
    pub provider_order: i32,
    pub added_at: Option<Timestamp>,
    pub groups: Vec<GroupKey>,
    /// The provider's TMDB id from the list (`tmdb`), when it sends one (DESIGN §29.10).
    pub tmdb: Option<u32>,
}

/// Lazy movie details (`get_vod_info`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MovieDetails {
    pub plot: Option<String>,
    pub genre: Option<String>,
    pub cast: Option<String>,
    pub director: Option<String>,
    pub duration_s: Option<u32>,
    pub release_date: Option<String>,
    pub tmdb_id: Option<String>,
    pub trailer: Option<String>,
    pub backdrop: Option<String>,
    pub rating: Option<f32>,
    pub poster: Option<String>,
    /// Container extension from `movie_data` (overrides the list's when present).
    pub container_ext: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportSeason {
    pub number: u32,
    pub name: Option<String>,
    pub cover: Option<String>,
    pub air_date: Option<String>,
    pub overview: Option<String>,
    pub episode_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportEpisode {
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    pub plot: Option<String>,
    pub duration_s: Option<u32>,
    pub still: Option<String>,
    pub air_date: Option<String>,
    pub rating: Option<f32>,
    pub stream: StreamLocator,
}

/// Lazy series details (`get_series_info`). Seasons may be empty while episodes exist; the
/// store synthesizes seasons from episodes in that case.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SeriesDetails {
    pub seasons: Vec<ImportSeason>,
    pub episodes: Vec<ImportEpisode>,
    pub plot: Option<String>,
    pub backdrop: Option<String>,
    /// `info.tmdb` / `info.tmdb_id` (DESIGN §29.10).
    pub tmdb_id: Option<u32>,
}

/// A series assembled from M3U episode entries, with every episode the playlist lists
/// (DESIGN §27.5). Written by [`write_series_episodes`]; there are no lazy details.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSeriesEpisodes {
    pub series: ImportSeries,
    pub episodes: Vec<ImportEpisode>,
}

/// User data to re-point from one item to another, possibly of another kind (M3U ⇄ Xtream
/// switches and M3U channels reclassified as movies/episodes; DESIGN §27.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMove {
    pub from: ItemRef,
    pub to: ItemRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KindCounts {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImportSummary {
    pub channels: KindCounts,
    pub movies: KindCounts,
    pub series: KindCounts,
    pub groups: KindCounts,
    pub reattached: u32,
    pub orphaned: u32,
}

/// Rows deleted per transaction by the sweep (§5.3: no transaction larger than one batch).
const SWEEP_CHUNK: i64 = 2_000;

/// Running added/updated counters of one import, kept in `meta` so they survive between the
/// per-batch writer closures (and connections) until `finish_catalogue` reads them.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Counters {
    added: u32,
    updated: u32,
}

fn counter_key(imp: &CatalogueImport, what: &str) -> String {
    format!("import:{}:{}:{what}", imp.source.0, imp.generation)
}

fn bump_counters(conn: &Connection, imp: &CatalogueImport, what: &str, added: u32, updated: u32) -> Result<()> {
    let key = counter_key(imp, what);
    let mut c: Counters =
        from_json_opt(conn.query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| r.get(0)).optional()?).unwrap_or_default();
    c.added += added;
    c.updated += updated;
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key.as_str(), sql::to_json(&c)?],
    )?;
    Ok(())
}

fn take_counters(conn: &Connection, imp: &CatalogueImport, what: &str) -> Result<Counters> {
    let key = counter_key(imp, what);
    let c = from_json_opt(conn.query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| r.get(0)).optional()?).unwrap_or_default();
    conn.execute("DELETE FROM meta WHERE key = ?1", [&key])?;
    Ok(c)
}

/// Catalogue table of a kind.
fn table_of(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Channel => "channel",
        ItemKind::Movie => "movie",
        ItemKind::Series => "series",
        ItemKind::Episode => "episode",
    }
}

/// Start an import: computes the next generation; nothing is written yet.
pub fn begin_catalogue(conn: &Connection, source: SourceId, now: Timestamp) -> Result<CatalogueImport> {
    let generation: i64 = or_not_found(conn.query_row("SELECT generation FROM source WHERE id = ?1", [source], |r| r.get(0)), "source", source)?;
    // counters of an earlier cancelled run
    conn.execute("DELETE FROM meta WHERE key LIKE ?1", [format!("import:{}:%", source.0)])?;
    Ok(CatalogueImport { source, generation: generation + 1, started_at: now })
}

/// `GroupKey → grp.id` of one kind of a source.
fn group_ids(conn: &Connection, imp: &CatalogueImport, kind: ItemKind) -> Result<HashMap<String, GroupId>> {
    // only groups already marked seen by this import: stale keys go through `resolve_groups`'
    // fallback so their `seen_gen` is bumped and the sweep keeps them
    let mut stmt = conn.prepare_cached("SELECT key, id FROM grp WHERE source_id = ?1 AND kind = ?2 AND seen_gen = ?3")?;
    let rows = stmt.query_map(params![imp.source, kind.as_str(), imp.generation], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Upsert one group; returns (id, inserted). A new movie / series group inherits the lock and
/// hidden flags of the source's `channel:X` group of the same name (`carry_group_locks`).
fn upsert_group(conn: &Connection, imp: &CatalogueImport, g: &ImportGroup) -> Result<(GroupId, bool)> {
    let existed: bool = conn
        .prepare_cached("SELECT 1 FROM grp WHERE source_id = ?1 AND kind = ?2 AND key = ?3")?
        .query_row(params![imp.source, g.kind.as_str(), g.key.0], |_| Ok(()))
        .optional()?
        .is_some();
    let id = conn
        .prepare_cached(
            "INSERT INTO grp(source_id, kind, key, name, raw_name, provider_id, provider_order, seen_gen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(source_id, kind, key) DO UPDATE SET name = excluded.name, raw_name = excluded.raw_name,
               provider_id = excluded.provider_id, provider_order = excluded.provider_order, seen_gen = excluded.seen_gen
             RETURNING id",
        )?
        .query_row(
            params![
                imp.source,
                g.kind.as_str(),
                g.key.0,
                g.name,
                g.raw_name.as_deref().unwrap_or(&g.name),
                g.provider_id,
                g.provider_order,
                imp.generation
            ],
            |r| r.get(0),
        )?;
    if !existed {
        carry_group_locks(conn, imp.source, g)?;
    }
    Ok((id, !existed))
}

/// Group ids of an item's group keys (primary first). A key that `upsert_groups` never saw in
/// this import is marked seen if it exists from an earlier one (keeping its name), otherwise
/// created on the fly (named after its key), rather than silently dropping the membership.
fn resolve_groups(
    conn: &Connection,
    imp: &CatalogueImport,
    kind: ItemKind,
    keys: &[GroupKey],
    map: &mut HashMap<String, GroupId>,
) -> Result<Vec<GroupId>> {
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        let id = match map.get(&k.0) {
            Some(id) => *id,
            None => {
                // a group the provider no longer lists: its raw name cannot be learned any more, so
                // a pre-P6 one keeps its display name as raw name (no endless "needs refresh")
                let kept: Option<GroupId> = conn
                    .prepare_cached(
                        "UPDATE grp SET seen_gen = ?4, raw_name = COALESCE(raw_name, name) WHERE source_id = ?1 AND kind = ?2 AND key = ?3 RETURNING id",
                    )?
                    .query_row(params![imp.source, kind.as_str(), k.0, imp.generation], |r| r.get(0))
                    .optional()?;
                let id = match kept {
                    Some(id) => id,
                    None => {
                        let name = sql::group_norm(&k.0);
                        let g = ImportGroup { kind, key: k.clone(), name, raw_name: None, provider_id: None, provider_order: i32::MAX };
                        upsert_group(conn, imp, &g)?.0
                    }
                };
                map.insert(k.0.clone(), id);
                id
            }
        };
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// Upsert all groups of the import (marks `seen_gen`). One transaction.
pub fn upsert_groups(conn: &mut Connection, imp: &CatalogueImport, groups: &[ImportGroup]) -> Result<()> {
    if let Some(g) = groups.iter().find(|g| g.kind == ItemKind::Episode) {
        return Err(Error::Other(format!("group {} has kind episode", g.key.0)));
    }
    let tx = conn.transaction()?;
    let (mut added, mut updated) = (0, 0);
    for g in groups {
        if upsert_group(&tx, imp, g)?.1 {
            added += 1;
        } else {
            updated += 1;
        }
    }
    bump_counters(&tx, imp, "group", added, updated)?;
    tx.commit()?;
    Ok(())
}

/// Does `(source, key)` exist in `table`? (added vs updated accounting)
fn exists(conn: &Connection, table: &str, source: SourceId, key: &ItemKey) -> Result<bool> {
    Ok(conn
        .prepare_cached(&format!("SELECT 1 FROM {table} WHERE source_id = ?1 AND key = ?2"))?
        .query_row(params![source, key.as_str()], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Replace the group memberships of one item. `position` = the item's provider order, so a
/// group's members sort in provider order.
fn write_memberships(conn: &Connection, link: &str, col: &str, id: i64, groups: &[GroupId], position: i32, fresh: bool) -> Result<()> {
    if !fresh {
        conn.prepare_cached(&format!("DELETE FROM {link} WHERE {col} = ?1"))?.execute([id])?;
    }
    let primary = if link == "channel_group" { ", is_primary" } else { "" };
    let primary_val = if link == "channel_group" { ", ?4" } else { "" };
    let mut ins = conn
        .prepare_cached(&format!("INSERT INTO {link}(group_id, {col}, position{primary}) VALUES (?1, ?2, ?3{primary_val}) ON CONFLICT DO NOTHING"))?;
    for (i, g) in groups.iter().enumerate() {
        if primary.is_empty() {
            ins.execute(params![g, id, position])?;
        } else {
            ins.execute(params![g, id, position, i == 0])?;
        }
    }
    Ok(())
}

/// Upsert one batch of channels in ONE transaction (prepared/cached statements), replacing
/// their `channel_group` rows. Returns (added, updated).
pub fn write_channels(conn: &mut Connection, imp: &CatalogueImport, batch: &[ImportChannel]) -> Result<(u32, u32)> {
    let tx = conn.transaction()?;
    let mut groups = group_ids(&tx, imp, ItemKind::Channel)?;
    let (mut added, mut updated) = (0u32, 0u32);
    for ch in batch {
        let fresh = !exists(&tx, "channel", imp.source, &ch.key)?;
        let headers = ch.headers.as_ref().filter(|h| !h.is_empty()).map(sql::to_json).transpose()?;
        let catchup = ch.catchup.as_ref().map(sql::to_json).transpose()?;
        let id: i64 = tx
            .prepare_cached(
                "INSERT INTO channel(source_id, key, provider_id, name, clean_name, norm_name, number, logo, tvg_id, tvg_name,
                                     tvg_shift_min, stream, url_key, headers, catchup, radio, unsupported, provider_order,
                                     added_at, seen_gen, quality, family)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, COALESCE(?19, ?20), ?21, ?22, ?23)
                 ON CONFLICT(source_id, key) DO UPDATE SET
                   provider_id = excluded.provider_id, name = excluded.name, clean_name = excluded.clean_name,
                   norm_name = excluded.norm_name, number = excluded.number, logo = excluded.logo, tvg_id = excluded.tvg_id,
                   tvg_name = excluded.tvg_name, tvg_shift_min = excluded.tvg_shift_min, stream = excluded.stream,
                   url_key = excluded.url_key, headers = excluded.headers, catchup = excluded.catchup, radio = excluded.radio,
                   unsupported = excluded.unsupported, provider_order = excluded.provider_order,
                   added_at = COALESCE(?19, channel.added_at), seen_gen = excluded.seen_gen, quality = excluded.quality,
                   family = excluded.family
                 RETURNING id",
            )?
            .query_row(
                params![
                    imp.source,
                    ch.key.as_str(),
                    ch.provider_id,
                    ch.name,
                    ch.clean_name,
                    ch.norm_name,
                    ch.number,
                    ch.logo,
                    ch.tvg_id,
                    ch.tvg_name,
                    ch.tvg_shift_min,
                    sql::to_json(&ch.stream)?,
                    ch.url_key,
                    headers,
                    catchup,
                    ch.radio,
                    ch.unsupported.as_ref().map(enum_str),
                    ch.provider_order,
                    ch.added_at,
                    imp.started_at,
                    imp.generation,
                    ch.quality.as_ref().map(|q| q.class.code()),
                    ch.quality.as_ref().map(|q| q.family.as_str()),
                ],
                |r| r.get(0),
            )?;
        let gids = resolve_groups(&tx, imp, ItemKind::Channel, &ch.groups, &mut groups)?;
        write_memberships(&tx, "channel_group", "channel_id", id, &gids, ch.provider_order, fresh)?;
        if fresh {
            added += 1;
        } else {
            updated += 1;
        }
    }
    bump_counters(&tx, imp, "channel", added, updated)?;
    tx.commit()?;
    Ok((added, updated))
}

pub fn write_movies(conn: &mut Connection, imp: &CatalogueImport, batch: &[ImportMovie]) -> Result<(u32, u32)> {
    let tx = conn.transaction()?;
    let mut groups = group_ids(&tx, imp, ItemKind::Movie)?;
    let (mut added, mut updated) = (0u32, 0u32);
    for m in batch {
        let fresh = !exists(&tx, "movie", imp.source, &m.key)?;
        let id: i64 = tx
            .prepare_cached(
                "INSERT INTO movie(source_id, key, provider_id, name, clean_name, norm_name, year, poster, rating, stream, url_key,
                                   provider_order, added_at, seen_gen, tmdb_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, COALESCE(?13, ?14), ?15, ?16)
                 ON CONFLICT(source_id, key) DO UPDATE SET
                   match_tags = CASE WHEN excluded.name IS movie.name AND (excluded.year IS NULL OR excluded.year IS movie.year)
                                     THEN movie.match_tags END,
                   tmdb_id = COALESCE(excluded.tmdb_id, movie.tmdb_id),
                   provider_id = excluded.provider_id, name = excluded.name, clean_name = excluded.clean_name,
                   norm_name = excluded.norm_name, year = COALESCE(excluded.year, movie.year),
                   poster = COALESCE(excluded.poster, movie.poster), rating = COALESCE(excluded.rating, movie.rating),
                   stream = excluded.stream, url_key = excluded.url_key, provider_order = excluded.provider_order,
                   added_at = COALESCE(?13, movie.added_at), seen_gen = excluded.seen_gen
                 RETURNING id",
            )?
            .query_row(
                params![
                    imp.source,
                    m.key.as_str(),
                    m.provider_id,
                    m.name,
                    m.clean_name,
                    m.norm_name,
                    m.year,
                    m.poster,
                    m.rating.map(f64::from),
                    sql::to_json(&m.stream)?,
                    m.url_key,
                    m.provider_order,
                    m.added_at,
                    imp.started_at,
                    imp.generation,
                    m.tmdb.map(|t| t.to_string()),
                ],
                |r| r.get(0),
            )?;
        let gids = resolve_groups(&tx, imp, ItemKind::Movie, &m.groups, &mut groups)?;
        write_memberships(&tx, "movie_group", "movie_id", id, &gids, m.provider_order, fresh)?;
        if fresh {
            added += 1;
        } else {
            updated += 1;
        }
    }
    bump_counters(&tx, imp, "movie", added, updated)?;
    tx.commit()?;
    Ok((added, updated))
}

/// Series rows. A changed provider `last_modified` clears `details_at`, so the detail page
/// re-fetches seasons/episodes.
pub fn write_series(conn: &mut Connection, imp: &CatalogueImport, batch: &[ImportSeries]) -> Result<(u32, u32)> {
    let tx = conn.transaction()?;
    let mut groups = group_ids(&tx, imp, ItemKind::Series)?;
    let (mut added, mut updated) = (0u32, 0u32);
    for s in batch {
        if upsert_series(&tx, imp, s, &mut groups)?.1 {
            added += 1;
        } else {
            updated += 1;
        }
    }
    bump_counters(&tx, imp, "series", added, updated)?;
    tx.commit()?;
    Ok((added, updated))
}

/// Upsert one series row and its group memberships; returns (id, inserted).
fn upsert_series(tx: &Connection, imp: &CatalogueImport, s: &ImportSeries, groups: &mut HashMap<String, GroupId>) -> Result<(SeriesId, bool)> {
    let fresh = !exists(tx, "series", imp.source, &s.key)?;
    let id: i64 = tx
        .prepare_cached(
            "INSERT INTO series(source_id, key, provider_id, name, clean_name, norm_name, year, cover, backdrop, rating, plot,
                                    genre, cast_, director, release_date, last_modified, provider_order, added_at, seen_gen, tmdb_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, COALESCE(?18, ?19), ?20, ?21)
                 ON CONFLICT(source_id, key) DO UPDATE SET
                   match_tags = CASE WHEN excluded.name IS series.name AND (excluded.year IS NULL OR excluded.year IS series.year)
                                     THEN series.match_tags END,
                   tmdb_id = COALESCE(excluded.tmdb_id, series.tmdb_id),
                   provider_id = excluded.provider_id, name = excluded.name, clean_name = excluded.clean_name,
                   norm_name = excluded.norm_name, year = COALESCE(excluded.year, series.year),
                   cover = COALESCE(excluded.cover, series.cover), backdrop = COALESCE(excluded.backdrop, series.backdrop),
                   rating = COALESCE(excluded.rating, series.rating), plot = COALESCE(excluded.plot, series.plot),
                   genre = COALESCE(excluded.genre, series.genre), cast_ = COALESCE(excluded.cast_, series.cast_),
                   director = COALESCE(excluded.director, series.director),
                   release_date = COALESCE(excluded.release_date, series.release_date),
                   details_at = CASE WHEN excluded.last_modified IS NOT NULL AND excluded.last_modified IS NOT series.last_modified
                                     THEN NULL ELSE series.details_at END,
                   last_modified = COALESCE(excluded.last_modified, series.last_modified),
                   provider_order = excluded.provider_order, added_at = COALESCE(?18, series.added_at),
                   seen_gen = excluded.seen_gen
                 RETURNING id",
        )?
        .query_row(
            params![
                imp.source,
                s.key.as_str(),
                s.provider_id,
                s.name,
                s.clean_name,
                s.norm_name,
                s.year,
                s.cover,
                s.backdrop,
                s.rating.map(f64::from),
                s.plot,
                s.genre,
                s.cast,
                s.director,
                s.release_date,
                s.last_modified,
                s.provider_order,
                s.added_at,
                imp.started_at,
                imp.generation,
                s.tmdb,
            ],
            |r| r.get(0),
        )?;
    let gids = resolve_groups(tx, imp, ItemKind::Series, &s.groups, groups)?;
    write_memberships(tx, "series_group", "series_id", id, &gids, s.provider_order, fresh)?;
    Ok((SeriesId(id), fresh))
}

/// Upsert M3U series (like [`write_series`]) and replace each one's episodes and seasons
/// (seasons synthesized from the episodes; `details_at` = the import's start, so detail pages
/// never ask the provider). One transaction per batch; returns series (added, updated).
/// DESIGN §27.5.
pub fn write_series_episodes(conn: &mut Connection, imp: &CatalogueImport, batch: &[ImportSeriesEpisodes]) -> Result<(u32, u32)> {
    let tx = conn.transaction()?;
    let mut groups = group_ids(&tx, imp, ItemKind::Series)?;
    let (mut added, mut updated) = (0u32, 0u32);
    for b in batch {
        let (id, fresh) = upsert_series(&tx, imp, &b.series, &mut groups)?;
        replace_episodes(&tx, id, imp.source, &b.episodes, &[])?;
        tx.prepare_cached("UPDATE series SET details_at = ?2 WHERE id = ?1")?.execute(params![id, imp.started_at])?;
        if fresh {
            added += 1;
        } else {
            updated += 1;
        }
    }
    bump_counters(&tx, imp, "series", added, updated)?;
    tx.commit()?;
    Ok((added, updated))
}

/// Apply `moves` inside the caller's transaction: every user-data row of `from` (favourites,
/// overrides, progress, history; reminders and manual EPG mappings only channel → channel)
/// is re-pointed to `to`, across kinds; where `to` already has a row the existing one wins.
/// Moves whose `from` holds no user data are skipped cheaply. Returns the items moved.
/// DESIGN §27.4.
pub fn apply_key_moves(conn: &Connection, moves: &[KeyMove]) -> Result<u32> {
    let mut held: HashMap<SourceId, HashSet<(ItemKind, ItemKey)>> = HashMap::new();
    let mut moved = 0;
    for m in moves.iter().filter(|m| m.from != m.to) {
        let keys = match held.entry(m.from.source) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(user_data_keys(conn, m.from.source)?),
        };
        if !keys.remove(&(m.from.kind, m.from.key.clone())) {
            continue;
        }
        if m.from.kind == m.to.kind {
            crate::store::user::move_user_data(conn, &m.from, &m.to)?;
        } else {
            move_across_kinds(conn, &m.from, &m.to)?;
        }
        moved += 1;
    }
    Ok(moved)
}

/// (kind, key) of every item of `source` holding user data (history included).
fn user_data_keys(conn: &Connection, source: SourceId) -> Result<HashSet<(ItemKind, ItemKey)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT kind, item_key FROM fav_item WHERE source_id = ?1
         UNION SELECT kind, item_key FROM item_override WHERE source_id = ?1
         UNION SELECT kind, item_key FROM watch_progress WHERE source_id = ?1
         UNION SELECT kind, item_key FROM history WHERE source_id = ?1
         UNION SELECT 'channel', channel_key FROM reminder WHERE source_id = ?1
         UNION SELECT 'channel', channel_key FROM channel_epg_map WHERE source_id = ?1 AND method = 'manual'",
    )?;
    let rows = stmt.query_map([source], |r| Ok((kind_from_sql(&r.get::<_, String>(0)?)?, ItemKey(r.get(1)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `move_user_data` to an item of another kind: favourites, overrides, progress and history
/// change kind; reminders, EPG mappings and the failure cache are for channels only and are
/// dropped. Where `to` already has a row, the existing one wins.
fn move_across_kinds(conn: &Connection, from: &ItemRef, to: &ItemRef) -> Result<()> {
    let fp = fp_opt(&crate::store::user::fingerprint_of(conn, to)?);
    let (f, t) = ((from.source, from.kind.as_str(), from.key.as_str()), (to.source, to.kind.as_str(), to.key.as_str()));
    for table in ["fav_item", "item_override", "watch_progress"] {
        conn.execute(
            &format!(
                "UPDATE OR IGNORE {table} SET source_id = ?4, kind = ?5, item_key = ?6, fp = COALESCE(?7, fp), orphaned_since = NULL
                 WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3"
            ),
            params![f.0, f.1, f.2, t.0, t.1, t.2, fp],
        )?;
        conn.execute(&format!("DELETE FROM {table} WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3"), params![f.0, f.1, f.2])?;
    }
    conn.execute(
        "UPDATE history SET source_id = ?4, kind = ?5, item_key = ?6 WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3",
        params![f.0, f.1, f.2, t.0, t.1, t.2],
    )?;
    if from.kind == ItemKind::Channel {
        for (table, key) in [("reminder", "channel_key"), ("channel_epg_map", "channel_key"), ("stream_failure", "item_key")] {
            conn.execute(&format!("DELETE FROM {table} WHERE source_id = ?1 AND {key} = ?2"), params![f.0, f.2])?;
        }
    }
    Ok(())
}

/// M3U → Xtream moves (DESIGN §27.4), from the rows present before an upgrade: every channel,
/// movie or episode holding user data whose stream URL is an Xtream stream URL goes to the
/// Xtream key of that stream (`live` → channel, `movie` → movie, `series` → episode), and so
/// does a channel in the short live form of `login`. This also moves the channel-kind data of
/// legacy movie / episode entries to the right kind.
pub fn m3u_to_xtream_moves(conn: &Connection, source: SourceId, login: &XtreamCredentials) -> Result<Vec<KeyMove>> {
    let mut moves = Vec::new();
    for (kind, key) in user_data_keys(conn, source)? {
        if kind == ItemKind::Series || key.0.starts_with("xt:") {
            continue;
        }
        let stream: Option<String> = conn
            .prepare_cached(&format!("SELECT stream FROM {} WHERE source_id = ?1 AND key = ?2", table_of(kind)))?
            .query_row(params![source, key.as_str()], |r| r.get(0))
            .optional()?;
        let Some(Ok(StreamLocator::Url { url })) = stream.map(|s| serde_json::from_str::<StreamLocator>(&s)) else { continue };
        let (to_kind, id) = match parse_stream_url(&url) {
            Some(s) => match s.kind {
                XtreamStreamKind::Live => (ItemKind::Channel, s.stream_id),
                XtreamStreamKind::Movie => (ItemKind::Movie, s.stream_id),
                XtreamStreamKind::Series => (ItemKind::Episode, s.stream_id),
            },
            None => match short_live_id(&url, login) {
                Some(id) => (ItemKind::Channel, id),
                None => continue,
            },
        };
        let to = ItemRef { source, kind: to_kind, key: xtream_key(to_kind, id) };
        moves.push(KeyMove { from: ItemRef { source, kind, key }, to });
    }
    moves.sort_by(|a, b| a.from.cmp(&b.from));
    Ok(moves)
}

/// Xtream → M3U moves (switching back, DESIGN §27.4): user data on `xt:` keys goes to the M3U
/// row carrying that Xtream id in `provider_id` (the first in provider order).
fn xtream_to_m3u_moves(conn: &Connection, source: SourceId) -> Result<Vec<KeyMove>> {
    let held = user_data_keys(conn, source)?;
    let mut moves = Vec::new();
    for kind in [ItemKind::Channel, ItemKind::Movie, ItemKind::Episode] {
        let base = xtream_key(kind, 0).0;
        let prefix = base.trim_end_matches('0');
        let mut wanted: Vec<&ItemKey> = held.iter().filter(|(k, key)| *k == kind && key.0.starts_with(prefix)).map(|(_, key)| key).collect();
        if wanted.is_empty() {
            continue;
        }
        wanted.sort();
        let order = if kind == ItemKind::Episode { "id" } else { "provider_order, id" };
        let mut stmt = conn.prepare(&format!(
            "SELECT provider_id, key FROM {} WHERE source_id = ?1 AND key LIKE 'm3u:%' AND provider_id IS NOT NULL ORDER BY {order}",
            table_of(kind)
        ))?;
        let mut by_id: HashMap<String, ItemKey> = HashMap::new();
        for row in stmt.query_map([source], |r| Ok((r.get::<_, String>(0)?, ItemKey(r.get(1)?))))? {
            let (id, key) = row?;
            by_id.entry(id).or_insert(key);
        }
        for key in wanted {
            if let Some(to) = by_id.get(&key.0[prefix.len()..]) {
                moves.push(KeyMove { from: ItemRef { source, kind, key: key.clone() }, to: ItemRef { source, kind, key: to.clone() } });
            }
        }
    }
    Ok(moves)
}

/// Legacy M3U channels reclassified as movies / episodes keep their key: their channel-kind
/// user data moves to the new kind (DESIGN §27.4 "kind fix-up").
fn kind_fixup_moves(conn: &Connection, source: SourceId) -> Result<Vec<KeyMove>> {
    let mut moves = Vec::new();
    for (kind, key) in user_data_keys(conn, source)? {
        if kind != ItemKind::Channel || exists(conn, "channel", source, &key)? {
            continue;
        }
        let to_kind = if exists(conn, "movie", source, &key)? {
            ItemKind::Movie
        } else if exists(conn, "episode", source, &key)? {
            ItemKind::Episode
        } else {
            continue;
        };
        moves.push(KeyMove { from: ItemRef { source, kind, key: key.clone() }, to: ItemRef { source, kind: to_kind, key } });
    }
    Ok(moves)
}

/// Copy the lock / hidden flags of the `channel:X` group override to the new `movie:X` /
/// `series:X` group `g`: reclassifying entries never weakens a parental lock (DESIGN §27.4).
/// Done when the group is created, in the same transaction, so an import that never finishes
/// cannot leave it unlocked; a group the user unlocks later is not created again and stays so.
fn carry_group_locks(conn: &Connection, source: SourceId, g: &ImportGroup) -> Result<()> {
    if !matches!(g.kind, ItemKind::Movie | ItemKind::Series) {
        return Ok(());
    }
    let Some(name) = g.key.0.strip_prefix(g.kind.as_str()).and_then(|k| k.strip_prefix(':')) else { return Ok(()) };
    conn.prepare_cached(
        "INSERT INTO group_override(source_id, kind, group_key, hidden, locked)
         SELECT source_id, ?2, ?3, hidden, locked FROM group_override
         WHERE source_id = ?1 AND kind = 'channel' AND group_key = ?4 AND (locked OR hidden)
         ON CONFLICT(source_id, kind, group_key) DO NOTHING",
    )?
    .execute(params![source, g.kind.as_str(), g.key.0, format!("channel:{name}")])?;
    Ok(())
}

/// Delete rows of `kind` not seen by this import, in chunks (one transaction each). Returns
/// rows removed.
fn sweep_kind(conn: &Connection, imp: &CatalogueImport, kind: ItemKind) -> Result<u32> {
    let t = table_of(kind);
    let sql = format!("DELETE FROM {t} WHERE id IN (SELECT id FROM {t} WHERE source_id = ?1 AND seen_gen < ?2 LIMIT {SWEEP_CHUNK})");
    let mut removed = 0usize;
    loop {
        let n = atomic(conn, |c| Ok(c.prepare_cached(&sql)?.execute(params![imp.source, imp.generation])?))?;
        removed += n;
        if n == 0 {
            return Ok(removed as u32);
        }
    }
}

/// Finish (one transaction): for each kind in `completed`, delete rows with `seen_gen < gen`
/// (and groups likewise); recompute group `item_count`; run [`reattach_user_data`]; update
/// source counts and `generation = gen`; call `store::epg::resolve_mappings` for the source.
///
/// The sweep itself runs first in chunks of 2 000 rows (one transaction each, DESIGN §5.3) so
/// that a provider dropping 50k items cannot stall user writes; everything after it is one
/// transaction. If the process dies in between, the next import simply sweeps again.
pub fn finish_catalogue(conn: &mut Connection, imp: &CatalogueImport, completed: &[ItemKind], now: Timestamp) -> Result<ImportSummary> {
    finish_catalogue_with(conn, imp, completed, now, &FinishOptions::default())
}

/// Extra work of one [`finish_catalogue_with`] (DESIGN §27.3–27.5), all of it inside the
/// finish transaction, before reattach.
#[derive(Debug, Clone, Default)]
pub struct FinishOptions {
    /// Switch the source to this kind (and login): the commit point of an Xtream upgrade or a
    /// switch back to the playlist (`store::sources::set_source_kind`).
    pub switch_kind: Option<(SourceKind, Option<XtreamCredentials>)>,
    /// Account info stored with the switch (`store::sources::set_account`).
    pub account: Option<AccountInfo>,
    /// User data to move first ([`apply_key_moves`]).
    pub moves: Vec<KeyMove>,
    /// The rows come from an M3U playlist: user data on `xt:` keys moves to the M3U rows
    /// carrying those Xtream ids, channel-kind data of entries reclassified as movies /
    /// episodes follows them, and episode user data is reattached over the whole source (M3U
    /// episodes are complete at import; Xtream episodes load lazily and are never reattached
    /// here).
    pub m3u: bool,
}

/// [`finish_catalogue`] with the [`FinishOptions`].
pub fn finish_catalogue_with(
    conn: &mut Connection,
    imp: &CatalogueImport,
    completed: &[ItemKind],
    now: Timestamp,
    opts: &FinishOptions,
) -> Result<ImportSummary> {
    let kinds = [ItemKind::Channel, ItemKind::Movie, ItemKind::Series];
    let mut removed: HashMap<ItemKind, u32> = HashMap::new();
    let mut groups_removed = 0u32;
    for kind in kinds.into_iter().filter(|k| completed.contains(k)) {
        removed.insert(kind, sweep_kind(conn, imp, kind)?);
        groups_removed += atomic(conn, |c| {
            Ok(c.execute("DELETE FROM grp WHERE source_id = ?1 AND kind = ?2 AND seen_gen < ?3", params![imp.source, kind.as_str(), imp.generation])?
                as u32)
        })?;
    }

    let tx = conn.transaction()?;
    if let Some((kind, login)) = &opts.switch_kind {
        crate::store::sources::set_source_kind(&tx, imp.source, *kind, login.as_ref())?;
        if *kind == SourceKind::Xtream {
            // said by the window that sees the refresh end, else by the next one to start
            crate::store::sources::note_unseen_upgrade(&tx, imp.source)?;
        }
    }
    if let Some(a) = &opts.account {
        crate::store::sources::set_account(&tx, imp.source, a)?;
    }
    apply_key_moves(&tx, &opts.moves)?;
    if opts.m3u {
        apply_key_moves(&tx, &xtream_to_m3u_moves(&tx, imp.source)?)?;
        apply_key_moves(&tx, &kind_fixup_moves(&tx, imp.source)?)?;
    }
    let mut summary = ImportSummary::default();
    for (kind, link) in [(ItemKind::Channel, "channel_group"), (ItemKind::Movie, "movie_group"), (ItemKind::Series, "series_group")] {
        tx.execute(
            &format!(
                "UPDATE grp SET item_count = (SELECT count(*) FROM {link} l WHERE l.group_id = grp.id)
                 WHERE source_id = ?1 AND kind = ?2"
            ),
            params![imp.source, kind.as_str()],
        )?;
        let c = take_counters(&tx, imp, kind.as_str())?;
        let total: i64 = tx.query_row(&format!("SELECT count(*) FROM {} WHERE source_id = ?1", table_of(kind)), [imp.source], |r| r.get(0))?;
        let counts = KindCounts { added: c.added, updated: c.updated, removed: removed.get(&kind).copied().unwrap_or(0), total: u32_of(total) };
        match kind {
            ItemKind::Channel => summary.channels = counts,
            ItemKind::Movie => summary.movies = counts,
            _ => summary.series = counts,
        }
    }
    distinguish_group_names(&tx, imp.source, Some(imp.generation))?;
    if completed.iter().any(|k| matches!(k, ItemKind::Movie | ItemKind::Series)) {
        // match keys of new / renamed titles (DESIGN §29.10)
        crate::store::titles::refresh_source(&tx, imp.source)?;
    }
    let gc = take_counters(&tx, imp, "group")?;
    let n_groups: i64 = tx.query_row("SELECT count(*) FROM grp WHERE source_id = ?1", [imp.source], |r| r.get(0))?;
    summary.groups = KindCounts { added: gc.added, updated: gc.updated, removed: groups_removed, total: u32_of(n_groups) };

    if completed.contains(&ItemKind::Channel) {
        // per-stream state of channels that are gone for good (user data is reattached below)
        tx.execute(
            "DELETE FROM stream_failure WHERE source_id = ?1
               AND NOT EXISTS (SELECT 1 FROM channel c WHERE c.source_id = ?1 AND c.key = stream_failure.item_key)",
            [imp.source],
        )?;
        tx.execute(
            "DELETE FROM channel_epg_map WHERE source_id = ?1 AND method != 'manual'
               AND NOT EXISTS (SELECT 1 FROM channel c WHERE c.source_id = ?1 AND c.key = channel_epg_map.channel_key)",
            [imp.source],
        )?;
    }

    let reattach_kinds: &[ItemKind] = if opts.m3u { &[ItemKind::Channel, ItemKind::Movie, ItemKind::Series, ItemKind::Episode] } else { &kinds };
    let (outcomes, newly_orphaned) = reattach(&tx, imp.source, reattach_kinds, now)?;
    if completed.contains(&ItemKind::Channel) {
        carry_video_sizes(&tx, imp.source)?;
        // trust evidence (DESIGN §13.0.1) of channel names the provider no longer lists
        tx.execute("DELETE FROM quality_check WHERE source_id = ?1 AND name NOT IN (SELECT name FROM channel WHERE source_id = ?1)", [imp.source])?;
    }
    summary.reattached = outcomes.iter().filter(|(_, o)| matches!(o, ReattachOutcome::Reattached { .. })).count() as u32;
    summary.orphaned = newly_orphaned;

    tx.execute(
        "UPDATE source SET generation = ?2, n_channels = ?3, n_movies = ?4, n_series = ?5, n_groups = ?6 WHERE id = ?1",
        params![imp.source, imp.generation, summary.channels.total, summary.movies.total, summary.series.total, summary.groups.total],
    )?;
    crate::store::epg::resolve_mappings(&tx)?;
    tx.commit()?;
    Ok(summary)
}

/// Re-attach favourites / overrides / progress / reminders / manual EPG mappings of `source`
/// (every table with `fp` + `orphaned_since`) whose item key no longer exists (DESIGN §8.3).
/// Candidate lookups use indexes (`channel_provider`, `channel_tvg`, `channel_norm`,
/// `channel_urlkey`, movie equivalents) or HashMaps built once per call. Steps, first unique
/// hit wins:
/// 1. same `provider_id` (Xtream renumbering keeps ids rarely, but cheap to check)
/// 2. same `tvg_id` AND same `norm_name`
/// 3. same `norm_name` AND same `norm_group`
/// 4. same `url_key`
/// 5. same `norm_name` alone, if unique in the source
///
/// Ambiguous or no hit → `orphaned_since = now` (kept, listed in Settings ▸ Orphans).
/// A previously orphaned row that matches again is re-attached and un-orphaned.
///
/// Covers channels, movies and series. Episodes are lazily loaded per series, so their user
/// data is re-attached by [`store_series_details`] when that series' episodes are replaced.
/// Rows whose key still exists get their stored fingerprint refreshed (and are reported as
/// `Unchanged` only when they were orphaned before).
pub fn reattach_user_data(conn: &Connection, source: SourceId, now: Timestamp) -> Result<Vec<(ItemKey, ReattachOutcome)>> {
    atomic(conn, |c| Ok(reattach(c, source, &[ItemKind::Channel, ItemKind::Movie, ItemKind::Series], now)?.0))
}

/// [`reattach_user_data`] for `kinds`; also returns how many items became orphans now.
fn reattach(conn: &Connection, source: SourceId, kinds: &[ItemKind], now: Timestamp) -> Result<(Vec<(ItemKey, ReattachOutcome)>, u32)> {
    let mut out = Vec::new();
    let mut newly = 0;
    for &kind in kinds {
        newly += reattach_kind(conn, source, kind, None, now, &mut out)?;
    }
    Ok((out, newly))
}

/// Distinct user-data keys of one kind in a source with their best stored fingerprint.
fn user_keys(conn: &Connection, source: SourceId, kind: ItemKind) -> Result<Vec<(ItemKey, Fingerprint)>> {
    let sql = crate::store::user::holders_for(kind)
        .map(|(table, key, extra)| format!("SELECT {key}, fp FROM {table} WHERE source_id = ?1{extra}"))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([source], |r| Ok((ItemKey(r.get::<_, String>(0)?), fp_from_json(&r.get::<_, String>(1)?))))?;
    let mut out: Vec<(ItemKey, Fingerprint)> = Vec::new();
    let mut index: HashMap<ItemKey, usize> = HashMap::new();
    for row in rows {
        let (key, fp) = row?;
        match index.get(&key) {
            Some(&i) => {
                if out[i].1 == Fingerprint::default() {
                    out[i].1 = fp;
                }
            }
            None => {
                index.insert(key.clone(), out.len());
                out.push((key, fp));
            }
        }
    }
    Ok(out)
}

/// Lookup tables over the current items of one kind (built once per reattach run).
#[derive(Default)]
struct Candidates {
    keys: Vec<ItemKey>,
    by_provider: HashMap<String, Vec<usize>>,
    by_tvg_name: HashMap<(String, String), Vec<usize>>,
    by_name_group: HashMap<(String, String), Vec<usize>>,
    by_url: HashMap<String, Vec<usize>>,
    by_name: HashMap<String, Vec<usize>>,
}

impl Candidates {
    fn load(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Self> {
        let mut c = Candidates::default();
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(args, crate::store::user::facts_from_row)?;
        for row in rows {
            let (key, fp) = row?;
            let i = c.keys.len();
            c.keys.push(key);
            if let Some(p) = fp.provider_id {
                c.by_provider.entry(p).or_default().push(i);
            }
            if fp.norm_name.is_empty() {
                continue;
            }
            if let Some(t) = fp.tvg_id {
                c.by_tvg_name.entry((t, fp.norm_name.clone())).or_default().push(i);
            }
            if let Some(g) = fp.norm_group {
                c.by_name_group.entry((fp.norm_name.clone(), g)).or_default().push(i);
            }
            if let Some(u) = fp.url_key {
                c.by_url.entry(u).or_default().push(i);
            }
            c.by_name.entry(fp.norm_name).or_default().push(i);
        }
        Ok(c)
    }

    /// The first step (§8.3 order) with exactly one candidate.
    fn find(&self, fp: &Fingerprint) -> Option<(&ItemKey, &'static str)> {
        let name = (!fp.norm_name.is_empty()).then_some(fp.norm_name.as_str());
        let steps: [(Option<&Vec<usize>>, &'static str); 5] = [
            (fp.provider_id.as_ref().and_then(|p| self.by_provider.get(p)), "provider-id"),
            (fp.tvg_id.as_ref().zip(name).and_then(|(t, n)| self.by_tvg_name.get(&(t.clone(), n.to_owned()))), "tvg-id+name"),
            (fp.norm_group.as_ref().zip(name).and_then(|(g, n)| self.by_name_group.get(&(n.to_owned(), g.clone()))), "name+group"),
            (fp.url_key.as_ref().and_then(|u| self.by_url.get(u)), "url"),
            (name.and_then(|n| self.by_name.get(n)), "name"),
        ];
        steps.into_iter().find_map(|(hits, via)| match hits {
            Some(v) if v.len() == 1 => Some((&self.keys[v[0]], via)),
            _ => None,
        })
    }
}

/// Reattach one kind. `episodes_of` = (series id, series norm name): only episode user data
/// whose fingerprint names that series is considered, against that series' episodes.
fn reattach_kind(
    conn: &Connection,
    source: SourceId,
    kind: ItemKind,
    episodes_of: Option<(SeriesId, &str)>,
    now: Timestamp,
    out: &mut Vec<(ItemKey, ReattachOutcome)>,
) -> Result<u32> {
    let mut keys = user_keys(conn, source, kind)?;
    if let Some((_, norm)) = episodes_of {
        let prefix = format!("{norm}:");
        keys.retain(|(_, fp)| fp.norm_name.starts_with(&prefix));
    }
    if keys.is_empty() {
        return Ok(0);
    }
    let table = table_of(kind);
    let mut missing = Vec::new();
    for (key, stored) in keys {
        let item = ItemRef { source, kind, key };
        if exists(conn, table, source, &item.key)? {
            let fp = crate::store::user::fingerprint_of(conn, &item)?;
            if crate::store::user::mark_attached(conn, &item, &fp)? {
                out.push((item.key, ReattachOutcome::Unchanged));
            }
        } else {
            missing.push((item, stored));
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    let candidates = match episodes_of {
        Some((series, _)) => {
            let sql = format!("{} AND x.series_id = ?2", crate::store::user::facts_sql(ItemKind::Episode, false));
            Candidates::load(conn, &sql, params![source, series])?
        }
        None => Candidates::load(conn, &crate::store::user::facts_sql(kind, false), [source])?,
    };
    let mut newly = 0;
    for (item, fp) in missing {
        match candidates.find(&fp) {
            Some((new_key, via)) => {
                let to = ItemRef { source, kind, key: new_key.clone() };
                crate::store::user::move_user_data(conn, &item, &to)?;
                tracing::debug!(kind = kind.as_str(), via, "re-attached user data to a new item key");
                out.push((item.key, ReattachOutcome::Reattached { new_key: new_key.clone(), via }));
            }
            None => {
                if crate::store::user::mark_orphaned(conn, &item, now)? {
                    newly += 1;
                }
                out.push((item.key, ReattachOutcome::Orphaned));
            }
        }
    }
    Ok(newly)
}

/// Category display names of a source that smart cleanup made identical although the provider
/// names differ get what it dropped (`smart_names::distinguish_groups`: `General · HEVC`), per
/// kind. With smart names off a display name is the trimmed raw one, so nothing collides.
/// `generation`: only the groups an import just wrote (fresh names), else every group with a
/// raw name (migration 0004).
pub(crate) fn distinguish_group_names(conn: &Connection, source: SourceId, generation: Option<i64>) -> Result<()> {
    let mut rows: Vec<(i64, String, String, String)> = conn
        .prepare_cached(
            "SELECT id, kind, name, raw_name FROM grp WHERE source_id = ?1 AND raw_name IS NOT NULL AND (?2 IS NULL OR seen_gen = ?2)
             ORDER BY kind, id",
        )?
        .query_map(params![source, generation], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let before: Vec<String> = rows.iter().map(|r| r.2.clone()).collect();
    for kind in rows.chunk_by_mut(|a, b| a.1 == b.1) {
        let mut pairs: Vec<(&mut String, &str)> = kind.iter_mut().map(|(_, _, name, raw)| (name, raw.as_str())).collect();
        crate::sources::smart_names::distinguish_groups(&mut pairs);
    }
    let mut write = conn.prepare_cached("UPDATE grp SET name = ?2 WHERE id = ?1")?;
    for (row, old) in rows.iter().zip(before) {
        if row.2 != old {
            write.execute(params![row.0, row.2])?;
        }
    }
    Ok(())
}

/// Measured picture sizes (`channel_video`) whose key no longer holds the channel they were
/// measured on — the key is gone, or now has another provider name (the quality duplicates of
/// an M3U channel share a base key and are numbered by playlist position, so a reordered or
/// grown playlist moves keys between them) or another stream: moved to the one channel that
/// has their name (and, among same-named channels, their `url_key`), else dropped. A
/// measurement is not user data: never listed as an orphan, never moved onto a channel that
/// has its own.
fn carry_video_sizes(conn: &Connection, source: SourceId) -> Result<()> {
    type Stale = (String, String, Option<String>, i64, i64, i64);
    let stale: Vec<Stale> = conn
        .prepare_cached(
            "SELECT v.item_key, v.name, v.url_key, v.width, v.height, v.at FROM channel_video v
             WHERE v.source_id = ?1 AND NOT EXISTS (SELECT 1 FROM channel c WHERE c.source_id = ?1 AND c.key = v.item_key
                   AND c.name = v.name AND (v.url_key IS NULL OR c.url_key IS NULL OR c.url_key = v.url_key))",
        )?
        .query_map([source], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if stale.is_empty() {
        return Ok(());
    }
    // all out first, then back in: two duplicates that swapped keys both find their channel
    let mut out = conn.prepare_cached("DELETE FROM channel_video WHERE source_id = ?1 AND item_key = ?2")?;
    for (key, ..) in &stale {
        out.execute(params![source, key])?;
    }
    // the source's channels by provider name (one pass; only the names looked for are kept)
    let wanted: HashSet<&str> = stale.iter().map(|s| s.1.as_str()).collect();
    let mut named: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
    let mut rows = conn.prepare_cached("SELECT name, key, url_key FROM channel WHERE source_id = ?1")?;
    let mut rows = rows.query([source])?;
    while let Some(r) = rows.next()? {
        if let rusqlite::types::ValueRef::Text(n) = r.get_ref(0)?
            && let Ok(n) = std::str::from_utf8(n)
            && wanted.contains(n)
        {
            named.entry(n.to_owned()).or_default().push((r.get(1)?, r.get(2)?));
        }
    }
    let mut put = conn.prepare_cached(
        "INSERT OR IGNORE INTO channel_video(source_id, item_key, name, url_key, width, height, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for (_, name, url_key, w, h, at) in &stale {
        let to = match named.get(name).map(Vec::as_slice).unwrap_or_default() {
            [one] => Some(one),
            many => {
                let mut by_url = many.iter().filter(|(_, u)| url_key.is_some() && u == url_key);
                by_url.next().filter(|_| by_url.next().is_none())
            }
        };
        if let Some((key, url)) = to {
            put.execute(params![source, key, name, url, w, h, at])?;
        }
    }
    Ok(())
}

/// Store lazily fetched movie details.
pub fn store_movie_details(conn: &Connection, movie: crate::model::MovieId, d: &MovieDetails, now: Timestamp) -> Result<()> {
    atomic(conn, |c| {
        let stream: String = or_not_found(c.query_row("SELECT stream FROM movie WHERE id = ?1", [movie], |r| r.get(0)), "movie", movie)?;
        let mut stream: StreamLocator = serde_json::from_str(&stream).map_err(Error::other)?;
        if let (StreamLocator::Xtream { ext, .. }, Some(new)) = (&mut stream, d.container_ext.as_ref().filter(|e| !e.is_empty())) {
            *ext = Some(new.clone());
        }
        c.execute(
            "UPDATE movie SET plot = COALESCE(?2, plot), genre = COALESCE(?3, genre), cast_ = COALESCE(?4, cast_),
                    director = COALESCE(?5, director), duration_s = COALESCE(?6, duration_s),
                    release_date = COALESCE(?7, release_date), tmdb_id = COALESCE(?8, tmdb_id), trailer = COALESCE(?9, trailer),
                    backdrop = COALESCE(?10, backdrop), rating = COALESCE(?11, rating), poster = COALESCE(?12, poster),
                    stream = ?13, details_at = ?14
             WHERE id = ?1",
            params![
                movie,
                d.plot,
                d.genre,
                d.cast,
                d.director,
                d.duration_s,
                d.release_date,
                d.tmdb_id,
                d.trailer,
                d.backdrop,
                d.rating.map(f64::from),
                d.poster,
                sql::to_json(&stream)?,
                now,
            ],
        )?;
        Ok(())
    })
}

/// Replace seasons + episodes of a series (upsert episodes by key, delete missing).
///
/// Episode user data (progress, favourites…) of this series whose key vanished is re-attached
/// here (same steps as [`reattach_user_data`], against this series' new episodes).
pub fn store_series_details(conn: &mut Connection, series: crate::model::SeriesId, d: &SeriesDetails, now: Timestamp) -> Result<()> {
    let tx = conn.transaction()?;
    let (source, norm): (SourceId, String) = or_not_found(
        tx.query_row("SELECT source_id, norm_name FROM series WHERE id = ?1", [series], |r| Ok((r.get(0)?, r.get(1)?))),
        "series",
        series,
    )?;
    replace_episodes(&tx, series, source, &d.episodes, &d.seasons)?;
    tx.execute(
        "UPDATE series SET plot = COALESCE(?2, plot), backdrop = COALESCE(?3, backdrop), details_at = ?4, tmdb_id = COALESCE(?5, tmdb_id)
         WHERE id = ?1",
        params![series, d.plot, d.backdrop, now, d.tmdb_id],
    )?;
    let mut outcomes = Vec::new();
    reattach_kind(&tx, source, ItemKind::Episode, Some((series, &norm)), now, &mut outcomes)?;
    tx.commit()?;
    Ok(())
}

/// Upsert the episodes of one series (by key), delete its other episodes and rebuild its
/// seasons: as sent, or synthesized from the episodes; counts fall back to the episodes.
fn replace_episodes(tx: &Connection, series: SeriesId, source: SourceId, episodes: &[ImportEpisode], seasons: &[ImportSeason]) -> Result<()> {
    {
        let mut up = tx.prepare_cached(
            "INSERT INTO episode(series_id, source_id, key, provider_id, season, episode, title, plot, duration_s, still, air_date,
                                 rating, stream)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(source_id, key) DO UPDATE SET
               series_id = excluded.series_id, provider_id = excluded.provider_id, season = excluded.season,
               episode = excluded.episode, title = excluded.title, plot = excluded.plot, duration_s = excluded.duration_s,
               still = excluded.still, air_date = excluded.air_date, rating = excluded.rating, stream = excluded.stream",
        )?;
        for e in episodes {
            up.execute(params![
                series,
                source,
                e.key.as_str(),
                e.provider_id,
                e.season,
                e.episode,
                e.title,
                e.plot,
                e.duration_s,
                e.still,
                e.air_date,
                e.rating.map(f64::from),
                sql::to_json(&e.stream)?,
            ])?;
        }
    }
    let keys: Vec<&str> = episodes.iter().map(|e| e.key.as_str()).collect();
    tx.execute("DELETE FROM episode WHERE series_id = ?1 AND key NOT IN (SELECT value FROM json_each(?2))", params![series, sql::to_json(&keys)?])?;

    let mut per_season: BTreeMap<u32, u32> = BTreeMap::new();
    for e in episodes {
        *per_season.entry(e.season).or_default() += 1;
    }
    tx.execute("DELETE FROM season WHERE series_id = ?1", [series])?;
    let mut ins = tx.prepare_cached(
        "INSERT INTO season(series_id, number, name, cover, air_date, overview, episode_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(series_id, number) DO NOTHING",
    )?;
    for s in seasons {
        let count = if s.episode_count > 0 { s.episode_count } else { per_season.get(&s.number).copied().unwrap_or(0) };
        ins.execute(params![series, s.number, s.name, s.cover, s.air_date, s.overview, count])?;
    }
    // seasons the provider did not list (or no season list at all) still get a row
    let none = Option::<String>::None;
    for (n, count) in per_season.iter().filter(|(n, _)| !seasons.iter().any(|s| s.number == **n)) {
        ins.execute(params![series, n, none, none, none, none, count])?;
    }
    Ok(())
}
