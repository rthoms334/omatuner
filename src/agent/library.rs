//! The library tools of the skill plan (§2): `match_titles`, `history`, `episodes`,
//! `favourites_edit`, `parental_status` and `lock`. OWNER: agent track.
//!
//! Visibility is `agent::query`'s: what the published snapshot shows now. Explicit content
//! while hidden, hidden and filtered items are "missing" / `not_found`, never counted. Locked
//! items show as locked and are never added to a favourites list. `lock` can only add locks:
//! there is no unlock, PIN or "show explicit" parameter anywhere.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

use super::proto::{AgentRef, Episodes, FavouritesEdit, History, Kind, Lock, MatchTitles, ProgressOut, TitleOut, fmt_time, item_ref};
use super::query::{Fail, Out, Q, clamp, clean, parse_ref, text, when};
use super::server::{Inner, to_value};
use crate::model::{ItemKind, ItemOverride, ItemRef, SeriesId, VodKind};
use crate::msg::DataScope;

// ------------------------------------------------------------------ match_titles

/// Titles one `match_titles` call may ask about.
pub const MAX_TITLES: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MatchOut {
    pub results: Vec<MatchResult>,
}

/// One asked title: what the library has of it (`matched` empty = missing).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MatchResult {
    /// The title as asked (echoed so the answers line up).
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    pub matched: Vec<TitleOut>,
    pub missing: bool,
}

/// The folded key `sources::vod_match` stacks on, of a title as the web writes it.
fn match_key(title: &str, year: Option<u16>) -> String {
    crate::sources::vod_match::parse(title, year, &crate::sources::vod_match::PrefixCodes::default()).key
}

/// (movies?, match key) → the rows with that key: (row id, year).
type ByKey = HashMap<(bool, String), Vec<(i64, Option<u16>)>>;

pub fn match_titles(q: &Q, p: &MatchTitles) -> Out<MatchOut> {
    if p.titles.len() > MAX_TITLES {
        return Err(Fail::bad(format!("at most {MAX_TITLES} titles per call")));
    }
    let mut asked = Vec::with_capacity(p.titles.len());
    for t in &p.titles {
        let title = text("title", &t.title)?;
        let movies = match t.kind.as_deref() {
            None => None,
            Some("movie") => Some(true),
            Some("series") => Some(false),
            Some(_) => return Err(Fail::bad("kind is movie or series")),
        };
        let tmdb = t.tmdb.map(|n| n.to_string());
        asked.push((title.to_owned(), t.year, movies, match_key(title, t.year), tmdb));
    }
    // (movies?) → key → [(row id, year)], and tmdb → row ids
    let mut by_key: ByKey = HashMap::new();
    let mut by_tmdb: HashMap<(bool, String), Vec<i64>> = HashMap::new();
    for (movies, table) in [(true, "movie"), (false, "series")] {
        let keys: Vec<&str> = asked.iter().filter(|a| a.2 != Some(!movies) && !a.3.is_empty()).map(|a| a.3.as_str()).collect();
        let tmdbs: Vec<&str> = asked.iter().filter(|a| a.2 != Some(!movies)).filter_map(|a| a.4.as_deref()).collect();
        if keys.is_empty() && tmdbs.is_empty() {
            continue;
        }
        let km = vec!["?"; keys.len().max(1)].join(",");
        let tm = vec!["?"; tmdbs.len().max(1)].join(",");
        let sql = format!(
            "SELECT id, match_key, year, CAST(tmdb_id AS TEXT) FROM {table}
             WHERE match_key IN ({km}) OR CAST(tmdb_id AS TEXT) IN ({tm})"
        );
        let mut args: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
        if keys.is_empty() {
            args.push(String::new());
        }
        args.extend(tmdbs.iter().map(|s| s.to_string()));
        if tmdbs.is_empty() {
            args.push(String::new());
        }
        let mut stmt = q.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<u16>>(2)?, r.get::<_, Option<String>>(3)?))
        })?;
        for row in rows {
            let (id, key, year, tmdb) = row?;
            if let Some(k) = key {
                by_key.entry((movies, k)).or_default().push((id, year));
            }
            if let Some(t) = tmdb.filter(|t| !t.is_empty()) {
                by_tmdb.entry((movies, t)).or_default().push(id);
            }
        }
    }
    let results = asked
        .into_iter()
        .map(|(title, year, movies, key, tmdb)| {
            let mut seen: HashSet<(bool, u32)> = HashSet::new();
            let mut matched = Vec::new();
            for m in [true, false] {
                if movies == Some(!m) {
                    continue;
                }
                let mut ids: Vec<i64> = tmdb.as_ref().and_then(|t| by_tmdb.get(&(m, t.clone()))).cloned().unwrap_or_default();
                if let Some(rows) = by_key.get(&(m, key.clone())) {
                    // a year one off is the same film (release vs. festival year); none = any
                    ids.extend(rows.iter().filter(|(_, y)| year.zip(*y).is_none_or(|(a, b)| a.abs_diff(b) <= 1)).map(|(id, _)| *id));
                }
                for id in ids {
                    let kind = if m { VodKind::Movie(crate::model::MovieId(id)) } else { VodKind::Series(SeriesId(id)) };
                    let Some(idx) = q.lib.vod.index_of(kind) else { continue };
                    let top = q.stack_key(m, idx);
                    if seen.insert((m, top)) {
                        matched.push(q.title_out(m, top));
                    }
                }
            }
            MatchResult { query: clean(&title), year, missing: matched.is_empty(), matched }
        })
        .collect();
    Ok(MatchOut { results })
}

