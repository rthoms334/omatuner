//! Lasting play orders over Up next (DESIGN §42 §6–§7). Reads use the live parental view;
//! mutations name visible entry ids only. The engine rechecks locks on every continuation.

use std::sync::Arc;

use serde_json::{Value, json};

use super::params::{Watchlist, WatchlistAction, WatchlistSelector};
use super::proto::{AgentRef, ErrorCode, item_ref};
use super::query::{Fail, Out, Q, clean, parse_ref, text};
use super::server::Inner;
use crate::model::{ItemKind, WatchlistItemId};
use crate::msg::DataScope;
use crate::queue::{PlayReply, QueueCommand, QueueReply};
use crate::store::watchlist as store;
use crate::watchlist::{EntryState, EntryStatus, Insert, ListView, NewEntry, PlayOrder};

/// Exact names, initials and prefixes of whole words are safe automatic resolutions.
fn matching(name: &str, query: &str) -> bool {
    let name = name.to_lowercase();
    let query = query.to_lowercase();
    let words: Vec<_> = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let query_words: Vec<_> = query.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let initials: String = words.iter().filter_map(|w| w.chars().next()).collect();
    !query_words.is_empty()
        && (initials == query
            || words.windows(query_words.len()).any(|window| window.iter().zip(&query_words).all(|(word, prefix)| word.starts_with(prefix))))
}

enum Selection {
    Found(Box<ListView>),
    Ambiguous(Vec<String>),
    DidYouMean(Vec<String>),
}

fn select(q: &Q, selector: &WatchlistSelector) -> Out<Selection> {
    let snapshot = store::snapshot_with_preferences(q.conn, q.lib, q.view.unlocked, true, q.view.hides_explicit, q.cfg.interface.copy_order)?;
    let suggestions: Vec<_> = snapshot.lists.iter().map(|l| clean(&l.list.name)).collect();
    let mut hits = match selector {
        WatchlistSelector::Id(id) => snapshot.lists.into_iter().filter(|l| l.list.id.0 == *id).collect::<Vec<_>>(),
        WatchlistSelector::Name(name) => {
            let name = text("list", name)?;
            let exact = snapshot.lists.iter().any(|l| l.list.name.eq_ignore_ascii_case(name));
            snapshot.lists.into_iter().filter(|l| if exact { l.list.name.eq_ignore_ascii_case(name) } else { matching(&l.list.name, name) }).collect()
        }
    };
    match hits.len() {
        0 if matches!(selector, WatchlistSelector::Name(_)) && !suggestions.is_empty() => Ok(Selection::DidYouMean(suggestions)),
        0 => Err(Fail::not_found("watchlist")),
        1 => Ok(Selection::Found(Box::new(hits.remove(0)))),
        _ => Ok(Selection::Ambiguous(hits.iter().map(|l| clean(&l.list.name)).collect())),
    }
}

fn entry_out(e: &EntryState) -> Value {
    let mut out = json!({
        "id": e.entry.id.0, "kind": e.entry.kind.as_str(), "title": clean(&e.entry.title),
        "year": e.entry.year, "state": e.status, "locked": e.locked,
    });
    if let Some(item) = &e.entry.item {
        out["ref"] = json!(item_ref(item));
    }
    if let Some(from) = e.entry.season_from {
        out["seasons"] = json!({"from": from, "to": e.entry.season_to.unwrap_or(from)});
    }
    if let Some(progress) = &e.progress {
        out["progress"] = json!({"position_s": progress.position_s, "duration_s": progress.duration_s});
        out["position_s"] = json!(progress.position_s);
    }
    if let Some(duration) = e.duration_s {
        out["duration_min"] = json!((duration / 60.0).round() as u32);
    }
    if let Some((season, episode)) = e.episode {
        out["s"] = json!(season);
        out["e"] = json!(episode);
        if let Some(series) = &e.series {
            // These fields let the existing reply guard use Subject::Episode, including
            // episodes saved independently of their series (DESIGN §42 §6).
            out["series"] = json!(clean(series));
            out["season"] = json!(season);
            out["episode"] = json!(episode);
        }
    }
    out
}

