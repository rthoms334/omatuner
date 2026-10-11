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

use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::config::CopyOrder;
use crate::error::Result;
use crate::model::{
    AccountInfo, AccountStatus, Episode, FavListId, ItemKey, ItemKind, ItemOverride, ItemRef, SeriesId, SourceId, StreamLocator, Timestamp, VodKind,
    VodSummary,
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

/// The episode a series' combined page opens at: `vod::next_episode` of the copy with the
/// newest episode progress, else of `copies[0]` (the copy that would play). With the episode
/// comes the copy it belongs to.
pub fn next_episode(conn: &Connection, copies: &[SeriesId]) -> Result<Option<(SeriesId, Episode)>> {
    let mut newest: Option<(Timestamp, SeriesId)> = None;
    for &s in copies {
        if let Some(p) = series_progress(conn, s)?
            && newest.is_none_or(|(at, _)| p.updated_at > at)
        {
            newest = Some((p.updated_at, s));
        }
    }
    let Some(series) = newest.map(|(_, s)| s).or(copies.first().copied()) else { return Ok(None) };
    Ok(crate::store::vod::next_episode(conn, series)?.map(|e| (series, e)))
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

#[cfg(test)]
mod tests {
    use super::*;

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
