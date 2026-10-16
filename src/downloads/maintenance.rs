//! Guide identity, Following recordings and Keep N refill (DESIGN §43 §3).
//! The service calls this off the UI thread; it shares Following's ledger, not its reminders.
use crate::config::Config;
use crate::downloads::{Job, Kind, SeriesScope, State, Target, planning};
use crate::error::Result;
use crate::model::{ItemKind, ItemRef};
use crate::shared::ParentalView;
use crate::store::{self, downloads as db, library::Library};
use rusqlite::Connection;

fn same(job: &Job, item: &ItemRef) -> bool {
    job.item().as_ref() == Some(item)
        || job.fp.get("download_copies").and_then(|v| serde_json::from_value::<Vec<ItemRef>>(v.clone()).ok()).is_some_and(|v| v.contains(item))
}

pub fn progress(c: &Connection, lib: &Library, item: &ItemRef) -> Result<Option<crate::model::WatchProgress>> {
    let mut latest = None;
    for item in store::watchlist::title_copies(c, lib, item)? {
        if let Some(p) = store::user::progress(c, &item)?
            && latest.as_ref().is_none_or(|old: &crate::model::WatchProgress| p.updated_at > old.updated_at)
        {
            latest = Some(p);
        }
    }
    Ok(latest)
}

pub fn refill(c: &Connection, lib: &Library, cfg: &Config, view: ParentalView, now: i64) -> Result<()> {
    for rule in db::rules(c)? {
        if rule.orphaned_since.is_some() {
            continue;
        }
        let series = ItemRef { source: rule.source_id, kind: ItemKind::Series, key: rule.series_key };
        let Ok(targets) = planning::series_targets(c, &series, SeriesScope::All) else { continue };
        let mut rows = db::jobs(c)?;
        let mut count = targets
            .iter()
            .filter(|t| {
                rows.iter()
                    .any(|j| same(j, &t.item) && !matches!(j.state, State::Cancelled | State::Failed | State::Missed) && j.watched_at.is_none())
            })
            .count();
        for target in targets {
            if count >= rule.keep as usize {
                break;
            }
            if progress(c, lib, &target.item)?.is_some_and(|p| p.finished)
                || rows.iter().any(|j| same(j, &target.item) && (j.state != State::Failed || j.added_at >= rule.added_at))
            {
                continue;
            }
            let Ok(mut job) = planning::new_job(c, lib, view, cfg, &target, false, now) else { continue };
            job.fp["download_keep_rule"] = rule.id.into();
            if let Ok(id) = db::add(c, &job) {
                job.id = id;
                rows.push(job);
                count += 1;
            }
        }
    }
    Ok(())
}

/// A recording follows the guide within half an hour, without silently replacing a title.
pub fn retime(c: &Connection, now: i64) -> Result<()> {
    for mut job in db::jobs(c)?
        .into_iter()
        .filter(|j| j.state == State::Scheduled && j.kind == Kind::Live && j.fp.get("download_programme").and_then(|v| v.as_bool()) != Some(false))
    {
        let Ok(crate::model::MediaRef::Channel(id)) = planning::media(c, &job) else { continue };
        let channel = store::live::channel(c, id)?;
        let (Some(epg), Some(start)) = (channel.epg_channel_id, job.prog_start) else { continue };
        if start < now {
            continue;
        }
        let shift = i64::from(channel.tvg_shift_min) * 60;
        let programmes = store::epg::window(c, epg, start - shift - 1800, start - shift + 1801)?;
        if let Some(p) = programmes
            .iter()
            .filter(|p| p.title.as_ref() == job.title && (p.start + shift - start).abs() <= 1800)
            .min_by_key(|p| (p.start + shift - start).abs())
        {
            job.prog_start = Some(p.start + shift);
            job.prog_stop = Some(p.stop + shift);
            if job.reason.as_deref() == Some("guide missing") {
                job.reason = None;
            }
        } else {
            job.reason = Some("guide missing".into());
        }
        db::put(c, &job)?;
    }
    Ok(())
}

/// Use channel-agnostic S/E or episode-title identity; a title without episode data is
/// recorded once, rather than treating its next day's repeat as new.
fn event_identity(key: &str) -> &str {
    key.split_once('@').map_or(key, |(title, _)| title)
}
fn identity(job: &Job) -> String {
    if let Some(key) = job.fp.get("download_event").and_then(|v| v.as_str()) {
        return key.to_owned();
    }
    let title = crate::textmatch::event::core_title(&job.title);
    match (job.fp.get("download_season").and_then(|v| v.as_u64()), job.fp.get("download_episode").and_then(|v| v.as_u64())) {
        (Some(s), Some(e)) => format!("{title}#s{s:02}e{e:02}"),
        _ => title,
    }
}

pub fn following(c: &Connection, lib: &Library, cfg: &Config, now: i64) -> Result<()> {
    let rules = store::follow::rules(c)?;
    let active: Vec<_> = rules.iter().filter(|r| cfg.follow.enabled && r.enabled && r.mode == crate::follow::FollowMode::Record).collect();
    let mut rows = db::jobs(c)?;
    // Deleting or pausing a rule cancels only its pending recordings, never saved files.
    for job in &mut rows {
        if let Some(rule) = job.fp.get("download_follow_rule").and_then(|v| v.as_i64())
            && !active.iter().any(|r| r.id.0 == rule)
            && matches!(job.state, State::Scheduled | State::Waiting | State::Paused)
        {
            job.state = State::Cancelled;
            job.reason = Some("rule removed".into());
            db::put(c, job)?;
        }
    }
    for airing in store::follow::upcoming(c, now, 1000)? {
        let Some(rule) = active.iter().find(|r| r.id == airing.rule) else { continue };
        if airing.state != crate::follow::plan::MatchState::Scheduled {
            continue;
        }
        let Some(item) = airing.channel else { continue };
        let event = event_identity(&airing.event_key);
        if let Some(existing) = rows.iter_mut().find(|j| identity(j) == event && j.reason.as_deref() != Some("rule removed")) {
            if existing.state == State::Scheduled
                && existing.fp.get("download_follow_rule").and_then(|v| v.as_i64()) == Some(rule.id.0)
                && existing.prog_start.is_some_and(|start| (start - airing.start).abs() <= 1800)
            {
                existing.prog_start = Some(airing.start);
                existing.prog_stop = Some(airing.stop);
                db::put(c, existing)?;
            }
            continue;
        }
        let target = Target {
            item,
            programme: Some(crate::queue::Slot { start: airing.start, stop: airing.stop, catchup_id: None }),
            minutes: None,
            any_copy: false,
        };
        let view = ParentalView { unlocked: !cfg.parental.pin_active(), hides_explicit: true, pin_active: cfg.parental.pin_active() };
        let Ok(mut job) = planning::new_job(c, lib, view, cfg, &target, rule.by_agent, now) else { continue };
        job.fp["download_event"] = event.into();
        job.fp["download_follow_rule"] = rule.id.0.into();
        job.fp["download_alternates"] = serde_json::to_value(airing.alternates).unwrap_or_default();
        if let Ok(id) = db::add(c, &job) {
            job.id = id;
            rows.push(job);
        }
    }
    Ok(())
}
