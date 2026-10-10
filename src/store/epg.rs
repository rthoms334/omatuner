//! EPG storage: generation-based bulk import, grid/now-next queries, FTS search, mappings.
//! Owner: WP-02.
//!
//! Import protocol (DESIGN §9.3):
//! ```text
//! let imp = begin_epg_import(conn, id)?;            // generation = active_gen + 1, offset captured
//! write_epg_channels(conn, &imp, &batch)?;          // upsert (source, xmltv_id) ON CONFLICT DO
//!                                                    //   UPDATE (row ids stable), display names
//! write_programmes(conn, &imp, &batch)?;            // ~5 000 rows / tx: programmes + their FTS
//!                                                    //   rows in the SAME tx; unknown channel
//!                                                    //   ids auto-created
//! let s = finish_epg_import(conn, &imp, now)?;      // SMALL tx: flip active_gen, counts,
//!                                                    //   max_stop, resolve mappings
//! while sweep_old_generations(conn, id, 20_000)? > 0 {}   // FTS rows by rowid, then programmes,
//!                                                    //   then epg channels unseen in active gen
//! while merge_fts_step(conn)? {}                     // if anything was swept: compact the FTS
//!                                                    //   index (~20 ms per call, one tx each)
//! ```
//! Readers always filter `programme.gen = epg_source.active_gen`, so the flip is atomic for
//! the UI and an aborted import ([`abort_epg_import`]) leaves the old guide intact.
//!
//! FTS contract (DESIGN §6.3): `programme_fts` is contentless with `contentless_delete=1`,
//! keyed by `programme.id` (AUTOINCREMENT, never reused). Every programme delete — sweep, prune,
//! abort, EPG-source/source removal — deletes the FTS rows by rowid FIRST, in the same
//! transaction, in chunks. `programme.epg_channel_id` has no ON DELETE CASCADE, so forgetting
//! this fails loudly. No transaction may exceed one batch (~20 ms) so user writes stay snappy.
//!
//! Times: every read returns EPG time (source offset applied, channel tvg-shift NOT applied);
//! callers apply `ProgrammeSlim::shifted` once. Exception: [`now_next`] takes the shifts.

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{
    ChannelId, EpgChannel, EpgChannelId, EpgMapping, EpgMatchMethod, EpgSourceId, EpisodeNum, ItemKey, ItemKind, ItemRef, NowNext, Programme,
    ProgrammeFlags, ProgrammeHit, ProgrammeId, ProgrammeSlim, SourceId, Timestamp,
};
use crate::store::sql::{self, atomic, enum_parse, enum_str, from_json_opt, or_not_found, u32_of};

