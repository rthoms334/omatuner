//! The copies of one title in "All playlists" (DESIGN §29.15, P2): which copy plays and what
//! each copy's state is ([`plan`]), and the user's actions on a stack card or a combined detail
//! page, written per copy by the recommendation's §3 table:
//!
//! | Action | Writes |
//! |---|---|
//! | ★ add | the copy that would play ([`favourite`]); nothing when a copy already is one |
//! | ★ remove | every copy, with Undo |
//! | ✓ mark watched | the copy that would play ([`watched`]) |
//! | ✓ unmark / forget | every copy's progress (episodes too for a series, [`forget_series`]), Undo restores the rows |
//! | Lock / unlock | every copy ([`lock`]) |
//! | Hide | every copy ([`hide`]) |
//! | Rename | copies without their own rename ([`rename`]); [`rename_conflicts`] lists the others to ask about |
//! | Same title as… | a join of two copies and their stacks ([`join`], P3) |
//! | Progress | the copy being played (the player saves it as before) |
//!
//! Every action returns an [`Undo`]: the rows of its table as they were, put back exactly by
//! [`undo`] (positions, timestamps and fingerprints included). Data stays stored per copy; a
//! stack card only combines it for display (`stacks::stack_marks`). Inside one playlist the
//! screens keep writing the one copy (`ui::screens::home::kit`).
//!
//! Which copy plays ([`plan`] → `stacks::play_order`): the per-title pin, then the latest
//! progress (for a series: its newest episode progress), then copies in trouble last — failed
//! in the last 24 h ([`FAILED_RECENTLY`]), on a playlist that refused a stream (403) in the last
//! 10 min ([`AT_LIMIT_RECENTLY`]: at its connection limit) or whose account expired — then the
//! audio language, the playlist order (or best quality), mkv / mp4, CAM / TS last. Locked
//! copies are left out unless the session is unlocked. The second copy of that order is the
//! fallback the player tries once (`player::CopyPlay`).

use std::collections::{BTreeMap, HashMap};

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::config::CopyOrder;
use crate::error::Result;
use crate::model::{
    AccountInfo, AccountStatus, Episode, FavListId, ItemKey, ItemKind, ItemOverride, ItemRef, Season, SeriesId, SourceId, StreamLocator, Timestamp,
    VodKind, VodSummary,
};
use crate::store::library::VodIndex;
use crate::store::sql::atomic;
use crate::store::stacks::{self, CopyFacts};
use crate::store::user;

/// A copy's own failure counts against it this long (it reads "failed 2 h ago").
pub const FAILED_RECENTLY: i64 = 86_400;
/// A refused stream (HTTP 401 / 403) on a playlist counts as "at its connection limit" for its
/// other copies this long.
pub const AT_LIMIT_RECENTLY: i64 = 600;

/// The durable reference of a movie or series.
pub fn vod_ref(v: &VodSummary) -> ItemRef {
    let kind = match v.kind {
        VodKind::Movie(_) => ItemKind::Movie,
        VodKind::Series(_) => ItemKind::Series,
    };
    ItemRef { source: v.source_id, kind, key: v.key.clone() }
}

/// Why a copy goes last in the play order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// It failed to play (`stream_failure`) at `at`.
    Failed { at: Timestamp },
    /// Its playlist refused a stream (401 / 403) at `at`: at its connection limit.
    AtLimit { at: Timestamp },
    /// Its playlist's account expired (or is banned / disabled).
    Expired,
}

/// Watch progress of a copy (a series: its newest episode progress).
#[derive(Debug, Clone, PartialEq)]
pub struct CopyProgress {
    pub position_s: f64,
    pub duration_s: Option<f64>,
    pub finished: bool,
    pub updated_at: Timestamp,
}

/// One copy of a title, as the detail page's Copies section and the player need it.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyState {
    /// Index into `VodIndex::movies` / `series`.
    pub item: u32,
    pub item_ref: ItemRef,
    /// Effective lock (its own or a category's): drawn disabled, the PIN on pick.
    pub locked: bool,
    /// "Always play from" this copy.
    pub pinned: bool,
    pub progress: Option<CopyProgress>,
    pub trouble: Option<Trouble>,
    /// Container extension (`mkv`, `mp4`) when the catalogue says (movies).
    pub container: Option<String>,
}

/// The status line of a copy's mini-card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyStatus {
    /// It is the copy Play starts.
    PlaysFirst,
    /// Locked while the session is: the PIN on pick.
    Locked,
    Failed {
        at: Timestamp,
    },
    AtLimit,
    Expired,
    /// Nothing to say.
    Plain,
}

/// The copies of a title and the order they play in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CopyPlan {
    /// Every copy in the stack's copy order (`Stacks::members`; the item alone when it has no
    /// other copies), locked ones included.
    pub copies: Vec<CopyState>,
    /// Items in play order (the first plays); locked copies are left out unless the session
    /// was unlocked.
    pub order: Vec<u32>,
}

