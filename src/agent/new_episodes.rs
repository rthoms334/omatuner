//! The `new_episodes` tool (DESIGN §44 §7): which new episodes of the user's shows arrived in a
//! playlist, why a show is tracked, and (when the user asks) clearing or muting.
//!
//! `list` and `tracked` read; reading never clears anything. `seen` only takes episodes off Home
//! and the badges (never watched state), `mute` stops alerts for one series; both need the
//! user's yes (RULES.md), toast in Tuner with Undo and are the user's to take back. Locked,
//! hidden, concealed and explicit titles are absent (the store's guards, plus `Q::conceals`);
//! titles and episode titles leave through the reply's spoiler guard (`agent::guard`), which
//! reads the `kind`, `title`, `series`, `season` and `episode` fields below. Titles and
//! playlist names are provider text: `clean`ed here, data to the model. "Check now" is not
//! exposed (Q11). Plays and downloads use the existing tools with the refs returned here.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{Value, json};

use super::params::{NewEpisodes, NewEpisodesAction};
use super::proto::{AgentRef, ErrorCode, fmt_time, item_ref};
use super::query::{Fail, Out, Q, clamp, clean, parse_ref, text, when};
use super::server::Inner;
use crate::model::{ArrivalId, ItemKind, ItemRef, Timestamp};
use crate::msg::DataScope;
use crate::new_episodes::{Arrival, Cleared, NewEpisodesSnapshot, SeriesNews, Tracked, Why};
use crate::store::new_episodes as store;

/// Series one `list` or `tracked` call returns at most.
const MAX_SERIES: u32 = 50;
/// Episode refs one `seen` may name.
const MAX_REFS: usize = 50;

fn enabled(q: &Q) -> bool {
    crate::new_episodes::ENABLED && q.cfg.new_episodes.enabled
}

fn opts(q: &Q) -> store::Opts {
    store::Opts::from_config(&q.cfg.new_episodes, q.now)
}

/// A time (`proto::parse_when`) or a span back from now: `7d`, `12h`, `30m`.
fn since(q: &Q, s: &str) -> Out<Timestamp> {
    let s = text("since", s)?;
    if let Some(n) = s.get(..s.len() - 1).and_then(|n| n.parse::<i64>().ok()).filter(|n| *n >= 0) {
        let unit = match s.as_bytes()[s.len() - 1] {
            b'd' => Some(86_400),
            b'h' => Some(3_600),
            b'm' => Some(60),
            _ => None,
        };
        if let Some(unit) = unit {
            return Ok(q.now - n.saturating_mul(unit));
        }
    }
    when("since", s, q.now)
}

fn playlists(q: &Q, copies: &[ItemRef]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for c in copies {
        if let Some(m) = q.lib.source(c.source) {
            let name = clean(&m.name);
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

fn why_label(w: &Why) -> String {
    match w {
        Why::List { .. } => format!("list:{}", clean(&w.label())),
        Why::Binged { .. } => w.label(),
    }
}

fn year(q: &Q, series: &ItemRef) -> Option<u16> {
    let idx = q.vod_index(series).ok().flatten()?;
    q.title_out(false, idx).year
}

fn state(opened: bool, cleared: Option<Cleared>) -> &'static str {
    match (cleared, opened) {
        (Some(c), _) => c.as_str(),
        (None, true) => "opened",
        (None, false) => "new",
    }
}

/// The episode (ref, title) an arrival of the title with these `series` rows points at.
fn episode_ref(q: &Q, ids: &[crate::model::SeriesId], season: u32, episode: u32) -> Option<(ItemRef, String)> {
    let mut stmt = q
        .conn
        .prepare_cached("SELECT source_id, key, title FROM episode WHERE series_id = ?1 AND season = ?2 AND episode = ?3 ORDER BY id LIMIT 1")
        .ok()?;
    ids.iter().find_map(|id| {
        stmt.query_row(rusqlite::params![id, season, episode], |r| {
            Ok((ItemRef { source: r.get(0)?, kind: ItemKind::Episode, key: crate::model::ItemKey(r.get(1)?) }, r.get::<_, String>(2)?))
        })
        .ok()
    })
}

fn new_item(title: &str, season: u32, episode: u32, r: Option<(ItemRef, String)>, arrived: Timestamp) -> Value {
    // `series`, `season` and `episode` let the reply guard use Subject::Episode
    let mut out = json!({"series": clean(title), "season": season, "episode": episode, "arrived_at": fmt_time(arrived)});
    if let Some((r, t)) = r {
        out["ref"] = json!(item_ref(&r));
        if !t.trim().is_empty() {
            out["title"] = json!(clean(&t));
        }
    }
    out
}

/// The next episode to watch of the title (Q4: it never skips what the user hasn't seen).
fn next_episode(q: &Q, title: &str, ids: &[crate::model::SeriesId]) -> Out<Option<Value>> {
    let Some((_, ep)) = crate::store::copies::next_episode(q.conn, ids)? else { return Ok(None) };
    let r = ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key.clone() };
    let mut out = json!({"ref": item_ref(&r), "series": clean(title), "season": ep.season, "episode": ep.episode});
    if !ep.title.trim().is_empty() {
        out["title"] = json!(clean(&ep.title));
    }
    if let Some(p) = crate::store::user::progress(q.conn, &r)?.filter(|p| !p.finished && p.position_s > 1.0) {
        out["resume_at_s"] = json!(p.position_s.round() as u64);
    }
    Ok(Some(out))
}

/// Done downloads of episodes (the reference of `downloaded`: the copy that was saved).
fn downloaded(h: &Arc<Inner>) -> HashSet<ItemRef> {
    h.shared
        .downloads()
        .map(|d| d.snapshot().jobs.iter().filter(|j| j.state == crate::downloads::State::Done).filter_map(|j| j.item()).collect())
        .unwrap_or_default()
}

/// The titles with news the agent may see.
fn visible<'a>(q: &Q, snap: &'a NewEpisodesSnapshot) -> impl Iterator<Item = &'a SeriesNews> {
    let hidden: Vec<bool> = snap.series.iter().map(|s| s.copies.iter().any(|c| q.conceals(c))).collect();
    snap.series.iter().zip(hidden).filter(|(_, h)| !*h).map(|(s, _)| s)
}