#[derive(Debug, Clone)]
pub struct EpgImport {
    pub epg_source: EpgSourceId,
    pub generation: i64,
    /// Source offset in seconds, added to every programme time at write.
    pub offset_s: i64,
    /// xmltv_id → epg_channel.id, filled as channels are written.
    pub channel_ids: HashMap<String, EpgChannelId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportEpgChannel {
    pub xmltv_id: String,
    /// (display name, `identity::normalize_name(display name)`) — the norm is computed by the
    /// importer (WP-06), so the store never calls into `sources::`.
    pub display_names: Vec<(String, String)>,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportProgramme {
    pub xmltv_id: String,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub description: Option<String>,
    pub categories: Vec<String>,
    pub icon: Option<String>,
    pub episode: Option<EpisodeNum>,
    pub rating: Option<String>,
    pub star_rating: Option<String>,
    pub catchup_id: Option<String>,
    pub is_new: bool,
    pub is_live: bool,
    pub previously_shown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EpgImportSummary {
    pub channels: u32,
    pub programmes: u32,
    pub max_stop: Option<Timestamp>,
    pub mapped_channels: u32,
}

/// Matching input row for one playlist channel (see `sources::epg_match`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMatchRow {
    pub channel: ItemRef,
    pub tvg_id: Option<String>,
    pub name: String,
    pub norm_name: String,
}

// ------------------------------------------------------------------ import

/// `meta` key holding the running counters of an in-progress import (programmes inserted,
/// latest stop), so `finish_epg_import` stays a few UPDATEs instead of scanning 400k rows.
fn import_meta_key(id: EpgSourceId, generation: i64) -> String { format!("epg-import:{}:{generation}", id.0) }

#[derive(Debug, Default, Serialize, Deserialize)]
struct ImportCounters {
    n: u64,
    max_stop: Option<Timestamp>,
}

const PROGRAMME_SLIM_COLS: &str = "p.id, p.start, p.stop, p.title, p.sub_title, p.flags, p.catchup_id IS NOT NULL";

const FLAG_NEW: i64 = 1;
const FLAG_LIVE: i64 = 2;
const FLAG_PREVIOUSLY_SHOWN: i64 = 4;

fn slim_from_row(r: &Row<'_>) -> rusqlite::Result<ProgrammeSlim> {
    let flags: i64 = r.get(5)?;
    Ok(ProgrammeSlim {
        id: r.get(0)?,
        start: r.get(1)?,
        stop: r.get(2)?,
        title: Arc::from(r.get::<_, String>(3)?),
        sub_title: r.get::<_, Option<String>>(4)?.map(Arc::from),
        flags: ProgrammeFlags { is_new: flags & FLAG_NEW != 0, is_live: flags & FLAG_LIVE != 0, has_catchup_id: r.get(6)? },
    })
}

/// Active generation of the EPG source owning `ch`, as a scalar subquery on `?1` (keeps the
/// `programme_grid` index usable: `epg_channel_id = ? AND gen = <const> AND start …`).
const ACTIVE_GEN_OF_CH: &str =
    "(SELECT es.active_gen FROM epg_channel ec JOIN epg_source es ON es.id = ec.epg_source_id WHERE ec.id = ?1)";

pub fn begin_epg_import(conn: &Connection, id: EpgSourceId) -> Result<EpgImport> {
    let (active, offset_min): (i64, i64) = or_not_found(
        conn.query_row("SELECT active_gen, offset_min FROM epg_source WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))),
        "EPG source",
        id,
    )?;
    // Skip over generations left behind by a crashed import: their rows are older than the
    // next flip and get swept with the other inactive generations.
    let highest: Option<i64> = conn.query_row("SELECT max(gen) FROM programme WHERE epg_source_id = ?1", [id], |r| r.get(0))?;
    let generation = active.max(highest.unwrap_or(0)) + 1;
    conn.execute("DELETE FROM meta WHERE key LIKE ?1", [format!("epg-import:{}:%", id.0)])?;
    Ok(EpgImport { epg_source: id, generation, offset_s: offset_min * 60, channel_ids: HashMap::new() })
}

/// Upsert channels (`ON CONFLICT(epg_source_id, xmltv_id) DO UPDATE` — never REPLACE: that
/// would cascade the live guide away) + replace their display names; sets `seen_gen`.
pub fn write_epg_channels(conn: &mut Connection, imp: &mut EpgImport, batch: &[ImportEpgChannel]) -> Result<()> {
    let tx = conn.transaction()?;
    // ids join `imp.channel_ids` only once committed: a rolled-back batch must not leave ids
    // that a later batch would reference
    let mut ids = Vec::with_capacity(batch.len());
    {
        let mut up = tx.prepare_cached(
            "INSERT INTO epg_channel(epg_source_id, xmltv_id, icon, seen_gen) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(epg_source_id, xmltv_id) DO UPDATE SET icon = excluded.icon, seen_gen = excluded.seen_gen
             RETURNING id",
        )?;
        let mut del = tx.prepare_cached("DELETE FROM epg_display_name WHERE epg_channel_id = ?1")?;
        let mut ins = tx.prepare_cached(
            "INSERT INTO epg_display_name(epg_channel_id, name, norm) VALUES (?1, ?2, ?3)
             ON CONFLICT(epg_channel_id, name) DO NOTHING",
        )?;
        for ch in batch {
            let id: EpgChannelId = up.query_row(params![imp.epg_source, ch.xmltv_id, ch.icon, imp.generation], |r| r.get(0))?;
            del.execute([id])?;
            for (name, norm) in &ch.display_names {
                ins.execute(params![id, name, norm])?;
            }
            ids.push((ch.xmltv_id.clone(), id));
        }
    }
    tx.commit()?;
    imp.channel_ids.extend(ids);
    Ok(())
}

/// Insert one batch of programmes for `imp.generation` in one transaction, together with their
/// `programme_fts` rows. `INSERT … ON CONFLICT(epg_channel_id, gen, start) DO NOTHING`:
/// duplicate (channel, start) inside a generation → FIRST wins (only inserted rows get FTS
/// rows). Returns rows inserted.
pub fn write_programmes(conn: &mut Connection, imp: &mut EpgImport, batch: &[ImportProgramme]) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut inserted = 0usize;
    let mut max_stop: Option<Timestamp> = None;
    // channels created by this batch; merged into `imp.channel_ids` after the commit
    let mut new_ids: HashMap<String, EpgChannelId> = HashMap::new();
    {
        // programmes of channels the file never declared: create the channel (keeps any icon)
        let mut auto = tx.prepare_cached(
            "INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (?1, ?2, ?3)
             ON CONFLICT(epg_source_id, xmltv_id) DO UPDATE SET seen_gen = excluded.seen_gen
             RETURNING id",
        )?;
        let mut ins = tx.prepare_cached(
            "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title, descr, categories, icon,
                                   season, episode, rating, star_rating, catchup_id, flags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(epg_channel_id, gen, start) DO NOTHING",
        )?;
        let mut fts = tx.prepare_cached("INSERT INTO programme_fts(rowid, title, sub_title) VALUES (?1, ?2, ?3)")?;
        for p in batch {
            let ch = match imp.channel_ids.get(&p.xmltv_id).or_else(|| new_ids.get(&p.xmltv_id)) {
                Some(id) => *id,
                None => {
                    let id: EpgChannelId = auto.query_row(params![imp.epg_source, p.xmltv_id, imp.generation], |r| r.get(0))?;
                    new_ids.insert(p.xmltv_id.clone(), id);
                    id
                }
            };
            let flags = (if p.is_new { FLAG_NEW } else { 0 })
                | (if p.is_live { FLAG_LIVE } else { 0 })
                | (if p.previously_shown { FLAG_PREVIOUSLY_SHOWN } else { 0 });
            let categories = (!p.categories.is_empty()).then(|| p.categories.join("\x1f"));
            let (start, stop) = (p.start + imp.offset_s, p.stop + imp.offset_s);
            let changed = ins.execute(params![
                imp.epg_source,
                ch,
                imp.generation,
                start,
                stop,
                p.title,
                p.sub_title,
                p.description,
                categories,
                p.icon,
                p.episode.and_then(|e| e.season),
                p.episode.and_then(|e| e.episode),
                p.rating,
                p.star_rating,
                p.catchup_id,
                flags,
            ])?;
            // 0 rows: a duplicate (channel, start), first one wins. Not `RETURNING id`: SQLite runs
            // that as a trigger per row, which made this insert ~6× slower (benches/import.rs).
            if changed == 0 {
                continue;
            }
            let id = tx.last_insert_rowid();
            fts.execute(params![id, p.title, p.sub_title])?;
            inserted += 1;
            max_stop = Some(max_stop.map_or(stop, |m| m.max(stop)));
        }
    }
    if inserted > 0 {
        let key = import_meta_key(imp.epg_source, imp.generation);
        let mut c: ImportCounters =
            from_json_opt(tx.query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| r.get(0)).optional()?).unwrap_or_default();
        c.n += inserted as u64;
        c.max_stop = c.max_stop.max(max_stop);
        tx.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key.as_str(), sql::to_json(&c)?],
        )?;
    }
    tx.commit()?;
    imp.channel_ids.extend(new_ids);
    Ok(inserted)
}