impl CopyPlan {
    /// The copy Play starts (`None`: every copy is locked and the session isn't open).
    pub fn first(&self) -> Option<u32> {
        self.order.first().copied()
    }
    /// The copy tried once when the first fails before its first frame.
    pub fn fallback(&self) -> Option<u32> {
        stacks::fallback(&self.order, self.first()?)
    }
    /// The copy to offer after `failed` failed mid-playback ("Play from TV 2 at 41:20"): the
    /// first other copy in play order.
    pub fn instead_of(&self, failed: u32) -> Option<u32> {
        self.order.iter().copied().find(|c| *c != failed)
    }
    pub fn get(&self, item: u32) -> Option<&CopyState> {
        self.copies.iter().find(|c| c.item == item)
    }
    /// The copy whose progress a combined card shows: the most recently played.
    pub fn resume(&self) -> Option<&CopyState> {
        self.copies.iter().filter(|c| c.progress.is_some()).max_by_key(|c| c.progress.as_ref().map(|p| p.updated_at))
    }
    /// Every copy's reference (the combined writes).
    pub fn refs(&self) -> Vec<ItemRef> {
        self.copies.iter().map(|c| c.item_ref.clone()).collect()
    }
    /// Series copies in play order, with the page's copy first (DESIGN §29.16).
    /// Locked copies still contribute rows; playing them requires the PIN as usual.
    pub fn series_ids(&self, vod: &VodIndex, page: u32) -> Vec<SeriesId> {
        let mut ids = Vec::new();
        for item in std::iter::once(page).chain(self.order.iter().copied()).chain(self.copies.iter().map(|c| c.item)) {
            if let Some(VodKind::Series(id)) = vod.series.get(item as usize).map(|v| v.kind)
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
        ids
    }
    /// The mini-card status of `item`.
    pub fn status(&self, item: u32) -> CopyStatus {
        let Some(c) = self.get(item) else { return CopyStatus::Plain };
        if c.locked && !self.order.contains(&item) {
            return CopyStatus::Locked;
        }
        if self.first() == Some(item) {
            return CopyStatus::PlaysFirst;
        }
        match c.trouble {
            Some(Trouble::Failed { at }) => CopyStatus::Failed { at },
            Some(Trouble::AtLimit { .. }) => CopyStatus::AtLimit,
            Some(Trouble::Expired) => CopyStatus::Expired,
            None => CopyStatus::Plain,
        }
    }
}

/// The copies of `item`'s title (`movies` = movie or series stacks) with their state, and the
/// order they play in (module docs). A few indexed reads per copy: call it when a detail page
/// opens, Play is pressed or a stream failed — not per frame.
pub fn plan(conn: &Connection, vod: &VodIndex, movies: bool, item: u32, unlocked: bool, order: CopyOrder, now: Timestamp) -> Result<CopyPlan> {
    let items = if movies { &vod.movies } else { &vod.series };
    let st = vod.stacks(movies);
    if items.get(item as usize).is_none() {
        return Ok(CopyPlan::default());
    }
    let members = st.copies(item);
    let mut accounts: HashMap<SourceId, Option<Trouble>> = HashMap::new();
    let mut copies = Vec::with_capacity(members.len());
    for &m in &members {
        let v = &items[m as usize];
        let item_ref = vod_ref(v);
        let (progress, failed_at, container) = match v.kind {
            VodKind::Movie(id) => {
                let p = user::progress(conn, &item_ref)?.map(|p| CopyProgress {
                    position_s: p.position_s,
                    duration_s: p.duration_s,
                    finished: p.finished,
                    updated_at: p.updated_at,
                });
                let failed = conn
                    .prepare_cached("SELECT at FROM stream_failure WHERE source_id = ?1 AND item_key = ?2")?
                    .query_row(params![v.source_id, v.key.as_str()], |r| r.get::<_, Timestamp>(0))
                    .optional()?;
                let stream: Option<String> =
                    conn.prepare_cached("SELECT stream FROM movie WHERE id = ?1")?.query_row([id], |r| r.get(0)).optional()?;
                (p, failed, stream.and_then(|s| container_of(&s)))
            }
            VodKind::Series(id) => (series_progress(conn, id)?, series_failure(conn, id)?, None),
        };
        let account = match accounts.get(&v.source_id) {
            Some(t) => *t,
            None => {
                let t = playlist_trouble(conn, v.source_id, now)?;
                accounts.insert(v.source_id, t);
                t
            }
        };
        let own = failed_at.filter(|at| *at >= now - FAILED_RECENTLY).map(|at| Trouble::Failed { at });
        copies.push(CopyState { item: m, item_ref, locked: v.locked, pinned: st.meta(m).pinned, progress, trouble: own.or(account), container });
    }
    let order = play_order_of(&copies, |m| st.facts(m, items), unlocked, order);
    Ok(CopyPlan { copies, order })
}

/// `stacks::play_order` over the copies' states (`facts` = what the snapshot knows of a copy).
fn play_order_of(copies: &[CopyState], facts: impl Fn(u32) -> CopyFacts, unlocked: bool, order: CopyOrder) -> Vec<u32> {
    let facts: Vec<CopyFacts> = copies
        .iter()
        .map(|c| CopyFacts {
            last_played: c.progress.as_ref().map(|p| p.updated_at),
            trouble: c.trouble.is_some(),
            container_ok: c.container.as_deref().map(|e| matches!(e, "mkv" | "mp4")),
            locked: c.locked,
            ..facts(c.item)
        })
        .collect();
    stacks::play_order(&facts, unlocked, order)
}

/// The container extension of a stored stream locator (never the URL itself: only its last
/// path segment's extension is kept).
fn container_of(stream_json: &str) -> Option<String> {
    let ext = match serde_json::from_str::<StreamLocator>(stream_json).ok()? {
        StreamLocator::Xtream { ext, .. } => ext?,
        StreamLocator::Url { url } => {
            let path = url.split(['?', '#']).next()?;
            let last = path.rsplit('/').next()?;
            last.rsplit_once('.')?.1.to_owned()
        }
    };
    let ext = ext.trim().to_ascii_lowercase();
    const VIDEO: [&str; 13] = ["mkv", "mp4", "m4v", "avi", "ts", "m2ts", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "m3u8"];
    VIDEO.contains(&ext.as_str()).then_some(ext)
}

/// The newest episode progress of a series copy.
fn series_progress(conn: &Connection, id: SeriesId) -> Result<Option<CopyProgress>> {
    Ok(conn
        .prepare_cached(
            "SELECT p.position_s, p.duration_s, p.finished, p.updated_at FROM watch_progress p
             JOIN episode e ON e.source_id = p.source_id AND e.key = p.item_key
             WHERE p.kind = 'episode' AND e.series_id = ?1 ORDER BY p.updated_at DESC LIMIT 1",
        )?
        .query_row([id], |r| Ok(CopyProgress { position_s: r.get(0)?, duration_s: r.get(1)?, finished: r.get(2)?, updated_at: r.get(3)? }))
        .optional()?)
}

/// The newest failure of a series copy's episodes.
fn series_failure(conn: &Connection, id: SeriesId) -> Result<Option<Timestamp>> {
    Ok(conn
        .prepare_cached(
            "SELECT max(f.at) FROM stream_failure f JOIN episode e ON e.source_id = f.source_id AND e.key = f.item_key
             WHERE e.series_id = ?1",
        )?
        .query_row([id], |r| r.get(0))?)
}

/// Trouble of a whole playlist: an expired account, or a refused stream lately.
fn playlist_trouble(conn: &Connection, source: SourceId, now: Timestamp) -> Result<Option<Trouble>> {
    let account: Option<String> =
        conn.prepare_cached("SELECT account FROM source WHERE id = ?1")?.query_row([source], |r| r.get(0)).optional()?.flatten();
    if let Some(a) = account.and_then(|j| serde_json::from_str::<AccountInfo>(&j).ok())
        && (matches!(a.status, AccountStatus::Expired | AccountStatus::Banned | AccountStatus::Disabled) || a.exp_date.is_some_and(|e| e < now))
    {
        return Ok(Some(Trouble::Expired));
    }
    let refused: Option<Timestamp> = conn
        .prepare_cached("SELECT max(at) FROM stream_failure WHERE source_id = ?1 AND reason = 'http-forbidden' AND at >= ?2")?
        .query_row(params![source, now - AT_LIMIT_RECENTLY], |r| r.get(0))?;
    Ok(refused.map(|at| Trouble::AtLimit { at }))
}

/// The next episode of the union (DESIGN §29.16), using merged progress: resume the
/// newest unfinished episode, else advance past the newest watched one. The returned row
/// belongs to the first copy in `copies` that has those coordinates.
pub fn next_episode(conn: &Connection, copies: &[SeriesId]) -> Result<Option<(SeriesId, Episode)>> {
    if let [id] = copies {
        return Ok(crate::store::vod::next_episode(conn, *id)?.map(|e| (*id, e)));
    }
    let eps = episodes(conn, copies, None)?;
    let progress = episode_progress(conn, copies)?;
    let latest = |finished| {
        eps.iter()
            .enumerate()
            .filter_map(|(i, e)| progress.get(&(e.season, e.episode)).filter(|p| p.finished == finished).map(|p| (i, p.updated_at)))
            .max_by_key(|(_, at)| *at)
            .map(|(i, _)| i)
    };
    let next = latest(false)
        .map(|i| &eps[i])
        .or_else(|| latest(true).and_then(|i| eps[i + 1..].iter().find(|e| !progress.get(&(e.season, e.episode)).is_some_and(|p| p.finished))))
        .or_else(|| eps.iter().find(|e| e.season >= 1 && !progress.get(&(e.season, e.episode)).is_some_and(|p| p.finished)))
        .or_else(|| eps.iter().find(|e| e.season >= 1))
        .or(eps.first());
    Ok(next.map(|e| (e.series_id, e.clone())))
}

/// Episodes matched by (season, episode), in season order (DESIGN §29.16). The first
/// copy with a row supplies its metadata and durable reference; callers put the page first.
pub fn episodes(conn: &Connection, ids: &[SeriesId], season: Option<u32>) -> Result<Vec<Episode>> {
    if let [id] = ids {
        return crate::store::vod::episodes(conn, *id, season);
    }
    let mut rows = BTreeMap::new();
    for &id in ids {
        for e in crate::store::vod::episodes(conn, id, season)? {
            rows.entry((e.season, e.episode)).or_insert(e);
        }
    }
    Ok(rows.into_values().collect())
}

/// All season headers, including seasons supplied only as episodes. Combined counts are
/// the actual distinct rows, rather than one provider's count (DESIGN §29.16).
pub fn seasons(conn: &Connection, ids: &[SeriesId]) -> Result<Vec<Season>> {
    if let [id] = ids {
        return crate::store::vod::seasons(conn, *id);
    }
    let mut rows = BTreeMap::new();
    for &id in ids {
        for mut s in crate::store::vod::seasons(conn, id)? {
            s.episode_count = 0;
            rows.entry(s.number).or_insert(s);
        }
    }
    for e in episodes(conn, ids, None)? {
        let s = rows.entry(e.season).or_insert(Season {
            series_id: e.series_id,
            number: e.season,
            name: None,
            cover: None,
            air_date: None,
            overview: None,
            episode_count: 0,
        });
        s.episode_count += 1;
    }
    Ok(rows.into_values().collect())
}

/// The same episode (season and number) in another copy of its series, when that copy's
/// episodes are loaded (the fallback and "Play from TV 2" of an episode).
pub fn same_episode(conn: &Connection, series: SeriesId, season: u32, episode: u32) -> Result<Option<crate::model::EpisodeId>> {
    Ok(conn
        .prepare_cached("SELECT id FROM episode WHERE series_id = ?1 AND season = ?2 AND episode = ?3 ORDER BY id LIMIT 1")?
        .query_row(params![series, season, episode], |r| r.get(0))
        .optional()?)
}

// ---------------------------------------------------------------------------------------------
// Series: episodes matched across copies (P3)

/// The series copies among `ids` whose episodes aren't loaded yet: Xtream series load them
/// lazily (`JobKind::LoadSeriesDetails`); a playlist's series has them from its import.
pub fn unloaded_series(conn: &Connection, ids: &[SeriesId]) -> Result<Vec<SeriesId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.details_at IS NULL AND src.kind = 'xtream' AND NOT EXISTS (SELECT 1 FROM episode e WHERE e.series_id = s.id)
         FROM series s JOIN source src ON src.id = s.source_id WHERE s.id = ?1",
    )?;
    let mut out = Vec::new();
    for &id in ids {
        if stmt.query_row([id], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(false) {
            out.push(id);
        }
    }
    Ok(out)
}

