//! Catalogue and copy decisions happen on a worker, when adding or starting a job
//! (DESIGN §43 §3). Stored jobs contain identities and display facts, never provider URLs.

use rusqlite::{Connection, OptionalExtension, params};

use crate::config::Config;
use crate::downloads::{Job, Kind, SeriesScope, State, Target};
use crate::error::{Error, Result};
use crate::model::{ChannelId, DownloadId, ItemKind, ItemRef, MediaRef, SeriesId, Timestamp, VodKind};
use crate::player::budget::SharedBudget;
use crate::shared::ParentalView;
use crate::store::{self, library::Library};

pub fn series_id(c: &Connection, item: &ItemRef) -> Result<SeriesId> {
    if item.kind == ItemKind::Episode {
        let ep = store::vod::resolve_episode(c, item)?.ok_or_else(|| Error::other("not_found"))?;
        return Ok(store::vod::episode(c, ep)?.series_id);
    }
    c.query_row("SELECT id FROM series WHERE source_id=?1 AND key=?2", params![item.source, item.key.as_str()], |r| r.get(0))
        .optional()?
        .ok_or_else(|| Error::other("not_found"))
}

pub fn media(c: &Connection, j: &Job) -> Result<MediaRef> {
    let item = j.item().ok_or_else(|| Error::other("not_found"))?;
    match j.kind {
        Kind::Movie => store::vod::resolve_movie(c, &item)?.map(MediaRef::Movie),
        Kind::Episode => store::vod::resolve_episode(c, &item)?.map(MediaRef::Episode),
        Kind::Live | Kind::Catchup => {
            let id: Option<ChannelId> = c
                .query_row("SELECT id FROM channel WHERE source_id=?1 AND key=?2", params![item.source, item.key.as_str()], |r| r.get(0))
                .optional()?;
            id.map(|channel| {
                if j.kind == Kind::Catchup {
                    MediaRef::Catchup { channel, start: j.start().unwrap_or(0), stop: j.stop().unwrap_or(0), catchup_id: j.catchup_id.clone() }
                } else {
                    MediaRef::Channel(channel)
                }
            })
        }
    }
    .ok_or_else(|| Error::other("not_found"))
}

pub fn concealed(c: &Connection, lib: &Library, view: ParentalView, j: &Job) -> bool {
    if let Some(item) = j.item() {
        lib.conceals(&item) || (view.hides_explicit && store::live::item_explicit(c, &item).unwrap_or(true))
    } else {
        view.hides_explicit && j.explicit()
    }
}

pub fn locked(c: &Connection, lib: &Library, job: &Job) -> bool {
    let Some(item) = job.item() else { return job.fp.get("download_locked").and_then(|v| v.as_bool()).unwrap_or(true) };
    let result = match item.kind {
        ItemKind::Channel => lib.live.by_ref(&item).and_then(|id| lib.live.get(id)).map(|ch| ch.locked),
        ItemKind::Movie => store::vod::resolve_movie(c, &item)
            .ok()
            .flatten()
            .and_then(|id| lib.vod.index_of(VodKind::Movie(id)))
            .and_then(|i| lib.vod.movies.get(i as usize))
            .map(|m| m.locked),
        ItemKind::Episode => series_id(c, &item)
            .ok()
            .and_then(|id| lib.vod.index_of(VodKind::Series(id)))
            .and_then(|i| lib.vod.series.get(i as usize))
            .map(|s| s.locked),
        _ => None,
    };
    result.unwrap_or_else(|| job.fp.get("download_locked").and_then(|v| v.as_bool()).unwrap_or(true))
}