/// One SHORT transaction: set `active_gen`, counts, `max_stop`, `last_refresh`, then
/// [`resolve_mappings`] (which only considers epg channels with `seen_gen = active_gen`).
/// FTS rows already exist (written per batch); unseen channels are removed by
/// [`sweep_old_generations`] once their programmes are gone.
pub fn finish_epg_import(conn: &mut Connection, imp: &EpgImport, now: Timestamp) -> Result<EpgImportSummary> {
    let tx = conn.transaction()?;
    let key = import_meta_key(imp.epg_source, imp.generation);
    let counters: ImportCounters =
        from_json_opt(tx.query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| r.get(0)).optional()?).unwrap_or_default();
    let channels: i64 = tx.query_row(
        "SELECT count(*) FROM epg_channel WHERE epg_source_id = ?1 AND seen_gen = ?2",
        params![imp.epg_source, imp.generation],
        |r| r.get(0),
    )?;
    let n = tx.execute(
        "UPDATE epg_source SET active_gen = ?2, n_channels = ?3, n_programmes = ?4, max_stop = ?5, last_refresh = ?6,
                last_attempt = ?6, last_error = NULL
         WHERE id = ?1",
        params![imp.epg_source, imp.generation, channels, counters.n as i64, counters.max_stop, now],
    )?;
    if n == 0 {
        return Err(sql::not_found("EPG source", imp.epg_source));
    }
    tx.execute("DELETE FROM meta WHERE key = ?1", [&key])?;
    resolve_mappings(&tx)?;
    let mapped: i64 = tx.query_row(
        "SELECT count(*) FROM channel_epg ce JOIN epg_channel ec ON ec.id = ce.epg_channel_id WHERE ec.epg_source_id = ?1",
        [imp.epg_source],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(EpgImportSummary {
        channels: u32_of(channels),
        programmes: u32_of(counters.n as i64),
        max_stop: counters.max_stop,
        mapped_channels: u32_of(mapped),
    })
}

/// Delete everything written for `imp.generation` (cancel/failure): FTS rows by rowid, then
/// programmes, chunked (one tx per chunk); then the guide channels only this run introduced,
/// and re-resolve mappings.
pub fn abort_epg_import(conn: &mut Connection, imp: &EpgImport) -> Result<()> {
    let sel = "SELECT id FROM programme WHERE epg_source_id = ?1 AND gen = ?2 LIMIT ?3";
    while delete_selected(conn, sel, params![imp.epg_source, imp.generation, ABORT_CHUNK])? > 0 {}
    atomic(conn, |c| {
        // channels only the aborted run introduced (no programmes left at all) go away
        c.execute(
            "DELETE FROM epg_channel WHERE epg_source_id = ?1 AND seen_gen = ?2
               AND NOT EXISTS (SELECT 1 FROM programme p WHERE p.epg_channel_id = epg_channel.id)",
            params![imp.epg_source, imp.generation],
        )?;
        // the rest belong to the active guide again if it has programmes for them; otherwise
        // they were already on their way out (older generations only) and stay sweepable
        c.execute(
            "UPDATE epg_channel SET seen_gen = CASE
                 WHEN EXISTS (SELECT 1 FROM programme p WHERE p.epg_channel_id = epg_channel.id AND p.gen = es.active_gen)
                 THEN es.active_gen ELSE es.active_gen - 1 END
             FROM (SELECT active_gen FROM epg_source WHERE id = ?1) AS es
             WHERE epg_source_id = ?1 AND seen_gen = ?2",
            params![imp.epg_source, imp.generation],
        )?;
        c.execute("DELETE FROM meta WHERE key = ?1", [import_meta_key(imp.epg_source, imp.generation)])?;
        // mappings that resolved to a removed channel fall back to the next guide
        resolve_mappings(c)
    })
}

const ABORT_CHUNK: i64 = 20_000;