fn list(q: &Q, p: &NewEpisodes, done: &HashSet<ItemRef>) -> Out<Value> {
    if !enabled(q) {
        return Ok(json!({"enabled": false, "series": []}));
    }
    let o = opts(q);
    let snap = store::snapshot(q.conn, q.lib, &o)?;
    let from = p.since.as_deref().map(|s| since(q, s)).transpose()?;
    let keep = |at: Timestamp| from.is_none_or(|f| at >= f);
    let mut series: Vec<Value> = Vec::new();
    let mut seen_copies: Vec<ItemRef> = Vec::new();
    for news in visible(q, &snap) {
        let mut new: Vec<Value> = Vec::new();
        for e in news.new.iter().filter(|e| keep(e.arrived_at)) {
            let r = e.episode_ref.clone().and_then(|r| q.episode(&r).ok().flatten().map(|(ep, _)| (r, ep.title)));
            let mut item = new_item(&news.title, e.season, e.episode, r, e.arrived_at);
            item["state"] = json!(state(e.opened, None));
            if let Some(quiet) = e.quiet {
                item["quiet"] = json!(quiet.as_str());
            }
            if e.episode_ref.as_ref().is_some_and(|r| done.contains(r)) {
                item["downloaded"] = json!(true);
            }
            new.push(item);
        }
        seen_copies.extend(news.copies.iter().cloned());
        if new.is_empty() && p.include_cleared != Some(true) {
            continue;
        }
        let mut entry = json!({
            "kind": "series", "title": clean(&news.title), "ref": item_ref(&news.copies[0]), "series_ref": item_ref(&news.copies[0]),
            "year": year(q, &news.copies[0]), "why": news.why.iter().map(why_label).collect::<Vec<_>>(),
            "playlists": playlists(q, &news.copies), "new": new,
        });
        if let Some(next) = next_episode(q, &news.title, &news.series)? {
            entry["next_episode"] = next;
        }
        series.push(entry);
    }
    if p.include_cleared == Some(true) {
        // watched, dismissed and expired arrivals of the last 30 days, under their title
        for a in store::recent(q.conn, q.lib, q.now, store::RECENT_DAYS)?.into_iter().filter(|a| a.cleared_how.is_some() && keep(a.arrived_at)) {
            add_cleared(q, &mut series, &snap, &a);
        }
    }
    series.retain(|s| s["new"].as_array().is_some_and(|n| !n.is_empty()));
    let limit = clamp(p.limit, 20, MAX_SERIES);
    let truncated = series.len() > limit;
    series.truncate(limit);
    Ok(json!({"enabled": true, "checked_at": snap.checked_at.map(fmt_time), "series": series, "truncated": truncated}))
}