// ------------------------------------------------------------------ history

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryOut {
    pub items: Vec<HistoryItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryItem {
    #[serde(rename = "ref")]
    pub item: String,
    /// `channel`, `movie` or `episode`.
    pub kind: String,
    pub title: String,
    /// Episodes: the series (and its numbers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    /// Last watched.
    pub at: String,
    /// Movies / episodes: watched to the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
}

/// History rows read per call before visibility filtering.
const HISTORY_FETCH: usize = 500;

pub fn history(q: &Q, p: &History) -> Out<HistoryOut> {
    let limit = clamp(p.limit, 30, 100);
    let from = p.from.as_deref().map(|s| when("from", s, q.now)).transpose()?.unwrap_or(0);
    let to = p.to.as_deref().map(|s| when("to", s, q.now)).transpose()?.unwrap_or(i64::MAX);
    let kind = p.kind.map(|k| match k {
        Kind::Channel => "channel",
        Kind::Movie => "movie",
        Kind::Series => "episode",
    });
    let mut stmt = q.conn.prepare_cached(
        "SELECT source_id, kind, item_key, max(at) AS last FROM history WHERE at >= ?1 AND at <= ?2 AND (?3 IS NULL OR kind = ?3)
         GROUP BY source_id, kind, item_key ORDER BY last DESC LIMIT ?4",
    )?;
    let rows: Vec<(crate::model::SourceId, String, String, i64)> = stmt
        .query_map(params![from, to, kind, HISTORY_FETCH as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut items = Vec::new();
    for (source, kind, key, at) in rows {
        if items.len() == limit {
            break;
        }
        let Some(k) = ItemKind::parse(&kind) else { continue };
        let r = ItemRef { source, kind: k, key: crate::model::ItemKey(key) };
        if q.conceals(&r) {
            continue;
        }
        let progress = || crate::store::user::progress(q.conn, &r).ok().flatten();
        let item = match k {
            ItemKind::Channel => {
                let Some(c) = q.channel_by_ref(&r) else { continue };
                HistoryItem {
                    item: item_ref(&r),
                    kind: "channel".into(),
                    title: clean(&c.name),
                    series: None,
                    season: None,
                    episode: None,
                    at: fmt_time(at),
                    finished: None,
                    genre: None,
                }
            }
            ItemKind::Movie => {
                let Some(idx) = q.vod_index(&r)? else { continue };
                let v = &q.lib.vod.movies[idx as usize];
                let genre = match v.kind {
                    VodKind::Movie(id) => crate::store::vod::movie(q.conn, id).ok().and_then(|m| m.genre).map(|g| clean(&g)),
                    VodKind::Series(_) => None,
                };
                HistoryItem {
                    item: item_ref(&r),
                    kind: "movie".into(),
                    title: clean(&v.name),
                    series: None,
                    season: None,
                    episode: None,
                    at: fmt_time(at),
                    finished: Some(progress().is_some_and(|w| w.finished)),
                    genre,
                }
            }
            ItemKind::Episode => {
                let Some((ep, idx)) = q.episode(&r)? else { continue };
                let s = &q.lib.vod.series[idx as usize];
                HistoryItem {
                    item: item_ref(&r),
                    kind: "episode".into(),
                    title: clean(&ep.title),
                    series: Some(clean(&s.name)),
                    season: Some(ep.season),
                    episode: Some(ep.episode),
                    at: fmt_time(at),
                    finished: Some(progress().is_some_and(|w| w.finished)),
                    genre: None,
                }
            }
            ItemKind::Series => continue,
        };
        items.push(item);
    }
    Ok(HistoryOut { items })
}

// ------------------------------------------------------------------ episodes

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EpisodesOut {
    #[serde(rename = "ref")]
    pub item: String,
    /// Always `series` (the guard reads the episodes as this series').
    pub kind: &'static str,
    pub name: String,
    pub locked: bool,
    pub episodes: Vec<EpisodeRow>,
    /// What `play` on the series starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_episode: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub details_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EpisodeRow {
    #[serde(rename = "ref")]
    pub item: String,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_date: Option<String>,
    /// Watched to the end (on any copy of the series).
    pub watched: bool,
    /// Where it stopped, when started and not finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressOut>,
}

/// Episodes listed at most.
const MAX_EPISODES: usize = 200;

fn plan_episodes(q: &Q, p: &Episodes) -> Out<(EpisodesOut, Option<crate::jobs::JobKind>)> {
    let AgentRef::Item(r) = parse_ref(&p.series_ref)? else { return Err(Fail::bad("series_ref is a series ref")) };
    if r.kind != ItemKind::Series {
        return Err(Fail::bad("series_ref is a series ref"));
    }
    let idx = q.vod_index(&r)?.ok_or_else(|| Fail::not_found("series"))?;
    let v = &q.lib.vod.series[idx as usize];
    let locked = q.needs_pin(q.title_locked(false, idx));
    let mut out = EpisodesOut {
        item: item_ref(&r),
        kind: "series",
        name: clean(&v.name),
        locked,
        episodes: Vec::new(),
        next_episode: None,
        details_pending: false,
    };
    if locked {
        return Ok((out, None));
    }
    let words: Vec<String> = match p.query.as_deref() {
        Some(s) => text("query", s)?.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect(),
        None => Vec::new(),
    };
    let copies = q.series_copies(idx);
    // the copy that plays first lists the episodes; progress of every copy counts
    let Some(&first) = copies.first() else { return Ok((out, None)) };
    let eps = crate::store::vod::episodes(q.conn, first, p.season)?;
    if eps.is_empty() {
        let s = crate::store::vod::series(q.conn, first)?;
        let lazy = s.details_loaded_at.is_none() && q.lib.source(s.source_id).is_some_and(|m| m.kind == crate::model::SourceKind::Xtream);
        out.details_pending = lazy;
        return Ok((out, lazy.then_some(crate::jobs::JobKind::LoadSeriesDetails(first))));
    }
    // (season, episode) → (finished, progress, updated) over every copy
    let mut seen: HashMap<(u32, u32), (bool, Option<ProgressOut>, i64)> = HashMap::new();
    let marks = vec!["?"; copies.len()].join(",");
    let mut stmt = q.conn.prepare(&format!(
        "SELECT e.season, e.episode, p.position_s, p.duration_s, p.finished, p.updated_at FROM watch_progress p
         JOIN episode e ON e.source_id = p.source_id AND e.key = p.item_key
         WHERE p.kind = 'episode' AND p.orphaned_since IS NULL AND e.series_id IN ({marks})"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(copies.iter()), |r| {
        Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?, r.get::<_, f64>(2)?, r.get::<_, Option<f64>>(3)?, r.get::<_, bool>(4)?, r.get::<_, i64>(5)?))
    })?;
    for row in rows {
        let (s, e, pos, dur, fin, at) = row?;
        if seen.get(&(s, e)).is_none_or(|(_, _, t)| at > *t) {
            let progress = (!fin && pos > 0.0).then(|| ProgressOut {
                position_min: (pos / 60.0) as u32,
                duration_min: dur.filter(|d| *d > 0.0).map(|d| (d / 60.0).round() as u32),
                finished: false,
                watched: fmt_time(at),
            });
            seen.insert((s, e), (fin, progress, at));
        }
    }
    for ep in eps {
        if out.episodes.len() == MAX_EPISODES {
            break;
        }
        if !words.is_empty() {
            let hay = format!("{} {}", ep.title, ep.plot.as_deref().unwrap_or_default()).to_lowercase();
            if !words.iter().all(|w| hay.contains(w.as_str())) {
                continue;
            }
        }
        let er = ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key.clone() };
        let (watched, progress) = seen.get(&(ep.season, ep.episode)).map(|(f, p, _)| (*f, p.clone())).unwrap_or_default();
        out.episodes.push(EpisodeRow {
            item: item_ref(&er),
            season: ep.season,
            episode: ep.episode,
            title: clean(&ep.title),
            plot: ep.plot.as_deref().map(clean).filter(|t| !t.trim().is_empty()),
            duration_min: ep.duration_s.filter(|d| *d > 0).map(|d| (f64::from(d) / 60.0).round() as u32),
            air_date: ep.air_date.as_deref().map(clean),
            watched,
            progress,
        });
    }
    out.next_episode = crate::store::copies::next_episode(q.conn, &copies)?
        .map(|(_, e)| item_ref(&ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key }));
    Ok((out, None))
}