fn list_out(view: &ListView) -> Value {
    let mut out = json!({
        "id": view.list.id.0, "name": clean(&view.list.name), "note": view.list.note.as_deref().map(clean),
        "play_order": view.list.play_order.as_str(), "done": view.done, "total": view.total,
    });
    let next = view
        .entries
        .iter()
        .find(|e| !e.done() && e.status != EntryStatus::Missing)
        .filter(|e| e.locked && (view.list.play_order == PlayOrder::Ordered || view.next.is_none()))
        .or_else(|| view.next_entry());
    if let Some(next) = next {
        out["next"] = entry_out(next);
    }
    out
}

fn show_out(view: &ListView, include_done: bool) -> Value {
    json!({"list": list_out(view), "items": view.entries.iter().filter(|e| include_done || e.status != EntryStatus::Done).map(entry_out).collect::<Vec<_>>()})
}

fn next_out(q: &Q, view: &ListView) -> Out<Value> {
    let mut out = json!({
        "item": view.next_entry().map(entry_out),
        "passed_over": view.passed_over.iter().map(|p| {
            let mut out = view.entries.iter().find(|e| e.entry.id == p.id).map(entry_out).unwrap_or_else(|| json!({}));
            out["why"] = json!(clean(&p.why));
            out
        }).collect::<Vec<_>>()
    });
    if let Some(next) = view.next_entry() {
        if let Some(progress) = &next.progress {
            out["resume_at"] = json!(progress.position_s);
        }
        if let Some(item) = &next.playable
            && item.kind == ItemKind::Episode
            && let Some((episode, idx)) = q.episode(item)?
        {
            // `series`, `season` and `episode` let the shared reply guard use Subject::Episode.
            out["episode"] = serde_json::to_value(q.episode_out(&episode)).map_err(|_| Fail::bad("couldn't read the episode"))?;
            out["episode"]["series"] = json!(clean(&q.lib.vod.series[idx as usize].name));
            out["episode"]["kind"] = json!("episode");
        }
    }
    Ok(out)
}

/// A next episode may be in a series whose details have never been opened. Load the planned copy
/// as `episodes` does, before claiming which episode is next (DESIGN §42 §3, §7).
fn pending_series(q: &Q, view: &ListView) -> Out<Vec<crate::jobs::JobKind>> {
    let Some(item) = view.next_entry().and_then(|e| e.playable.as_ref()).filter(|item| item.kind == ItemKind::Series) else { return Ok(Vec::new()) };
    let Some(idx) = q.vod_index(item)? else { return Ok(Vec::new()) };
    let crate::model::VodKind::Series(id) = q.lib.vod.series[idx as usize].kind else { return Ok(Vec::new()) };
    let series = crate::store::vod::series(q.conn, id)?;
    let mut jobs = Vec::new();
    if series.details_loaded_at.is_none() && q.lib.source(series.source_id).is_some_and(|source| source.kind == crate::model::SourceKind::Xtream) {
        jobs.push(crate::jobs::JobKind::LoadSeriesDetails(id));
    }
    Ok(jobs)
}