/// Put a cleared arrival under its title's entry (a new entry when the title has no open news).
fn add_cleared(q: &Q, series: &mut Vec<Value>, snap: &NewEpisodesSnapshot, a: &Arrival) {
    let Some(copy) = a.series.as_ref().filter(|c| !q.conceals(c)) else { return };
    let news = snap.of(copy);
    let key = news.map_or_else(|| copy.clone(), |n| n.copies[0].clone());
    let Some(idx) = q.vod_index(copy).ok().flatten() else { return };
    let ids = news.map_or_else(|| q.series_copies(idx).unwrap_or_default(), |n| n.series.clone());
    let title = news.map_or(a.series_title.as_str(), |n| n.title.as_str());
    let mut item = new_item(title, a.season, a.episode, episode_ref(q, &ids, a.season, a.episode), a.arrived_at);
    item["state"] = json!(state(a.opened_at.is_some(), a.cleared_how));
    let r = item_ref(&key);
    match series.iter_mut().find(|s| s["ref"] == r) {
        Some(s) => {
            if let Some(n) = s["new"].as_array_mut() {
                n.push(item);
            }
        }
        None => series.push(json!({
            "kind": "series", "title": clean(title), "ref": r, "series_ref": r, "year": year(q, &key),
            "playlists": playlists(q, std::slice::from_ref(copy)), "new": [item],
        })),
    }
}

fn tracked_list(q: &Q, p: &NewEpisodes) -> Out<Value> {
    let cfg = &q.cfg.new_episodes;
    if !enabled(q) {
        return Ok(json!({"enabled": false, "notify": cfg.notify, "binged": cfg.binged, "series": []}));
    }
    let tracked: Vec<Tracked> = store::tracked(q.conn, q.lib, &opts(q))?.into_iter().filter(|t| !t.copies.iter().any(|c| q.conceals(c))).collect();
    let last = store::last_new(q.conn, &tracked)?;
    let limit = clamp(p.limit, MAX_SERIES, 200);
    let truncated = tracked.len() > limit;
    let series: Vec<Value> = tracked
        .iter()
        .zip(last)
        .take(limit)
        .map(|(t, last)| {
            let mut out = json!({
                "kind": "series", "title": clean(&t.title), "ref": item_ref(&t.copies[0]), "series_ref": item_ref(&t.copies[0]),
                "year": year(q, &t.copies[0]), "why": t.why.iter().map(why_label).collect::<Vec<_>>(), "playlists": playlists(q, &t.copies),
            });
            if t.muted {
                out["muted"] = json!(true);
            }
            if let Some(at) = last {
                out["last_new_at"] = json!(fmt_time(at));
            }
            out
        })
        .collect();
    Ok(json!({"enabled": true, "notify": cfg.notify, "binged": cfg.binged, "series": series, "truncated": truncated}))
}

/// What a `seen` clears: arrival ids, the titles they belong to and the refs that matched
/// nothing visible.
struct Seen {
    ids: Vec<ArrivalId>,
    titles: usize,
    refused: Vec<Value>,
}

fn plan_seen(q: &Q, p: &NewEpisodes) -> Out<Seen> {
    if !enabled(q) {
        return Err(Fail::new(ErrorCode::Unavailable, "New episodes is switched off in Tuner"));
    }
    let snap = store::snapshot(q.conn, q.lib, &opts(q))?;
    let news: Vec<&SeriesNews> = visible(q, &snap).collect();
    let mut seen = Seen { ids: Vec::new(), titles: 0, refused: Vec::new() };
    let mut titles: HashSet<usize> = HashSet::new();
    let mut take = |i: usize, only: Option<(u32, u32)>, seen: &mut Seen| {
        for e in &news[i].new {
            if only.is_none_or(|o| o == (e.season, e.episode)) && !seen.ids.contains(&e.arrival) {
                seen.ids.push(e.arrival);
                titles.insert(i);
            }
        }
    };
    let find = |item: &ItemRef| news.iter().position(|n| n.copies.contains(item));
    let mut asked = 0;
    if p.all == Some(true) {
        asked += 1;
        (0..news.len()).for_each(|i| take(i, None, &mut seen));
    }
    if let Some(r) = p.series_ref.as_deref() {
        asked += 1;
        match parse_ref(r)? {
            AgentRef::Item(item) if item.kind == ItemKind::Series => match find(&item) {
                Some(i) => take(i, None, &mut seen),
                None => seen.refused.push(json!({"ref": r, "error": "no new episodes"})),
            },
            _ => return Err(Fail::bad("series_ref is a series ref")),
        }
    }
    if let Some(refs) = p.refs.as_deref() {
        asked += 1;
        if refs.is_empty() || refs.len() > MAX_REFS {
            return Err(Fail::bad(format!("refs takes 1 to {MAX_REFS} episode refs")));
        }
        for r in refs {
            let item = match parse_ref(r)? {
                AgentRef::Item(i) => i,
                AgentRef::Programme { .. } => return Err(Fail::bad("refs are episode refs from list")),
            };
            let hit = match item.kind {
                ItemKind::Series => find(&item).map(|i| (i, None)),
                ItemKind::Episode => q
                    .episode(&item)?
                    .and_then(|(ep, _)| news.iter().position(|n| n.series.contains(&ep.series_id)).map(|i| (i, Some((ep.season, ep.episode))))),
                _ => None,
            };
            match hit {
                Some((i, only)) => take(i, only, &mut seen),
                None => seen.refused.push(json!({"ref": r, "error": "not_found"})),
            }
        }
    }
    if asked == 0 {
        return Err(Fail::bad("seen needs series_ref, refs or all"));
    }
    seen.titles = titles.len();
    Ok(seen)
}