/// `episodes`, fetching a series' episodes from the provider first when they aren't loaded
/// (up to 3 s, like `details`).
pub(super) async fn episodes(h: &Arc<Inner>, p: Episodes) -> Out<Value> {
    let again = p.clone();
    let (mut out, job) = h.run(true, move |q| plan_episodes(q, &p)).await?;
    if let Some(job) = job {
        h.jobs.submit(job);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while out.details_pending && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let p = again.clone();
            out = h.run(true, move |q| plan_episodes(q, &p)).await?.0;
        }
    }
    to_value(&out)
}

// ------------------------------------------------------------------ favourites_edit

/// Refs one `favourites_edit` / `lock` call may name.
pub const MAX_EDIT: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Refused {
    #[serde(rename = "ref")]
    pub item: String,
    /// `not_found`, `locked: needs your PIN in Tuner`, `not_lockable`.
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FavEditOut {
    pub list: String,
    pub created: bool,
    pub added: u32,
    pub removed: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<Refused>,
    /// Non-channel saves belong to Watchlists (DESIGN §42 §4.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_watchlist: Option<&'static str>,
}

/// What `favourites_edit` writes.
struct FavPlan {
    list: Option<crate::model::FavListId>,
    name: String,
    add: Vec<ItemRef>,
    remove: Vec<ItemRef>,
    refused: Vec<Refused>,
}

/// A ref that names a visible channel, movie or series (`Err`: why not).
fn visible_item(q: &Q, s: &str) -> Result<(ItemRef, bool), &'static str> {
    let Ok(AgentRef::Item(r)) = parse_ref(s) else { return Err("not_found") };
    let locked = match r.kind {
        ItemKind::Channel => q.channel_by_ref(&r).map(|c| c.locked),
        ItemKind::Movie | ItemKind::Series => {
            let movies = r.kind == ItemKind::Movie;
            q.vod_index(&r).ok().flatten().map(|i| q.title_locked(movies, i))
        }
        ItemKind::Episode => None,
    };
    match locked {
        Some(l) if !q.conceals(&r) => Ok((r, l)),
        _ => Err("not_found"),
    }
}

