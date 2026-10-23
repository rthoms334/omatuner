//! New episodes' store (DESIGN §44 §3–§4). OWNER: N-core.
//!
//! * **Tracked titles** ([`tracked`]): a title (a stack of series copies in the library, or one
//!   series) is tracked when a `series` entry of any watchlist holds one of its copies (its
//!   season range is the coverage), or — with "Also series I've binged" — when more than
//!   `binge_pct` of a season ≥ 1 is finished across its copies with at least [`BINGE_MIN`]
//!   finished episodes in it (Q6). Arrivals still unwatched (and, for a tracked title, episodes
//!   its ledger doesn't know yet) don't count as known, so a new episode never lowers the ratio.
//!   A locked, concealed or explicit copy excludes the whole title (failing closed); hidden
//!   copies aren't in the library.
//! * **The ledger** (`newep_copy`, `newep_known`): per tracked copy, every (season, episode) it
//!   ever had. A title that becomes tracked is baselined silently; a copy found later alerts
//!   only beyond the title's frontier (the newest coordinate of its other copies), unless its
//!   playlist's first import brought it. A copy no longer tracked loses its rows, so a later
//!   add baselines again.
//! * **The diff** ([`check`], one write transaction): per title, the coordinates of its loaded
//!   copies minus the ledger, once per title whatever the copies. Then coverage, mute, already
//!   watched (any copy finished or started), the spoiler guard's seen events (recorded as
//!   watched, `quiet = 'seen'`) and Following's ledger (an airing it reminded about, switched
//!   to or recorded in the last 30 days: on Home, no notification, `quiet = 'following'`).
//!   [`check`] with `diff: false` only baselines what became tracked (`jobs::new_episodes`'
//!   local `track`).
//! * **Clearing** (§3.5): finished in any copy (derived when a [`snapshot`] is built, written
//!   back by the next check), dismissed ([`dismiss`], [`dismiss_title`], Undo), expired after
//!   `home_days` (Q3). The Watchlists chip goes with [`mark_opened`], the sidebar dot with
//!   [`watchlists_opened`].
//! * **Mute** ([`mute`], Q1): one `newep_mute` row per copy; a muted title stays tracked and
//!   its ledger keeps up, so unmuting never replays what arrived meanwhile.
//! * **Backup** ([`export`] / [`import`]): the arrivals (Home's state) and the mutes; never the
//!   ledger — a restore re-baselines silently.
//!
//! Reads are made when a snapshot is rebuilt or a screen opens, never per frame. Nothing here
//! logs: titles never reach the log at INFO.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};

use crate::config::NewEpisodesConfig;
use crate::error::{Error, Result};
use crate::model::{ArrivalId, ItemKey, ItemKind, ItemRef, SeriesId, SourceId, Timestamp, VodKind};
use crate::new_episodes::{Arrival, Cleared, Coverage, NewEpisode, NewEpisodesSnapshot, Quiet, SeriesNews, Tracked, Why};
use crate::store::library::Library;
use crate::store::{copies, sql, user};

/// "Binged" needs at least this many finished episodes in the season (Q6), besides the share.
pub const BINGE_MIN: usize = 3;
/// Settings ▸ New episodes ▸ Recent shows this many days.
pub const RECENT_DAYS: i64 = 30;
/// Cleared arrivals are deleted this long after they cleared.
const PRUNE_AFTER_S: i64 = 90 * 86_400;
/// A Following ledger row this recent (or upcoming) makes an arrival quiet (§3.3).
const FOLLOWING_WINDOW_S: i64 = 30 * 86_400;

/// `meta` keys: the first baseline (the first-run card shows after it), the card acknowledged,
/// the Watchlists screen last opened (the sidebar dot), the last check.
pub const META_BASELINED: &str = "newep:baselined_at";
pub const META_CARD: &str = "newep:card";
pub const META_WATCHLISTS_OPENED: &str = "newep:watchlists_opened_at";
pub const META_CHECKED: &str = "newep:checked_at";

/// `meta` key marking a playlist whose provider doesn't move `last_modified` (§5, Fallback):
/// its tracked copies are fetched at every refresh.
pub fn unreliable_key(source: SourceId) -> String {
    format!("newep:lm_unreliable:{}", source.0)
}

/// The settings one computation uses (`[new_episodes]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opts {
    pub binged: bool,
    pub binge_pct: u32,
    pub home_days: u32,
    pub now: Timestamp,
}

impl Opts {
    pub fn from_config(cfg: &NewEpisodesConfig, now: Timestamp) -> Self {
        Self { binged: cfg.binged, binge_pct: cfg.binge_pct.min(100), home_days: cfg.home_days, now }
    }

    /// Arrivals older than this leave Home (Q3).
    fn expired_before(&self) -> Timestamp {
        self.now - i64::from(self.home_days.max(1)) * 86_400
    }
}

// ------------------------------------------------------------------ titles

/// A title as the library sees it: its visible copies (indices into `lib.vod.series`), the top
/// copy first.
#[derive(Debug, Clone)]
struct Title {
    members: Vec<u32>,
    refs: Vec<ItemRef>,
    ids: Vec<SeriesId>,
    name: String,
}

impl Title {
    fn of(lib: &Library, idx: u32) -> Self {
        let stacks = lib.vod.stacks(false);
        let members = match stacks.stack_of(idx) {
            Some(s) => {
                let top = stacks.top(s);
                std::iter::once(top).chain(stacks.members(s).iter().copied().filter(|&i| i != top)).collect()
            }
            None => vec![idx],
        };
        let rows = &lib.vod.series;
        let refs = members.iter().map(|&i| copies::vod_ref(&rows[i as usize])).collect();
        let ids = members
            .iter()
            .filter_map(|&i| match rows[i as usize].kind {
                VodKind::Series(id) => Some(id),
                VodKind::Movie(_) => None,
            })
            .collect();
        Self { name: rows[members[0] as usize].name.to_string(), members, refs, ids }
    }

    /// The grouping key: its smallest member.
    fn key(lib: &Library, idx: u32) -> u32 {
        lib.vod.stacks(false).copies(idx).into_iter().min().unwrap_or(idx)
    }
}

fn series_id(conn: &Connection, source: SourceId, key: &str) -> Result<Option<SeriesId>> {
    Ok(conn.prepare_cached("SELECT id FROM series WHERE source_id = ?1 AND key = ?2")?.query_row(params![source, key], |r| r.get(0)).optional()?)
}

/// The library index of series copy (`source`, `key`); `None` when it isn't visible.
fn index_of(conn: &Connection, lib: &Library, source: SourceId, key: &str) -> Result<Option<u32>> {
    Ok(series_id(conn, source, key)?.and_then(|id| lib.vod.index_of(VodKind::Series(id))))
}

