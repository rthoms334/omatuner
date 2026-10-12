//! The action tools' decisions (DESIGN §30.4, §30.5, §30.8, §30.13): what `play` plays and
//! says, where an agent's playback opens, what `schedule_switch` writes, whether
//! `cancel_switch` may remove a row, and `status`.
//! Pure over a [`Q`]; the handler (`agent::server`) does the sending and writing. Owner:
//! MCP-app track.

use super::proto::{
    AgentRef, Airing, CancelSwitch, ErrorCode, MultiviewStatusOut, NowPlayingOut, OpenedIn, Play, PlayOut, PlayOutcome, PlayerStateOut, QueueNextOut,
    QueueStatusOut, ScheduleSwitch, SleepOut, StatusOut, StreamsOut, SwitchOut, TileName, TileOut, fmt_time, item_ref,
};
use super::query::{Fail, Out, Q, channel_ref, clean, parse_ref, when};
use super::{AgentCommand, AgentPlay, PlayTarget};
use crate::jobs::JobKind;
use crate::model::{ItemKind, MediaRef, OpenIn, PlayableKind, Reminder, ReminderId, ReminderState, VodKind};
use crate::player::{PlayerErrorKind, PlayerSnapshot, PlayerState};
use crate::reminders::ui::{Placement, WindowNow};

/// Pending automatic switches allowed at once.
pub const MAX_SWITCHES: usize = 20;
/// How far ahead a switch may be scheduled.
pub const MAX_SWITCH_AHEAD_S: i64 = 14 * 86_400;

/// `HH:MM` alone (no date): today's, or tomorrow's once it has passed.
pub(super) fn is_clock_time(s: &str) -> bool {
    let digits = |x: &str| !x.is_empty() && x.len() <= 2 && x.bytes().all(|b| b.is_ascii_digit());
    s.trim().split_once(':').is_some_and(|(h, m)| digits(h) && m.len() == 2 && digits(m))
}

/// Where an agent's play shows (`agent_show`): the placement the app applies, whether the
/// handler raises Tuner's window, and whether the window goes fullscreen (asked for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentShow {
    pub place: Placement,
    pub raise: bool,
    /// `open_in: full_window` ("full screen", "the TV"): the window fullscreen too.
    pub fullscreen: bool,
}

impl AgentShow {
    /// The pop-out (never raised: it takes no keyboard).
    pub const POP_OUT: Self = Self { place: Placement::PopOut, raise: false, fullscreen: false };
    /// Only what plays changes (the screen stays; the mini player shows it).
    pub const IN_PLACE: Self = Self { place: Placement::InPlace, raise: false, fullscreen: false };

    /// Tuner's window, raised or not.
    pub const fn window(raise: bool) -> Self {
        Self { place: Placement::FullWindow, raise, fullscreen: false }
    }
}

/// Where an agent's play opens (DESIGN §30.13). An explicit `open_in` wins, else the setting
/// (Settings ▸ AI agents "Agents play in"; a hand-written `where-i-am` counts as the pop-out,
/// the screen's two choices). The PIN is typed in Tuner's window; without Hyprland there is no
/// pop-out; `full_window` also makes the window fullscreen. Unasked, the pop-out is for a
/// window the user can't see: a window on screen (`WindowNow::visible`: focused, or on a
/// workspace a monitor shows — beside the agent's terminal, on the other monitor) plays in the
/// full window whatever screen it shows, not raised (it is where the user looks; the terminal
/// keeps the keyboard), a fullscreen one staying fullscreen; a window out of sight pops out;
/// two pictures stay in the window, raised when it's out of sight; a pop-out stays the
/// pop-out. Pure.
pub fn agent_show(asked: Option<OpenIn>, setting: OpenIn, w: WindowNow, pin: bool) -> AgentShow {
    let full = AgentShow { fullscreen: true, ..AgentShow::window(true) };
    match asked {
        Some(OpenIn::FullWindow) => full,
        _ if pin => AgentShow::window(true),
        Some(OpenIn::WhereIAm) => AgentShow::IN_PLACE,
        // off Hyprland: the window, as before the pop-out default
        _ if !w.can_pop => AgentShow::window(true),
        // asked for: also out of a fullscreen window
        Some(OpenIn::PopOut) => AgentShow::POP_OUT,
        None => match setting {
            OpenIn::FullWindow => AgentShow::window(true),
            OpenIn::PopOut | OpenIn::WhereIAm if w.popped_out => AgentShow::POP_OUT,
            OpenIn::PopOut | OpenIn::WhereIAm if w.multiview => AgentShow::window(!w.visible),
            OpenIn::PopOut | OpenIn::WhereIAm if w.visible => AgentShow::window(false),
            OpenIn::PopOut | OpenIn::WhereIAm => AgentShow::POP_OUT,
        },
    }
}