/// Delete up to `chunk` programme rows of inactive generations of `id` (FTS rows by rowid
/// first, same tx). When none remain, deletes epg channels of `id` not seen in the active
/// generation (their `channel_epg` rows cascade). Returns rows deleted; call until 0.
///
/// "Inactive" means older than the active generation: a newer generation is an import in
/// progress (or the leftovers of a crashed one, swept after the next flip).
///
/// Turns FTS5's 'deletemerge' off first (persistent; written only while it is still on): by
/// default FTS5 rewrites a segment whenever a commit leaves more than 10 % of it deleted, so
/// sweeping a 415k-programme generation in chunks rewrote the big segments over and over (≈ 8 s
/// and writer stalls of up to 2 s per chunk). The deleted entries are dropped afterwards by
/// [`merge_fts_step`] (import job and `Maintenance`).
pub fn sweep_old_generations(conn: &mut Connection, id: EpgSourceId, chunk: usize) -> Result<usize> {
    let active: i64 = or_not_found(conn.query_row("SELECT active_gen FROM epg_source WHERE id = ?1", [id], |r| r.get(0)), "EPG source", id)?;
    let deletemerge: Option<i64> =
        conn.query_row("SELECT v FROM programme_fts_config WHERE k = 'deletemerge'", [], |r| r.get(0)).optional()?;
    if deletemerge != Some(0) {
        conn.execute("INSERT INTO programme_fts(programme_fts, rank) VALUES('deletemerge', 0)", [])?;
    }
    let sel = "SELECT id FROM programme WHERE epg_source_id = ?1 AND gen < ?2 LIMIT ?3";
    let n = delete_selected(conn, sel, params![id, active, chunk as i64])?;
    if n > 0 {
        return Ok(n);
    }
    Ok(conn.execute(
        "DELETE FROM epg_channel WHERE epg_source_id = ?1 AND seen_gen < ?2
           AND NOT EXISTS (SELECT 1 FROM programme p WHERE p.epg_channel_id = epg_channel.id)",
        params![id, active],
    )?)
}

/// One step of merging the programme search index down to a single segment, dropping deleted
/// entries (FTS5 'merge' with a negative page budget: all segments, even a lone one holding
/// deletions). Writes ≈ 400 pages (~20 ms) in its own transaction; returns false once there was
/// nothing left to merge — call until false after deleting programmes (≈ 15 steps, 0.3 s in
/// total, for 415k programmes; the same work as one 'optimize' without the long transaction).
pub fn merge_fts_step(conn: &mut Connection) -> Result<bool> {
    let before = conn.total_changes();
    conn.execute("INSERT INTO programme_fts(programme_fts, rank) VALUES('merge', -400)", [])?;
    // SQLite docs: a change count below 2 means the merge was a no-op
    Ok(conn.total_changes() - before >= 2)
}

/// Delete programmes with `stop < before` (keeps catch-up windows: caller passes
/// now − max(keep_past_days, max catch-up days)). FTS rows first. At most `chunk` rows per call
/// (one tx); returns rows deleted — call until 0.
pub fn prune(conn: &mut Connection, before: Timestamp, chunk: usize) -> Result<usize> {
    delete_selected(conn, "SELECT id FROM programme WHERE stop < ?1 LIMIT ?2", params![before, chunk as i64])
}

/// Which programmes [`delete_programmes_of`] removes.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgrammeScope {
    /// Before deleting an EPG source.
    EpgSource(EpgSourceId),
    /// Before deleting individual epg channels.
    EpgChannels(Vec<EpgChannelId>),
    /// Before deleting a playlist source (its provider EPG sources).
    PlaylistSource(SourceId),
}

/// Delete up to `chunk` programmes (FTS rows first) in `scope`. Call until 0, then delete the
/// parent rows. Required before any delete of `epg_channel`, `epg_source` or `source`.
pub fn delete_programmes_of(conn: &mut Connection, scope: &ProgrammeScope, chunk: usize) -> Result<usize> {
    delete_programmes_in(conn, scope, chunk)
}

/// [`delete_programmes_of`] on a shared connection (also usable inside an open transaction).
pub(crate) fn delete_programmes_in(conn: &Connection, scope: &ProgrammeScope, chunk: usize) -> Result<usize> {
    let chunk = chunk as i64;
    match scope {
        ProgrammeScope::EpgSource(id) => {
            delete_selected(conn, "SELECT id FROM programme WHERE epg_source_id = ?1 LIMIT ?2", params![id, chunk])
        }
        ProgrammeScope::EpgChannels(ids) => delete_selected(
            conn,
            "SELECT id FROM programme WHERE epg_channel_id IN (SELECT value FROM json_each(?1)) LIMIT ?2",
            params![sql::json_ids(ids.iter().map(|i| i.0)), chunk],
        ),
        ProgrammeScope::PlaylistSource(id) => delete_selected(
            conn,
            "SELECT id FROM programme WHERE epg_source_id IN (SELECT id FROM epg_source WHERE playlist_source_id = ?1) LIMIT ?2",
            params![id, chunk],
        ),
    }
}

