//! The action tools' decisions (DESIGN §30.4, §30.5, §30.8): what `play` plays and says,
//! what `schedule_switch` writes, whether `cancel_switch` may remove a row, and `status`.
//! Pure over a [`Q`]; the handler (`agent::server`) does the sending and writing. Owner:
//! MCP-app track.

use super::proto::{
    AgentRef, Airing, CancelSwitch, ErrorCode, NowPlayingOut, Play, PlayOut, PlayOutcome, PlayerStateOut, ScheduleSwitch, StatusOut, SwitchOut,
    fmt_time, item_ref,
};
use super::query::{Fail, Out, Q, channel_ref, parse_ref, when};
use super::{AgentCommand, AgentPlay, PlayTarget};
use crate::jobs::JobKind;
use crate::model::{ItemKind, MediaRef, PlayableKind, Reminder, ReminderId, ReminderState, VodKind};
use crate::player::{PlayerErrorKind, PlayerSnapshot, PlayerState};

/// Pending automatic switches allowed at once.
pub const MAX_SWITCHES: usize = 20;

/// What `play` decided.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayStep {
    /// Send this to the app (and raise the window); reply `out`.
    Ready { command: AgentCommand, out: PlayOut, log_ref: String },
    /// A series whose episodes aren't loaded yet: run the job, then decide again.
    NeedsDetails(JobKind),
}