/// The series an item belongs to: itself, or an episode's series.
fn series_of(conn: &Connection, item: &ItemRef) -> Result<Option<(SourceId, String)>> {
    match item.kind {
        ItemKind::Series => Ok(Some((item.source, item.key.0.clone()))),
        ItemKind::Episode => Ok(conn
            .prepare_cached("SELECT s.source_id, s.key FROM episode e JOIN series s ON s.id = e.series_id WHERE e.source_id = ?1 AND e.key = ?2")?
            .query_row(params![item.source, item.key.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?),
        _ => Ok(None),
    }
}

/// The copies of the title holding `item` (a series or an episode): every visible copy, or the
/// series alone when the library doesn't show it.
fn title_refs(conn: &Connection, lib: &Library, item: &ItemRef) -> Result<Vec<ItemRef>> {
    let Some((source, key)) = series_of(conn, item)? else { return Ok(Vec::new()) };
    Ok(match index_of(conn, lib, source, &key)? {
        Some(idx) => Title::of(lib, idx).refs,
        None => vec![ItemRef { source, kind: ItemKind::Series, key: ItemKey(key) }],
    })
}

/// Locked, concealed or explicit copies exclude the whole title (§3.1, §6), in every parental
/// state.
fn guarded(conn: &Connection, lib: &Library, t: &Title) -> Result<bool> {
    for (&i, r) in t.members.iter().zip(&t.refs) {
        if lib.vod.series[i as usize].locked || lib.conceals(r) || lib.explicit.contains(r) {
            return Ok(true);
        }
    }
    // a snapshot without explicit content left the title's explicit copies out of its stack:
    // look for them by the identities stacks use (TMDB id, match key and year)
    if !lib.visibility.explicit {
        let mut stmt = conn.prepare_cached(
            "SELECT o.source_id, o.key FROM series s JOIN series o ON o.id <> s.id
               AND ((NULLIF(CAST(s.tmdb_id AS INTEGER), 0) IS NOT NULL AND o.tmdb_id = s.tmdb_id)
                    OR (s.match_hash IS NOT NULL AND o.match_hash = s.match_hash AND o.year IS s.year))
             WHERE s.id = ?1",
        )?;
        for &id in &t.ids {
            let others = stmt.query_map([id], |r| Ok(ItemRef { source: r.get(0)?, kind: ItemKind::Series, key: ItemKey(r.get(1)?) }))?;
            for o in others {
                if lib.explicit.contains(&o?) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn muted(conn: &Connection, refs: &[ItemRef]) -> Result<bool> {
    let mut stmt = conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM newep_mute WHERE source_id = ?1 AND series_key = ?2)")?;
    for r in refs {
        if stmt.query_row(params![r.source, r.key.as_str()], |r| r.get::<_, bool>(0))? {
            return Ok(true);
        }
    }
    Ok(false)
}

// ------------------------------------------------------------------ the tracked set

/// The tracked titles (§3.1): watchlist series entries, plus binged titles when `opts.binged`;
/// guarded titles left out. Muted titles are in, marked.
pub fn tracked(conn: &Connection, lib: &Library, opts: &Opts) -> Result<Vec<Tracked>> {
    struct Draft {
        title: Title,
        why: Vec<Why>,
        coverage: Coverage,
    }
    fn draft<'d>(drafts: &'d mut BTreeMap<u32, Draft>, lib: &Library, idx: u32) -> &'d mut Draft {
        drafts.entry(Title::key(lib, idx)).or_insert_with(|| Draft { title: Title::of(lib, idx), why: Vec::new(), coverage: Coverage::default() })
    }
    let mut drafts: BTreeMap<u32, Draft> = BTreeMap::new();
    // series entries; episode entries track nothing, nor do placeholders until they attach
    type EntryRow = (crate::model::WatchlistId, String, SourceId, String, Option<u32>, Option<u32>, Timestamp);
    let entries: Vec<EntryRow> = conn
        .prepare_cached(
            "SELECT w.id, w.name, i.source_id, i.item_key, i.season_from, i.season_to, i.added_at FROM watchlist_item i JOIN watchlist w ON w.id = i.list_id
             WHERE i.kind = 'series' AND i.source_id IS NOT NULL ORDER BY w.is_default DESC, w.sort_index, w.id, i.position, i.id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (list, name, source, key, from, to, added_at) in entries {
        let Some(idx) = index_of(conn, lib, source, &key)? else { continue };
        let d = draft(&mut drafts, lib, idx);
        let why = Why::List { list, name, from, to };
        if !d.why.contains(&why) {
            d.why.push(why);
        }
        d.coverage.add(from.zip(to), added_at);
    }
    if opts.binged {
        for (idx, why) in binged(conn, lib, opts)? {
            let d = draft(&mut drafts, lib, idx);
            d.why.push(why);
            d.coverage.add(None, opts.now);
        }
    }
    let mut out = Vec::new();
    for d in drafts.into_values() {
        if guarded(conn, lib, &d.title)? {
            continue;
        }
        let muted = muted(conn, &d.title.refs)?;
        out.push(Tracked { title: d.title.name, copies: d.title.refs, series: d.title.ids, why: d.why, coverage: d.coverage, muted });
    }
    Ok(out)
}

/// The binged titles (§3.1, Q6), by a member index, with the season that makes them binged
/// (the latest such season).
fn binged(conn: &Connection, lib: &Library, opts: &Opts) -> Result<Vec<(u32, Why)>> {
    // finished episodes in seasons ≥ 1 (part-watched ones don't count; Ctrl+W marks do)
    let rows: Vec<(SeriesId, u32, u32)> = conn
        .prepare_cached(
            "SELECT e.series_id, e.season, e.episode FROM watch_progress p JOIN episode e ON e.source_id = p.source_id AND e.key = p.item_key
             WHERE p.kind = 'episode' AND p.finished = 1 AND e.season >= 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // per title: season → finished episode numbers, any copy watched → watched
    let mut finished: BTreeMap<u32, (u32, BTreeMap<u32, BTreeSet<u32>>)> = BTreeMap::new();
    for (id, season, episode) in rows {
        let Some(idx) = lib.vod.index_of(VodKind::Series(id)) else { continue };
        finished.entry(Title::key(lib, idx)).or_insert_with(|| (idx, BTreeMap::new())).1.entry(season).or_default().insert(episode);
    }
    let mut known_stmt = conn.prepare_cached("SELECT DISTINCT episode FROM episode WHERE series_id = ?1 AND season = ?2")?;
    let mut out = Vec::new();
    for (idx, seasons) in finished.into_values() {
        if !seasons.values().any(|f| f.len() >= BINGE_MIN) {
            continue;
        }
        let t = Title::of(lib, idx);
        let unwatched_arrivals = arrival_coords(conn, &t.refs)?;
        // a tracked title's episodes not in its ledger yet are about to be arrivals
        let mut ledger: HashSet<(u32, u32)> = HashSet::new();
        for r in &t.refs {
            ledger.extend(known_of(conn, r)?);
        }
        let mut best = None;
        for (&season, f) in seasons.iter().filter(|(_, f)| f.len() >= BINGE_MIN) {
            let mut known: BTreeSet<u32> = BTreeSet::new();
            for &id in &t.ids {
                for e in known_stmt.query_map(params![id, season], |r| r.get::<_, u32>(0))? {
                    known.insert(e?);
                }
            }
            // an arrival not watched yet (or about to be one) never lowers the ratio
            known.retain(|e| f.contains(e) || (!unwatched_arrivals.contains(&(season, *e)) && (ledger.is_empty() || ledger.contains(&(season, *e)))));
            if f.len() * 100 > opts.binge_pct as usize * known.len() {
                best = Some(Why::Binged { season, finished: f.len() as u32, known: known.len() as u32 });
            }
        }
        if let Some(why) = best {
            out.push((idx, why));
        }
    }
    Ok(out)
}

/// The coordinates of every arrival recorded for these copies.
fn arrival_coords(conn: &Connection, refs: &[ItemRef]) -> Result<HashSet<(u32, u32)>> {
    let mut stmt = conn.prepare_cached("SELECT season, episode FROM newep_arrival WHERE source_id = ?1 AND series_key = ?2")?;
    let mut out = HashSet::new();
    for r in refs {
        for row in stmt.query_map(params![r.source, r.key.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))? {
            out.insert(row?);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ the check

/// What [`check`] is told besides the tracked set.
#[derive(Debug, Clone, Default)]
pub struct CheckInput {
    /// A check: diff and record arrivals. `false`: only sync the ledger with the tracked set and
    /// baseline what became tracked (local `track`, never an alert).
    pub diff: bool,
    /// Playlists whose first import this was: their new copies baseline silently (§3.2).
    pub silent: HashSet<SourceId>,
    /// Copies the fallback fetched (details old, `last_modified` unchanged): an arrival in one
    /// while `last_modified` still equals `lm_seen` marks its playlist unreliable (§5).
    pub fallback: HashSet<SeriesId>,
}

/// What [`check`] did (counts only: the job logs them at INFO).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckReport {
    pub tracked: usize,
    /// New arrivals to announce (neither quiet nor muted), title by title.
    pub announce: Vec<ArrivalId>,
    /// Arrivals recorded, quiet ones included.
    pub arrivals: usize,
    pub quiet: usize,
    /// Titles with arrivals.
    pub titles: usize,
    /// Copies baselined (their first coordinates recorded).
    pub baselined: usize,
    /// Playlists newly marked as not moving `last_modified`.
    pub unreliable: Vec<SourceId>,
    /// This was the feature's first baseline (the first-run card shows now).
    pub first_run: bool,
}

/// The ledger row of a copy.
#[derive(Debug, Clone, Copy)]
struct CopyRow {
    baselined_at: Option<Timestamp>,
    frontier: bool,
    lm_seen: Option<i64>,
}

/// A copy's catalogue facts.
#[derive(Debug, Clone)]
struct CopyFacts {
    loaded: bool,
    last_modified: Option<i64>,
    clean_name: String,
    norm_name: String,
}

fn copy_row(conn: &Connection, r: &ItemRef) -> Result<Option<CopyRow>> {
    Ok(conn
        .prepare_cached("SELECT baselined_at, frontier, lm_seen FROM newep_copy WHERE source_id = ?1 AND series_key = ?2")?
        .query_row(params![r.source, r.key.as_str()], |r| Ok(CopyRow { baselined_at: r.get(0)?, frontier: r.get(1)?, lm_seen: r.get(2)? }))
        .optional()?)
}

fn copy_facts(conn: &Connection, id: SeriesId) -> Result<Option<CopyFacts>> {
    Ok(conn
        .prepare_cached(
            "SELECT details_at IS NOT NULL OR EXISTS(SELECT 1 FROM episode e WHERE e.series_id = s.id), last_modified, clean_name, norm_name
             FROM series s WHERE s.id = ?1",
        )?
        .query_row([id], |r| Ok(CopyFacts { loaded: r.get(0)?, last_modified: r.get(1)?, clean_name: r.get(2)?, norm_name: r.get(3)? }))
        .optional()?)
}

fn known_of(conn: &Connection, r: &ItemRef) -> Result<BTreeSet<(u32, u32)>> {
    Ok(conn
        .prepare_cached("SELECT season, episode FROM newep_known WHERE source_id = ?1 AND series_key = ?2")?
        .query_map(params![r.source, r.key.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Season → when the ledger of any of these copies first had it.
fn seasons_first_seen(conn: &Connection, refs: &[ItemRef]) -> Result<BTreeMap<u32, Timestamp>> {
    let mut stmt = conn.prepare_cached("SELECT season, min(first_seen) FROM newep_known WHERE source_id = ?1 AND series_key = ?2 GROUP BY season")?;
    let mut out: BTreeMap<u32, Timestamp> = BTreeMap::new();
    for r in refs {
        for row in stmt.query_map(params![r.source, r.key.as_str()], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, Timestamp>(1)?)))? {
            let (season, first) = row?;
            out.entry(season).and_modify(|f| *f = (*f).min(first)).or_insert(first);
        }
    }
    Ok(out)
}

fn current_of(conn: &Connection, id: SeriesId) -> Result<BTreeSet<(u32, u32)>> {
    Ok(conn
        .prepare_cached("SELECT DISTINCT season, episode FROM episode WHERE series_id = ?1")?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Check (or, `diff: false`, only track) in one transaction: the tracked set from `lib`, the
/// ledger synced with it, the diff, the arrivals. See the module docs.
pub fn check(conn: &mut Connection, lib: &Library, opts: &Opts, input: &CheckInput) -> Result<CheckReport> {
    let tx = conn.transaction()?;
    let tracked = tracked(&tx, lib, opts)?;
    let report = check_tracked(&tx, &tracked, opts, input)?;
    if input.diff {
        sync_cleared(&tx, lib, opts)?;
        crate::store::follow::meta_set(&tx, META_CHECKED, &opts.now.to_string())?;
    }
    tx.commit()?;
    Ok(report)
}

/// [`check`] over a tracked set already computed (inside the caller's transaction).
fn check_tracked(conn: &Connection, tracked: &[Tracked], opts: &Opts, input: &CheckInput) -> Result<CheckReport> {
    let now = opts.now;
    let mut report = CheckReport { tracked: tracked.len(), ..Default::default() };
    // copies no longer tracked lose their ledger: a later add baselines again
    let keep: HashSet<(SourceId, &str)> = tracked.iter().flat_map(|t| t.copies.iter().map(|r| (r.source, r.key.as_str()))).collect();
    let stored: Vec<(SourceId, String)> = conn
        .prepare_cached("SELECT source_id, series_key FROM newep_copy")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (source, key) in stored.iter().filter(|(s, k)| !keep.contains(&(*s, k.as_str()))) {
        conn.execute("DELETE FROM newep_copy WHERE source_id = ?1 AND series_key = ?2", params![source, key])?;
    }
    let first_run = crate::store::follow::meta_get(conn, META_BASELINED)?.is_none();
    let mut unreliable: BTreeSet<SourceId> = BTreeSet::new();
    for t in tracked {
        let rows: Vec<Option<CopyRow>> = t.copies.iter().map(|r| copy_row(conn, r)).collect::<Result<_>>()?;
        // a title that just became tracked baselines silently, every copy; a copy found later
        // alerts beyond the title's frontier (unless its playlist's first import brought it)
        let title_new = rows.iter().all(Option::is_none);
        for r in t.copies.iter().zip(&rows).filter(|(_, row)| row.is_none()).map(|(r, _)| r) {
            let frontier = !title_new && !input.silent.contains(&r.source);
            conn.execute(
                "INSERT INTO newep_copy(source_id, series_key, frontier) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
                params![r.source, r.key.as_str(), frontier],
            )?;
        }
        // the ledger before this check: what counts as known, the frontier, the newest season
        let mut known: Vec<BTreeSet<(u32, u32)>> = Vec::with_capacity(t.copies.len());
        for r in &t.copies {
            known.push(known_of(conn, r)?);
        }
        let ledger: BTreeSet<(u32, u32)> = known.iter().flatten().copied().collect();
        // open-ended ranges look at the seasons the title had when their entry started
        // tracking it (its baseline at the earliest), not at what arrived since
        let baselined_at = rows.iter().flatten().filter_map(|r| r.baselined_at).min();
        let seasons = seasons_first_seen(conn, &t.copies)?;
        let newest_at = |at: Timestamp| -> Option<u32> {
            let cutoff = at.max(baselined_at?);
            seasons.iter().filter(|(_, first)| **first <= cutoff).map(|(s, _)| *s).max()
        };
        // (season, episode) → the copy it arrived in first
        let mut found: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for (i, (r, &id)) in t.copies.iter().zip(&t.series).enumerate() {
            let Some(facts) = copy_facts(conn, id)? else { continue };
            let Some(row) = copy_row(conn, r)? else { continue };
            if !facts.loaded {
                continue; // an Xtream copy whose episodes never loaded: baselines at first load
            }
            let current = current_of(conn, id)?;
            let candidates: Vec<(u32, u32)> = match row.baselined_at {
                Some(_) if !input.diff => continue, // the check diffs it
                Some(_) => current.difference(&ledger).copied().collect(),
                None => {
                    let silent = !row.frontier || input.silent.contains(&r.source) || ledger.is_empty();
                    if !silent && !input.diff {
                        continue; // a new copy of a tracked title: the check decides
                    }
                    report.baselined += 1;
                    match ledger.iter().max() {
                        Some(&frontier) if !silent => current.iter().filter(|c| **c > frontier).copied().collect(),
                        _ => Vec::new(),
                    }
                }
            };
            if !candidates.is_empty()
                && row.baselined_at.is_some()
                && input.fallback.contains(&id)
                && facts.last_modified.is_some()
                && facts.last_modified == row.lm_seen
            {
                unreliable.insert(r.source);
            }
            for c in candidates {
                found.entry(c).or_insert(i);
            }
            let mut add = conn.prepare_cached(
                "INSERT INTO newep_known(source_id, series_key, season, episode, first_seen) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
            )?;
            for &(s, e) in current.difference(&known[i]) {
                add.execute(params![r.source, r.key.as_str(), s, e, now])?;
            }
            conn.execute(
                "UPDATE newep_copy SET baselined_at = COALESCE(baselined_at, ?3), lm_seen = ?4 WHERE source_id = ?1 AND series_key = ?2",
                params![r.source, r.key.as_str(), now, facts.last_modified],
            )?;
        }
        if found.is_empty() || !input.diff {
            continue;
        }
        let mut recorded = false;
        let progress = copies::episode_progress(conn, &t.series)?;
        let facts: Vec<CopyFacts> = t.series.iter().filter_map(|&id| copy_facts(conn, id).transpose()).collect::<Result<_>>()?;
        let why = serde_json::to_string(&t.why.iter().map(Why::code).collect::<Vec<_>>()).map_err(Error::other)?;
        for ((season, episode), i) in found {
            if !t.coverage.covers(season, &newest_at) || t.muted {
                continue;
            }
            // already watched or started in any copy (progress moved with a re-key, or watched
            // on another copy): recorded, never alerted
            if progress.get(&(season, episode)).is_some_and(|p| p.finished || p.position_s > 0.0) {
                continue;
            }
            let quiet = if seen_on_tv(conn, &facts, season, episode)? {
                Some(Quiet::Seen)
            } else if followed(conn, &facts, season, episode, now)? {
                Some(Quiet::Following)
            } else {
                None
            };
            let copy = &t.copies[i];
            let fp = user::fingerprint_of(conn, copy)?;
            // seen on TV: recorded as watched (Settings ▸ Recent shows it, Home doesn't)
            let cleared = (quiet == Some(Quiet::Seen)).then_some(now);
            conn.execute(
                "INSERT INTO newep_arrival(source_id, series_key, season, episode, series_title, why, arrived_at, quiet, cleared_at, cleared_how, fp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, COALESCE(?11, '{}'))",
                params![
                    copy.source,
                    copy.key.as_str(),
                    season,
                    episode,
                    t.title,
                    why,
                    now,
                    quiet.map(Quiet::as_str),
                    cleared,
                    cleared.map(|_| Cleared::Watched.as_str()),
                    sql::fp_opt(&fp)
                ],
            )?;
            recorded = true;
            report.arrivals += 1;
            match quiet {
                Some(_) => report.quiet += 1,
                None => report.announce.push(ArrivalId(conn.last_insert_rowid())),
            }
        }
        report.titles += usize::from(recorded);
    }
    for &source in &unreliable {
        let key = unreliable_key(source);
        if crate::store::follow::meta_get(conn, &key)?.is_none() {
            crate::store::follow::meta_set(conn, &key, &now.to_string())?;
            report.unreliable.push(source);
        }
    }
    if first_run {
        crate::store::follow::meta_set(conn, META_BASELINED, &now.to_string())?;
        report.first_run = true;
    }
    Ok(report)
}

/// The spoiler guard saw it (live, catch-up or marked): `spoiler_seen` holds guarded subjects
/// only, so this is best effort (§3.3).
fn seen_on_tv(conn: &Connection, facts: &[CopyFacts], season: u32, episode: u32) -> Result<bool> {
    let mut stmt = conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM spoiler_seen WHERE series_norm = ?1 AND season = ?2 AND episode = ?3)")?;
    for norm in facts.iter().map(|f| f.norm_name.as_str()).collect::<BTreeSet<_>>() {
        if stmt.query_row(params![norm, season, episode], |r| r.get::<_, bool>(0))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Following reminded about, switched to or recorded an airing of it in the last 30 days (or
/// will): one alert per episode (§3.3). The guide's title and the series name meet in
/// `core_title` (`textmatch::event::event_key`'s `title#sNNeNN`).
fn followed(conn: &Connection, facts: &[CopyFacts], season: u32, episode: u32, now: Timestamp) -> Result<bool> {
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM follow_match WHERE event_key = ?1 AND state IN ('scheduled', 'adopted', 'aired') AND start >= ?2)",
    )?;
    let keys: BTreeSet<String> =
        facts.iter().map(|f| format!("{}#s{season:02}e{episode:02}", crate::textmatch::event::core_title(&f.clean_name))).collect();
    for key in keys {
        if stmt.query_row(params![key, now - FOLLOWING_WINDOW_S], |r| r.get::<_, bool>(0))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The master switch went off (or the ledger must start over): forget every copy's ledger, so
/// switching it on again baselines silently (§3.4). Arrivals stay until cleared.
pub fn forget_ledger(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM newep_copy", [])?)
}

// ------------------------------------------------------------------ reading arrivals

const ARRIVAL_COLS: &str =
    "id, source_id, series_key, season, episode, series_title, why, arrived_at, notified_at, quiet, opened_at, cleared_at, cleared_how";

fn arrival_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Arrival> {
    let source: Option<SourceId> = r.get(1)?;
    let key: Option<String> = r.get(2)?;
    Ok(Arrival {
        id: r.get(0)?,
        series: source.zip(key).map(|(source, key)| ItemRef { source, kind: ItemKind::Series, key: ItemKey(key) }),
        season: r.get(3)?,
        episode: r.get(4)?,
        series_title: r.get(5)?,
        why: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
        arrived_at: r.get(7)?,
        notified_at: r.get(8)?,
        quiet: r.get::<_, Option<String>>(9)?.as_deref().and_then(Quiet::parse),
        opened_at: r.get(10)?,
        cleared_at: r.get(11)?,
        cleared_how: r.get::<_, Option<String>>(12)?.as_deref().and_then(Cleared::parse),
    })
}

/// Arrivals by id (the notifications of one check).
pub fn arrivals(conn: &Connection, ids: &[ArrivalId]) -> Result<Vec<Arrival>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {ARRIVAL_COLS} FROM newep_arrival WHERE id = ?1"))?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(a) = stmt.query_row([id], arrival_row).optional()? {
            out.push(a);
        }
    }
    Ok(out)
}

/// Arrivals not cleared yet, newest first.
fn open_arrivals(conn: &Connection) -> Result<Vec<Arrival>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {ARRIVAL_COLS} FROM newep_arrival WHERE cleared_at IS NULL ORDER BY arrived_at DESC, id DESC"))?
        .query_map([], arrival_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Resolves arrivals' copies to visible, unguarded titles (cached per copy).
struct Resolver<'a> {
    conn: &'a Connection,
    lib: &'a Library,
    by_copy: HashMap<(SourceId, String), Option<u32>>,
    titles: HashMap<u32, Option<Title>>,
}

impl<'a> Resolver<'a> {
    fn new(conn: &'a Connection, lib: &'a Library) -> Self {
        Self { conn, lib, by_copy: HashMap::new(), titles: HashMap::new() }
    }

    /// The title key of an arrival's copy; `None` when it isn't visible or is guarded.
    fn title_key(&mut self, series: &ItemRef) -> Result<Option<u32>> {
        let k = (series.source, series.key.0.clone());
        if let Some(v) = self.by_copy.get(&k) {
            return Ok(*v);
        }
        let key = match index_of(self.conn, self.lib, series.source, series.key.as_str())? {
            Some(idx) => {
                let key = Title::key(self.lib, idx);
                if !self.titles.contains_key(&key) {
                    let t = Title::of(self.lib, idx);
                    let ok = !guarded(self.conn, self.lib, &t)?;
                    self.titles.insert(key, ok.then_some(t));
                }
                self.titles[&key].as_ref().map(|_| key)
            }
            None => None,
        };
        self.by_copy.insert(k, key);
        Ok(key)
    }

    fn title(&self, key: u32) -> &Title {
        self.titles[&key].as_ref().expect("resolved title")
    }
}

/// Write back what readers derive: arrivals finished in any copy (`watched`), older than
/// `home_days` (`expired`), and those whose playlist is gone without another copy to show them
/// through; then delete arrivals cleared long ago.
fn sync_cleared(conn: &Connection, lib: &Library, opts: &Opts) -> Result<()> {
    let now = opts.now;
    let mut res = Resolver::new(conn, lib);
    let mut progress: HashMap<u32, HashMap<(u32, u32), crate::model::WatchProgress>> = HashMap::new();
    let mut clear = conn.prepare_cached("UPDATE newep_arrival SET cleared_at = ?2, cleared_how = ?3 WHERE id = ?1 AND cleared_at IS NULL")?;
    for a in open_arrivals(conn)? {
        let how = match &a.series {
            // a deleted playlist's arrival no other copy took over
            None => Some(Cleared::Expired),
            Some(_) if a.arrived_at < opts.expired_before() => Some(Cleared::Expired),
            Some(series) => match res.title_key(series)? {
                Some(key) => {
                    let watched = match progress.entry(key) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => e.insert(copies::episode_progress(conn, &res.title(key).ids)?),
                    };
                    watched.get(&(a.season, a.episode)).is_some_and(|p| p.finished).then_some(Cleared::Watched)
                }
                None => None,
            },
        };
        if let Some(how) = how {
            clear.execute(params![a.id, now, how.as_str()])?;
        }
    }
    conn.execute("DELETE FROM newep_arrival WHERE cleared_at IS NOT NULL AND cleared_at < ?1", [now - PRUNE_AFTER_S])?;
    Ok(())
}

/// What Home, Watchlists, the sidebar and agents read (§2): one entry per title with arrivals
/// not yet watched, dismissed or expired, newest first. Watched is derived here (the next check
/// writes it back). Locked, hidden and explicit titles are absent.
pub fn snapshot(conn: &Connection, lib: &Library, opts: &Opts) -> Result<NewEpisodesSnapshot> {
    let checked_at = crate::store::follow::meta_get(conn, META_CHECKED)?.and_then(|v| v.parse().ok());
    let open = open_arrivals(conn)?;
    if open.is_empty() {
        return Ok(NewEpisodesSnapshot { series: Vec::new(), list_dot: false, checked_at });
    }
    let mut res = Resolver::new(conn, lib);
    let mut by_title: Vec<(u32, Vec<Arrival>)> = Vec::new();
    for a in open {
        let Some(series) = &a.series else { continue };
        if a.arrived_at < opts.expired_before() {
            continue;
        }
        let Some(key) = res.title_key(series)? else { continue };
        match by_title.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(a),
            None => by_title.push((key, vec![a])),
        }
    }
    let tracked = if by_title.is_empty() { Vec::new() } else { tracked(conn, lib, opts)? };
    let opened_dot: Timestamp = crate::store::follow::meta_get(conn, META_WATCHLISTS_OPENED)?.and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut series = Vec::new();
    let mut list_dot = false;
    for (key, mut arrivals) in by_title {
        let t = res.title(key).clone();
        let progress = copies::episode_progress(conn, &t.ids)?;
        arrivals.retain(|a| !progress.get(&(a.season, a.episode)).is_some_and(|p| p.finished));
        if arrivals.is_empty() {
            continue;
        }
        arrivals.sort_by_key(|a| (a.season, a.episode));
        let why = tracked.iter().find(|tr| tr.copies.iter().any(|c| t.refs.contains(c))).map(|tr| tr.why.clone()).unwrap_or_default();
        let mut new = Vec::with_capacity(arrivals.len());
        for a in &arrivals {
            let p = progress.get(&(a.season, a.episode));
            new.push(NewEpisode {
                arrival: a.id,
                season: a.season,
                episode: a.episode,
                episode_ref: episode_ref(conn, &t, a)?,
                arrived_at: a.arrived_at,
                quiet: a.quiet,
                opened: a.opened_at.is_some(),
                progress: p.and_then(|p| p.duration_s.filter(|d| *d > 0.0).map(|d| (p.position_s / d).clamp(0.0, 1.0) as f32)),
            });
        }
        let newest_at = arrivals.iter().map(|a| a.arrived_at).max().unwrap_or(0);
        let news = SeriesNews { title: t.name.clone(), copies: t.refs.clone(), series: t.ids.clone(), why, new, newest_at };
        list_dot |= news.on_list() && newest_at > opened_dot;
        series.push(news);
    }
    series.sort_by(|a, b| b.newest_at.cmp(&a.newest_at).then_with(|| a.title.cmp(&b.title)));
    Ok(NewEpisodesSnapshot { series, list_dot, checked_at })
}

/// The episode an arrival plays or downloads: in the copy it arrived in, else in any copy.
fn episode_ref(conn: &Connection, t: &Title, a: &Arrival) -> Result<Option<ItemRef>> {
    let mut stmt =
        conn.prepare_cached("SELECT source_id, key FROM episode WHERE series_id = ?1 AND season = ?2 AND episode = ?3 ORDER BY id LIMIT 1")?;
    let first = a.series.as_ref().and_then(|s| t.refs.iter().position(|r| r == s)).unwrap_or(0);
    let order = std::iter::once(first).chain((0..t.ids.len()).filter(|&i| i != first));
    for i in order {
        let Some(&id) = t.ids.get(i) else { continue };
        if let Some(r) = stmt
            .query_row(params![id, a.season, a.episode], |r| Ok(ItemRef { source: r.get(0)?, kind: ItemKind::Episode, key: ItemKey(r.get(1)?) }))
            .optional()?
        {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

/// The arrivals of the title holding `item` (a series copy) that are still open: the series
/// page's "New" chips and its "2 new episodes since Tue" line. Empty for a guarded title.
pub fn arrivals_of(conn: &Connection, lib: &Library, item: &ItemRef) -> Result<Vec<Arrival>> {
    let Some((source, key)) = series_of(conn, item)? else { return Ok(Vec::new()) };
    let Some(idx) = index_of(conn, lib, source, &key)? else { return Ok(Vec::new()) };
    let t = Title::of(lib, idx);
    if guarded(conn, lib, &t)? {
        return Ok(Vec::new());
    }
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {ARRIVAL_COLS} FROM newep_arrival WHERE source_id = ?1 AND series_key = ?2 AND cleared_at IS NULL"))?;
    let mut out = Vec::new();
    for r in &t.refs {
        for a in stmt.query_map(params![r.source, r.key.as_str()], arrival_row)? {
            out.push(a?);
        }
    }
    out.sort_by_key(|a| (a.season, a.episode));
    Ok(out)
}

/// Settings ▸ New episodes ▸ Recent: the arrivals of the last `days` (cleared and quiet ones
/// too), newest first, without guarded or vanished titles.
pub fn recent(conn: &Connection, lib: &Library, now: Timestamp, days: i64) -> Result<Vec<Arrival>> {
    let rows: Vec<Arrival> = conn
        .prepare_cached(&format!("SELECT {ARRIVAL_COLS} FROM newep_arrival WHERE arrived_at >= ?1 ORDER BY arrived_at DESC, id DESC"))?
        .query_map([now - days * 86_400], arrival_row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut res = Resolver::new(conn, lib);
    let mut out = Vec::new();
    for a in rows {
        if let Some(s) = &a.series
            && res.title_key(s)?.is_some()
        {
            out.push(a);
        }
    }
    Ok(out)
}

/// The newest arrival of each tracked title (Settings' "last new Tue 21 Oct"), in order.
pub fn last_new(conn: &Connection, tracked: &[Tracked]) -> Result<Vec<Option<Timestamp>>> {
    let mut stmt = conn.prepare_cached("SELECT max(arrived_at) FROM newep_arrival WHERE source_id = ?1 AND series_key = ?2")?;
    tracked
        .iter()
        .map(|t| {
            let mut best: Option<Timestamp> = None;
            for r in &t.copies {
                let at: Option<Timestamp> = stmt.query_row(params![r.source, r.key.as_str()], |r| r.get(0))?;
                best = best.max(at);
            }
            Ok(best)
        })
        .collect()
}

// ------------------------------------------------------------------ the job's reads

/// A tracked Xtream copy the check may fetch (`jobs::details::load_series_checked`).
#[derive(Debug, Clone, PartialEq)]
pub struct FetchCandidate {
    pub series: SeriesId,
    pub source: SourceId,
    /// When its episodes were fetched; `None`: never, or `last_modified` moved since
    /// (`store::import::upsert_series` clears it).
    pub details_at: Option<Timestamp>,
    /// Its playlist doesn't move `last_modified` ([`unreliable_key`]): fetched at every refresh.
    pub unreliable: bool,
}

/// The tracked copies on Xtream playlists (playlist series arrive with their episodes).
pub fn fetch_candidates(conn: &Connection, lib: &Library, opts: &Opts) -> Result<Vec<FetchCandidate>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.details_at, s.source_id FROM series s JOIN source src ON src.id = s.source_id WHERE s.id = ?1 AND src.kind = 'xtream'",
    )?;
    let mut unreliable: HashMap<SourceId, bool> = HashMap::new();
    let mut out = Vec::new();
    for t in tracked(conn, lib, opts)? {
        for &id in &t.series {
            let Some((details_at, source)) = stmt.query_row([id], |r| Ok((r.get(0)?, r.get::<_, SourceId>(1)?))).optional()? else { continue };
            let unreliable = match unreliable.get(&source) {
                Some(u) => *u,
                None => {
                    let u = crate::store::follow::meta_get(conn, &unreliable_key(source))?.is_some();
                    unreliable.insert(source, u);
                    u
                }
            };
            out.push(FetchCandidate { series: id, source, details_at, unreliable });
        }
    }
    Ok(out)
}

/// The check fetched these copies' episodes at `now` (`newep_copy.checked_at`).
pub fn mark_checked(conn: &Connection, series: &[SeriesId], now: Timestamp) -> Result<()> {
    let mut stmt = conn
        .prepare_cached("UPDATE newep_copy SET checked_at = ?2 WHERE (source_id, series_key) IN (SELECT source_id, key FROM series WHERE id = ?1)")?;
    for id in series {
        stmt.execute(params![id, now])?;
    }
    Ok(())
}

/// One title's new episodes for a notification: `jobs::new_episodes` adds the spoiler guard and
/// the words (§2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Announcement {
    /// The title's display name (the guard decides whether it shows).
    pub title: String,
    /// `series.norm_name` of its top copy (`Subject::Episode`).
    pub series_norm: String,
    pub copies: Vec<ItemRef>,
    pub arrivals: Vec<ArrivalId>,
    /// (season, episode), in order.
    pub episodes: Vec<(u32, u32)>,
    /// The new episodes, for Download (in the copy each arrived in, else any copy).
    pub refs: Vec<ItemRef>,
    /// The playlist the first of them arrived in ("on TV").
    pub playlist: Option<SourceId>,
    /// What Play plays (Q4): the next unwatched episode — (its series copy, the episode,
    /// season, number).
    pub next: Option<(ItemRef, ItemRef, u32, u32)>,
    /// The episode's own title when exactly one is new.
    pub episode_title: Option<String>,
    /// Every one of them is queued in Downloads already (a Keep N rule): no Download button.
    pub downloading: bool,
}

/// The arrivals `ids` (one check's, or several held ones) as notifications, title by title in
/// arrival order. Quiet, cleared and since-watched arrivals and guarded titles are left out.
pub fn announcements(conn: &Connection, lib: &Library, ids: &[ArrivalId]) -> Result<Vec<Announcement>> {
    let mut res = Resolver::new(conn, lib);
    let mut groups: Vec<(u32, Vec<Arrival>)> = Vec::new();
    for a in arrivals(conn, ids)? {
        if a.quiet.is_some() || a.cleared_at.is_some() {
            continue;
        }
        let Some(series) = &a.series else { continue };
        let Some(key) = res.title_key(series)? else { continue };
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(a),
            None => groups.push((key, vec![a])),
        }
    }
    let mut queued = conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM download WHERE kind = 'episode' AND source_id = ?1 AND item_key = ?2
                         AND state IN ('scheduled', 'waiting', 'running', 'finishing', 'paused', 'done'))",
    )?;
    let mut out = Vec::new();
    for (key, mut rows) in groups {
        let t = res.title(key).clone();
        let progress = copies::episode_progress(conn, &t.ids)?;
        rows.retain(|a| !progress.get(&(a.season, a.episode)).is_some_and(|p| p.finished || p.position_s > 0.0));
        if rows.is_empty() {
            continue;
        }
        rows.sort_by_key(|a| (a.season, a.episode));
        let mut refs = Vec::new();
        for a in &rows {
            refs.extend(episode_ref(conn, &t, a)?);
        }
        let mut downloading = !refs.is_empty();
        for r in &refs {
            downloading &= queued.query_row(params![r.source, r.key.as_str()], |r| r.get::<_, bool>(0))?;
        }
        let next = copies::next_episode(conn, &t.ids)?.and_then(|(sid, e)| {
            let series = t.ids.iter().position(|id| *id == sid).map(|i| t.refs[i].clone())?;
            Some((series, ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key.clone() }, e.season, e.episode))
        });
        let episode_title = match refs.as_slice() {
            [one] if rows.len() == 1 => conn
                .prepare_cached("SELECT title FROM episode WHERE source_id = ?1 AND key = ?2")?
                .query_row(params![one.source, one.key.as_str()], |r| r.get::<_, String>(0))
                .optional()?
                .filter(|t| !t.trim().is_empty()),
            _ => None,
        };
        let series_norm: String = conn.query_row("SELECT norm_name FROM series WHERE id = ?1", [t.ids[0]], |r| r.get(0))?;
        out.push(Announcement {
            title: t.name.clone(),
            series_norm,
            copies: t.refs.clone(),
            arrivals: rows.iter().map(|a| a.id).collect(),
            episodes: rows.iter().map(|a| (a.season, a.episode)).collect(),
            refs,
            playlist: rows[0].series.as_ref().map(|s| s.source),
            next,
            episode_title,
            downloading,
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------ the screens' writes

/// What a dismiss or a mute changed, put back by [`undo`] (Undo for 10 s).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Undo {
    dismissed: Vec<ArrivalId>,
    muted: Vec<ItemRef>,
    unmuted: Vec<(ItemRef, Timestamp, String)>,
}

impl Undo {
    pub fn is_empty(&self) -> bool {
        self.dismissed.is_empty() && self.muted.is_empty() && self.unmuted.is_empty()
    }
}

/// Dismiss arrivals (Delete on a Home tile, the menu, an agent's `seen` when the user asks):
/// off the Home row and the badges; watched state is never touched.
pub fn dismiss(conn: &Connection, ids: &[ArrivalId], now: Timestamp) -> Result<Undo> {
    let mut stmt = conn.prepare_cached("UPDATE newep_arrival SET cleared_at = ?2, cleared_how = 'dismissed' WHERE id = ?1 AND cleared_at IS NULL")?;
    let mut undo = Undo::default();
    for &id in ids {
        if stmt.execute(params![id, now])? > 0 {
            undo.dismissed.push(id);
        }
    }
    Ok(undo)
}

/// Dismiss every open arrival of the title holding `item` (a Home tile: all its copies).
pub fn dismiss_title(conn: &Connection, lib: &Library, item: &ItemRef, now: Timestamp) -> Result<Undo> {
    let ids = open_ids(conn, &title_refs(conn, lib, item)?)?;
    dismiss(conn, &ids, now)
}

fn open_ids(conn: &Connection, refs: &[ItemRef]) -> Result<Vec<ArrivalId>> {
    let mut stmt = conn.prepare_cached("SELECT id FROM newep_arrival WHERE source_id = ?1 AND series_key = ?2 AND cleared_at IS NULL")?;
    let mut ids = Vec::new();
    for r in refs {
        for id in stmt.query_map(params![r.source, r.key.as_str()], |r| r.get(0))? {
            ids.push(id?);
        }
    }
    Ok(ids)
}

/// Put back what a [`dismiss`] or [`mute`] changed.
pub fn undo(conn: &Connection, u: &Undo) -> Result<()> {
    sql::atomic(conn, |c| {
        let mut back =
            c.prepare_cached("UPDATE newep_arrival SET cleared_at = NULL, cleared_how = NULL WHERE id = ?1 AND cleared_how = 'dismissed'")?;
        for id in &u.dismissed {
            back.execute([id])?;
        }
        for r in &u.muted {
            c.execute("DELETE FROM newep_mute WHERE source_id = ?1 AND series_key = ?2", params![r.source, r.key.as_str()])?;
        }
        for (r, at, fp) in &u.unmuted {
            c.execute(
                "INSERT INTO newep_mute(source_id, series_key, muted_at, fp) VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
                params![r.source, r.key.as_str(), at, fp],
            )?;
        }
        Ok(())
    })
}

/// The title holding `item` (a series copy, or an episode being played) was opened: its series
/// page, its watchlist entry, one of its new episodes played. The Watchlists chip goes for it
/// in every list (§3.5). Returns how many arrivals changed.
pub fn mark_opened(conn: &Connection, lib: &Library, item: &ItemRef, now: Timestamp) -> Result<usize> {
    let mut stmt = conn.prepare_cached(
        "UPDATE newep_arrival SET opened_at = ?3 WHERE source_id = ?1 AND series_key = ?2 AND opened_at IS NULL AND cleared_at IS NULL",
    )?;
    let mut n = 0;
    for r in title_refs(conn, lib, item)? {
        n += stmt.execute(params![r.source, r.key.as_str(), now])?;
    }
    Ok(n)
}

/// The Watchlists screen opened: the sidebar's dot goes until something newer arrives.
pub fn watchlists_opened(conn: &Connection, now: Timestamp) -> Result<()> {
    crate::store::follow::meta_set(conn, META_WATCHLISTS_OPENED, &now.to_string())
}

/// "Stop alerts for this series" (`on`) or allow them again (Q1), for every copy of the title
/// holding `item`. Muting also takes the title's open arrivals off Home. With Undo.
pub fn mute(conn: &Connection, lib: &Library, item: &ItemRef, on: bool, now: Timestamp) -> Result<Undo> {
    let refs = title_refs(conn, lib, item)?;
    sql::atomic(conn, |c| {
        let mut undo = Undo::default();
        if on {
            for r in &refs {
                let fp = user::fingerprint_of(c, r)?;
                if c.execute(
                    "INSERT INTO newep_mute(source_id, series_key, muted_at, fp) VALUES (?1, ?2, ?3, COALESCE(?4, '{}')) ON CONFLICT DO NOTHING",
                    params![r.source, r.key.as_str(), now, sql::fp_opt(&fp)],
                )? > 0
                {
                    undo.muted.push(r.clone());
                }
            }
            undo.dismissed = dismiss(c, &open_ids(c, &refs)?, now)?.dismissed;
        } else {
            let mut stmt = c.prepare_cached("SELECT muted_at, fp FROM newep_mute WHERE source_id = ?1 AND series_key = ?2")?;
            for r in &refs {
                if let Some((at, fp)) = stmt.query_row(params![r.source, r.key.as_str()], |r| Ok((r.get(0)?, r.get(1)?))).optional()? {
                    c.execute("DELETE FROM newep_mute WHERE source_id = ?1 AND series_key = ?2", params![r.source, r.key.as_str()])?;
                    undo.unmuted.push((r.clone(), at, fp));
                }
            }
        }
        Ok(undo)
    })
}

/// The first-run card is due: the first baseline happened and the card wasn't acknowledged.
pub fn card_due(conn: &Connection) -> Result<bool> {
    Ok(crate::store::follow::meta_get(conn, META_BASELINED)?.is_some() && crate::store::follow::meta_get(conn, META_CARD)?.is_none())
}

/// The first-run card was answered (any button or Esc).
pub fn ack_card(conn: &Connection, now: Timestamp) -> Result<()> {
    crate::store::follow::meta_set(conn, META_CARD, &now.to_string())
}

/// Record that these arrivals were announced (a notification or its toast).
pub fn mark_notified(conn: &Connection, ids: &[ArrivalId], now: Timestamp) -> Result<()> {
    let mut stmt = conn.prepare_cached("UPDATE newep_arrival SET notified_at = ?2 WHERE id = ?1 AND notified_at IS NULL")?;
    for id in ids {
        stmt.execute(params![id, now])?;
    }
    Ok(())
}

// ------------------------------------------------------------------ playlists

/// A playlist is about to be deleted: its open arrivals move to another copy of their title
/// (the same TMDB id, or the same match key and year) so the Home tile stays; without one they
/// keep their row with no playlist and the next check clears them (§4).
pub(crate) fn before_source_delete(conn: &Connection, source: SourceId) -> Result<()> {
    let rows: Vec<(ArrivalId, String)> = conn
        .prepare_cached("SELECT id, series_key FROM newep_arrival WHERE source_id = ?1 AND cleared_at IS NULL")?
        .query_map([source], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut other = conn.prepare_cached(
        "SELECT o.source_id, o.key FROM series s JOIN series o ON o.source_id <> s.source_id
           AND ((NULLIF(CAST(s.tmdb_id AS INTEGER), 0) IS NOT NULL AND o.tmdb_id = s.tmdb_id)
                OR (s.match_hash IS NOT NULL AND o.match_hash = s.match_hash AND o.year IS s.year))
         WHERE s.source_id = ?1 AND s.key = ?2 ORDER BY o.source_id, o.id LIMIT 1",
    )?;
    for (id, key) in rows {
        if let Some((to, to_key)) = other.query_row(params![source, key], |r| Ok((r.get::<_, SourceId>(0)?, r.get::<_, String>(1)?))).optional()? {
            conn.execute("UPDATE newep_arrival SET source_id = ?2, series_key = ?3 WHERE id = ?1", params![id, to, to_key])?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ backup

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct BackupArrival {
    series: ItemRef,
    season: u32,
    episode: u32,
    title: String,
    #[serde(default)]
    why: Vec<String>,
    arrived_at: Timestamp,
    #[serde(default)]
    notified_at: Option<Timestamp>,
    #[serde(default)]
    quiet: Option<Quiet>,
    #[serde(default)]
    opened_at: Option<Timestamp>,
    #[serde(default)]
    cleared_at: Option<Timestamp>,
    #[serde(default)]
    cleared_how: Option<Cleared>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct BackupMute {
    series: ItemRef,
    muted_at: Timestamp,
}

/// The arrivals (Home's state) and the mutes; `None` when there are none. The ledger is never
/// exported: a restore re-baselines silently.
pub fn export(conn: &Connection) -> Result<Option<serde_json::Value>> {
    // the backup taken before an older migration (`backup::before_watchlists`) predates 0018
    if !crate::store::migrations::has_column(conn, "newep_arrival", "id")? {
        return Ok(None);
    }
    let arrivals: Vec<BackupArrival> = conn
        .prepare_cached(&format!("SELECT {ARRIVAL_COLS} FROM newep_arrival WHERE source_id IS NOT NULL ORDER BY id"))?
        .query_map([], arrival_row)?
        .filter_map(|a| {
            a.map(|a| {
                Some(BackupArrival {
                    series: a.series?,
                    season: a.season,
                    episode: a.episode,
                    title: a.series_title,
                    why: a.why,
                    arrived_at: a.arrived_at,
                    notified_at: a.notified_at,
                    quiet: a.quiet,
                    opened_at: a.opened_at,
                    cleared_at: a.cleared_at,
                    cleared_how: a.cleared_how,
                })
            })
            .transpose()
        })
        .collect::<rusqlite::Result<_>>()?;
    let mutes: Vec<BackupMute> = conn
        .prepare_cached("SELECT source_id, series_key, muted_at FROM newep_mute ORDER BY source_id, series_key")?
        .query_map([], |r| {
            Ok(BackupMute { series: ItemRef { source: r.get(0)?, kind: ItemKind::Series, key: ItemKey(r.get(1)?) }, muted_at: r.get(2)? })
        })?
        .collect::<rusqlite::Result<_>>()?;
    if arrivals.is_empty() && mutes.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({ "v": 1, "arrivals": arrivals, "mutes": mutes })))
}

/// Restore [`export`]'s rows onto this database's items (`remap`); an arrival already here for
/// the same copy and episode is kept as it is. Returns the rows added.
pub fn import(conn: &Connection, value: &serde_json::Value, remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<usize> {
    let field = |k: &str| value.get(k).cloned().unwrap_or_else(|| serde_json::json!([]));
    let arrivals: Vec<BackupArrival> = serde_json::from_value(field("arrivals")).map_err(Error::other)?;
    let mutes: Vec<BackupMute> = serde_json::from_value(field("mutes")).map_err(Error::other)?;
    let mut added = 0;
    for a in arrivals {
        let Some(series) = remap(&a.series) else { continue };
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM newep_arrival WHERE source_id = ?1 AND series_key = ?2 AND season = ?3 AND episode = ?4)",
            params![series.source, series.key.as_str(), a.season, a.episode],
            |r| r.get(0),
        )?;
        if exists {
            continue;
        }
        let fp = user::fingerprint_of(conn, &series)?;
        added += conn.execute(
            "INSERT INTO newep_arrival(source_id, series_key, season, episode, series_title, why, arrived_at, notified_at, quiet, opened_at,
                                       cleared_at, cleared_how, fp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, COALESCE(?13, '{}'))",
            params![
                series.source,
                series.key.as_str(),
                a.season,
                a.episode,
                a.title,
                serde_json::to_string(&a.why).map_err(Error::other)?,
                a.arrived_at,
                a.notified_at,
                a.quiet.map(Quiet::as_str),
                a.opened_at,
                a.cleared_at,
                a.cleared_how.map(Cleared::as_str),
                sql::fp_opt(&fp)
            ],
        )?;
    }
    for m in mutes {
        let Some(series) = remap(&m.series) else { continue };
        let fp = user::fingerprint_of(conn, &series)?;
        added += conn.execute(
            "INSERT INTO newep_mute(source_id, series_key, muted_at, fp) VALUES (?1, ?2, ?3, COALESCE(?4, '{}')) ON CONFLICT DO NOTHING",
            params![series.source, series.key.as_str(), m.muted_at, sql::fp_opt(&fp)],
        )?;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, library, titles, watchlist};
    use crate::watchlist::{Insert, PlayOrder};

    const NOW: Timestamp = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn with_db(f: impl FnOnce(&mut Connection) -> Result<()> + Send + 'static) {
        let store = Store::open_temp("new-episodes-store", None).unwrap();
        store
            .writer()
            .blocking(move |c| {
                c.execute_batch(
                    "INSERT INTO source(id,name,kind,created_at) VALUES (1,'TV','m3u-url',0),(2,'TV 2','m3u-url',0),(3,'TV 3','m3u-url',0);",
                )?;
                f(c)
            })
            .unwrap();
    }

    /// A playlist series (episodes complete at import: `details_at` set).
    fn series(c: &Connection, id: i64, source: i64, key: &str, name: &str, year: i64) -> Result<ItemRef> {
        c.execute(
            "INSERT INTO series(id,source_id,key,name,clean_name,norm_name,year,provider_order,seen_gen,details_at) VALUES (?1,?2,?3,?4,?4,?5,?6,0,1,1)",
            params![id, source, key, name, crate::sources::identity::normalize_name(name), year],
        )?;
        Ok(ItemRef { source: SourceId(source), kind: ItemKind::Series, key: ItemKey(key.into()) })
    }

    /// Episodes `eps` of `season` in series `id`.
    fn eps(c: &Connection, id: i64, season: u32, eps: impl IntoIterator<Item = u32>) -> Result<()> {
        let source: i64 = c.query_row("SELECT source_id FROM series WHERE id = ?1", [id], |r| r.get(0))?;
        for e in eps {
            c.execute(
                "INSERT INTO episode(series_id,source_id,key,season,episode,title,duration_s,stream) VALUES (?1,?2,?3,?4,?5,'Episode',3000,'{}')",
                params![id, source, format!("e{id}-{season}-{e}"), season, e],
            )?;
        }
        Ok(())
    }

    fn episode(c: &Connection, id: i64, season: u32, e: u32) -> Result<ItemRef> {
        let (source, key) =
            c.query_row("SELECT source_id, key FROM episode WHERE series_id = ?1 AND season = ?2 AND episode = ?3", params![id, season, e], |r| {
                Ok((r.get::<_, SourceId>(0)?, r.get::<_, String>(1)?))
            })?;
        Ok(ItemRef { source, kind: ItemKind::Episode, key: ItemKey(key) })
    }

    fn watch(c: &Connection, id: i64, season: u32, e: u32, finished: bool) -> Result<()> {
        let r = episode(c, id, season, e)?;
        c.execute(
            "INSERT INTO watch_progress(source_id,kind,item_key,position_s,duration_s,finished,updated_at,title) VALUES (?1,'episode',?2,?3,3000,?4,?5,'')",
            params![r.source, r.key.as_str(), if finished { 3000.0 } else { 600.0 }, finished, NOW - DAY],
        )?;
        Ok(())
    }

    fn lib(c: &Connection) -> Result<Library> {
        titles::refresh_all(c)?;
        library::load(c)
    }

    /// Put `item` on a list (`None`: My list), whole or for seasons `range`.
    fn list(c: &Connection, lib: &Library, name: Option<&str>, item: &ItemRef, range: Option<(u32, u32)>) -> Result<()> {
        let id = match name {
            Some(n) => watchlist::create(c, n, None, PlayOrder::Any, "app", NOW)?,
            None => watchlist::default_list(c)?,
        };
        let mut e = watchlist::new_entry(c, item)?;
        (e.season_from, e.season_to) = (range.map(|r| r.0), range.map(|r| r.1));
        assert_eq!(watchlist::add(c, lib, id, &[e], Insert::End, NOW)?.added.len(), 1);
        Ok(())
    }

    fn opts(now: Timestamp) -> Opts {
        Opts { binged: true, binge_pct: 60, home_days: 30, now }
    }

    fn run(c: &mut Connection, lib: &Library, now: Timestamp) -> Result<CheckReport> {
        check(c, lib, &opts(now), &CheckInput { diff: true, ..Default::default() })
    }

    fn coords(c: &Connection, ids: &[ArrivalId]) -> Result<Vec<(u32, u32)>> {
        Ok(arrivals(c, ids)?.iter().map(|a| (a.season, a.episode)).collect())
    }

    /// Check, then the coordinates it would announce.
    fn announced(c: &mut Connection, lib: &Library, now: Timestamp) -> Result<Vec<(u32, u32)>> {
        let ids = run(c, lib, now)?.announce;
        coords(c, &ids)
    }

    /// §8.1–8.2: the first check records everything silently; then every new episode alerts,
    /// a filled gap too. A title added later, a title tracked before its playlist's next
    /// import (local track) and the switch turned off and on baseline silently.
    #[test]
    fn baseline_is_silent_then_every_new_episode_alerts() {
        with_db(|c| {
            let landman = series(c, 1, 1, "landman", "Landman", 2024)?;
            eps(c, 1, 1, [1, 2, 3, 4, 6])?;
            eps(c, 1, 2, 1..=8)?;
            let lib = lib(c)?;
            list(c, &lib, None, &landman, None)?;
            let r = run(c, &lib, NOW)?;
            assert_eq!((r.tracked, r.arrivals, r.baselined, r.first_run), (1, 0, 1, true));
            assert!(card_due(c)?);
            assert_eq!(c.query_row("SELECT count(*) FROM newep_known", [], |r| r.get::<_, i64>(0))?, 13);

            eps(c, 1, 2, [9, 10])?;
            eps(c, 1, 1, [5])?;
            let r = run(c, &lib, NOW + DAY)?;
            assert_eq!((r.arrivals, r.titles, r.first_run), (3, 1, false));
            assert_eq!(coords(c, &r.announce)?, [(1, 5), (2, 9), (2, 10)]);
            let snap = snapshot(c, &lib, &opts(NOW + DAY))?;
            assert_eq!(snap.series.len(), 1);
            assert_eq!(snap.series[0].span(), "S1E5 · S2E9–E10");
            assert!(snap.list_dot && snap.series[0].unopened());
            assert_eq!(run(c, &lib, NOW + 2 * DAY)?.arrivals, 0, "once only");

            // an old show added later: nothing that exists counts as new
            let wire = series(c, 2, 1, "wire", "The Wire", 2002)?;
            for s in 1..=5 {
                eps(c, 2, s, 1..=10)?;
            }
            let lib = super::tests::lib(c)?;
            list(c, &lib, None, &wire, None)?;
            assert_eq!(run(c, &lib, NOW + 3 * DAY)?.arrivals, 0);

            // tracked by a list edit (local track), then the playlist's import brings S2E1
            let sev = series(c, 3, 2, "sev", "Severance", 2022)?;
            eps(c, 3, 1, 1..=9)?;
            let lib = super::tests::lib(c)?;
            list(c, &lib, None, &sev, None)?;
            let r = check(c, &lib, &opts(NOW + 3 * DAY), &CheckInput::default())?;
            assert_eq!((r.arrivals, r.baselined), (0, 1));
            eps(c, 3, 2, [1])?;
            assert_eq!(announced(c, &lib, NOW + 4 * DAY)?, [(2, 1)]);

            // the switch off and on again: whatever arrived meanwhile is baselined silently
            forget_ledger(c)?;
            eps(c, 1, 2, [11])?;
            assert_eq!(run(c, &lib, NOW + 5 * DAY)?.arrivals, 0);
            Ok(())
        })
    }

    /// §8.3: one alert per title whatever the copies; a new copy alerts beyond the title's
    /// frontier only; a playlist's first import is silent.
    #[test]
    fn copies_alert_once_and_new_copies_only_beyond_the_frontier() {
        with_db(|c| {
            let bear = series(c, 1, 1, "bear", "The Bear", 2022)?;
            for s in 1..=2 {
                eps(c, 1, s, 1..=3)?;
            }
            let lib = lib(c)?;
            list(c, &lib, None, &bear, None)?;
            run(c, &lib, NOW)?;
            // TV 2 gets a copy that has S3 already: S3 is new, S1–S2 aren't
            series(c, 2, 2, "bear2", "The Bear", 2022)?;
            for s in 1..=3 {
                eps(c, 2, s, 1..=3)?;
            }
            let lib = super::tests::lib(c)?;
            assert_eq!(Title::of(&lib, lib.vod.index_of(VodKind::Series(SeriesId(1))).unwrap()).refs.len(), 2, "one stack");
            let r = run(c, &lib, NOW + DAY)?;
            assert_eq!(coords(c, &r.announce)?, [(3, 1), (3, 2), (3, 3)]);
            assert!(arrivals(c, &r.announce)?.iter().all(|a| a.series.as_ref().is_some_and(|s| s.source == SourceId(2))));
            // a playlist's first import: silent, even beyond the frontier
            series(c, 3, 3, "bear3", "The Bear", 2022)?;
            for s in 1..=4 {
                eps(c, 3, s, 1..=3)?;
            }
            let lib = super::tests::lib(c)?;
            let first = CheckInput { diff: true, silent: [SourceId(3)].into(), ..Default::default() };
            assert_eq!(check(c, &lib, &opts(NOW + 2 * DAY), &first)?.arrivals, 0);
            // S5E1 on TV on Monday alerts; on TV 2 on Wednesday it's known
            eps(c, 1, 5, [1])?;
            assert_eq!(announced(c, &lib, NOW + 3 * DAY)?, [(5, 1)]);
            eps(c, 2, 5, [1])?;
            assert_eq!(run(c, &lib, NOW + 5 * DAY)?.arrivals, 0);
            let tv2: i64 = c.query_row("SELECT count(*) FROM newep_known WHERE source_id = 2 AND season = 5", [], |r| r.get(0))?;
            assert_eq!(tv2, 1, "recorded for TV 2 too");
            Ok(())
        })
    }

    /// §8.4: an empty `series_info` deletes the episodes and the next fetch restores them; a
    /// re-uploaded episode comes back under another key: never an arrival.
    #[test]
    fn episodes_that_come_and_go_never_alert_twice() {
        with_db(|c| {
            let s = series(c, 1, 1, "landman", "Landman", 2024)?;
            eps(c, 1, 1, 1..=8)?;
            let lib = lib(c)?;
            list(c, &lib, None, &s, None)?;
            run(c, &lib, NOW)?;
            c.execute("DELETE FROM episode WHERE series_id = 1", [])?;
            assert_eq!(run(c, &lib, NOW + DAY)?.arrivals, 0);
            eps(c, 1, 1, 1..=8)?;
            assert_eq!(run(c, &lib, NOW + 2 * DAY)?.arrivals, 0);
            c.execute("UPDATE episode SET key = 'reupload' WHERE series_id = 1 AND season = 1 AND episode = 8", [])?;
            assert_eq!(run(c, &lib, NOW + 3 * DAY)?.arrivals, 0);
            Ok(())
        })
    }

    /// §8.5: over 60 % of a season with at least 3 finished (Q6); copies combine; part-watched
    /// and season 0 don't count; a new arrival never lowers the ratio.
    #[test]
    fn the_binge_rule() {
        type Setup = fn(&Connection) -> Result<()>;
        let cases: [(&str, Setup, bool); 6] = [
            ("5 of 8", |c| (1..=5).try_for_each(|e| watch(c, 1, 1, e, true)), true),
            ("4 of 8", |c| (1..=4).try_for_each(|e| watch(c, 1, 1, e, true)), false),
            (
                "3 on TV + 2 on TV 2",
                |c| (1..=3).try_for_each(|e| watch(c, 1, 1, e, true)).and((4..=5).try_for_each(|e| watch(c, 2, 1, e, true))),
                true,
            ),
            ("4 finished + 1 started", |c| (1..=4).try_for_each(|e| watch(c, 1, 1, e, true)).and(watch(c, 1, 1, 5, false)), false),
            ("specials", |c| (1..=8).try_for_each(|e| watch(c, 1, 0, e, true)), false),
            ("1 of 1", |c| watch(c, 1, 2, 1, true), false),
        ];
        for (name, setup, binged) in cases {
            with_db(move |c| {
                for (id, source, key) in [(1, 1, "slow"), (2, 2, "slow2")] {
                    series(c, id, source, key, "Slow Horses", 2022)?;
                    eps(c, id, 0, 1..=8)?;
                    eps(c, id, 1, 1..=8)?;
                    eps(c, id, 2, [1])?;
                }
                setup(c)?;
                let lib = lib(c)?;
                let t = tracked(c, &lib, &opts(NOW))?;
                assert_eq!(t.len(), usize::from(binged), "{name}");
                assert!(tracked(c, &lib, &Opts { binged: false, ..opts(NOW) })?.is_empty(), "{name}: binged off");
                Ok(())
            });
        }
        with_db(|c| {
            series(c, 1, 1, "slow", "Slow Horses", 2022)?;
            eps(c, 1, 1, 1..=8)?;
            (1..=5).try_for_each(|e| watch(c, 1, 1, e, true))?;
            let lib = lib(c)?;
            run(c, &lib, NOW)?;
            eps(c, 1, 1, [9])?;
            assert_eq!(announced(c, &lib, NOW + DAY)?, [(1, 9)], "binged series alert too");
            let t = tracked(c, &lib, &opts(NOW + DAY))?;
            assert_eq!(t[0].why, [Why::Binged { season: 1, finished: 5, known: 8 }], "the arrival didn't lower the ratio");
            Ok(())
        });
    }

    /// §8.6: an open-ended range follows new seasons, a closed one only fills gaps; two
    /// entries of a title give the union; an episode entry tracks nothing.
    #[test]
    fn season_ranges() {
        with_db(|c| {
            let bear = series(c, 1, 1, "bear", "The Bear", 2022)?;
            let mando = series(c, 2, 1, "mando", "The Mandalorian", 2019)?;
            let sev = series(c, 3, 1, "sev", "Severance", 2022)?;
            series(c, 4, 1, "andor", "Andor", 2022)?;
            for id in 1..=4 {
                for s in 1..=3 {
                    eps(c, id, s, 1..=4)?;
                }
            }
            eps(c, 2, 2, [6])?; // Mandalorian S2 has a gap at E5
            let lib = lib(c)?;
            list(c, &lib, None, &bear, Some((1, 3)))?;
            list(c, &lib, None, &mando, Some((1, 2)))?;
            list(c, &lib, None, &sev, Some((1, 1)))?;
            list(c, &lib, Some("Sci-fi"), &sev, Some((3, 3)))?;
            let ep = episode(c, 4, 1, 1)?;
            list(c, &lib, None, &ep, None)?;
            run(c, &lib, NOW)?;
            eps(c, 1, 4, [1])?;
            eps(c, 2, 2, [5])?;
            eps(c, 2, 3, [5])?;
            eps(c, 2, 4, [1])?;
            eps(c, 3, 1, [5])?;
            eps(c, 3, 2, [5])?;
            eps(c, 3, 4, [1])?;
            eps(c, 4, 4, [1])?;
            let r = run(c, &lib, NOW + DAY)?;
            let mut got: Vec<(String, u32, u32)> = arrivals(c, &r.announce)?.into_iter().map(|a| (a.series_title, a.season, a.episode)).collect();
            got.sort();
            let want = [("Severance", 1, 5), ("Severance", 4, 1), ("The Bear", 4, 1), ("The Mandalorian", 2, 5)];
            assert_eq!(got, want.map(|(t, s, e)| (t.to_owned(), s, e)));
            assert_eq!(tracked(c, &lib, &opts(NOW))?.len(), 3, "the episode entry tracks nothing");

            // open-endedness is decided once: the rest of a new season, and the season after,
            // alert too; a closed range stays closed
            eps(c, 1, 4, [2])?;
            eps(c, 1, 5, [1])?;
            eps(c, 2, 4, [2])?;
            eps(c, 2, 5, [1])?;
            let r = run(c, &lib, NOW + 2 * DAY)?;
            let mut got: Vec<(String, u32, u32)> = arrivals(c, &r.announce)?.into_iter().map(|a| (a.series_title, a.season, a.episode)).collect();
            got.sort();
            assert_eq!(got, [("The Bear", 4, 2), ("The Bear", 5, 1)].map(|(t, s, e)| (t.to_owned(), s, e)));

            // an entry added to a tracked title once S5 exists: its "S1–S4" is closed, whatever
            // the title had when it was first tracked
            list(c, &lib, Some("Later"), &mando, Some((1, 4)))?;
            c.execute("UPDATE watchlist_item SET added_at = ?1 WHERE item_key = 'mando' AND season_to = 4", [NOW + 2 * DAY])?;
            eps(c, 2, 6, [1])?;
            assert!(announced(c, &lib, NOW + 3 * DAY)?.is_empty(), "a range closed when added stays closed");
            Ok(())
        })
    }

    /// §8.7: a locked copy, an explicit copy (shown or hidden) and a hidden title never give
    /// arrivals or counts.
    #[test]
    fn guarded_titles_never_alert() {
        for case in ["locked", "explicit shown", "explicit hidden", "hidden"] {
            with_db(move |c| {
                let a = series(c, 1, 1, "andor", "Andor", 2022)?;
                series(c, 2, 2, "andor2", "Andor", 2022)?;
                for id in 1..=2 {
                    eps(c, id, 1, 1..=4)?;
                }
                let lib = lib(c)?;
                list(c, &lib, None, &a, None)?;
                run(c, &lib, NOW)?;
                let col = match case {
                    "locked" => "locked",
                    "hidden" => "hidden",
                    _ => "",
                };
                if col.is_empty() {
                    c.execute("UPDATE series SET explicit = 1 WHERE id = 2", [])?;
                } else {
                    c.execute(&format!("INSERT INTO item_override(source_id,kind,item_key,{col},fp) VALUES (2,'series','andor2',1,'{{}}')"), [])?;
                }
                titles::refresh_all(c)?;
                let visibility = if case == "explicit hidden" { library::Visibility::NO_EXPLICIT } else { library::Visibility::ALL };
                let lib = library::load_visible(c, visibility)?;
                eps(c, 1, 2, [1])?;
                eps(c, 2, 2, [1])?;
                let r = run(c, &lib, NOW + DAY)?;
                // a hidden copy only leaves the title: the visible copy still alerts
                let want = usize::from(case == "hidden");
                assert_eq!((r.arrivals, r.tracked), (want, want), "{case}");
                if case != "hidden" {
                    assert!(snapshot(c, &lib, &opts(NOW + DAY))?.series.is_empty(), "{case}");
                }
                Ok(())
            });
        }
        // a hidden title (its only copy hidden) is absent
        with_db(|c| {
            let a = series(c, 1, 1, "andor", "Andor", 2022)?;
            eps(c, 1, 1, 1..=4)?;
            let lib = lib(c)?;
            list(c, &lib, None, &a, None)?;
            c.execute("INSERT INTO item_override(source_id,kind,item_key,hidden,fp) VALUES (1,'series','andor',1,'{}')", [])?;
            let lib = super::tests::lib(c)?;
            eps(c, 1, 2, [1])?;
            assert_eq!(run(c, &lib, NOW)?.tracked, 0);
            Ok(())
        });
    }

    /// §8.8: an airing Following handled makes the arrival quiet (Home only); one the spoiler
    /// guard saw is recorded as watched; one watched on demand already is recorded only.
    #[test]
    fn following_and_seen_on_tv_make_arrivals_quiet() {
        with_db(|c| {
            let tm = series(c, 1, 1, "taskmaster", "Taskmaster", 2015)?;
            eps(c, 1, 20, 1..=3)?;
            let lib = lib(c)?;
            list(c, &lib, None, &tm, None)?;
            run(c, &lib, NOW)?;
            c.execute_batch(&format!(
                "INSERT INTO follow_rule(id,name,kind,spec,priority,created_at,updated_at) VALUES (1,'Taskmaster','show','{{}}',0,0,0);
                 INSERT INTO follow_match(rule_id,event_key,start,stop,title,state,seen_at) VALUES (1,'taskmaster#s20e04',{},{},'Taskmaster','aired',{});
                 INSERT INTO spoiler_seen(event_key,first_start,series_norm,season,episode,seen_at,how) VALUES ('taskmaster#s20e05',{},'taskmaster',20,5,{},'live');",
                NOW - 2 * DAY,
                NOW - 2 * DAY + 3600,
                NOW - 3 * DAY,
                NOW - DAY,
                NOW - DAY
            ))?;
            eps(c, 1, 20, [4, 5, 6])?;
            watch(c, 1, 20, 6, false)?;
            let r = run(c, &lib, NOW)?;
            assert_eq!((r.arrivals, r.quiet, r.announce.len()), (2, 2, 0));
            let snap = snapshot(c, &lib, &opts(NOW))?;
            assert_eq!(snap.series[0].new.iter().map(|e| (e.episode, e.quiet)).collect::<Vec<_>>(), [(4, Some(Quiet::Following))]);
            let seen = recent(c, &lib, NOW, RECENT_DAYS)?.into_iter().find(|a| a.episode == 5).unwrap();
            assert_eq!((seen.quiet, seen.cleared_how), (Some(Quiet::Seen), Some(Cleared::Watched)));
            Ok(())
        })
    }

    /// §8.9 (store side): only tracked Xtream copies are fetch candidates; an arrival the
    /// fallback found while `last_modified` stayed put marks the playlist unreliable.
    #[test]
    fn only_tracked_xtream_copies_are_fetched_and_a_static_last_modified_is_caught() {
        with_db(|c| {
            c.execute("INSERT INTO source(id,name,kind,created_at) VALUES (4,'Panel','xtream',0)", [])?;
            let tracked_ref = series(c, 1, 4, "xt:series:1", "Landman", 2024)?;
            series(c, 2, 4, "xt:series:2", "Other", 2020)?;
            c.execute("UPDATE series SET details_at = NULL, last_modified = 100 WHERE id IN (1, 2)", [])?;
            let lib = lib(c)?;
            list(c, &lib, None, &tracked_ref, None)?;
            let cands = fetch_candidates(c, &lib, &opts(NOW))?;
            assert_eq!(cands, [FetchCandidate { series: SeriesId(1), source: SourceId(4), details_at: None, unreliable: false }]);
            // the first fetch loads it: a silent baseline that remembers last_modified
            eps(c, 1, 1, 1..=3)?;
            c.execute("UPDATE series SET details_at = ?1 WHERE id = 1", [NOW - 8 * DAY])?;
            run(c, &lib, NOW - 8 * DAY)?;
            // a week later the fallback fetches it although last_modified never moved, and finds E4
            eps(c, 1, 1, [4])?;
            let input = CheckInput { diff: true, fallback: [SeriesId(1)].into(), ..Default::default() };
            let r = check(c, &lib, &opts(NOW), &input)?;
            assert_eq!((r.arrivals, r.unreliable.as_slice()), (1, [SourceId(4)].as_slice()));
            assert!(fetch_candidates(c, &lib, &opts(NOW))?[0].unreliable);
            Ok(())
        })
    }

    /// §3.5 and Q1: watched in any copy, dismissed (with Undo) or 30 days old leaves Home; a
    /// muted title keeps its ledger, so unmuting doesn't replay what arrived meanwhile.
    #[test]
    fn clearing_and_mute() {
        with_db(|c| {
            let s = series(c, 1, 1, "landman", "Landman", 2024)?;
            eps(c, 1, 1, 1..=4)?;
            let lib = lib(c)?;
            list(c, &lib, None, &s, None)?;
            run(c, &lib, NOW)?;
            eps(c, 1, 1, [5, 6, 7])?;
            let ids = run(c, &lib, NOW)?.announce;
            let open = |c: &Connection, now| -> Result<Vec<u32>> {
                Ok(snapshot(c, &lib, &opts(now))?.series.first().map(|s| s.new.iter().map(|e| e.episode).collect()).unwrap_or_default())
            };
            watch(c, 1, 1, 5, true)?;
            assert_eq!(open(c, NOW)?, [6, 7]);
            let undo_it = dismiss(c, &ids[1..2], NOW)?;
            assert_eq!(open(c, NOW)?, [7]);
            undo(c, &undo_it)?;
            assert_eq!(open(c, NOW)?, [6, 7]);
            assert!(open(c, NOW + 31 * DAY)?.is_empty(), "expired");
            mark_opened(c, &lib, &s, NOW)?;
            assert!(!snapshot(c, &lib, &opts(NOW))?.series[0].unopened());
            watchlists_opened(c, NOW + 1)?;
            assert!(!snapshot(c, &lib, &opts(NOW))?.list_dot);

            mute(c, &lib, &s, true, NOW)?;
            assert!(open(c, NOW)?.is_empty(), "muting takes the tile off Home");
            eps(c, 1, 1, [8])?;
            assert_eq!(run(c, &lib, NOW + DAY)?.arrivals, 0);
            mute(c, &lib, &s, false, NOW + DAY)?;
            assert_eq!(run(c, &lib, NOW + 2 * DAY)?.arrivals, 0, "unmuting doesn't replay");
            eps(c, 1, 1, [9])?;
            assert_eq!(run(c, &lib, NOW + 3 * DAY)?.arrivals, 1);
            Ok(())
        })
    }
}