fn plan_fav(q: &Q, p: &FavouritesEdit) -> Out<FavPlan> {
    if p.add.len() + p.remove.len() > MAX_EDIT {
        return Err(Fail::bad(format!("at most {MAX_EDIT} refs per call")));
    }
    let lists = crate::store::user::fav_lists(q.conn)?;
    let (list, name) = match (p.create.as_deref(), p.list.as_deref()) {
        (Some(_), Some(_)) => return Err(Fail::bad("give list or create, not both")),
        (Some(c), None) => {
            let c = text("create", c)?;
            if c.chars().count() > 60 {
                return Err(Fail::bad("a list name is at most 60 characters"));
            }
            match lists.iter().find(|l| l.name.eq_ignore_ascii_case(c)) {
                Some(l) => (Some(l.id), l.name.clone()),
                None => (None, c.to_owned()),
            }
        }
        (None, Some(l)) => {
            let l = text("list", l)?;
            let hit = lists.iter().find(|x| x.name.eq_ignore_ascii_case(l)).or_else(|| {
                let w = l.to_lowercase();
                let mut it = lists.iter().filter(|x| x.name.to_lowercase().contains(&w));
                let one = it.next();
                one.filter(|_| it.next().is_none())
            });
            let hit = hit.ok_or_else(|| Fail::not_found("favourites list"))?;
            (Some(hit.id), hit.name.clone())
        }
        (None, None) => {
            let id = crate::store::user::default_fav_list(q.conn)?;
            (Some(id), lists.iter().find(|l| l.id == id).map(|l| l.name.clone()).unwrap_or_default())
        }
    };
    let mut plan = FavPlan { list, name, add: Vec::new(), remove: Vec::new(), refused: Vec::new() };
    for s in &p.add {
        if let Ok(AgentRef::Item(r)) = parse_ref(s)
            && r.kind != ItemKind::Channel
        {
            plan.refused.push(Refused { item: s.clone(), error: "use_watchlist".into() });
            continue;
        }
        match visible_item(q, s) {
            // locked items are never added, whatever the session: a list is a way in
            Ok((_, true)) => plan.refused.push(Refused { item: s.clone(), error: crate::queue::Refusal::Locked.code().into() }),
            Ok((r, false)) => plan.add.push(r),
            Err(e) => plan.refused.push(Refused { item: s.clone(), error: e.into() }),
        }
    }
    for s in &p.remove {
        match parse_ref(s) {
            Ok(AgentRef::Item(r)) if r.kind != ItemKind::Channel => {
                plan.refused.push(Refused { item: s.clone(), error: "use_watchlist".into() });
            }
            Ok(AgentRef::Item(r)) if !q.conceals(&r) => plan.remove.push(r),
            _ => plan.refused.push(Refused { item: s.clone(), error: "not_found".into() }),
        }
    }
    if plan.add.is_empty() && plan.remove.is_empty() && plan.refused.is_empty() && p.create.is_none() {
        return Err(Fail::bad("nothing to do: give add, remove or create"));
    }
    Ok(plan)
}