/// What `opened_in` says for `show` in window `w`.
pub fn opened_in(show: AgentShow, w: WindowNow) -> OpenedIn {
    match show.place {
        Placement::PopOut => OpenedIn::Popout,
        Placement::FullWindow => OpenedIn::Window,
        Placement::InPlace if w.popped_out => OpenedIn::Popout,
        Placement::InPlace => OpenedIn::Window,
    }
}

/// `open_in` as the tools take it: `popout`, `full_window` or `where_i_am` (the reminders'
/// names).
pub fn parse_open_in(s: Option<&str>) -> Out<Option<OpenIn>> {
    Ok(match s {
        None => None,
        Some("popout") => Some(OpenIn::PopOut),
        Some("full_window") => Some(OpenIn::FullWindow),
        Some("where_i_am") => Some(OpenIn::WhereIAm),
        Some(_) => return Err(Fail::bad("open_in is popout, full_window or where_i_am")),
    })
}

/// `OpenIn`'s tool name (see [`parse_open_in`]).
pub fn open_in_name(o: OpenIn) -> &'static str {
    match o {
        OpenIn::PopOut => "popout",
        OpenIn::FullWindow => "full_window",
        OpenIn::WhereIAm => "where_i_am",
    }
}

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
        command: AgentCommand::Play(AgentPlay { target, from_start, show: AgentShow::window(true), device: None }),
        out: clean_play(out),
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
                        opened_in: None,
                    },
                ))
            };
            match q.airing(&prog) {
                Airing::Upcoming => Err(Fail::new(
                    ErrorCode::NotStarted,
                    clean(&format!("{} starts at {} on {}: use schedule_switch to switch then.", prog.title, fmt_time(prog.start), c.name)),
                )),
                Airing::Past if q.catchup(c, &prog) => archive(PlayOutcome::CatchUp),
                Airing::Past => {
                    Err(Fail::new(ErrorCode::Unavailable, clean(&format!("{} has ended and {} has no catch-up for it.", prog.title, c.name))))
                }
                Airing::Live if from_start && q.catchup(c, &prog) => archive(PlayOutcome::CatchUp),
                Airing::Live => {
                    let live_only = if from_start { format!(" {} can't restart programmes, so it plays live.", c.name) } else { String::new() };
                    Ok(ready(
                        PlayTarget::Media(MediaRef::Channel(c.id)),
                        PlayOut {
                            outcome: if locked { PlayOutcome::PinRequired } else { PlayOutcome::Playing },
                            item: item_ref(&channel_ref(c)),
                            title: prog.title.to_string(),
                            resume_at_min: None,
                            message: if locked { pin(&c.name) } else { format!("Switched to {} for {}.{live_only}", c.name, prog.title) },
                            opened_in: None,
                        },
                    ))
                }
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
                        opened_in: None,
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
                        opened_in: None,
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
                            opened_in: None,
                        },
                    ));
                }
                let Some((_, ep)) = crate::store::copies::next_episode(q.conn, &q.series_copies(idx))? else {
                    let lazy = q.lib.source(v.source_id).is_some_and(|s| s.kind == crate::model::SourceKind::Xtream)
                        && crate::store::vod::series(q.conn, id)?.details_loaded_at.is_none();
                    if lazy {
                        return Ok(PlayStep::NeedsDetails(JobKind::LoadSeriesDetails(id)));
                    }
                    return Err(Fail::new(ErrorCode::Unavailable, clean(&format!("{} has no episodes to play.", v.name))));
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
                        PlayOut {
                            outcome: PlayOutcome::PinRequired,
                            item: item_ref(&item),
                            message: pin(&title),
                            title,
                            resume_at_min: None,
                            opened_in: None,
                        },
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
        command: AgentCommand::Play(AgentPlay { target, from_start, show: AgentShow::window(true), device: None }),
        out: clean_play(PlayOut { outcome: PlayOutcome::Playing, item: out.item, title, resume_at_min: resume, message, opened_in: None }),
        log_ref: log_ref.to_owned(),
    })
}