async fn resolve_next(h: &Arc<Inner>, view: ListView) -> Out<Value> {
    let id = view.list.id;
    let (mut view, mut jobs) = h
        .run(true, move |q| {
            let jobs = pending_series(q, &view)?;
            Ok((view, jobs))
        })
        .await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut submitted = std::collections::HashSet::new();
    while !jobs.is_empty() {
        for job in jobs {
            if let crate::jobs::JobKind::LoadSeriesDetails(id) = job
                && submitted.insert(id)
            {
                h.jobs.submit(crate::jobs::JobKind::LoadSeriesDetails(id));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(Fail::new(ErrorCode::Unavailable, "Tuner is still loading this series' episodes; try next again in a few seconds"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        (view, jobs) = h
            .run(true, move |q| {
                let view = store::view_with_preferences(q.conn, q.lib, id, q.view.unlocked, true, q.view.hides_explicit, q.cfg.interface.copy_order)?
                    .ok_or_else(|| Fail::not_found("watchlist"))?;
                let jobs = pending_series(q, &view)?;
                Ok((view, jobs))
            })
            .await?;
    }
    h.run(true, move |q| next_out(q, &view)).await
}

fn position(value: Option<&str>) -> Out<Insert> {
    Ok(match value.map(|s| text("position", s)).transpose()? {
        None | Some("end" | "bottom") => Insert::End,
        Some("next") => Insert::Next,
        Some("top") => Insert::Index(0),
        Some(n) => Insert::Index(n.parse().map_err(|_| Fail::bad("give end, next or an index"))?),
    })
}

fn play_order(value: Option<&str>) -> Out<PlayOrder> {
    match value {
        None | Some("ordered") => Ok(PlayOrder::Ordered),
        Some("any") => Ok(PlayOrder::Any),
        _ => Err(Fail::bad("play_order is ordered or any")),
    }
}

/// A refused ref is echoed only after it has been parsed. Its title is never looked up.
fn additions(q: &Q, p: &Watchlist) -> Out<(Vec<NewEntry>, Vec<Value>)> {
    let refs = p.refs.as_deref().unwrap_or_default();
    let placeholders = p.placeholders.as_deref().unwrap_or_default();
    if refs.len() + placeholders.len() > 50 {
        return Err(Fail::bad("at most 50 refs and placeholders per add"));
    }
    if p.action == WatchlistAction::Add && refs.is_empty() && placeholders.is_empty() {
        return Err(Fail::bad("add needs refs or placeholders"));
    }
    if p.seasons.as_ref().is_some_and(|s| s.from > s.to || s.to > i32::MAX as u32) {
        return Err(Fail::bad("seasons must run from a lower season to a higher one"));
    }
    let (mut entries, mut refused) = (Vec::new(), Vec::new());
    for s in refs {
        let reference = parse_ref(s)?;
        let AgentRef::Item(item) = reference else {
            refused.push(json!({"ref": s, "error": "Watchlists hold films and series; use Up next or a reminder for live TV"}));
            continue;
        };
        if item.kind == ItemKind::Channel {
            refused.push(json!({"ref": s, "error": "Watchlists hold films and series; use Up next or a reminder for live TV"}));
            continue;
        }
        let visible_locked = match item.kind {
            ItemKind::Movie | ItemKind::Series => q.vod_index(&item)?.map(|idx| q.title_locked(item.kind == ItemKind::Movie, idx)),
            ItemKind::Episode => q.episode(&item)?.map(|(_, idx)| q.title_locked(false, idx)),
            ItemKind::Channel => None,
        };
        let error = if q.conceals(&item) || visible_locked.is_none() {
            Some("not_found")
        } else if visible_locked == Some(true) {
            Some("locked: needs your PIN in Tuner")
        } else if p.seasons.is_some() && item.kind != ItemKind::Series {
            Some("seasons are for a series")
        } else {
            None
        };
        if let Some(error) = error {
            refused.push(json!({"ref": s, "error": error}));
            continue;
        }
        let mut entry = store::new_entry(q.conn, &item)?;
        if let Some(seasons) = &p.seasons {
            entry.season_from = Some(seasons.from);
            entry.season_to = Some(seasons.to);
        }
        entries.push(entry);
    }
    for (index, placeholder) in placeholders.iter().enumerate() {
        let kind = match placeholder.kind {
            ItemKind::Movie => ItemKind::Movie,
            ItemKind::Series => ItemKind::Series,
            ItemKind::Episode => ItemKind::Episode,
            _ => {
                refused.push(
                    json!({"title": clean(placeholder.title.as_deref().unwrap_or_default()), "error": "Watchlists hold films and series; use Up next or a reminder for live TV"}),
                );
                continue;
            }
        };
        let title = if kind == ItemKind::Episode {
            if placeholder.year.is_none() || placeholder.season.zip(placeholder.episode).is_none() {
                return Err(Fail::bad("episode placeholders need year, season and episode"));
            }
            text("series_title", placeholder.series_title.as_deref().ok_or_else(|| Fail::bad("episode placeholders need series_title"))?)?
        } else {
            if placeholder.series_title.is_some() || placeholder.season.is_some() || placeholder.episode.is_some() {
                return Err(Fail::bad("series_title, season and episode are for episode placeholders"));
            }
            text("title", placeholder.title.as_deref().ok_or_else(|| Fail::bad("movie and series placeholders need title"))?)?
        };
        let tmdb_id = placeholder.tmdb_id.map(i64::try_from).transpose().map_err(|_| Fail::bad("tmdb_id is too large"))?;
        if p.seasons.is_some() && kind != ItemKind::Series {
            refused.push(json!({"title": clean(title), "error": "seasons are for a series"}));
            continue;
        }
        let mut entry = NewEntry {
            item: None,
            kind,
            title: title.into(),
            year: placeholder.year,
            tmdb_id,
            series_title: (kind == ItemKind::Episode).then(|| title.to_owned()),
            season: placeholder.season,
            episode: placeholder.episode,
            season_from: p.seasons.as_ref().map(|s| s.from),
            season_to: p.seasons.as_ref().map(|s| s.to),
        };
        // A placeholder cannot smuggle a known locked or concealed title into a list. A
        // matching, visible title attaches immediately, just as an import would attach it.
        if let Some(item) = store::match_placeholder(q.conn, &entry)? {
            let visible_locked = match item.kind {
                ItemKind::Movie | ItemKind::Series => q.vod_index(&item)?.map(|idx| q.title_locked(item.kind == ItemKind::Movie, idx)),
                ItemKind::Episode => q.episode(&item)?.map(|(_, idx)| q.title_locked(false, idx)),
                ItemKind::Channel => None,
            };
            if q.conceals(&item) || visible_locked.is_none() {
                refused.push(json!({"index": refs.len() + index, "error": "not_found"}));
                continue;
            }
            if visible_locked == Some(true) {
                refused.push(json!({"title": clean(title), "error": "locked: needs your PIN in Tuner"}));
                continue;
            }
            if kind == ItemKind::Episode {
                entry = store::new_entry(q.conn, &item)?;
            } else {
                entry.item = Some(item);
            }
        }
        entries.push(entry);
    }
    Ok((entries, refused))
}

fn selected_ids(view: &ListView, p: &Watchlist) -> Out<Vec<WatchlistItemId>> {
    let ids = p.ids.as_deref().filter(|ids| !ids.is_empty()).ok_or_else(|| Fail::bad("give ids from show"))?;
    if ids.len() > 300 || ids.iter().any(|id| !view.entries.iter().any(|e| e.entry.id.0 == *id)) {
        return Err(Fail::not_found("watchlist entry"));
    }
    Ok(ids.iter().map(|id| WatchlistItemId(*id)).collect())
}

async fn ask(h: &Arc<Inner>, command: QueueCommand) -> Out<QueueReply> {
    let engine = h.shared.queue_handle().ok_or_else(|| Fail::new(ErrorCode::Unavailable, "Up next isn't running in this Tuner"))?;
    match engine.request(command).await {
        QueueReply::Error(message) => Err(Fail::new(ErrorCode::Unavailable, message)),
        reply => Ok(reply),
    }
}

pub(super) async fn handle(h: &Arc<Inner>, p: Watchlist, who: &crate::model::AgentName) -> Out<Value> {
    let action = p.action;
    let opened = super::act::parse_open_in(p.open_in.as_deref())?;
    if opened.is_some() && action != WatchlistAction::Play {
        return Err(Fail::bad("open_in is for play"));
    }
    if action == WatchlistAction::Lists {
        return h
            .run(true, |q| {
                Ok(json!(
                    store::snapshot_with_preferences(q.conn, q.lib, q.view.unlocked, true, q.view.hides_explicit, q.cfg.interface.copy_order)?
                        .lists
                        .iter()
                        .map(list_out)
                        .collect::<Vec<_>>()
                ))
            })
            .await;
    }
    if action == WatchlistAction::Stop {
        let snapshot = h.shared.queue();
        let context = snapshot
            .watchlist
            .as_ref()
            .filter(|list| list.playing_through)
            .ok_or_else(|| Fail::new(ErrorCode::Unavailable, "No watchlist is playing"))?;
        let list = context.id;
        let current = snapshot.current.filter(|_| snapshot.driving).and_then(|id| snapshot.row(id)).map(|row| row.entry.clone());
        let (list, current) = h
            .run(true, move |q| {
                let view =
                    store::view_with_preferences(q.conn, q.lib, list, q.view.unlocked, true, q.view.hides_explicit, q.cfg.interface.copy_order)?
                        .ok_or_else(|| Fail::not_found("watchlist"))?;
                let title = current.filter(|row| !q.conceals(&row.item)).map(|row| clean(&row.title));
                Ok((clean(&view.list.name), title))
            })
            .await?;
        ask(h, QueueCommand::StopList).await?;
        tracing::info!("agent: watchlist stop");
        return Ok(json!({"list": clean(&list), "stopped_after": current}));
    }
    if action == WatchlistAction::Create {
        let name = text("name", p.name.as_deref().ok_or_else(|| Fail::bad("create needs name"))?)?.to_owned();
        let note = p.note.as_deref().map(|s| text("note", s).map(str::to_owned)).transpose()?;
        let order = play_order(p.play_order.as_deref())?;
        let by = who.name().to_owned();
        let (entries, mut refused) = h.run(true, move |q| additions(q, &p)).await?;
        let lib = h.shared.library();
        let now = crate::model::now();
        let (id, result) = h
            .write(DataScope::Watchlists, move |c| {
                let tx = c.unchecked_transaction()?;
                let id = store::create(&tx, &name, note.as_deref(), order, &by, now)?;
                let result = store::add(&tx, &lib, id, &entries, Insert::End, now)?;
                tx.commit()?;
                Ok((id, result))
            })
            .await?;
        refused.extend(result.refused.into_iter().map(|(index, why)| json!({"index": index, "error": clean(&why)})));
        let list = h
            .run(true, move |q| {
                Ok(store::view_with_preferences(q.conn, q.lib, id, q.view.unlocked, true, q.view.hides_explicit, q.cfg.interface.copy_order)?
                    .as_ref()
                    .map(list_out))
            })
            .await?;
        tracing::info!("agent: watchlist create #{} → {} added", id.0, result.added.len());
        return Ok(json!({"list": list, "added": result.added.iter().map(|i| i.0).collect::<Vec<_>>(), "refused": refused}));
    }
    let selector = p.list.clone().ok_or_else(|| Fail::bad("give a list name or id"))?;
    let view = match h.run(true, move |q| select(q, &selector)).await? {
        Selection::Ambiguous(names) => return Ok(json!({"ambiguous": names})),
        Selection::DidYouMean(names) => return Ok(json!({"did_you_mean": names})),
        Selection::Found(view) => view,
    };
    let list = view.list.id;
    let list_name = clean(&view.list.name);
    match action {
        WatchlistAction::Show => Ok(show_out(&view, p.include_done.unwrap_or(false))),
        WatchlistAction::Next => resolve_next(h, *view).await,
        WatchlistAction::Play => {
            let entry = match p.id {
                Some(id) => view.entries.iter().find(|e| e.entry.id.0 == id).ok_or_else(|| Fail::not_found("watchlist entry"))?,
                None => view.next_entry().ok_or_else(|| Fail::new(ErrorCode::Unavailable, "Nothing in this watchlist can play now"))?,
            };
            if entry.locked {
                return Err(Fail::new(ErrorCode::Unavailable, "locked: needs your PIN in Tuner"));
            }
            if entry.playable.is_none() {
                return Err(Fail::new(ErrorCode::Unavailable, "Not in your playlists"));
            }
            let id = entry.entry.id;
            let just_one = p.just_one.unwrap_or(false);
            let reply = ask(h, QueueCommand::PlayListAgent { list, from: Some(id), just_one }).await?;
            let opened = super::queue::show(h, &reply, opened, true).await;
            let mut out = match reply {
                QueueReply::Play(PlayReply::Playing(_)) => json!({"playing": entry_out(entry)}),
                QueueReply::Play(PlayReply::WaitingForPin(_)) => json!({"waiting_for_pin": true}),
                QueueReply::Play(PlayReply::Waiting(_, reason)) => json!({"waiting": format!("{reason:?}").to_lowercase()}),
                _ => json!({"playing": null}),
            };
            out["list"] = json!(list_name);
            if let Some(opened) = opened {
                out["opened_in"] = json!(opened);
            }
            if !just_one && out.get("playing").is_some_and(|v| !v.is_null()) {
                out["then"] = json!({"list": clean(&view.list.name), "after": "Up next"});
            }
            tracing::info!("agent: watchlist play #{} entry #{}", list.0, id.0);
            Ok(out)
        }
        WatchlistAction::Add => {
            let at = match position(p.at.as_deref())? {
                Insert::Index(index) => view.entries.get(index).map_or(Insert::End, |e| Insert::Index(e.entry.position)),
                Insert::Next => view.next_entry().map_or(Insert::End, |e| Insert::Index(e.entry.position + 1)),
                Insert::End => Insert::End,
            };
            let (entries, mut refused) = h.run(true, move |q| additions(q, &p)).await?;
            let lib = h.shared.library();
            let result = h.write(DataScope::Watchlists, move |c| store::add(c, &lib, list, &entries, at, crate::model::now())).await?;
            refused.extend(result.refused.into_iter().map(|(index, why)| json!({"index": index, "error": clean(&why)})));
            tracing::info!("agent: watchlist add #{} → {} added", list.0, result.added.len());
            Ok(json!({"list": list_name, "added": result.added.iter().map(|id| id.0).collect::<Vec<_>>(), "refused": refused}))
        }
        WatchlistAction::Move | WatchlistAction::Remove | WatchlistAction::Skip => {
            let ids = selected_ids(&view, &p)?;
            let returned = ids.iter().map(|id| id.0).collect::<Vec<_>>();
            let to = if action == WatchlistAction::Move {
                match position(Some(p.to.as_deref().ok_or_else(|| Fail::bad("move needs to"))?))? {
                    Insert::End => usize::MAX,
                    Insert::Next => view.next_entry().map_or(0, |next| {
                        next.entry.position + 1
                            - view.entries.iter().filter(|e| e.entry.position <= next.entry.position && ids.contains(&e.entry.id)).count()
                    }),
                    Insert::Index(index) => view.entries.get(index).map_or(usize::MAX, |e| e.entry.position),
                }
            } else {
                0
            };
            h.write(DataScope::Watchlists, move |c| {
                match action {
                    WatchlistAction::Move => store::move_entries(c, list, &ids, to)?,
                    WatchlistAction::Remove => {
                        store::remove(c, list, &ids)?;
                    }
                    WatchlistAction::Skip => {
                        for id in &ids {
                            let on = p.skipped.unwrap_or_else(|| !view.entries.iter().any(|e| e.entry.id == *id && e.entry.skipped));
                            store::skip(c, list, &[*id], on)?;
                        }
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })
            .await?;
            tracing::info!("agent: watchlist edit #{} → {} entries", list.0, returned.len());
            Ok(json!({"list": list_name, "items": returned}))
        }
        WatchlistAction::Restart | WatchlistAction::Rename | WatchlistAction::Delete => {
            let name = p.name.as_deref().map(|s| text("name", s).map(str::to_owned)).transpose()?;
            let note = p.note.as_deref().map(|s| text("note", s).map(str::to_owned)).transpose()?;
            if action == WatchlistAction::Rename && name.is_none() {
                return Err(Fail::bad("rename needs name"));
            }
            let order = p.play_order.as_deref().map(|order| play_order(Some(order))).transpose()?;
            let before = list_out(&view);
            h.write(DataScope::Watchlists, move |c| {
                match action {
                    WatchlistAction::Restart => {
                        store::restart(c, list, crate::model::now())?;
                    }
                    WatchlistAction::Rename => {
                        store::rename(c, list, name.as_deref().unwrap(), note.as_deref().or(view.list.note.as_deref()))?;
                        if let Some(order) = order {
                            store::set_order(c, list, order)?;
                        }
                    }
                    WatchlistAction::Delete => {
                        store::delete(c, list)?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })
            .await?;
            if action == WatchlistAction::Delete {
                if h.shared.queue().context == Some(list) {
                    ask(h, QueueCommand::StopList).await?;
                }
                return Ok(json!({"list": before}));
            }
            let after = h
                .run(true, move |q| {
                    Ok(store::view_with_preferences(q.conn, q.lib, list, q.view.unlocked, true, q.view.hides_explicit, q.cfg.interface.copy_order)?
                        .as_ref()
                        .map(list_out))
                })
                .await?;
            tracing::info!("agent: watchlist edit #{}", list.0);
            Ok(json!({"list": after}))
        }
        WatchlistAction::Lists | WatchlistAction::Create | WatchlistAction::Stop => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_accept_initials_and_partial_words_without_choosing_between_hits() {
        assert!(matching("Star Wars", "SW"));
        assert!(matching("Star Wars", "star wars"));
        assert!(!matching("Star Wars in story order", "str wrs"));
        assert!(!matching("Marvel in release order", "Mario"));
        assert!(matching("Star Wars in story order", "sta wa"));
        assert!(!matching("Star Wars", "ars"));
        assert!(matching("Star Wars", "star"));
        assert!(matching("Star Trek", "star"));
        assert!(!matching("Christmas films", "SW"));
    }
}