pub fn new_job(c: &Connection, lib: &Library, view: ParentalView, cfg: &Config, target: &Target, agent: bool, now: Timestamp) -> Result<Job> {
    let item = &target.item;
    if lib.conceals(item) || (view.hides_explicit && store::live::item_explicit(c, item).unwrap_or(true)) {
        return Err(Error::other("not_found"));
    }
    let mut fp = store::user::fingerprint_of(c, item)?;
    fp.url_key = None;
    let mut fp = serde_json::to_value(fp).map_err(Error::other)?;
    if matches!(item.kind, ItemKind::Movie | ItemKind::Episode) {
        fp["download_copies"] = serde_json::to_value(store::watchlist::title_copies(c, lib, item)?).map_err(Error::other)?;
    }
    fp["download_explicit"] = store::live::item_explicit(c, item).unwrap_or(true).into();
    let (kind, title, subtitle, duration, locked) = match item.kind {
        ItemKind::Movie => {
            let id = store::vod::resolve_movie(c, item)?.ok_or_else(|| Error::other("not_found"))?;
            let m = store::vod::movie(c, id)?;
            let locked = lib.vod.index_of(VodKind::Movie(id)).and_then(|i| lib.vod.movies.get(i as usize)).map_or(m.locked, |m| m.locked);
            (Kind::Movie, m.display_name().to_owned(), m.year.map(|y| y.to_string()), m.duration_s, locked)
        }
        ItemKind::Episode => {
            let id = store::vod::resolve_episode(c, item)?.ok_or_else(|| Error::other("not_found"))?;
            let ep = store::vod::episode(c, id)?;
            let series = store::vod::series(c, ep.series_id)?;
            let locked =
                lib.vod.index_of(VodKind::Series(series.id)).and_then(|i| lib.vod.series.get(i as usize)).map_or(series.locked, |s| s.locked);
            fp["download_season"] = ep.season.into();
            fp["download_episode"] = ep.episode.into();
            fp["download_series_key"] = series.key.as_str().into();
            (
                Kind::Episode,
                series.display_name().to_owned(),
                Some(format!("S{:02}E{:02} · {}", ep.season, ep.episode, ep.title)),
                ep.duration_s,
                locked,
            )
        }
        ItemKind::Channel => {
            let id: ChannelId =
                c.query_row("SELECT id FROM channel WHERE source_id=?1 AND key=?2", params![item.source, item.key.as_str()], |r| r.get(0))?;
            let channel = store::live::channel(c, id)?;
            let slot = target.programme.as_ref();
            let title = slot
                .and_then(|s| {
                    channel
                        .epg_channel_id
                        .and_then(|epg| {
                            store::epg::window(
                                c,
                                epg,
                                s.start - i64::from(channel.tvg_shift_min) * 60,
                                s.start - i64::from(channel.tvg_shift_min) * 60 + 1,
                            )
                            .ok()
                        })
                        .and_then(|ps| ps.first().map(|p| p.title.to_string()))
                })
                .unwrap_or_else(|| channel.display_name().to_owned());
            let kind = if slot.is_some_and(|s| s.stop <= now) { Kind::Catchup } else { Kind::Live };
            let locked = lib.live.get(id).map_or(channel.locked, |ch| ch.locked);
            (kind, title, Some(channel.display_name().to_owned()), None, locked)
        }
        ItemKind::Series => return Err(Error::other("Choose the episodes to download")),
    };
    if agent && locked {
        return Err(Error::other("locked: needs your PIN in Tuner"));
    }
    fp["download_locked"] = locked.into();
    fp["download_programme"] = target.programme.is_some().into();
    if let Some(duration) = duration {
        fp["download_duration_s"] = duration.into();
    }
    let (start, stop, catchup_id) = if let Some(slot) = &target.programme {
        (Some(slot.start), Some(slot.stop), slot.catchup_id.clone())
    } else if kind == Kind::Live {
        let minutes = target.minutes.unwrap_or(30);
        if !(1..=1440).contains(&minutes) {
            return Err(Error::other("Record for between 1 and 1440 minutes"));
        }
        (Some(now), Some(now + i64::from(minutes) * 60), None)
    } else {
        (None, None, None)
    };
    Ok(Job {
        id: DownloadId(0),
        position: 0,
        kind,
        source_id: Some(item.source),
        item_key: Some(item.key.clone()),
        any_copy: target.any_copy,
        prog_start: start,
        prog_stop: stop,
        pad_before_s: if target.programme.is_some() { cfg.downloads.pad_before_s } else { 0 },
        pad_after_s: if target.programme.is_some() { cfg.downloads.pad_after_s } else { 0 },
        catchup_id,
        title,
        subtitle,
        state: if kind == Kind::Live && start.is_some_and(|s| s > now) { State::Scheduled } else { State::Waiting },
        reason: None,
        store_name: None,
        bytes_done: 0,
        bytes_total: None,
        etag: None,
        last_modified: None,
        attempts: 0,
        added_at: now,
        added_by: if agent { "agent" } else { "app" }.into(),
        done_at: None,
        watched_at: None,
        fp,
        orphaned_since: None,
    })
}

pub fn series_targets(c: &Connection, item: &ItemRef, scope: SeriesScope) -> Result<Vec<Target>> {
    let id = series_id(c, item)?;
    let episodes = store::vod::episodes(c, id, if let SeriesScope::Season(s) = scope { Some(s) } else { None })?;
    let limit = match scope {
        SeriesScope::Next(n) | SeriesScope::Keep(n) => n as usize,
        _ => usize::MAX,
    };
    let mut targets = Vec::new();
    for ep in episodes {
        let item = ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key };
        if matches!(scope, SeriesScope::Next(_) | SeriesScope::Keep(_)) && store::user::progress(c, &item)?.is_some_and(|p| p.finished) {
            continue;
        }
        targets.push(Target { item, programme: None, minutes: None, any_copy: true });
        if targets.len() >= limit {
            break;
        }
    }
    Ok(targets)
}