pub fn plan_play(q: &Q, p: &Play) -> Out<PlayStep> {
    let from_start = p.from_start.unwrap_or(false);
    let r = parse_ref(&p.item)?;
    let log_ref = r.to_string();
    let ready = |target: PlayTarget, out: PlayOut| PlayStep::Ready {
        command: AgentCommand::Play(AgentPlay { target, from_start }),
        out,
        log_ref: log_ref.clone(),
    };
    let pin = |title: &str| format!("Tuner is asking for the parental PIN on its screen; {title} plays once it is typed there.");
    match r {
        AgentRef::Programme { channel, start } => {
            let c = q.channel_by_ref(&channel).ok_or_else(|| Fail::not_found("programme"))?;
            let prog = q.programme_at(c, start)?.ok_or_else(|| Fail::not_found("programme"))?;
            let locked = q.needs_pin(c.locked);
            let catchup_id = || -> Out<Option<String>> {
                Ok(if prog.flags.has_catchup_id { crate::store::epg::programme(q.conn, prog.id)?.catchup_id } else { None })
            };
            let archive = |out: PlayOutcome| -> Out<PlayStep> {
                let media = MediaRef::Catchup { channel: c.id, start: prog.start, stop: prog.stop, catchup_id: catchup_id()? };
                let message = if locked { pin(&prog.title) } else { format!("Playing {} from the archive on {}.", prog.title, c.name) };
                Ok(ready(
                    PlayTarget::Media(media),
                    PlayOut {
                        outcome: if locked { PlayOutcome::PinRequired } else { out },
                        item: item_ref(&channel_ref(c)),
                        title: prog.title.to_string(),
                        resume_at_min: None,
                        message,
                    },
                ))
            };
            match q.airing(&prog) {
                Airing::Upcoming => Err(Fail::new(
                    ErrorCode::NotStarted,
                    format!("{} starts at {} on {}: use schedule_switch to switch then.", prog.title, fmt_time(prog.start), c.name),
                )),
                Airing::Past if q.catchup(c, &prog) => archive(PlayOutcome::CatchUp),
                Airing::Past => Err(Fail::new(ErrorCode::Unavailable, format!("{} has ended and {} has no catch-up for it.", prog.title, c.name))),
                Airing::Live if from_start && q.catchup(c, &prog) => archive(PlayOutcome::CatchUp),
                Airing::Live => Ok(ready(
                    PlayTarget::Media(MediaRef::Channel(c.id)),
                    PlayOut {
                        outcome: if locked { PlayOutcome::PinRequired } else { PlayOutcome::Playing },
                        item: item_ref(&channel_ref(c)),
                        title: prog.title.to_string(),
                        resume_at_min: None,
                        message: if locked { pin(&c.name) } else { format!("Switched to {} for {}.", c.name, prog.title) },
                    },
                )),
            }
        }
        AgentRef::Item(item) => match item.kind {
            ItemKind::Channel => {
                let c = q.channel_by_ref(&item).ok_or_else(|| Fail::not_found("channel"))?;
                let locked = q.needs_pin(c.locked);
                Ok(ready(
                    PlayTarget::Media(MediaRef::Channel(c.id)),
                    PlayOut {
                        outcome: if locked { PlayOutcome::PinRequired } else { PlayOutcome::Playing },
                        item: item_ref(&item),
                        title: c.name.to_string(),
                        resume_at_min: None,
                        message: if locked { pin(&c.name) } else { format!("Playing {}.", c.name) },
                    },
                ))
            }
            ItemKind::Movie => {
                let idx = q.vod_index(&item)?.ok_or_else(|| Fail::not_found("movie"))?;
                let v = &q.lib.vod.movies[idx as usize];
                let VodKind::Movie(id) = v.kind else { return Err(Fail::not_found("movie")) };
                let locked = q.needs_pin(q.title_locked(true, idx));
                let resume = if from_start {
                    None
                } else {
                    q.title_out(true, idx).progress.filter(|p| !p.finished && p.position_min > 0).map(|p| p.position_min)
                };
                let message = match (locked, resume) {
                    (true, _) => pin(&v.name),
                    (false, Some(m)) => format!("Playing {} from {m} min.", v.name),
                    (false, None) => format!("Playing {}.", v.name),
                };
                Ok(ready(
                    PlayTarget::Media(MediaRef::Movie(id)),
                    PlayOut {
                        outcome: if locked { PlayOutcome::PinRequired } else { PlayOutcome::Playing },
                        item: item_ref(&item),
                        title: v.name.to_string(),
                        resume_at_min: resume.filter(|_| !locked),
                        message,
                    },
                ))
            }
            ItemKind::Series => {
                let idx = q.vod_index(&item)?.ok_or_else(|| Fail::not_found("series"))?;
                let v = &q.lib.vod.series[idx as usize];
                let VodKind::Series(id) = v.kind else { return Err(Fail::not_found("series")) };
                let locked = q.needs_pin(q.title_locked(false, idx));
                if locked {
                    return Ok(ready(
                        PlayTarget::Series(id),
                        PlayOut {
                            outcome: PlayOutcome::PinRequired,
                            item: item_ref(&item),
                            title: v.name.to_string(),
                            resume_at_min: None,
                            message: pin(&v.name),
                        },
                    ));
                }
                let Some((_, ep)) = crate::store::copies::next_episode(q.conn, &q.series_copies(idx))? else {
                    let lazy = q.lib.source(v.source_id).is_some_and(|s| s.kind == crate::model::SourceKind::Xtream)
                        && crate::store::vod::series(q.conn, id)?.details_loaded_at.is_none();
                    if lazy {
                        return Ok(PlayStep::NeedsDetails(JobKind::LoadSeriesDetails(id)));
                    }
                    return Err(Fail::new(ErrorCode::Unavailable, format!("{} has no episodes to play.", v.name)));
                };
                episode_step(q, &ep, v.name.as_ref(), from_start, PlayTarget::Series(id), &log_ref)
            }
            ItemKind::Episode => {
                let (ep, idx) = q.episode(&item)?.ok_or_else(|| Fail::not_found("episode"))?;
                let series = &q.lib.vod.series[idx as usize];
                if q.needs_pin(q.title_locked(false, idx)) {
                    let title = format!("{} — S{:02}E{:02}", series.name, ep.season, ep.episode);
                    return Ok(ready(
                        PlayTarget::Media(MediaRef::Episode(ep.id)),
                        PlayOut { outcome: PlayOutcome::PinRequired, item: item_ref(&item), message: pin(&title), title, resume_at_min: None },
                    ));
                }
                episode_step(q, &ep, series.name.as_ref(), from_start, PlayTarget::Media(MediaRef::Episode(ep.id)), &log_ref)
            }
        },
    }
}