/// One chunk: select programme ids with `select`, delete their FTS rows by rowid, then the
/// programmes — in one savepoint (a transaction when none is open).
fn delete_selected(conn: &Connection, select: &str, args: impl rusqlite::Params) -> Result<usize> {
    atomic(conn, |c| {
        let ids: Vec<i64> = {
            let mut stmt = c.prepare_cached(select)?;
            let rows = stmt.query_map(args, |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if ids.is_empty() {
            return Ok(0);
        }
        let list = sql::json_ids(ids.iter().copied());
        c.prepare_cached("DELETE FROM programme_fts WHERE rowid IN (SELECT value FROM json_each(?1))")?.execute([&list])?;
        Ok(c.prepare_cached("DELETE FROM programme WHERE id IN (SELECT value FROM json_each(?1))")?.execute([&list])?)
    })
}

/// Maintenance safety net: `DELETE FROM programme_fts WHERE rowid NOT IN (SELECT id FROM
/// programme)`. Returns rows removed (should be 0).
pub fn fts_cleanup(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM programme_fts WHERE rowid NOT IN (SELECT id FROM programme)", [])?)
}

// ------------------------------------------------------------------ queries (UI thread safe)

/// `?1` = epg channel, `?2` = from, `?3` = to (see [`window`]).
fn window_sql() -> String {
    format!(
        "SELECT {PROGRAMME_SLIM_COLS} FROM programme p
         WHERE p.epg_channel_id = ?1 AND p.gen = {ACTIVE_GEN_OF_CH}
           AND p.start >= ?2 - 86400 AND p.start < ?3 AND p.stop > ?2
         ORDER BY p.start"
    )
}

/// `?1` = epg channel, `?2` = EPG time: the first two programmes still running or upcoming.
fn now_next_sql() -> String {
    format!(
        "SELECT {PROGRAMME_SLIM_COLS} FROM programme p
         WHERE p.epg_channel_id = ?1 AND p.gen = {ACTIVE_GEN_OF_CH} AND p.start >= ?2 - 86400 AND p.stop > ?2
         ORDER BY p.start LIMIT 2"
    )
}

/// Programmes of `ch` (active generation) overlapping `[from, to)` (EPG time), ordered by
/// start. Uses `programme_grid`: `start >= from - 86400 AND start < to AND stop > from`.
/// Must stay < 1 ms for a 6 h window.
pub fn window(conn: &Connection, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> Result<Vec<ProgrammeSlim>> {
    let mut stmt = conn.prepare_cached(&window_sql())?;
    let rows = stmt.query_map(params![ch, from, to], slim_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Now/next for each (epg channel, tvg-shift minutes) at `now`: queries at `now − shift·60`
/// and returns programmes ALREADY SHIFTED (two "+1" channels sharing a guide channel get
/// different answers). One prepared statement executed per key.
pub fn now_next(conn: &Connection, chans: &[(EpgChannelId, i32)], now: Timestamp) -> Result<HashMap<(EpgChannelId, i32), NowNext>> {
    // the first two programmes still running or upcoming at t: [now, next] or [next, …]
    let mut stmt = conn.prepare_cached(&now_next_sql())?;
    let mut out = HashMap::with_capacity(chans.len());
    for &(ch, shift) in chans {
        if out.contains_key(&(ch, shift)) {
            continue;
        }
        let t = now - i64::from(shift) * 60;
        let rows: Vec<ProgrammeSlim> = stmt.query_map(params![ch, t], slim_from_row)?.collect::<rusqlite::Result<_>>()?;
        let mut it = rows.into_iter();
        let nn = match it.next() {
            Some(first) if first.start <= t => NowNext { now: Some(first.shifted(shift)), next: it.next().map(|p| p.shifted(shift)) },
            Some(first) => NowNext { now: None, next: Some(first.shifted(shift)) },
            None => NowNext::default(),
        };
        out.insert((ch, shift), nn);
    }
    Ok(out)
}

/// Next `limit` programmes starting at or after `from` (EPG time; Live preview "Upcoming").
pub fn upcoming(conn: &Connection, ch: EpgChannelId, from: Timestamp, limit: usize) -> Result<Vec<ProgrammeSlim>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PROGRAMME_SLIM_COLS} FROM programme p
         WHERE p.epg_channel_id = ?1 AND p.gen = {ACTIVE_GEN_OF_CH} AND p.start >= ?2
         ORDER BY p.start LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![ch, from, limit as i64], slim_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Full programme (details popover).
pub fn programme(conn: &Connection, id: ProgrammeId) -> Result<Programme> {
    let r = conn.query_row(
        "SELECT id, epg_channel_id, start, stop, title, sub_title, descr, categories, icon, season, episode, rating,
                star_rating, catchup_id, flags
         FROM programme WHERE id = ?1",
        [id],
        |r| {
            let season: Option<u32> = r.get(9)?;
            let episode: Option<u32> = r.get(10)?;
            let flags: i64 = r.get(14)?;
            Ok(Programme {
                id: r.get(0)?,
                epg_channel_id: r.get(1)?,
                start: r.get(2)?,
                stop: r.get(3)?,
                title: r.get(4)?,
                sub_title: r.get(5)?,
                description: r.get(6)?,
                categories: r
                    .get::<_, Option<String>>(7)?
                    .map(|c| c.split('\x1f').filter(|s| !s.is_empty()).map(str::to_owned).collect())
                    .unwrap_or_default(),
                icon: r.get(8)?,
                episode: (season.is_some() || episode.is_some()).then_some(EpisodeNum { season, episode }),
                rating: r.get(11)?,
                star_rating: r.get(12)?,
                catchup_id: r.get(13)?,
                is_new: flags & FLAG_NEW != 0,
                is_live: flags & FLAG_LIVE != 0,
                previously_shown: flags & FLAG_PREVIOUSLY_SHOWN != 0,
            })
        },
    );
    or_not_found(r, "programme", id)
}

/// Safe FTS5 expression for user text: alphanumeric tokens, each quoted, the last one a
/// prefix (`tom & jer` → `"tom" "jer"*`). `None` when the text has no searchable token.
pub(crate) fn fts_expression(query: &str) -> Option<String> {
    let tokens: Vec<&str> = query.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
    let (last, rest) = tokens.split_last()?;
    let mut out: Vec<String> = rest.iter().map(|t| format!("\"{t}\"")).collect();
    out.push(format!("\"{last}\"*"));
    Some(out.join(" "))
}

/// FTS search over title/sub_title of active generations (`JOIN programme p ON p.id = f.rowid
/// … AND p.gen = es.active_gen`). `query` is user text; the function
/// builds a safe FTS5 expression (each token quoted, last token prefix `*`). Results: airing now
/// first, then upcoming by start, then past (catch-up) by recency; each joined to the playlist
/// channels mapped to its epg channel via `channel_epg`.
///
/// Only programmes of enabled guides that are mapped to at least one playlist channel are
/// returned (anything else could not be played or reminded).
pub fn search(conn: &Connection, query: &str, now: Timestamp, limit: usize) -> Result<Vec<ProgrammeHit>> {
    let Some(expr) = fts_expression(query) else { return Ok(Vec::new()) };
    let hits: Vec<(ProgrammeSlim, EpgChannelId)> = {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {PROGRAMME_SLIM_COLS}, p.epg_channel_id
             FROM programme_fts f
             JOIN programme p ON p.id = f.rowid
             JOIN epg_channel ec ON ec.id = p.epg_channel_id
             JOIN epg_source es ON es.id = ec.epg_source_id
             WHERE programme_fts MATCH ?1 AND p.gen = es.active_gen AND es.enabled = 1
               AND EXISTS (SELECT 1 FROM channel_epg ce WHERE ce.epg_channel_id = p.epg_channel_id)
             ORDER BY CASE WHEN p.start <= ?2 AND p.stop > ?2 THEN 0 WHEN p.start > ?2 THEN 1 ELSE 2 END,
                      CASE WHEN p.start > ?2 THEN p.start ELSE -p.start END,
                      p.id
             LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![expr, now, limit as i64], |r| Ok((slim_from_row(r)?, r.get(7)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut chans = conn.prepare_cached("SELECT channel_id FROM channel_epg WHERE epg_channel_id = ?1 ORDER BY channel_id")?;
    let mut cache: HashMap<EpgChannelId, Vec<ChannelId>> = HashMap::new();
    let mut out = Vec::with_capacity(hits.len());
    for (programme, epg_channel_id) in hits {
        let channel_ids = match cache.get(&epg_channel_id) {
            Some(v) => v.clone(),
            None => {
                let v: Vec<ChannelId> = chans.query_map([epg_channel_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                cache.insert(epg_channel_id, v.clone());
                v
            }
        };
        out.push(ProgrammeHit { programme, epg_channel_id, channel_ids });
    }
    Ok(out)
}

/// The epg channel resolved for a playlist channel (from `channel_epg`).
pub fn epg_channel_for(conn: &Connection, ch: ChannelId) -> Result<Option<EpgChannelId>> {
    Ok(conn.prepare_cached("SELECT epg_channel_id FROM channel_epg WHERE channel_id = ?1")?.query_row([ch], |r| r.get(0)).optional()?)
}

pub fn epg_channel(conn: &Connection, id: EpgChannelId) -> Result<EpgChannel> {
    let r = conn.query_row("SELECT id, epg_source_id, xmltv_id, icon FROM epg_channel WHERE id = ?1", [id], |r| {
        Ok(EpgChannel { id: r.get(0)?, epg_source_id: r.get(1)?, xmltv_id: r.get(2)?, display_names: Vec::new(), icon: r.get(3)? })
    });
    let mut ch = or_not_found(r, "EPG channel", id)?;
    let mut stmt = conn.prepare_cached("SELECT name FROM epg_display_name WHERE epg_channel_id = ?1")?;
    ch.display_names = stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ch)
}

// ------------------------------------------------------------------ mappings

/// All epg channels of a source with display names (input to the matcher).
///
/// Channels not seen by the active (or an in-progress) generation are left out: they are
/// about to be swept and mapping them would only produce dead `channel_epg` candidates.
pub fn epg_channels(conn: &Connection, id: EpgSourceId) -> Result<Vec<EpgChannel>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ec.id, ec.epg_source_id, ec.xmltv_id, ec.icon FROM epg_channel ec JOIN epg_source es ON es.id = ec.epg_source_id
         WHERE ec.epg_source_id = ?1 AND ec.seen_gen >= es.active_gen ORDER BY ec.id",
    )?;
    let mut out: Vec<EpgChannel> = stmt
        .query_map([id], |r| {
            Ok(EpgChannel { id: r.get(0)?, epg_source_id: r.get(1)?, xmltv_id: r.get(2)?, display_names: Vec::new(), icon: r.get(3)? })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let index: HashMap<EpgChannelId, usize> = out.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
    let mut names = conn.prepare_cached(
        "SELECT dn.epg_channel_id, dn.name FROM epg_display_name dn JOIN epg_channel ec ON ec.id = dn.epg_channel_id
         WHERE ec.epg_source_id = ?1",
    )?;
    let rows = names.query_map([id], |r| Ok((r.get::<_, EpgChannelId>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (ch, name) = row?;
        if let Some(i) = index.get(&ch) {
            out[*i].display_names.push(name);
        }
    }
    Ok(out)
}

/// Matching input for every channel of a playlist source.
///
/// `norm_name` follows DESIGN §9.4 step 4: `normalize_name(tvg-name or name)`.
pub fn channels_for_matching(conn: &Connection, source: SourceId) -> Result<Vec<ChannelMatchRow>> {
    let mut stmt =
        conn.prepare_cached("SELECT key, tvg_id, name, tvg_name, norm_name FROM channel WHERE source_id = ?1 ORDER BY provider_order, id")?;
    let rows = stmt.query_map([source], |r| {
        let tvg_name: Option<String> = r.get(3)?;
        let stored: String = r.get(4)?;
        let norm_name = match tvg_name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(t) => crate::sources::identity::normalize_name(t),
            None => stored,
        };
        Ok(ChannelMatchRow {
            channel: ItemRef { source, kind: ItemKind::Channel, key: ItemKey(r.get(0)?) },
            tvg_id: r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
            name: r.get(2)?,
            norm_name,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Replace automatic mappings for (source, epg_source); manual rows are preserved.
pub fn replace_auto_mappings(
    conn: &mut Connection,
    source: SourceId,
    epg_source: EpgSourceId,
    maps: &[EpgMapping],
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM channel_epg_map WHERE source_id = ?1 AND epg_source_id = ?2 AND method != 'manual'",
        params![source, epg_source],
    )?;
    {
        // a manual row for the same (channel, epg source) wins: DO NOTHING
        let mut ins = tx.prepare_cached(
            "INSERT INTO channel_epg_map(source_id, channel_key, epg_source_id, xmltv_id, method, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(source_id, channel_key, epg_source_id) DO NOTHING",
        )?;
        for m in maps.iter().filter(|m| m.method != EpgMatchMethod::Manual && m.channel.source == source && m.epg_source_id == epg_source) {
            ins.execute(params![source, m.channel.key.as_str(), epg_source, m.xmltv_id, enum_str(&m.method), f64::from(m.confidence)])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Manual assignment from the EPG mapping screen (method = Manual; wins over everything). The
/// channel's fingerprint is computed inside (`user::fingerprint_of`) so manual mappings reattach
/// like other user data.
///
/// A channel has at most one manual mapping; `channel_epg` is updated for that channel
/// immediately (no full [`resolve_mappings`] run).
pub fn set_manual_mapping(conn: &Connection, channel: &ItemRef, epg_source: EpgSourceId, xmltv_id: &str) -> Result<()> {
    atomic(conn, |c| {
        let fp = crate::store::user::fingerprint_of(c, channel)?;
        c.execute(
            "DELETE FROM channel_epg_map WHERE source_id = ?1 AND channel_key = ?2 AND method = 'manual' AND epg_source_id != ?3",
            params![channel.source, channel.key.as_str(), epg_source],
        )?;
        c.execute(
            "INSERT INTO channel_epg_map(source_id, channel_key, epg_source_id, xmltv_id, method, confidence, fp, orphaned_since)
             VALUES (?1, ?2, ?3, ?4, 'manual', 1.0, COALESCE(?5, '{}'), NULL)
             ON CONFLICT(source_id, channel_key, epg_source_id) DO UPDATE SET
               xmltv_id = excluded.xmltv_id, method = 'manual', confidence = 1.0,
               fp = COALESCE(?5, channel_epg_map.fp), orphaned_since = NULL",
            params![channel.source, channel.key.as_str(), epg_source, xmltv_id, sql::fp_opt(&fp)],
        )?;
        resolve_channel(c, channel)
    })
}

/// Remove the manual mapping for a channel (automatic ones re-apply on next resolve).
pub fn clear_manual_mapping(conn: &Connection, channel: &ItemRef) -> Result<()> {
    atomic(conn, |c| {
        c.execute(
            "DELETE FROM channel_epg_map WHERE source_id = ?1 AND channel_key = ?2 AND method = 'manual'",
            params![channel.source, channel.key.as_str()],
        )?;
        resolve_channel(c, channel)
    })
}

/// Every mapping row of a channel, best first (manual, then guide priority, then method).
pub fn mappings_for_channel(conn: &Connection, channel: &ItemRef) -> Result<Vec<EpgMapping>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT m.epg_source_id, m.xmltv_id, m.method, m.confidence FROM channel_epg_map m
         JOIN epg_source es ON es.id = m.epg_source_id
         WHERE m.source_id = ?1 AND m.channel_key = ?2
         ORDER BY m.method = 'manual' DESC, es.priority, es.id, {METHOD_RANK}"
    ))?;
    let rows = stmt.query_map(params![channel.source, channel.key.as_str()], |r| {
        let method: String = r.get(2)?;
        Ok(EpgMapping {
            channel: channel.clone(),
            epg_source_id: r.get(0)?,
            xmltv_id: r.get(1)?,
            method: enum_parse(&method).unwrap_or(EpgMatchMethod::Fuzzy),
            confidence: r.get::<_, f64>(3)? as f32,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `EpgMatchMethod` priority (enum order) as SQL.
const METHOD_RANK: &str = "CASE m.method WHEN 'manual' THEN 0 WHEN 'tvg-id-exact' THEN 1 WHEN 'tvg-id-caseless' THEN 2 \
     WHEN 'xtream-epg-id' THEN 3 WHEN 'feed-stripped' THEN 4 WHEN 'normalized-name' THEN 5 ELSE 6 END";

/// The winning mapping per channel. `{filter}` narrows the channel set (`AND c.…`).
fn resolve_select(filter: &str) -> String {
    format!(
        "SELECT channel_id, epg_channel_id FROM (
           SELECT c.id AS channel_id, ec.id AS epg_channel_id,
                  row_number() OVER (PARTITION BY c.id
                                     ORDER BY m.method = 'manual' DESC, es.priority, es.id, {METHOD_RANK}, m.confidence DESC)
                    AS rn
           FROM channel_epg_map m
           JOIN channel c ON c.source_id = m.source_id AND c.key = m.channel_key
           JOIN epg_source es ON es.id = m.epg_source_id AND es.enabled = 1
           JOIN epg_channel ec ON ec.epg_source_id = m.epg_source_id AND ec.xmltv_id = m.xmltv_id
                               AND ec.seen_gen >= es.active_gen
           WHERE m.orphaned_since IS NULL {filter}
         ) WHERE rn = 1"
    )
}

/// Re-resolve one channel (manual mapping edits).
pub(crate) fn resolve_channel(conn: &Connection, channel: &ItemRef) -> Result<()> {
    conn.execute(
        "DELETE FROM channel_epg WHERE channel_id IN (SELECT id FROM channel WHERE source_id = ?1 AND key = ?2)",
        params![channel.source, channel.key.as_str()],
    )?;
    conn.execute(
        &format!("INSERT INTO channel_epg(channel_id, epg_channel_id) {}", resolve_select("AND c.source_id = ?1 AND c.key = ?2")),
        params![channel.source, channel.key.as_str()],
    )?;
    Ok(())
}

/// Rebuild `channel_epg`: for each channel, the mapping with (Manual first, then lowest
/// epg_source.priority, then best method) whose epg channel exists in an enabled source with
/// `epg_channel.seen_gen = epg_source.active_gen`.
/// Pure SQL; < 200 ms for 100k channels.
///
/// `seen_gen >= active_gen` in practice: channels refreshed by an import that is still running
/// (or was aborted) carry a newer `seen_gen` and must keep their guide meanwhile.
pub fn resolve_mappings(conn: &Connection) -> Result<()> {
    atomic(conn, |c| {
        c.execute("DELETE FROM channel_epg", [])?;
        c.execute(&format!("INSERT INTO channel_epg(channel_id, epg_channel_id) {}", resolve_select("")), [])?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        crate::store::migrations::migrate(&mut c).unwrap();
        c.execute_batch(
            "INSERT INTO epg_source(id,name,kind,active_gen) VALUES (1,'g','url',1);
             INSERT INTO epg_channel(id,epg_source_id,xmltv_id,seen_gen) VALUES (1,1,'a',1),(2,1,'b',1);",
        )
        .unwrap();
        for ch in 1..=2 {
            for i in 0..200 {
                c.execute(
                    "INSERT INTO programme(epg_source_id,epg_channel_id,gen,start,stop,title) VALUES (1,?1,1,?2,?3,'x')",
                    params![ch, i * 1800, i * 1800 + 1800],
                )
                .unwrap();
            }
        }
        c
    }

    fn plan(c: &Connection, sql: &str, args: impl rusqlite::Params) -> String {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt.query_map(args, |r| r.get::<_, String>(3)).unwrap();
        rows.map(Result::unwrap).collect::<Vec<_>>().join("\n")
    }

    /// DESIGN §6.2/§24.2: grid and now/next reads are range scans of `programme_grid`
    /// (channel, gen, start) — never a scan of the programme table — with and without stats.
    #[test]
    fn window_and_now_next_use_programme_grid() {
        let c = db();
        for analyzed in [false, true] {
            if analyzed {
                c.execute_batch("ANALYZE").unwrap();
            }
            let p = plan(&c, &window_sql(), params![1, 3600, 7200]);
            assert!(p.contains("SEARCH p USING INDEX programme_grid (epg_channel_id=? AND gen=? AND start>? AND start<?)"), "{p}");
            assert!(!p.contains("SCAN p"), "{p}");
            let p = plan(&c, &now_next_sql(), params![1, 3600]);
            assert!(p.contains("SEARCH p USING INDEX programme_grid (epg_channel_id=? AND gen=? AND start>?)"), "{p}");
            assert!(!p.contains("SCAN p"), "{p}");
        }
        let w = window(&c, EpgChannelId(1), 3600, 7200).unwrap();
        assert_eq!(w.iter().map(|p| p.start).collect::<Vec<_>>(), vec![3600, 5400]);
    }

    #[test]
    fn fts_expression_quotes_tokens_and_prefixes_the_last() {
        assert_eq!(fts_expression("Tom & Jer").as_deref(), Some("\"Tom\" \"Jer\"*"));
        assert_eq!(fts_expression("  \"news\" OR x* ").as_deref(), Some("\"news\" \"OR\" \"x\"*"));
        assert_eq!(fts_expression("Café-Noir").as_deref(), Some("\"Café\" \"Noir\"*"));
        assert_eq!(fts_expression(" -- * "), None);
        assert_eq!(fts_expression(""), None);
    }
}