/// The watch progress of the episodes of the series copies `ids`, matched by (season,
/// episode): a watched mark wins (any copy watched → watched), else the latest progress.
pub fn episode_progress(conn: &Connection, ids: &[SeriesId]) -> Result<HashMap<(u32, u32), crate::model::WatchProgress>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT e.season, e.episode, {} FROM watch_progress p JOIN episode e ON e.source_id = p.source_id AND e.key = p.item_key
         WHERE p.kind = 'episode' AND e.series_id = ?1",
        user::PROGRESS_COLS.split(", ").map(|c| format!("p.{c}")).collect::<Vec<_>>().join(", ")
    ))?;
    let mut out: HashMap<(u32, u32), crate::model::WatchProgress> = HashMap::new();
    for &id in ids {
        let rows = stmt.query_map([id], |r| {
            let at = (r.get::<_, u32>(0)?, r.get::<_, u32>(1)?);
            // the progress columns follow the two episode columns
            let p = user::progress_from_offset(r, 2)?;
            Ok((at, p))
        })?;
        for row in rows {
            let (at, p) = row?;
            match out.get(&at) {
                Some(have) if (have.finished, have.updated_at) >= (p.finished, p.updated_at) => {}
                _ => {
                    out.insert(at, p);
                }
            }
        }
    }
    Ok(out)
}

/// Mark the episodes `eps` (season, episode) of every series copy in `copies` watched, or
/// forget their progress (`on` false) — the series page's ✓ and "Mark season watched" on a
/// combined series. Episodes are matched by season and number (a copy that lacks one is
/// skipped). Returns the Undo (the rows as they were) and how many episodes it wrote.
pub fn mark_episodes(conn: &Connection, copies: &[SeriesId], eps: &[(u32, u32)], on: bool, now: Timestamp) -> Result<(Undo, usize)> {
    atomic(conn, |c| {
        let mut items: Vec<(ItemRef, String)> = Vec::new();
        let mut find = c.prepare_cached(
            "SELECT e.source_id, e.key, COALESCE(NULLIF(e.title, ''), 'Episode ' || e.episode) FROM episode e
             WHERE e.series_id = ?1 AND e.season = ?2 AND e.episode = ?3 ORDER BY e.id LIMIT 1",
        )?;
        for &s in copies {
            for &(season, number) in eps {
                if let Some((source, key, title)) = find
                    .query_row(params![s, season, number], |r| Ok((r.get::<_, SourceId>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
                    .optional()?
                {
                    let title = format!("S{season:02}E{number:02} · {title}");
                    items.push((ItemRef { source, kind: ItemKind::Episode, key: ItemKey(key) }, title));
                }
            }
        }
        let refs: Vec<ItemRef> = items.iter().map(|(r, _)| r.clone()).collect();
        let before = snapshot(c, Table::Progress, &refs)?;
        let mut del = c.prepare_cached("DELETE FROM watch_progress WHERE source_id = ?1 AND kind = 'episode' AND item_key = ?2")?;
        let mut up = c.prepare_cached(
            "INSERT INTO watch_progress(source_id, kind, item_key, position_s, finished, updated_at, title, fp)
             VALUES (?1, 'episode', ?2, 0, 1, ?3, ?4, COALESCE(?5, '{}'))
             ON CONFLICT(source_id, kind, item_key) DO UPDATE SET finished = 1, updated_at = excluded.updated_at,
               title = excluded.title, fp = COALESCE(?5, watch_progress.fp)",
        )?;
        for (r, title) in &items {
            if on {
                let fp = user::fingerprint_of(c, r)?;
                up.execute(params![r.source, r.key.as_str(), now, title, crate::store::sql::fp_opt(&fp)])?;
            } else {
                del.execute(params![r.source, r.key.as_str()])?;
            }
        }
        Ok((before, items.len()))
    })
}

// ---------------------------------------------------------------------------------------------
// Deleting a playlist: moving its favourites and progress to another playlist's copy (P3)

/// How a copy in another playlist was found to be the same title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedBy {
    Tmdb,
    /// Same match key and the same known year.
    KeyYear,
    /// The user combined them ("Same title as…").
    Joined,
}

/// One title (or episode) whose data can move to another playlist's copy.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveItem {
    pub from: ItemRef,
    pub to: ItemRef,
    /// "Dune (2021)", "Severance S01E03".
    pub title: String,
    /// The copy's own title when it reads differently ("Dune: Part One (2021)", a TMDB or
    /// joined match), else `None`.
    pub to_title: Option<String>,
    /// The movie or series it belongs to in the deleted playlist (an episode's series): the
    /// dialog doesn't name a title concealed while the session is locked.
    pub title_ref: ItemRef,
    pub matched: MatchedBy,
    /// Favourites lists holding it.
    pub lists: Vec<FavListId>,
    /// Watchlists whose entries follow this title (DESIGN §42 §4).
    pub watchlists: Vec<crate::model::WatchlistId>,
    /// Its progress row (position, watched) when the plan was made; [`move_titles`] moves the
    /// row as it is when it runs.
    pub progress: Option<crate::model::WatchProgress>,
}

/// Something [`move_plan`] can't move, with why.
#[derive(Debug, Clone, PartialEq)]
pub struct CantMove {
    /// "Dune (2021)", "1 channel favourite".
    pub what: String,
    /// "no copy in another playlist", "channels aren't matched across playlists".
    pub why: String,
    /// The movie or series it is about (`None` for channels): the dialog doesn't name a
    /// concealed one.
    pub title_ref: Option<ItemRef>,
}

/// What [`move_plan`] found for a playlist about to be deleted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MovePlan {
    pub items: Vec<MoveItem>,
    pub cannot: Vec<CantMove>,
    /// Series copies (in other playlists) whose episodes must load before their episodes'
    /// progress can move.
    pub waiting: Vec<SeriesId>,
    /// The episodes that wait for them ("Severance S01E03") with their series.
    pub pending: Vec<(String, ItemRef)>,
}

impl MovePlan {
    /// Anything to say about the user's data (the dialog shows its move section).
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.cannot.is_empty() && self.pending.is_empty()
    }
}

/// A movie or series row's matching facts.
#[derive(Debug, Clone, PartialEq)]
struct TitleRow {
    id: i64,
    source: SourceId,
    key: String,
    /// Display name (the user's rename, else the clean name).
    name: String,
    year: Option<i64>,
    hash: Option<i64>,
    tmdb: Option<i64>,
    /// An explicit origin ("US", "GB"): remakes of different origins never match.
    origin: Option<String>,
}

const TITLE_COLS: &str =
    "x.id, x.source_id, x.key, COALESCE(o.rename, x.clean_name), x.year, x.match_hash, NULLIF(CAST(x.tmdb_id AS INTEGER), 0), x.match_origin";