/// Provider names in a `play` answer without their links (`query::clean`).
fn clean_play(out: PlayOut) -> PlayOut {
    PlayOut { title: clean(&out.title), message: clean(&out.message), ..out }
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
            let at_text = p.at.as_deref().ok_or_else(|| Fail::bad("a channel ref needs at (when to switch)"))?;
            let mut at = when("at", at_text, q.now)?;
            // a bare clock time that has passed today means tomorrow ("21:00" asked at 22:00)
            if at <= q.now && is_clock_time(at_text) {
                at += 86_400;
            }
            if at > q.now.saturating_add(MAX_SWITCH_AHEAD_S) {
                return Err(Fail::bad("at is more than 14 days ahead"));
            }
            // the programme starting then, when the guide has one
            let prog = q.programme_at(c, at)?;
            let stop = prog.as_ref().map_or(at.saturating_add(3600), |p| p.stop);
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
        mode: Default::default(),
        offset_s: 0,
        open_in: None,
        rule_id: None,
    };
    let out = SwitchOut {
        id: 0,
        channel: q.channel_out(c),
        title: title.as_deref().map(clean),
        start: fmt_time(start),
        warn_at: fmt_time(start - Reminder::SWITCH_WARN_S),
    };
    Ok(SwitchPlan { reminder, out, log_ref })
}