fn episode_step(q: &Q, ep: &crate::model::Episode, series: &str, from_start: bool, target: PlayTarget, log_ref: &str) -> Out<PlayStep> {
    let out = q.episode_out(ep);
    let title = format!("{series} — S{:02}E{:02} · {}", ep.season, ep.episode, ep.title);
    let resume = if from_start { None } else { out.progress.filter(|p| !p.finished && p.position_min > 0).map(|p| p.position_min) };
    let message = match resume {
        Some(m) => format!("Playing {title} from {m} min."),
        None => format!("Playing {title}."),
    };
    Ok(PlayStep::Ready {
        command: AgentCommand::Play(AgentPlay { target, from_start }),
        out: PlayOut { outcome: PlayOutcome::Playing, item: out.item, title, resume_at_min: resume, message },
        log_ref: log_ref.to_owned(),
    })
}

/// What `schedule_switch` writes and replies (the id comes from the write).
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchPlan {
    pub reminder: Reminder,
    pub out: SwitchOut,
    pub log_ref: String,
}

pub fn plan_switch(q: &Q, p: &ScheduleSwitch) -> Out<SwitchPlan> {
    let r = parse_ref(&p.item)?;
    let log_ref = r.to_string();
    let (c, title, start, stop) = match &r {
        AgentRef::Programme { channel, start } => {
            if p.at.is_some() {
                return Err(Fail::bad("at is only for a channel ref: a programme switches at its own start"));
            }
            let c = q.channel_by_ref(channel).ok_or_else(|| Fail::not_found("programme"))?;
            let prog = q.programme_at(c, *start)?.ok_or_else(|| Fail::not_found("programme"))?;
            (c, Some(prog.title.to_string()), prog.start, prog.stop)
        }
        AgentRef::Item(item) if item.kind == ItemKind::Channel => {
            let c = q.channel_by_ref(item).ok_or_else(|| Fail::not_found("channel"))?;
            let at = p.at.as_deref().ok_or_else(|| Fail::bad("a channel ref needs at (when to switch)"))?;
            let at = when("at", at, q.now)?;
            // the programme starting then, when the guide has one
            let prog = q.programme_at(c, at)?;
            let stop = prog.as_ref().map_or(at + 3600, |p| p.stop);
            (c, prog.map(|p| p.title.to_string()), at, stop)
        }
        AgentRef::Item(_) => return Err(Fail::bad("schedule_switch takes a programme ref, or a channel ref with at")),
    };
    if start <= q.now {
        return Err(Fail::new(ErrorCode::Unavailable, "That has already started: use play instead."));
    }
    let reminder = Reminder {
        id: ReminderId(0),
        channel: channel_ref(c),
        channel_name: c.name.to_string(),
        programme_title: title.clone().unwrap_or_default(),
        start,
        stop,
        lead_s: Reminder::SWITCH_LEAD,
        state: ReminderState::Pending,
    };
    let out = SwitchOut { id: 0, channel: q.channel_out(c), title, start: fmt_time(start), warn_at: fmt_time(start - Reminder::SWITCH_WARN_S) };
    Ok(SwitchPlan { reminder, out, log_ref })
}

/// A pending switch the agent may cancel (`not_found` for anything else, a concealed one too).
pub fn check_cancel(q: &Q, p: &CancelSwitch) -> Out<ReminderId> {
    let id = ReminderId(p.id);
    match crate::store::user::reminder(q.conn, id)? {
        Some(r) if r.switches() && r.state == ReminderState::Pending && !q.lib.conceals(&r.channel) => Ok(id),
        _ => Err(Fail::not_found("scheduled switch")),
    }
}