async fn seen(h: &Arc<Inner>, p: NewEpisodes, who: &crate::model::AgentName) -> Out<Value> {
    let plan = h.run(true, move |q| plan_seen(q, &p)).await?;
    let ids = plan.ids.clone();
    let undo = h.write(DataScope::NewEpisodes, move |c| store::dismiss(c, &ids, crate::model::now())).await?;
    tracing::info!("agent: new_episodes seen → {} episodes in {} series", plan.ids.len(), plan.titles);
    if !undo.is_empty() {
        let n = plan.ids.len();
        let text =
            format!("{} cleared {} from New episodes", who.subject(), if n == 1 { "1 new episode".to_owned() } else { format!("{n} new episodes") });
        crate::ui::new_episodes::undo_toast(&h.bus, text, undo);
    }
    Ok(json!({"cleared": plan.ids.len(), "series": plan.titles, "refused": plan.refused}))
}

async fn mute(h: &Arc<Inner>, p: NewEpisodes, who: &crate::model::AgentName) -> Out<Value> {
    let on = p.muted.unwrap_or(true);
    let reference = p.series_ref.clone().ok_or_else(|| Fail::bad("mute needs series_ref"))?;
    let (item, title) = h
        .run(true, move |q| {
            if !enabled(q) {
                return Err(Fail::new(ErrorCode::Unavailable, "New episodes is switched off in Tuner"));
            }
            let AgentRef::Item(item) = parse_ref(&reference)? else { return Err(Fail::bad("series_ref is a series ref")) };
            if item.kind != ItemKind::Series || q.conceals(&item) {
                return Err(Fail::not_found("series"));
            }
            // only a tracked title can be muted: that excludes locked, hidden and explicit ones
            let hit = store::tracked(q.conn, q.lib, &opts(q))?.into_iter().find(|t| t.copies.contains(&item));
            hit.map(|t| (item, t.title)).ok_or_else(|| Fail::not_found("tracked series"))
        })
        .await?;
    let (lib, target) = (h.shared.library(), item.clone());
    let undo = h.write(DataScope::NewEpisodes, move |c| store::mute(c, &lib, &target, on, crate::model::now())).await?;
    tracing::info!("agent: new_episodes {} a series", if on { "muted" } else { "unmuted" });
    if !undo.is_empty() {
        let name = clean(&title);
        let text = if on {
            format!("{} stopped New episodes alerts for {name}", who.subject())
        } else {
            format!("{} allowed New episodes alerts for {name} again", who.subject())
        };
        crate::ui::new_episodes::undo_toast(&h.bus, text, undo);
    }
    Ok(json!({"muted": on, "ref": item_ref(&item), "title": clean(&title)}))
}

pub(super) async fn handle(h: &Arc<Inner>, p: NewEpisodes, who: &crate::model::AgentName) -> Out<Value> {
    match p.action {
        NewEpisodesAction::List => {
            let done = downloaded(h);
            h.run(true, move |q| list(q, &p, &done)).await
        }
        NewEpisodesAction::Tracked => h.run(true, move |q| tracked_list(q, &p)).await,
        NewEpisodesAction::Seen => seen(h, p, who).await,
        NewEpisodesAction::Mute => mute(h, p, who).await,
    }
}

/// How many series and episodes are new, for `status` (the counts only, no titles). `None`
/// while the feature is off.
pub(super) fn counts(q: &Q) -> Out<Option<Value>> {
    if !enabled(q) {
        return Ok(None);
    }
    let snap = store::snapshot(q.conn, q.lib, &opts(q))?;
    let (series, episodes) = visible(q, &snap).fold((0, 0), |(s, e), n| (s + 1, e + n.new.len()));
    Ok(Some(json!({"series": series, "episodes": episodes})))
}