pub(super) async fn favourites_edit(h: &Arc<Inner>, p: FavouritesEdit) -> Out<Value> {
    let plan = h.run(true, move |q| plan_fav(q, &p)).await?;
    let now = crate::model::now();
    let FavPlan { list, name, add, remove, refused } = plan;
    if add.is_empty() && remove.is_empty() && !refused.is_empty() && refused.iter().all(|r| r.error == "use_watchlist") {
        return to_value(&FavEditOut { list: clean(&name), created: false, added: 0, removed: 0, refused, use_watchlist: Some("watchlist") });
    }
    let created = list.is_none();
    let list_name = name.clone();
    let (added, removed) = h
        .write(DataScope::Favourites, move |c| {
            let id = match list {
                Some(id) => id,
                None => crate::store::user::create_fav_list(c, &name, "")?,
            };
            let had: HashSet<ItemRef> = crate::store::user::fav_items(c, id)?.into_iter().map(|e| e.item).collect();
            let mut added = 0;
            for r in add.iter().filter(|r| !had.contains(r)) {
                crate::store::user::set_favourite(c, id, r, true, now)?;
                added += 1;
            }
            let mut removed = 0;
            for r in remove.iter().filter(|r| had.contains(r)) {
                crate::store::user::set_favourite(c, id, r, false, now)?;
                removed += 1;
            }
            Ok((added, removed))
        })
        .await?;
    tracing::info!("agent: favourites_edit → +{added} −{removed} ({} refused)", refused.len());
    let use_watchlist = refused.iter().any(|r| r.error == "use_watchlist").then_some("watchlist");
    to_value(&FavEditOut { list: clean(&list_name), created, added, removed, refused, use_watchlist })
}