/// A pending switch the agent may cancel (`not_found` for anything else, a concealed one too).
pub fn check_cancel(q: &Q, p: &CancelSwitch) -> Out<ReminderId> {
    let id = ReminderId(p.id);
    match crate::store::user::reminder(q.conn, id)? {
        Some(r) if r.switches() && r.state == ReminderState::Pending && !q.conceals(&r.channel) && q.channel_by_ref(&r.channel).is_some() => Ok(id),
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
            if q.conceals(&r.channel) {
                return None;
            }
            let c = q.channel_by_ref(&r.channel)?;
            let title = Some(clean(&r.programme_title)).filter(|t| !t.trim().is_empty());
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

/// What `status` adds from outside the player (the handler gathers it).
#[derive(Debug, Clone, Default)]
pub struct StatusMore {
    pub queue: std::sync::Arc<crate::queue::QueueSnapshot>,
    pub tiles: crate::playback::TilesState,
    /// Picture B's snapshot, while it exists.
    pub tile_b: Option<std::sync::Arc<PlayerSnapshot>>,
    pub sleep: Option<crate::reminders::sleep::SleepStatus>,
}

/// [`status`] with Up next, multi-view, the playlists' streams and the sleep timer.
pub fn status_with(q: &Q, snap: &PlayerSnapshot, more: &StatusMore) -> Out<StatusOut> {
    let mut out = status(q, snap)?;
    let queue = &more.queue;
    if queue.pending() > 0 {
        let next = queue.next.and_then(|id| queue.row(id)).map(|r| QueueNextOut { item: item_ref(&r.entry.item), title: clean(&r.entry.title) });
        out.queue = Some(QueueStatusOut {
            current_id: queue.current.map(|i| i.0),
            next,
            left: queue.pending() as u32,
            eta_end: queue.eta_end.map(|(t, _)| fmt_time(t)),
            playing: queue.driving,
        });
    }
    let mut snaps: Vec<(crate::model::TileId, &PlayerSnapshot)> = vec![(crate::model::TileId::A, snap)];
    if let Some(b) = more.tile_b.as_deref() {
        snaps.push((crate::model::TileId::B, b));
    }
    if more.tiles.on {
        let tiles = snaps
            .iter()
            .map(|(t, s)| {
                let shown = s.playable.as_ref().filter(|p| !p.item.as_ref().is_some_and(|i| q.conceals(i)));
                TileOut {
                    tile: TileName::of(*t),
                    item: shown.and_then(|p| p.item.as_ref()).map(item_ref),
                    title: shown.map(|p| clean(&p.title)),
                    playlist: s.claim.and_then(|src| q.lib.source(src)).map(|m| clean(&m.name)),
                    state: state_out(s),
                    sound: more.tiles.audible == *t,
                    focused: more.tiles.focus == *t,
                }
            })
            .collect();
        out.multiview = Some(MultiviewStatusOut { on: true, tiles });
    }
    // the connection limit of each playlist with channels, and the pictures holding it
    let accounts: std::collections::HashMap<crate::model::SourceId, Option<u32>> =
        crate::store::sources::list_sources(q.conn)?.into_iter().map(|s| (s.id, s.status.account.and_then(|a| a.max_connections))).collect();
    out.streams = q
        .lib
        .sources
        .iter()
        .filter(|s| s.enabled && s.has_live)
        .map(|s| StreamsOut {
            playlist: clean(&s.name),
            limit: accounts.get(&s.id).copied().flatten(),
            in_use: snaps.iter().filter(|(_, x)| x.claim == Some(s.id)).count() as u32,
        })
        .collect();
    out.sleep_timer = more.sleep.map(|s| sleep_out(&s));
    Ok(out)
}

/// The sleep timer as tools show it.
pub fn sleep_out(s: &crate::reminders::sleep::SleepStatus) -> SleepOut {
    use crate::reminders::sleep::{SleepAction, SleepWhen};
    SleepOut {
        when: match s.when {
            SleepWhen::At(_) => "at",
            SleepWhen::EndOfProgramme => "end_of_programme",
            SleepWhen::AfterItems(_) => "after_items",
            SleepWhen::EndOfQueue => "end_of_queue",
        }
        .into(),
        then: match s.action {
            SleepAction::Stop => "stop",
            SleepAction::Pause => "pause",
        }
        .into(),
        fires_at: s.fires_at.map(fmt_time),
        items_left: s.items_left,
    }
}

fn state_out(snap: &PlayerSnapshot) -> PlayerStateOut {
    match &snap.state {
        PlayerState::Idle | PlayerState::Ended => PlayerStateOut::Idle,
        PlayerState::Zapping { .. } | PlayerState::Loading { .. } => PlayerStateOut::Loading,
        PlayerState::Playing if snap.paused => PlayerStateOut::Paused,
        PlayerState::Playing => PlayerStateOut::Playing,
        PlayerState::Paused => PlayerStateOut::Paused,
        PlayerState::Buffering { .. } => PlayerStateOut::Buffering,
        PlayerState::Reconnecting { .. } => PlayerStateOut::Reconnecting,
        PlayerState::Error(_) => PlayerStateOut::Error,
    }
}

pub fn status(q: &Q, snap: &PlayerSnapshot) -> Out<StatusOut> {
    let state = state_out(snap);
    let idle = state == PlayerStateOut::Idle;
    let now_playing = snap.playable.as_ref().filter(|_| !idle).and_then(|p| {
        // concealed (explicit while hidden): say nothing about it. The snapshot's explicit
        // index has no episodes and the store is asked too (the session may have relocked by
        // its timer while it played, with no frame to stop it); a stream of no known item
        // (an archive with its channel gone) counts as concealed while explicit is hidden
        let concealed = match &p.item {
            Some(i) => q.conceals(i),
            None => q.view.hides_explicit && p.kind != PlayableKind::Url,
        };
        if concealed {
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
            _ => clean(&p.title),
        };
        Some(NowPlayingOut {
            item: p.item.as_ref().map(item_ref),
            title,
            subtitle: p.subtitle.as_deref().map(clean).filter(|s| !s.trim().is_empty()),
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
        queue: None,
        multiview: None,
        streams: Vec::new(),
        sleep_timer: None,
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
        PlayerErrorKind::Cast(c) => c.reason().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DESIGN §30.13's order: an explicit `full_window` (fullscreen), the PIN, the rest of
    /// `open_in`, then no Hyprland, then the setting — which, unasked, plays in a window on
    /// screen (focused or not, watching or browsing: the agent's terminal has the keyboard) and
    /// a pop-out, and pops out of a window nobody sees.
    #[test]
    fn agent_show_resolves_explicit_then_setting_then_the_window() {
        use OpenIn::{FullWindow, PopOut, WhereIAm};
        let away = WindowNow { can_pop: true, ..Default::default() };
        let pop = AgentShow::POP_OUT;
        let raised = AgentShow::window(true);
        let kept = AgentShow::window(false);
        let full = AgentShow { fullscreen: true, ..raised };
        let in_place = AgentShow::IN_PLACE;
        let shown = WindowNow { visible: true, ..away };
        let fullscreen = WindowNow { fullscreen: true, watching: true, ..shown };
        let cases = [
            // (asked, setting, window, pin) → show
            (None, PopOut, away, false, pop),
            (None, PopOut, WindowNow { watching: true, ..away }, false, pop),
            // tiled beside the terminal, or on the other monitor: the terminal has the keyboard;
            // browsing (Live TV's preview, Home) too — the player screen fills the window
            (None, PopOut, shown, false, kept),
            (None, PopOut, WindowNow { focused: true, visible: true, ..away }, false, kept),
            (None, PopOut, WindowNow { watching: true, ..shown }, false, kept),
            (None, PopOut, WindowNow { focused: true, watching: true, ..shown }, false, kept),
            (None, PopOut, fullscreen, false, kept),
            (None, PopOut, WindowNow { fullscreen: true, watching: true, ..away }, false, pop),
            (None, PopOut, WindowNow { multiview: true, ..shown }, false, kept),
            (None, PopOut, WindowNow { multiview: true, ..away }, false, raised),
            (None, PopOut, WindowNow { popped_out: true, ..away }, false, pop),
            (None, WhereIAm, away, false, pop),
            (None, WhereIAm, shown, false, kept),
            (None, FullWindow, WindowNow { popped_out: true, ..away }, false, raised),
            (None, FullWindow, away, true, raised),
            (Some(FullWindow), PopOut, WindowNow { popped_out: true, ..away }, false, full),
            (Some(FullWindow), PopOut, away, true, full),
            (Some(FullWindow), PopOut, WindowNow { can_pop: false, ..away }, false, full),
            (Some(PopOut), FullWindow, fullscreen, false, pop),
            (Some(WhereIAm), PopOut, away, false, in_place),
            (None, PopOut, WindowNow { can_pop: false, ..away }, false, raised),
            (Some(PopOut), PopOut, WindowNow { can_pop: false, ..away }, false, raised),
            (Some(PopOut), PopOut, away, true, raised),
        ];
        for (asked, setting, w, pin, want) in cases {
            assert_eq!(agent_show(asked, setting, w, pin), want, "{asked:?} {setting:?} {w:?} pin {pin}");
        }
        assert_eq!(opened_in(in_place, WindowNow { popped_out: true, ..away }), OpenedIn::Popout);
        assert_eq!(opened_in(in_place, away), OpenedIn::Window);
        assert_eq!(opened_in(kept, fullscreen), OpenedIn::Window);
    }
}