/// The scheduled switches `status` lists (visible channels only).
pub fn scheduled(q: &Q) -> Out<Vec<SwitchOut>> {
    let late = q.now - crate::platform::notify::LATE_SWITCH_S;
    Ok(crate::store::user::pending_switches(q.conn)?
        .into_iter()
        .filter(|r| r.start >= late)
        .filter_map(|r| {
            let c = q.channel_by_ref(&r.channel)?;
            let title = Some(r.programme_title.clone()).filter(|t| !t.trim().is_empty());
            Some(SwitchOut {
                id: r.id.0,
                channel: q.channel_out(c),
                title,
                start: fmt_time(r.start),
                warn_at: fmt_time(r.start - Reminder::SWITCH_WARN_S),
            })
        })
        .collect())
}

pub fn status(q: &Q, snap: &PlayerSnapshot) -> Out<StatusOut> {
    let state = match &snap.state {
        PlayerState::Idle | PlayerState::Ended => PlayerStateOut::Idle,
        PlayerState::Zapping { .. } | PlayerState::Loading { .. } => PlayerStateOut::Loading,
        PlayerState::Playing if snap.paused => PlayerStateOut::Paused,
        PlayerState::Playing => PlayerStateOut::Playing,
        PlayerState::Paused => PlayerStateOut::Paused,
        PlayerState::Buffering { .. } => PlayerStateOut::Buffering,
        PlayerState::Reconnecting { .. } => PlayerStateOut::Reconnecting,
        PlayerState::Error(_) => PlayerStateOut::Error,
    };
    let idle = state == PlayerStateOut::Idle;
    let now_playing = snap.playable.as_ref().filter(|_| !idle).and_then(|p| {
        // concealed (explicit while hidden): say nothing about it
        if p.item.as_ref().is_some_and(|i| q.lib.conceals(i)) {
            return None;
        }
        let kind = match p.kind {
            PlayableKind::Live => "live",
            PlayableKind::Catchup { .. } => "catchup",
            PlayableKind::Movie => "movie",
            PlayableKind::Episode { .. } => "episode",
            PlayableKind::Url => "file",
        };
        let title = match p.kind {
            // a file or URL: its last path segment, never the address
            PlayableKind::Url => p.title.rsplit(['/', '\\']).next().unwrap_or_default().split(['?', '#']).next().unwrap_or_default().to_owned(),
            _ => p.title.clone(),
        };
        Some(NowPlayingOut {
            item: p.item.as_ref().map(item_ref),
            title,
            subtitle: p.subtitle.clone().filter(|s| !s.trim().is_empty()),
            kind: kind.into(),
        })
    });
    let error = match &snap.state {
        PlayerState::Error(e) => Some(error_kind(e.kind)),
        PlayerState::Reconnecting { reason, .. } => Some(error_kind(*reason)),
        _ => None,
    };
    Ok(StatusOut {
        state,
        position_s: snap.position.filter(|_| !idle).map(|p| p.round()),
        duration_s: snap.duration.filter(|_| !idle).map(|d| d.round()),
        now_playing,
        paused: snap.paused,
        volume: snap.volume,
        muted: snap.muted,
        error,
        parental_locked: q.view.pin_active && !q.view.unlocked,
        scheduled: scheduled(q)?,
    })
}

/// The error kind's name (never the provider's message).
pub fn error_kind(k: PlayerErrorKind) -> String {
    match k {
        PlayerErrorKind::Network => "network".into(),
        PlayerErrorKind::Timeout => "timeout".into(),
        PlayerErrorKind::Stalled => "stalled".into(),
        PlayerErrorKind::StreamEnded => "stream-ended".into(),
        PlayerErrorKind::HttpForbidden => "http-forbidden".into(),
        PlayerErrorKind::HttpNotFound => "http-not-found".into(),
        PlayerErrorKind::HttpOther(code) => format!("http-{code}"),
        PlayerErrorKind::Unsupported => "unsupported".into(),
        PlayerErrorKind::Drm => "drm".into(),
        PlayerErrorKind::Resolve => "resolve".into(),
        PlayerErrorKind::Budget => "connection-limit".into(),
        PlayerErrorKind::ProviderError => "provider-error".into(),
        PlayerErrorKind::Mpv => "player".into(),
    }
}