// ------------------------------------------------------------------ parental_status / lock

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParentalOut {
    /// A PIN is set and the parental lock is on (locked items need it).
    pub pin_set: bool,
    pub enabled: bool,
    /// The PIN was entered and the session hasn't relocked.
    pub session_unlocked: bool,
    /// Minutes until an unlocked session relocks by itself (0 = only when Tuner restarts).
    pub relock_min: u32,
    /// Explicit content shows now.
    pub explicit_shown: bool,
}

pub fn parental_status(q: &Q) -> Out<ParentalOut> {
    let p = &q.cfg.parental;
    Ok(ParentalOut {
        pin_set: q.view.pin_active,
        enabled: p.enabled,
        session_unlocked: q.view.pin_active && q.view.unlocked,
        relock_min: p.relock_min,
        explicit_shown: !q.view.hides_explicit,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LockOut {
    /// Items newly locked (a title counts once, every copy locked).
    pub locked: u32,
    pub already_locked: u32,
    /// The session was locked now.
    pub session_locked: bool,
    /// No PIN is set: locks act once the user sets one in Tuner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<Refused>,
}

/// The rows a lock writes: every copy of a title, the channel itself.
fn plan_lock(q: &Q, p: &Lock) -> Out<(Vec<Vec<ItemRef>>, u32, Vec<Refused>)> {
    if p.refs.len() > MAX_EDIT {
        return Err(Fail::bad(format!("at most {MAX_EDIT} refs per call")));
    }
    if p.refs.is_empty() && p.session_now != Some(true) {
        return Err(Fail::bad("nothing to do: give refs or session_now"));
    }
    let (mut groups, mut already, mut refused) = (Vec::new(), 0, Vec::new());
    for s in &p.refs {
        match visible_item(q, s) {
            Ok((_, true)) => already += 1,
            Ok((r, false)) => groups.push(match r.kind {
                ItemKind::Movie | ItemKind::Series => {
                    let movies = r.kind == ItemKind::Movie;
                    let idx = q.vod_index(&r)?.ok_or_else(|| Fail::not_found("title"))?;
                    let items = if movies { &q.lib.vod.movies } else { &q.lib.vod.series };
                    q.lib
                        .vod
                        .stacks(movies)
                        .copies(idx)
                        .into_iter()
                        .filter_map(|c| items.get(c as usize))
                        .map(crate::store::copies::vod_ref)
                        .collect()
                }
                _ => vec![r],
            }),
            Err(_) if matches!(parse_ref(s), Ok(AgentRef::Item(ItemRef { kind: ItemKind::Episode, .. })) | Ok(AgentRef::Programme { .. })) => {
                refused.push(Refused { item: s.clone(), error: "not_lockable: lock the channel or the series".into() })
            }
            Err(e) => refused.push(Refused { item: s.clone(), error: e.into() }),
        }
    }
    Ok((groups, already, refused))
}

pub(super) async fn lock(h: &Arc<Inner>, p: Lock) -> Out<Value> {
    let session_now = p.session_now == Some(true);
    let (groups, already, refused) = h.run(true, move |q| plan_lock(q, &p)).await?;
    let n = groups.len() as u32;
    if !groups.is_empty() {
        h.write(DataScope::Overrides, move |c| {
            for r in groups.iter().flatten() {
                let ov = crate::store::user::item_override(c, r)?.unwrap_or_default();
                if !ov.locked {
                    crate::store::user::set_item_override(c, r, &ItemOverride { locked: true, ..ov })?;
                }
            }
            Ok(())
        })
        .await?;
    }
    if session_now {
        // the handler's own view locks at once; the app locks its session on the event
        h.shared.set_unlocked_at(None);
        h.bus.send(crate::msg::AppEvent::Agent(super::AgentCommand::LockSession));
    }
    tracing::info!("agent: lock → {n} locked, session {}", if session_now { "locked" } else { "unchanged" });
    let pin_set = h.cfg().parental.pin_active();
    to_value(&LockOut {
        locked: n,
        already_locked: already,
        session_locked: session_now,
        note: (!pin_set).then(|| "No parental PIN is set: these locks act once the user sets a PIN in Tuner's Settings ▸ Parental.".into()),
        refused,
    })
}