fn title_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<TitleRow> {
    Ok(TitleRow {
        id: r.get(0)?,
        source: r.get(1)?,
        key: r.get(2)?,
        name: r.get(3)?,
        year: r.get(4)?,
        hash: r.get(5)?,
        tmdb: r.get(6)?,
        origin: r.get(7)?,
    })
}

fn title_row(conn: &Connection, table: &str, source: SourceId, key: &str) -> Result<Option<TitleRow>> {
    let sql = format!(
        "SELECT {TITLE_COLS} FROM {table} x LEFT JOIN item_override o ON o.source_id = x.source_id AND o.kind = '{table}' AND o.item_key = x.key
         WHERE x.source_id = ?1 AND x.key = ?2"
    );
    Ok(conn.prepare_cached(&sql)?.query_row(params![source, key], title_from).optional()?)
}

/// The movies or series of the other playlists, read once per [`move_plan`] (a scan of the
/// table, instead of one per favourite / progress row) and indexed by TMDB id and match hash,
/// in the playlist order.
struct Candidates {
    rows: Vec<TitleRow>,
    by_tmdb: HashMap<i64, Vec<usize>>,
    by_hash: HashMap<i64, Vec<usize>>,
    /// (source, key) → row index, for join partners.
    by_key: HashMap<(SourceId, String), usize>,
    /// Copies split off by the user ("Not the same title").
    split: std::collections::HashSet<(SourceId, String)>,
}

impl Candidates {
    fn read(conn: &Connection, table: &str, from: SourceId) -> Result<Self> {
        let sql = format!(
            "SELECT {TITLE_COLS} FROM {table} x JOIN source s ON s.id = x.source_id
             LEFT JOIN item_override o ON o.source_id = x.source_id AND o.kind = '{table}' AND o.item_key = x.key
             WHERE x.source_id != ?1 AND (x.match_hash IS NOT NULL OR NULLIF(CAST(x.tmdb_id AS INTEGER), 0) IS NOT NULL
                                          OR EXISTS (SELECT 1 FROM title_link t WHERE t.source_id = x.source_id AND t.kind = '{table}'
                                                       AND t.item_key = x.key AND t.action = 'join'))
             ORDER BY s.enabled DESC, s.sort_index, s.id, x.provider_order, x.id"
        );
        let mut c =
            Candidates { rows: Vec::new(), by_tmdb: HashMap::new(), by_hash: HashMap::new(), by_key: HashMap::new(), split: Default::default() };
        let mut stmt = conn.prepare(&sql)?;
        for row in stmt.query_map([from], title_from)? {
            let row = row?;
            let i = c.rows.len();
            if let Some(t) = row.tmdb {
                c.by_tmdb.entry(t).or_default().push(i);
            }
            if let Some(h) = row.hash {
                c.by_hash.entry(h).or_default().push(i);
            }
            c.by_key.insert((row.source, row.key.clone()), i);
            c.rows.push(row);
        }
        let mut stmt = conn.prepare("SELECT source_id, item_key FROM title_link WHERE kind = ?1 AND action = 'split'")?;
        for r in stmt.query_map([table], |r| Ok((r.get::<_, SourceId>(0)?, r.get::<_, String>(1)?)))? {
            c.split.insert(r?);
        }
        Ok(c)
    }

    /// The copy of `t` (a title of the deleted playlist) its data moves to — matched by TMDB
    /// id, by match hash (key and year; never against a different TMDB id or another explicit
    /// origin), or joined by the user; never a copy either side split off ("Not the same
    /// title"). The first in the playlist order.
    fn other_copy_of(&self, conn: &Connection, kind: ItemKind, t: &TitleRow) -> Result<Option<(TitleRow, MatchedBy)>> {
        if self.split.contains(&(t.source, t.key.clone())) {
            return Ok(None);
        }
        let me = ItemRef { source: t.source, kind, key: ItemKey(t.key.clone()) };
        let joined: Vec<usize> = crate::store::titles::group_members(conn, std::slice::from_ref(&me))?
            .into_iter()
            .filter(|r| r.source != t.source)
            .filter_map(|r| self.by_key.get(&(r.source, r.key.0)).copied())
            .collect();
        let by_tmdb = t.tmdb.and_then(|x| self.by_tmdb.get(&x)).map(Vec::as_slice).unwrap_or_default();
        let by_hash = t.hash.and_then(|x| self.by_hash.get(&x)).map(Vec::as_slice).unwrap_or_default();
        let compatible = |o: &TitleRow| {
            (t.tmdb.is_none() || o.tmdb.is_none() || t.tmdb == o.tmdb) && (t.origin.is_none() || o.origin.is_none() || t.origin == o.origin)
        };
        let mut found: Vec<usize> =
            by_tmdb.iter().chain(joined.iter()).copied().chain(by_hash.iter().copied().filter(|i| compatible(&self.rows[*i]))).collect();
        found.sort_unstable();
        found.dedup();
        let Some(i) = found.into_iter().find(|i| !self.split.contains(&(self.rows[*i].source, self.rows[*i].key.clone()))) else {
            return Ok(None);
        };
        let o = self.rows[i].clone();
        let how = if t.tmdb.is_some() && o.tmdb == t.tmdb {
            MatchedBy::Tmdb
        } else if joined.contains(&i) {
            MatchedBy::Joined
        } else {
            MatchedBy::KeyYear
        };
        Ok(Some((o, how)))
    }
}

/// A series of the deleted playlist and its other copy (if any).
type SeriesCopy = (TitleRow, Option<(TitleRow, MatchedBy)>);

fn with_year(name: &str, year: Option<i64>) -> String {
    match year {
        Some(y) => format!("{name} ({y})"),
        None => name.to_owned(),
    }
}