/// Copy planning is deferred until the worker starts, so another account can make progress
/// while the preferred one is in use. A resumed part stays pinned to its validators.
pub fn pick(c: &Connection, lib: &Library, cfg: &Config, budget: &SharedBudget, j: &Job) -> Result<Option<ItemRef>> {
    let Some(item) = j.item() else { return Ok(None) };
    if !j.any_copy || j.bytes_done > 0 || j.kind.recording() {
        return Ok((budget.admits(j.id, item.source) && budget.download_holder(item.source).is_none()).then_some(item));
    }
    let (movies, index, ep) = match media(c, j)? {
        MediaRef::Movie(id) => (true, lib.vod.index_of(VodKind::Movie(id)), None),
        MediaRef::Episode(id) => {
            let ep = store::vod::episode(c, id)?;
            (false, lib.vod.index_of(VodKind::Series(ep.series_id)), Some(ep))
        }
        _ => return Ok(budget.admits(j.id, item.source).then_some(item)),
    };
    let Some(index) = index else {
        return Ok((budget.admits(j.id, item.source) && budget.download_holder(item.source).is_none()).then_some(item));
    };
    // Downloading a locked title in the app is permitted; playback still asks for the PIN.
    let plan = store::copies::plan(c, &lib.vod, movies, index, true, cfg.interface.copy_order, crate::model::now())?;
    for index in plan.order.iter().copied() {
        let Some(copy) = plan.get(index) else { continue };
        if (j.added_by == "agent" && copy.locked) || lib.conceals(&copy.item_ref) {
            continue;
        }
        if !budget.admits(j.id, copy.item_ref.source) || budget.download_holder(copy.item_ref.source).is_some() {
            continue;
        }
        if let Some(ep) = &ep {
            let Some(VodKind::Series(series)) = lib.vod.series.get(index as usize).map(|s| s.kind) else { continue };
            if let Some(id) = store::copies::same_episode(c, series, ep.season, ep.episode)? {
                let ep = store::vod::episode(c, id)?;
                return Ok(Some(ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key }));
            }
        } else {
            return Ok(Some(copy.item_ref.clone()));
        }
    }
    Ok(None)
}

/// Copies offered by the clash prompt. Live channels use the shared ranking; VOD uses the
/// same title stack / S/E match as ordinary playback. Busy accounts are never offered.
pub fn alternate(c: &Connection, lib: &Library, budget: &SharedBudget, job: &Job) -> Result<Option<ItemRef>> {
    let Some(item) = job.item() else { return Ok(None) };
    for copy in alternatives(c, lib, &item)? {
        if copy.source != item.source && budget.admits(job.id, copy.source) && budget.download_holder(copy.source).is_none() {
            return Ok(Some(copy));
        }
    }
    Ok(None)
}
pub fn alternatives(c: &Connection, lib: &Library, item: &ItemRef) -> Result<Vec<ItemRef>> {
    if item.kind != ItemKind::Channel {
        return store::watchlist::title_copies(c, lib, item);
    }
    let Some(id) = lib.live.by_ref(item) else { return Ok(vec![]) };
    let copies = store::copies::channel_copies(lib, c, id)?;
    let candidates: Vec<_> =
        copies.iter().filter(|x| !x.locked && !x.explicit).filter_map(|x| lib.live.get(x.id)).map(store::copies::RankCand::from).collect();
    let rank = store::copies::RankCtx { source_order: lib.sources.iter().map(|s| s.id).collect(), ..Default::default() };
    Ok(store::copies::rank_channels(&candidates, &rank)
        .iter()
        .filter_map(|(id, _)| lib.live.get(*id))
        .map(|ch| ItemRef { source: ch.source_id, kind: ItemKind::Channel, key: ch.key.clone() })
        .collect())
}

/// Refresh copy-specific metadata together with the item identity (§3, §6).
pub fn change_copy(c: &Connection, lib: &Library, cfg: &Config, job: &mut Job, item: ItemRef) -> Result<()> {
    let target = Target {
        item,
        programme: job.prog_start.zip(job.prog_stop).map(|(start, stop)| crate::queue::Slot { start, stop, catchup_id: job.catchup_id.clone() }),
        minutes: None,
        any_copy: job.any_copy,
    };
    let view = ParentalView { unlocked: true, hides_explicit: lib.conceals(&target.item), pin_active: false };
    let fresh = new_job(c, lib, view, cfg, &target, job.added_by == "agent", job.added_at)?;
    job.source_id = fresh.source_id;
    job.item_key = fresh.item_key;
    job.title = fresh.title;
    job.subtitle = fresh.subtitle;
    if let (Some(saved), Some(new)) = (job.fp.as_object_mut(), fresh.fp.as_object()) {
        saved.extend(new.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    Ok(())
}