/// What moving the favourites and progress of playlist `from` would do before it is deleted
/// (the delete dialog lists it; nothing is written). Movies and series move to their copy in
/// another playlist — matched by TMDB id, or by match key and the same known year, or joined
/// by the user — never to a copy either side split off. Episode progress moves to the same
/// season and episode of the series' other copy, once that copy's episodes are loaded
/// (`waiting` names the copies to load first). Channels never move (they aren't matched
/// across playlists).
pub fn move_plan(conn: &Connection, from: SourceId) -> Result<MovePlan> {
    let mut plan = MovePlan::default();
    // the user data of the playlist: favourites (any list) and progress, by item
    let mut favs: Vec<(ItemRef, FavListId)> = Vec::new();
    let mut stmt = conn.prepare_cached("SELECT kind, item_key, list_id FROM fav_item WHERE source_id = ?1 ORDER BY list_id, position")?;
    for row in stmt.query_map([from], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, FavListId>(2)?)))? {
        let (kind, key, list) = row?;
        let Some(kind) = ItemKind::parse(&kind) else { continue };
        favs.push((ItemRef { source: from, kind, key: ItemKey(key) }, list));
    }
    let mut progress: Vec<crate::model::WatchProgress> = Vec::new();
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {} FROM watch_progress WHERE source_id = ?1 ORDER BY updated_at DESC", user::PROGRESS_COLS))?;
    for p in stmt.query_map([from], user::progress_from_row)? {
        progress.push(p?);
    }
    let channels = favs.iter().filter(|(r, _)| r.kind == ItemKind::Channel).count();
    if channels > 0 {
        let what = if channels == 1 { "1 channel favourite".to_owned() } else { format!("{channels} channel favourites") };
        plan.cannot.push(CantMove { what, why: "channels aren't matched across playlists".to_owned(), title_ref: None });
    }
    let pinned = conn
        .prepare_cached("SELECT title,source_id,kind,item_key FROM watchlist_item WHERE source_id=?1 AND any_copy=0")?
        .query_map([from], |r| {
            Ok(CantMove {
                what: r.get(0)?,
                why: "watchlist entry is pinned to this copy".into(),
                title_ref: Some(ItemRef {
                    source: r.get(1)?,
                    kind: crate::store::sql::kind_from_sql(&r.get::<_, String>(2)?)?,
                    key: ItemKey(r.get(3)?),
                }),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    plan.cannot.extend(pinned);
    let watchlist_refs: Vec<ItemRef> = conn
        .prepare_cached("SELECT DISTINCT source_id,kind,item_key FROM watchlist_item WHERE source_id=?1 AND any_copy=1 ORDER BY list_id,position,id")?
        .query_map([from], |r| {
            Ok(ItemRef { source: r.get(0)?, kind: crate::store::sql::kind_from_sql(&r.get::<_, String>(1)?)?, key: ItemKey(r.get(2)?) })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // every movie / series / episode with data, in a stable order
    let mut refs: Vec<ItemRef> = Vec::new();
    for r in favs.iter().map(|(r, _)| r).chain(watchlist_refs.iter()).chain(progress.iter().map(|p| &p.item)) {
        if r.kind != ItemKind::Channel && !refs.contains(r) {
            refs.push(r.clone());
        }
    }
    // the other playlists' titles, read once per table when needed
    let (mut movies, mut series): (Option<Candidates>, Option<Candidates>) = (None, None);
    // a series' row and its other copy, once per series (its episodes share them)
    let mut series_copy: HashMap<i64, Option<SeriesCopy>> = HashMap::new();
    let mut waiting_titles: HashMap<SeriesId, Vec<(String, ItemRef)>> = HashMap::new();
    let gone = |prog: Option<crate::model::WatchProgress>, r: &ItemRef, title_ref: Option<ItemRef>| CantMove {
        what: prog.map(|p| p.title).filter(|t| !t.is_empty()).unwrap_or_else(|| r.key.to_string()),
        why: "no longer in the playlist".to_owned(),
        title_ref,
    };
    let differs = |a: &TitleRow, b: &TitleRow| (b.name != a.name || b.year != a.year).then(|| with_year(&b.name, b.year));
    for r in refs {
        let lists: Vec<FavListId> = favs.iter().filter(|(f, _)| *f == r).map(|(_, l)| *l).collect();
        let watchlists: Vec<crate::model::WatchlistId> = conn
            .prepare_cached("SELECT DISTINCT list_id FROM watchlist_item WHERE source_id=?1 AND kind=?2 AND item_key=?3 AND any_copy=1")?
            .query_map(params![r.source, r.kind.as_str(), r.key.as_str()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let prog = progress.iter().find(|p| p.item == r).cloned();
        match r.kind {
            ItemKind::Movie | ItemKind::Series => {
                let (table, cands) = if r.kind == ItemKind::Movie { ("movie", &mut movies) } else { ("series", &mut series) };
                let Some(t) = title_row(conn, table, from, r.key.as_str())? else {
                    plan.cannot.push(gone(prog, &r, Some(r.clone())));
                    continue;
                };
                if cands.is_none() {
                    *cands = Some(Candidates::read(conn, table, from)?);
                }
                let title = with_year(&t.name, t.year);
                match cands.as_ref().map(|c| c.other_copy_of(conn, r.kind, &t)).transpose()?.flatten() {
                    Some((o, matched)) => plan.items.push(MoveItem {
                        to: ItemRef { source: o.source, kind: r.kind, key: ItemKey(o.key.clone()) },
                        to_title: differs(&t, &o),
                        title_ref: r.clone(),
                        from: r,
                        title,
                        matched,
                        lists,
                        watchlists,
                        progress: prog,
                    }),
                    None => plan.cannot.push(CantMove { what: title, why: "no copy in another playlist".to_owned(), title_ref: Some(r) }),
                }
            }
            ItemKind::Episode => {
                let ep: Option<(i64, u32, u32)> = conn
                    .prepare_cached("SELECT series_id, season, episode FROM episode WHERE source_id = ?1 AND key = ?2")?
                    .query_row(params![from, r.key.as_str()], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))
                    .optional()?;
                let Some((series_id, season, number)) = ep else {
                    plan.cannot.push(gone(prog, &r, None));
                    continue;
                };
                if let std::collections::hash_map::Entry::Vacant(slot) = series_copy.entry(series_id) {
                    let key: Option<String> =
                        conn.prepare_cached("SELECT key FROM series WHERE id = ?1")?.query_row([series_id], |x| x.get(0)).optional()?;
                    let found = match key.map(|k| title_row(conn, "series", from, &k)).transpose()?.flatten() {
                        Some(t) => {
                            if series.is_none() {
                                series = Some(Candidates::read(conn, "series", from)?);
                            }
                            let other = series.as_ref().map(|c| c.other_copy_of(conn, ItemKind::Series, &t)).transpose()?.flatten();
                            Some((t, other))
                        }
                        None => None,
                    };
                    slot.insert(found);
                }
                let Some((t, other)) = series_copy.get(&series_id).cloned().flatten() else {
                    plan.cannot.push(gone(prog, &r, None));
                    continue;
                };
                let title_ref = ItemRef { source: from, kind: ItemKind::Series, key: ItemKey(t.key.clone()) };
                let title = format!("{} S{season:02}E{number:02}", t.name);
                let Some((o, matched)) = other else {
                    plan.cannot.push(CantMove { what: title, why: "no copy in another playlist".to_owned(), title_ref: Some(title_ref) });
                    continue;
                };
                let other = SeriesId(o.id);
                if !unloaded_series(conn, &[other])?.is_empty() {
                    waiting_titles.entry(other).or_default().push((title, title_ref));
                    continue;
                }
                match same_episode(conn, other, season, number)? {
                    Some(e) => {
                        let key: String = conn.query_row("SELECT key FROM episode WHERE id = ?1", [e], |x| x.get(0))?;
                        plan.items.push(MoveItem {
                            from: r,
                            to: ItemRef { source: o.source, kind: ItemKind::Episode, key: ItemKey(key) },
                            to_title: differs(&t, &o).map(|n| format!("{n} S{season:02}E{number:02}")),
                            title_ref,
                            title,
                            matched,
                            lists,
                            watchlists,
                            progress: prog,
                        });
                    }
                    None => {
                        plan.cannot.push(CantMove { what: title, why: "that episode isn't in the other copy".to_owned(), title_ref: Some(title_ref) })
                    }
                }
            }
            ItemKind::Channel => {}
        }
    }
    let mut waiting: Vec<SeriesId> = waiting_titles.keys().copied().collect();
    waiting.sort_unstable();
    plan.pending = waiting.iter().flat_map(|s| waiting_titles.remove(s).unwrap_or_default()).collect();
    plan.waiting = waiting;
    Ok(plan)
}

/// What [`move_titles`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Moved {
    pub favourites: usize,
    pub watchlists: usize,
    pub progress: usize,
}

/// Copy the favourites and progress of `items` to their other copies (before their playlist
/// is deleted, which takes the originals with it): a favourite joins the same list at the same
/// place (unless the copy is already there); progress replaces the copy's own only when it is
/// newer (or the copy has none).
pub fn move_titles(conn: &Connection, items: &[MoveItem]) -> Result<Moved> {
    atomic(conn, |c| {
        let mut moved = Moved::default();
        for m in items {
            moved.watchlists += c.execute("UPDATE watchlist_item SET source_id=?3,item_key=?4,fp=?5,orphaned_since=NULL WHERE source_id=?1 AND kind=?6 AND item_key=?2 AND any_copy=1",params![m.from.source,m.from.key.as_str(),m.to.source,m.to.key.as_str(),crate::store::sql::fp_to_json(&user::fingerprint_of(c,&m.to)?),m.from.kind.as_str()])?;
            for &list in &m.lists {
                let fp = crate::store::sql::fp_opt(&user::fingerprint_of(c, &m.to)?);
                moved.favourites += c.execute(
                    "INSERT INTO fav_item(list_id, source_id, kind, item_key, position, added_at, fp)
                     SELECT ?1, ?2, ?3, ?4, f.position, f.added_at, COALESCE(?5, '{}') FROM fav_item f
                      WHERE f.list_id = ?1 AND f.source_id = ?6 AND f.kind = ?3 AND f.item_key = ?7
                     ON CONFLICT(list_id, source_id, kind, item_key) DO NOTHING",
                    params![list, m.to.source, m.to.kind.as_str(), m.to.key.as_str(), fp, m.from.source, m.from.key.as_str()],
                )?;
            }
            if m.progress.is_some() {
                // the row as it is now (playback may have saved progress since the plan)
                let fp = crate::store::sql::fp_opt(&user::fingerprint_of(c, &m.to)?);
                moved.progress += c.execute(
                    "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, finished, updated_at, title, fp)
                     SELECT ?1, ?2, ?3, p.position_s, p.duration_s, p.finished, p.updated_at, p.title, COALESCE(?4, '{}')
                       FROM watch_progress p WHERE p.source_id = ?5 AND p.kind = ?2 AND p.item_key = ?6
                     ON CONFLICT(source_id, kind, item_key) DO UPDATE SET position_s = excluded.position_s, duration_s = excluded.duration_s,
                       finished = excluded.finished, updated_at = excluded.updated_at, title = excluded.title
                     WHERE excluded.updated_at > watch_progress.updated_at",
                    params![m.to.source, m.to.kind.as_str(), m.to.key.as_str(), fp, m.from.source, m.from.key.as_str()],
                )?;
            }
        }
        Ok(moved)
    })
}

// ---------------------------------------------------------------------------------------------
// Combined writes

/// The user-data table an [`Undo`] restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Favourites(FavListId),
    Progress,
    Overrides,
    /// `title_link`: splits and pins.
    Links,
}

impl Table {
    fn name(self) -> &'static str {
        match self {
            Table::Favourites(_) => "fav_item",
            Table::Progress => "watch_progress",
            Table::Overrides => "item_override",
            Table::Links => "title_link",
        }
    }
}

/// The rows a combined write touched, as they were before it: [`undo`] deletes the rows of
/// those copies and puts these back. Write it with [`Undo::scope`].
#[derive(Debug, Clone, PartialEq)]
pub struct Undo {
    table: Table,
    refs: Vec<ItemRef>,
    cols: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Undo {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Undo { table: Table::Progress, refs: Vec::new(), cols: Vec::new(), rows: Vec::new() }
    }

    /// The data scope to notify when it is written (so the UI reloads what it shows).
    pub fn scope(&self) -> crate::msg::DataScope {
        match self.table {
            Table::Favourites(_) => crate::msg::DataScope::Favourites,
            Table::Progress => crate::msg::DataScope::Progress,
            Table::Overrides | Table::Links => crate::msg::DataScope::Overrides,
        }
    }
}

fn ref_filter(table: Table) -> &'static str {
    match table {
        Table::Favourites(_) => "list_id = ?4 AND source_id = ?1 AND kind = ?2 AND item_key = ?3",
        _ => "source_id = ?1 AND kind = ?2 AND item_key = ?3",
    }
}

fn ref_params(table: Table, r: &ItemRef) -> Vec<Value> {
    let mut v = vec![Value::Integer(r.source.0), Value::Text(r.kind.as_str().to_owned()), Value::Text(r.key.0.clone())];
    if let Table::Favourites(list) = table {
        v.push(Value::Integer(list.0));
    }
    v
}

/// The rows of `refs` in `table` before a write.
fn snapshot(conn: &Connection, table: Table, refs: &[ItemRef]) -> Result<Undo> {
    let mut stmt = conn.prepare_cached(&format!("SELECT * FROM {} WHERE {}", table.name(), ref_filter(table)))?;
    let cols: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut rows = Vec::new();
    for r in refs {
        let mut q = stmt.query(params_from_iter(ref_params(table, r)))?;
        while let Some(row) = q.next()? {
            rows.push((0..cols.len()).map(|i| row.get::<_, Value>(i)).collect::<rusqlite::Result<Vec<_>>>()?);
        }
    }
    Ok(Undo { table, refs: refs.to_vec(), cols, rows })
}

/// How many combined actions can still be undone.
pub const UNDO_KEEP: usize = 8;

/// The last [`UNDO_KEEP`] combined actions' [`Undo`]s, by number (a toast's `Action::Undo(n)`).
/// Owned by the app's `Services`, shared with the store writer that fills it.
#[derive(Debug, Clone, Default)]
pub struct UndoStash(std::sync::Arc<std::sync::Mutex<Stashed>>);

/// The last number handed out, and the kept actions by number.
type Stashed = (u16, std::collections::VecDeque<(u16, Undo)>);

impl UndoStash {
    /// Keep `u`; returns its number.
    pub fn stash(&self, u: Undo) -> u16 {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        g.0 = g.0.wrapping_add(1).max(1);
        let n = g.0;
        g.1.push_back((n, u));
        while g.1.len() > UNDO_KEEP {
            g.1.pop_front();
        }
        n
    }

    /// The rows of Undo number `n` (once).
    pub fn take(&self, n: u16) -> Option<Undo> {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let at = g.1.iter().position(|(k, _)| *k == n)?;
        g.1.remove(at).map(|(_, u)| u)
    }
}

/// Put back the rows a combined write found (module docs).
pub fn undo(conn: &Connection, u: &Undo) -> Result<()> {
    atomic(conn, |c| {
        let mut del = c.prepare_cached(&format!("DELETE FROM {} WHERE {}", u.table.name(), ref_filter(u.table)))?;
        for r in &u.refs {
            del.execute(params_from_iter(ref_params(u.table, r)))?;
        }
        if !u.rows.is_empty() {
            let marks = (1..=u.cols.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
            let mut ins = c.prepare_cached(&format!("INSERT INTO {}({}) VALUES ({marks})", u.table.name(), u.cols.join(", ")))?;
            for row in &u.rows {
                ins.execute(params_from_iter(row.iter()))?;
            }
        }
        Ok(())
    })
}

/// ★ on a combined title: add puts the copy that would play (`plays`) in `list` — unless a
/// copy already is there; remove takes every copy out of it.
pub fn favourite(conn: &Connection, list: FavListId, copies: &[ItemRef], plays: &ItemRef, on: bool, now: Timestamp) -> Result<Undo> {
    atomic(conn, |c| {
        let before = snapshot(c, Table::Favourites(list), copies)?;
        if on {
            if before.rows.is_empty() {
                user::set_favourite(c, list, plays, true, now)?;
            }
        } else {
            for r in copies {
                user::set_favourite(c, list, r, false, now)?;
            }
        }
        Ok(before)
    })
}

/// ✓ on a combined movie: mark puts the copy that would play (`plays`) as watched; unmark
/// ("forget", remove from Continue watching) deletes every copy's progress.
pub fn watched(conn: &Connection, copies: &[ItemRef], plays: (&ItemRef, &str), on: bool, now: Timestamp) -> Result<Undo> {
    atomic(conn, |c| {
        let before = snapshot(c, Table::Progress, copies)?;
        if on {
            let fp = user::fingerprint_of(c, plays.0)?;
            c.prepare_cached(
                "INSERT INTO watch_progress(source_id, kind, item_key, position_s, finished, updated_at, title, fp)
                 VALUES (?1, ?2, ?3, 0, 1, ?4, ?5, COALESCE(?6, '{}'))
                 ON CONFLICT(source_id, kind, item_key) DO UPDATE SET finished = 1, updated_at = excluded.updated_at,
                   title = excluded.title, fp = COALESCE(?6, watch_progress.fp)",
            )?
            .execute(params![
                plays.0.source,
                plays.0.kind.as_str(),
                plays.0.key.as_str(),
                now,
                plays.1,
                crate::store::sql::fp_opt(&fp)
            ])?;
        } else {
            let mut del = c.prepare_cached("DELETE FROM watch_progress WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3")?;
            for r in copies {
                del.execute(params![r.source, r.kind.as_str(), r.key.as_str()])?;
            }
        }
        Ok(before)
    })
}

/// Mark an explicit set of film/episode references, preserving their progress for Undo.
pub fn watched_items(conn: &Connection, items: &[(ItemRef, String)], on: bool, now: Timestamp) -> Result<Undo> {
    atomic(conn, |c| {
        let refs = items.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>();
        let before = snapshot(c, Table::Progress, &refs)?;
        for (item, title) in items {
            watched(c, std::slice::from_ref(item), (item, title), on, now)?;
        }
        Ok(before)
    })
}

/// Whether any episode of these series copies appears in Continue watching. Unlike the
/// merged episode progress, this also sees unfinished copies of an episode watched elsewhere.
pub fn series_in_continue_watching(conn: &Connection, copies: &[SeriesId]) -> Result<bool> {
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM watch_progress p JOIN episode e ON e.source_id = p.source_id AND e.key = p.item_key
         WHERE p.kind = 'episode' AND e.series_id = ?1 AND p.finished = 0 AND p.position_s > 0 AND p.orphaned_since IS NULL)",
    )?;
    for s in copies {
        if stmt.query_row([s], |r| r.get::<_, bool>(0))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// "Forget" a combined series (remove it from Continue watching): the progress of every
/// episode of every copy.
pub fn forget_series(conn: &Connection, copies: &[SeriesId]) -> Result<Undo> {
    atomic(conn, |c| {
        let mut refs = Vec::new();
        let mut stmt = c.prepare_cached(
            "SELECT p.source_id, p.item_key FROM watch_progress p JOIN episode e ON e.source_id = p.source_id AND e.key = p.item_key
             WHERE p.kind = 'episode' AND e.series_id = ?1",
        )?;
        for s in copies {
            for r in stmt.query_map([s], |r| Ok(ItemRef { source: r.get(0)?, kind: ItemKind::Episode, key: ItemKey(r.get(1)?) }))? {
                refs.push(r?);
            }
        }
        let before = snapshot(c, Table::Progress, &refs)?;
        let mut del = c.prepare_cached("DELETE FROM watch_progress WHERE source_id = ?1 AND kind = 'episode' AND item_key = ?2")?;
        for r in &refs {
            del.execute(params![r.source, r.key.as_str()])?;
        }
        Ok(before)
    })
}

/// Change every copy's own override with `f` (read in the write: fields this action doesn't
/// know about survive).
fn edit_overrides(conn: &Connection, copies: &[ItemRef], mut f: impl FnMut(&ItemRef, &mut ItemOverride)) -> Result<Undo> {
    atomic(conn, |c| {
        let before = snapshot(c, Table::Overrides, copies)?;
        for r in copies {
            let mut ov = user::item_override(c, r)?.unwrap_or_default();
            f(r, &mut ov);
            user::set_item_override(c, r, &ov)?;
        }
        Ok(before)
    })
}

/// Lock / unlock every copy (a category's lock stays: the caller says so).
pub fn lock(conn: &Connection, copies: &[ItemRef], on: bool) -> Result<Undo> {
    edit_overrides(conn, copies, |_, ov| ov.locked = on)
}

/// Hide every copy (the toast names their playlists).
pub fn hide(conn: &Connection, copies: &[ItemRef]) -> Result<Undo> {
    edit_overrides(conn, copies, |_, ov| ov.hidden = true)
}

/// The copies that have a rename of their own other than `name` (the UI asks before
/// [`rename`] overwrites them), with that rename.
pub fn rename_conflicts(conn: &Connection, copies: &[ItemRef], name: &str) -> Result<Vec<(ItemRef, String)>> {
    let mut out = Vec::new();
    for r in copies {
        if let Some(own) = user::item_override(conn, r)?.and_then(|o| o.rename)
            && own != name
        {
            out.push((r.clone(), own));
        }
    }
    Ok(out)
}

/// "Not the same title" (`on`) or its Undo from Settings ▸ Hidden: the copy leaves its stack
/// for good (`titles::set_split`; the library rebuilds on `DataScope::Overrides`).
pub fn split(conn: &Connection, item: &ItemRef, on: bool, now: Timestamp) -> Result<Undo> {
    atomic(conn, |c| {
        let before = snapshot(c, Table::Links, std::slice::from_ref(item))?;
        crate::store::titles::set_split(c, item, on, now)?;
        Ok(before)
    })
}

/// "Same title as…" (P3): `item` and `other` stack together from now on, with the stacks
/// they are in (`titles::join`). The Undo covers every copy of both join groups.
pub fn join(conn: &Connection, item: &ItemRef, other: &ItemRef, now: Timestamp) -> Result<Undo> {
    atomic(conn, |c| {
        let refs = crate::store::titles::group_members(c, &[item.clone(), other.clone()])?;
        let before = snapshot(c, Table::Links, &refs)?;
        crate::store::titles::join(c, item, other, now)?;
        Ok(before)
    })
}

/// "Always play from …" on `item` (one pin per title: the other `copies` lose theirs), or off.
pub fn pin(conn: &Connection, item: &ItemRef, copies: &[ItemRef], on: bool, now: Timestamp) -> Result<Undo> {
    atomic(conn, |c| {
        let before = snapshot(c, Table::Links, copies)?;
        crate::store::titles::set_pin(c, item, copies, on, now)?;
        Ok(before)
    })
}

/// Rename a combined title: every copy without its own rename gets `name`; copies with one
/// keep it unless `overwrite`. An empty `name` clears the renames it may change.
pub fn rename(conn: &Connection, copies: &[ItemRef], name: &str, overwrite: bool) -> Result<Undo> {
    let name = name.trim();
    let new = (!name.is_empty()).then(|| name.to_owned());
    edit_overrides(conn, copies, |_, ov| {
        if ov.rename.is_none() || overwrite {
            ov.rename = new.clone();
        }
    })
}

// ------------------------------------------------------------------ channel copies (B5)

/// The same live channel in another playlist (P3's "Also on" rule, B5): Live "Also on", the
/// multi-view planner, Following's channel choice, Up next's live slots, Guide fixes' "Copy has
/// guide", MCP `details.copies`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelCopy {
    pub id: crate::model::ChannelId,
    pub source: SourceId,
    pub quality: Option<crate::model::Quality>,
    pub locked: bool,
    pub explicit: bool,
}

/// The copies of `ch` in `lib`: same `family` (normalised name, quality words dropped), another
/// playlist, not hidden, not unsupported, and no tvg-id veto (a copy with another tvg-id than
/// `ch`'s is another channel — shared or wrong tvg-ids never make copies, and "E4 +1" is never a
/// copy of "E4"). In playlist order, the best quality first inside one, then provider order.
/// `tvg` reads the tvg-ids of the ids it is given; `None` (no frame budget now) → `None`.
pub fn channel_copies_with(
    lib: &crate::store::library::Library,
    ch: &crate::model::ChannelSummary,
    tvg: impl FnOnce(&[crate::model::ChannelId]) -> Option<HashMap<crate::model::ChannelId, String>>,
) -> Option<Vec<ChannelCopy>> {
    let live = &lib.live;
    let mut alts: Vec<&crate::model::ChannelSummary> = live
        .channels
        .iter()
        .filter(|c| c.family == ch.family && c.source_id != ch.source_id && !c.unsupported && !live.hidden.contains(&c.id))
        .collect();
    if alts.is_empty() {
        return Some(Vec::new());
    }
    let mut ids: Vec<crate::model::ChannelId> = alts.iter().map(|c| c.id).collect();
    ids.push(ch.id);
    let tvg = tvg(&ids)?;
    let id_of = |c: crate::model::ChannelId| tvg.get(&c).map(|t| t.trim()).filter(|t| !t.is_empty());
    if let Some(own) = id_of(ch.id) {
        alts.retain(|c| id_of(c.id).is_none_or(|t| t.eq_ignore_ascii_case(own)));
    }
    let rank = |s: SourceId| lib.sources.iter().position(|m| m.id == s).unwrap_or(usize::MAX);
    alts.sort_by_key(|c| (rank(c.source_id), std::cmp::Reverse(c.quality.map(|q| q.class as u8)), c.provider_order));
    Some(
        alts.into_iter()
            .map(|c| ChannelCopy {
                id: c.id,
                source: c.source_id,
                quality: c.quality.map(|q| q.class),
                locked: c.locked,
                explicit: lib.explicit.channels.contains(&c.id),
            })
            .collect(),
    )
}

/// [`channel_copies_with`] reading the tvg-ids from `conn` (off the UI thread: the multi-view
/// controller's zap substitution, Following, the agent).
pub fn channel_copies(lib: &crate::store::library::Library, conn: &Connection, ch: crate::model::ChannelId) -> Result<Vec<ChannelCopy>> {
    let Some(summary) = lib.live.get(ch) else { return Ok(Vec::new()) };
    let mut err = None;
    let copies = channel_copies_with(lib, summary, |ids| tvg_ids(conn, ids).map_err(|e| err = Some(e)).ok());
    match err {
        Some(e) => Err(e),
        None => Ok(copies.unwrap_or_default()),
    }
}

/// The tvg-ids of `ids` (one indexed read each).
pub fn tvg_ids(conn: &Connection, ids: &[crate::model::ChannelId]) -> Result<HashMap<crate::model::ChannelId, String>> {
    let mut stmt = conn.prepare_cached("SELECT tvg_id FROM channel WHERE id = ?1")?;
    let mut out = HashMap::new();
    for id in ids {
        if let Some(t) = stmt.query_row([id.0], |r| r.get::<_, Option<String>>(0)).optional()?.flatten() {
            out.insert(*id, t);
        }
    }
    Ok(out)
}

/// One channel to rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankCand {
    pub id: crate::model::ChannelId,
    pub source: SourceId,
    pub quality: Option<crate::model::Quality>,
    pub locked: bool,
    pub provider_order: i32,
}

impl From<&crate::model::ChannelSummary> for RankCand {
    fn from(c: &crate::model::ChannelSummary) -> Self {
        Self { id: c.id, source: c.source_id, quality: c.quality.map(|q| q.class), locked: c.locked, provider_order: c.provider_order }
    }
}

/// What the ranking weighs (B5).
#[derive(Debug, Clone, Default)]
pub struct RankCtx {
    /// A rule's / row's pinned channel.
    pub pinned: Option<crate::model::ChannelId>,
    /// The parental session is locked (unlocked channels first).
    pub session_locked: bool,
    /// Channels with trouble in the last 24 h (`skip_on_zap` / `stream_failure`).
    pub trouble: std::collections::HashSet<crate::model::ChannelId>,
    /// (list position, position in the list) of favourite channels.
    pub favourites: HashMap<crate::model::ChannelId, (usize, usize)>,
    /// Best quality wanted (e.g. a Following rule's `prefer.max_quality`).
    pub max_quality: Option<crate::model::Quality>,
    /// Fit to this picture height (physical px; multi-view's small picture): the lowest quality
    /// at least this tall first.
    pub target_px: Option<u32>,
    /// Playlist order (`Library::sources`).
    pub source_order: Vec<SourceId>,
}

/// A channel's place in the ranking (smaller first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RankKey {
    not_pinned: bool,
    locked: bool,
    trouble: bool,
    favourite: (bool, usize, usize),
    quality: (u8, i32),
    playlist: usize,
    provider_order: i32,
}

/// Nominal picture height of a quality class.
fn height(q: crate::model::Quality) -> i32 {
    match q {
        crate::model::Quality::Sd => 576,
        crate::model::Quality::Hd => 720,
        crate::model::Quality::Fhd => 1080,
        crate::model::Quality::Uhd => 2160,
    }
}

/// The ONE channel ordering (B5): pinned; unlocked (while locked); no trouble in 24 h; in
/// favourites (earlier list and position first); quality (best, capped by `max_quality`, or
/// fitted to `target_px`; unknown last); playlist order; provider order.
pub fn rank_channels(cands: &[RankCand], ctx: &RankCtx) -> Vec<(crate::model::ChannelId, RankKey)> {
    let quality = |q: Option<crate::model::Quality>| -> (u8, i32) {
        let Some(q) = q else { return (3, 0) };
        let h = height(q);
        match (ctx.target_px, ctx.max_quality) {
            (Some(t), _) if h >= t as i32 => (0, h),
            (Some(_), _) => (1, -h),
            (None, Some(cap)) if q > cap => (1, h),
            _ => (0, -h),
        }
    };
    let mut out: Vec<(crate::model::ChannelId, RankKey)> = cands
        .iter()
        .map(|c| {
            let fav = ctx.favourites.get(&c.id).copied();
            (
                c.id,
                RankKey {
                    not_pinned: ctx.pinned != Some(c.id),
                    locked: ctx.session_locked && c.locked,
                    trouble: ctx.trouble.contains(&c.id),
                    favourite: (fav.is_none(), fav.map_or(0, |f| f.0), fav.map_or(0, |f| f.1)),
                    quality: quality(c.quality),
                    playlist: ctx.source_order.iter().position(|s| *s == c.source).unwrap_or(usize::MAX),
                    provider_order: c.provider_order,
                },
            )
        })
        .collect();
    out.sort_by_key(|(_, k)| *k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Undo numbers are handed out once and only the last [`UNDO_KEEP`] actions can be undone.
    #[test]
    fn undo_stash_keeps_the_last_actions_once() {
        let stash = UndoStash::default();
        let first = stash.stash(Undo::empty());
        let kept: Vec<u16> = (0..UNDO_KEEP).map(|_| stash.stash(Undo::empty())).collect();
        assert!(stash.take(first).is_none(), "pushed out by {UNDO_KEEP} newer actions");
        assert!(stash.take(kept[UNDO_KEEP - 1]).is_some());
        assert!(stash.take(kept[UNDO_KEEP - 1]).is_none(), "once");
    }

    /// One ordering for every feature (B5): pinned, unlocked while locked, no trouble,
    /// favourites, quality (capped or fitted), playlist order, provider order.
    #[test]
    fn rank_channels_orders_by_the_shared_rule() {
        use crate::model::{ChannelId, Quality};
        let c = |id: i64, src: i64, q: Option<Quality>, locked: bool| RankCand {
            id: ChannelId(id),
            source: SourceId(src),
            quality: q,
            locked,
            provider_order: 0,
        };
        let cands = [
            c(1, 2, Some(Quality::Hd), false),
            c(2, 1, Some(Quality::Uhd), false),
            c(3, 1, Some(Quality::Fhd), true),
            c(4, 2, None, false),
            c(5, 1, Some(Quality::Hd), false),
        ];
        let order = |ctx: &RankCtx| rank_channels(&cands, ctx).into_iter().map(|(id, _)| id.0).collect::<Vec<_>>();
        let base = RankCtx { source_order: vec![SourceId(1), SourceId(2)], ..Default::default() };
        // quality first, then playlist order; unknown quality last
        assert_eq!(order(&base), [2, 3, 5, 1, 4]);
        // a favourite beats quality; a channel with recent trouble goes after every other
        let mut ctx = base.clone();
        ctx.favourites.insert(ChannelId(1), (0, 3));
        ctx.trouble.insert(ChannelId(2));
        assert_eq!(order(&ctx), [1, 3, 5, 4, 2]);
        // while locked, locked channels go after unlocked ones; a pin wins over everything
        let ctx = RankCtx { session_locked: true, pinned: Some(ChannelId(4)), ..base.clone() };
        assert_eq!(order(&ctx), [4, 2, 5, 1, 3]);
        // a small picture fits the lowest quality at least its height (HD for 700 px)
        let ctx = RankCtx { target_px: Some(700), ..base.clone() };
        assert_eq!(order(&ctx)[..2], [5, 1]);
        // a cap of FHD puts 4K after FHD and HD
        let ctx = RankCtx { max_quality: Some(Quality::Fhd), ..base };
        assert_eq!(order(&ctx), [3, 5, 1, 2, 4]);
    }

    /// "E4" and "E4 +1" are different families: a +1 channel is never a copy of its base (it
    /// airs an hour later), whatever their guide mapping says.
    #[test]
    fn a_plus_one_channel_is_not_a_copy_of_its_base() {
        let n = crate::sources::identity::normalize_name;
        assert_ne!(n("E4"), n("E4 +1"));
        assert_eq!(n("E4"), n("e4"));
    }

    #[test]
    fn container_of_reads_only_the_extension() {
        assert_eq!(container_of(r#"{"t":"xtream","kind":"movie","stream_id":7,"ext":"MKV"}"#).as_deref(), Some("mkv"));
        assert_eq!(container_of(r#"{"t":"xtream","kind":"movie","stream_id":7,"ext":null}"#), None);
        assert_eq!(container_of(r#"{"t":"url","url":"http://h/movie/u/p/12.mp4?token=x.y"}"#).as_deref(), Some("mp4"));
        assert_eq!(container_of(r#"{"t":"url","url":"http://h/play/12"}"#), None);
        assert_eq!(container_of(r#"{"t":"url","url":"http://h.example.com"}"#), None, "a host is no container");
        assert_eq!(container_of("not json"), None);
    }
}
