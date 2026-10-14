//! The action tools' decisions (DESIGN §30.4, §30.5, §30.8, §30.13): what `play` plays and
//! says, where an agent's playback opens, what `schedule_switch` writes, whether
//! `cancel_switch` may remove a row, and `status`.
//! Pure over a [`Q`]; the handler (`agent::server`) does the sending and writing. Owner:
//! MCP-app track.

use super::proto::{
    AgentRef, Airing, CancelSwitch, CastDeviceOut, CastDevicesOut, CastingOut, ErrorCode, MultiviewStatusOut, NowPlayingOut, OpenedIn, Play, PlayOut,
    PlayOutcome, PlayerStateOut, QueueNextOut, QueueStatusOut, ScheduleSwitch, SleepOut, StatusOut, StreamsOut, SwitchOut, TileName, TileOut,
    fmt_time, item_ref,
};
use super::query::{Fail, Out, Q, channel_ref, clean, parse_ref, when};
use super::{AgentCommand, AgentPlay, PlayTarget};
use crate::cast::firewall::{Detection, FirewallState};
use crate::cast::{Device, Pairing, Protocol};
use crate::jobs::JobKind;
use crate::model::{
    CastErrorKind, CastPhase, DeviceId, ItemKind, MediaRef, OpenIn, PlayableKind, ReceiverKind, Reminder, ReminderId, ReminderState, VodKind,
};
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

// ------------------------------------------------------------------ casting (DESIGN §40 §12)

/// The ref `cast_devices` hands out for a TV: `device:<32 hex>` (opaque; no address in it).
pub fn device_ref(id: DeviceId) -> String {
    format!("device:{id}")
}

/// `play {device}`'s TV among `devices`: its `device:<hex>` ref, or its exact name (case
/// ignored; a name two TVs share is refused, the ref tells them apart). `not_found` never says
/// which of the two was wrong.
pub fn resolve_device(devices: &[Device], asked: &str) -> Out<Device> {
    let asked = asked.trim();
    let no_tv = || Fail::new(ErrorCode::NotFound, "No such TV in Tuner (pass back a ref cast_devices returned, or its exact name)");
    if let Some(hex) = asked.strip_prefix("device:") {
        let id = hex.parse::<DeviceId>().map_err(|_| Fail::bad("not a device ref: pass back a `ref` cast_devices returned"))?;
        return devices.iter().find(|d| d.id == id).cloned().ok_or_else(no_tv);
    }
    let mut named = devices.iter().filter(|d| d.name.eq_ignore_ascii_case(asked));
    match (named.next(), named.next()) {
        (Some(d), None) => Ok(d.clone()),
        (Some(_), Some(_)) => Err(Fail::bad("more than one TV has that name: pass its ref from cast_devices")),
        _ => Err(no_tv()),
    }
}

/// A receiver kind as the tools name it (the design's names; the model's own serde names differ
/// for `AirPlayTv`).
pub fn kind_name(k: ReceiverKind) -> &'static str {
    match k {
        ReceiverKind::AppleTv => "apple_tv",
        ReceiverKind::AirPlayTv => "airplay_tv",
        ReceiverKind::Chromecast => "chromecast",
        ReceiverKind::Dlna => "dlna",
    }
}

/// `cast_devices`: the registry as an agent sees it; no address. `casting`: the TV the picture
/// plays on now.
pub fn cast_devices(devices: &[Device], casting: Option<DeviceId>) -> CastDevicesOut {
    CastDevicesOut {
        devices: devices
            .iter()
            .map(|d| CastDeviceOut {
                device_ref: device_ref(d.id),
                name: clean(&d.name),
                kind: kind_name(d.kind).into(),
                paired: !matches!(d.pairing, Pairing::Required),
                available: d.available,
                casting: casting == Some(d.id),
            })
            .collect(),
    }
}

/// What lets a cast to a TV start (`cast_gate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastGate {
    Go,
    /// The TV isn't paired: say so (an outcome, not an error). Agents never pair.
    PairingRequired,
    /// `unavailable` with this typed reason.
    Refused(&'static str),
}

/// `multiview-on`: two pictures are on, and casting needs one (DESIGN §40 §5.5).
pub const REASON_MULTIVIEW: &str = "multiview-on";

/// Whether a cast to `d` can start now, before the item is looked at (DESIGN §40 §12): multi-view
/// is off, the TV is one Phase 1 casts to (an Apple-made or third-party AirPlay TV that isn't a
/// Mac, nor "Same Home only"), it is paired, Tuner has seen it, and the firewall isn't known to
/// block the port *for this TV*.
///
/// `firewall` counts only when it was made toward `d`'s own address and found the network
/// toward it ([`Detection::state_for`]): ufw rules are scoped by source (`from 192.168.1.0/24`),
/// so a detection toward the mDNS group or another TV says nothing about this one (§9.4
/// "Toward which TV"; a real TV was refused on one). Anything else, and an unknown state, blocks
/// nothing here: the session's own check (the TV never fetched) is the authority.
pub fn cast_gate(d: &Device, multiview: bool, firewall: Option<&Detection>) -> CastGate {
    let toward = d.addr().and_then(|a| match a.ip() {
        std::net::IpAddr::V4(v4) => Some(v4),
        std::net::IpAddr::V6(_) => None,
    });
    let firewall = firewall.zip(toward).and_then(|(f, tv)| f.state_for(tv));
    let phase_one = d.protocol == Protocol::AirPlay && matches!(d.kind, ReceiverKind::AppleTv | ReceiverKind::AirPlayTv);
    if multiview {
        CastGate::Refused(REASON_MULTIVIEW)
    } else if !phase_one || !d.selectable() {
        CastGate::Refused(CastErrorKind::Unsupported.reason())
    } else if d.pairing == Pairing::Required {
        CastGate::PairingRequired
    } else if !d.available {
        CastGate::Refused(CastErrorKind::Unreachable.reason())
    } else if matches!(firewall, Some(FirewallState::Blocked { .. })) {
        CastGate::Refused(CastErrorKind::Firewall.reason())
    } else {
        CastGate::Go
    }
}

/// The one fixed sentence of a refusal (no device or provider text, DESIGN §30.6); the typed
/// reason leads, as `unavailable: casting` does for `control`.
pub fn refusal(reason: &str) -> Fail {
    let why = match reason {
        REASON_MULTIVIEW => "multi-view is on; casting needs a single picture (close multi-view first)",
        "cast-unsupported" => "Tuner can't cast to that TV",
        "cast-unreachable" => "the TV isn't answering; check that it is on and on the same network",
        "cast-firewall" => {
            "the firewall blocks the TV from reaching Tuner; the user can press Allow on firewall in Tuner (Settings ▸ Cast ▸ Network)"
        }
        _ => "casting isn't possible now",
    };
    Fail::new(ErrorCode::Unavailable, format!("{reason}: {why}"))
}

/// A `play` that was decided to go to a TV: casting instead of playing here (DESIGN §40 §12).
/// The window stays where it is: nothing pops out or is raised (`AgentShow::IN_PLACE`), except
/// for the PIN, which is typed in Tuner's window, so that prompt raises it as for any play, and
/// nothing is cast before the unlock. The result says `opened_in: device` and names the TV.
pub fn for_device(command: &mut AgentCommand, out: &mut PlayOut, d: &Device) {
    let AgentCommand::Play(play) = command else { return };
    play.device = Some(d.id);
    out.device = Some(clean(&d.name));
    if out.outcome == PlayOutcome::PinRequired {
        out.message = clean(&format!("{} It will play on {}.", out.message, d.name));
        return;
    }
    play.show = AgentShow::IN_PLACE;
    let resume = out.resume_at_min.map(|m| format!(" It resumes from {m} min.")).unwrap_or_default();
    out.message = clean(&format!("Casting {} to {}.{resume}", out.title, d.name));
    out.outcome = PlayOutcome::CastingStarted;
    out.opened_in = Some(OpenedIn::Device);
}

/// A `play` without `device` while picture A casts: it lands on that TV (the controller's output
/// is the TV; RULES.md), so it is answered as a device play — the TV named, and no word of a
/// window, which stays where it is (DESIGN §40 §3.8) unless the PIN prompt needs it (then
/// `opened_in` stays the window the prompt is in, as for a `play {device}`).
pub fn on_the_tv(out: &mut PlayOut, tv: &str) {
    out.device = Some(clean(tv));
    if out.outcome == PlayOutcome::PinRequired {
        out.message = clean(&format!("{} It will play on {tv}.", out.message));
        return;
    }
    out.opened_in = Some(OpenedIn::Device);
    out.message = clean(&format!("{} It's on {tv}.", out.message));
}

/// `status.play_on` (DESIGN §40 §3.9): the Play on switch's TV (`snap.play_on`, picture A's),
/// named from `devices` (a TV the registry no longer lists reads "your TV").
pub fn play_on_out(snap: &PlayerSnapshot, devices: &[Device]) -> Option<super::proto::PlayOnOut> {
    let id = snap.play_on?;
    let name = devices.iter().find(|d| d.id == id).map_or("your TV", |d| d.name.as_str());
    Some(super::proto::PlayOnOut { device_ref: device_ref(id), device: clean(name) })
}

/// The reply to a `play {device}` for an unpaired TV: nothing was sent.
pub fn pairing_required(out: &PlayOut, d: &Device) -> PlayOut {
    PlayOut {
        outcome: PlayOutcome::PairingRequired,
        message: clean(&format!("Pair {} in Tuner first (Settings ▸ Cast).", d.name)),
        opened_in: None,
        device: Some(clean(&d.name)),
        ..out.clone()
    }
}

/// What the cast service remembers of the last cast: `status.casting` once the cast ended
/// (`CastApi::last_error`, `CastApi::diagnose().device`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CastLast {
    pub error: Option<CastErrorKind>,
    /// The TV last cast to: its id, name and kind.
    pub device: Option<(DeviceId, String, ReceiverKind)>,
}

/// `status.casting` (DESIGN §40 §12): the TV the picture plays on, and the typed reason of the
/// failure, so that an agent can explain it while Tuner is out of sight (§3.8). `error`: the
/// player's own cast error when it shows one, else the service's last one once the session was
/// lost. A cast that ended without the user asking (stopped from the TV's remote) leaves no
/// casting picture; while nothing plays here, the TV last cast to and the reason stay.
pub fn casting_out(snap: &PlayerSnapshot, last: &CastLast) -> Option<CastingOut> {
    let cast_kind = |k: PlayerErrorKind| if let PlayerErrorKind::Cast(c) = k { Some(c) } else { None };
    let shown = match &snap.state {
        PlayerState::Error(e) => cast_kind(e.kind),
        PlayerState::Reconnecting { reason, .. } => cast_kind(*reason),
        _ => None,
    };
    let idle = matches!(snap.state, PlayerState::Idle | PlayerState::Ended);
    let out = |id: DeviceId, name: &str, kind, phase, behind_live_s, error: Option<CastErrorKind>| CastingOut {
        device_ref: device_ref(id),
        device: clean(name),
        kind: kind_name(kind).into(),
        phase,
        behind_live_s,
        error: error.map(|e| e.reason().into()),
    };
    match (&snap.cast, &last.device) {
        (Some(v), _) => Some(out(v.device, &v.name, v.kind, v.phase, v.behind_live_s, shown.or(last.error.filter(|_| v.phase == CastPhase::Lost)))),
        (None, Some((id, name, kind))) if idle || shown.is_some() => {
            let error = shown.or(last.error)?;
            Some(out(*id, name, *kind, CastPhase::Lost, None, Some(error)))
        }
        _ => None,
    }
}

/// What `control` refuses because of the TV (DESIGN §40 §12): the picture is on it, so there is no
/// window to move (`popout`, `window`: `unavailable: casting`), the volume is the TV's own
/// (`volume`, `mute`, `unmute`: `unsupported` on a receiver without one, AirPlay), and
/// `stop_casting` / `play_here` need a cast — except `play_here` once the TV stopped one
/// (`tv_stopped`: `StoppedOnTv` with nothing playing), which plays the last item here from
/// where the TV was, as the alert's Play here does (the fix skill offers it).
pub fn cast_control_guard(action: super::proto::ControlAction, cast: Option<&crate::model::CastView>, tv_stopped: bool) -> Option<Fail> {
    use super::proto::ControlAction as A;
    match (action, cast) {
        (A::PlayHere, None) if tv_stopped => None,
        (A::Popout | A::Window, Some(_)) => {
            Some(Fail::new(ErrorCode::Unavailable, "casting: the picture is on the TV; stop casting or play here first"))
        }
        (A::Volume | A::Mute | A::Unmute, Some(v)) if !v.volume => {
            Some(Fail::new(ErrorCode::Unsupported, "Volume is the TV's: use its remote while casting to it"))
        }
        (A::StopCasting | A::PlayHere, None) => Some(Fail::new(ErrorCode::Unavailable, "Nothing is being cast")),
        _ => None,
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
                        device: None,
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
                            device: None,
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
                        device: None,
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
                        device: None,
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
                            device: None,
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
                            device: None,
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
        out: clean_play(PlayOut {
            outcome: PlayOutcome::Playing,
            item: out.item,
            title,
            resume_at_min: resume,
            message,
            opened_in: None,
            device: None,
        }),
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
    /// `status.casting` ([`casting_out`]).
    pub casting: Option<CastingOut>,
    /// `status.play_on` ([`play_on_out`]).
    pub play_on: Option<super::proto::PlayOnOut>,
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
    out.casting = more.casting.clone();
    out.play_on = more.play_on.clone();
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
        casting: None,
        play_on: None,
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

    /// "Living Room" at 192.168.1.5 (the user's Apple TV), as the registry has it.
    fn tv(name: &str, pairing: Pairing) -> Device {
        let at = std::net::SocketAddr::from((LIVING_ROOM, 7000));
        Device { pairing, ..Device::new(DeviceId::derive("airplay", name), name, ReceiverKind::AppleTv, Protocol::AirPlay).at(at) }
    }

    const LIVING_ROOM: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 168, 1, 5);

    /// This computer on the user's Wi-Fi.
    fn wlan0() -> crate::cast::firewall::LanNet {
        crate::cast::firewall::LanNet { iface: "wlan0".into(), addr: std::net::Ipv4Addr::new(192, 168, 1, 20), prefix: 24 }
    }

    /// A detection with `state`, made toward `toward` on the user's Wi-Fi.
    fn det(state: FirewallState, toward: std::net::Ipv4Addr) -> Detection {
        Detection { state, toward, ufw: true, pkexec: true, net: Some(wlan0()), tagged: Vec::new(), port: 47800 }
    }

    /// DESIGN §40 §12: what stops a cast before the item is looked at, in the order an agent
    /// can act on it: multi-view, a TV Phase 1 can't cast to, pairing, a TV not seen, a firewall
    /// that blocks this TV. An unknown firewall blocks nothing here.
    #[test]
    fn a_cast_is_gated_in_the_order_an_agent_can_act_on() {
        use crate::cast::Usability;
        use crate::cast::firewall::UnknownWhy;
        let paired = tv("Living Room", Pairing::Paired(crate::cast::PairVariant::Hkp3));
        let blocked = det(FirewallState::Blocked { shadowed: false }, LIVING_ROOM);
        let blocked = Some(&blocked);
        let allowed = det(FirewallState::Allowed { ours: true }, LIVING_ROOM);
        let unknown = det(FirewallState::Unknown { why: UnknownWhy::Nftables }, LIVING_ROOM);
        let shadowed = det(FirewallState::Blocked { shadowed: true }, LIVING_ROOM);
        let mac = Device { usability: Usability::Mac, ..paired.clone() };
        let home_only = Device { usability: Usability::HomeOnly, ..paired.clone() };
        let chromecast = Device { kind: ReceiverKind::Chromecast, protocol: Protocol::GoogleCast, ..paired.clone() };
        let unseen = Device { available: false, ..paired.clone() };
        let unpaired = Device { pairing: Pairing::Required, ..unseen.clone() };
        let cases = [
            (&paired, false, None, CastGate::Go),
            (&paired, false, Some(&allowed), CastGate::Go),
            (&paired, false, Some(&unknown), CastGate::Go),
            (&tv("No pairing", Pairing::NotNeeded), false, None, CastGate::Go),
            (&paired, true, blocked, CastGate::Refused("multiview-on")),
            (&mac, false, None, CastGate::Refused("cast-unsupported")),
            (&home_only, false, None, CastGate::Refused("cast-unsupported")),
            (&chromecast, false, None, CastGate::Refused("cast-unsupported")),
            (&unpaired, false, blocked, CastGate::PairingRequired),
            (&unseen, false, blocked, CastGate::Refused("cast-unreachable")),
            (&paired, false, blocked, CastGate::Refused("cast-firewall")),
            (&paired, false, Some(&shadowed), CastGate::Refused("cast-firewall")),
        ];
        for (d, multiview, firewall, want) in cases {
            assert_eq!(cast_gate(d, multiview, firewall), want, "{d:?} multiview {multiview} {firewall:?}");
        }
        // the sentence of a refusal leads with the typed reason and carries no device text
        let f = refusal("cast-firewall");
        assert_eq!(f.code, ErrorCode::Unavailable);
        assert!(f.message.starts_with("cast-firewall: ") && f.message.contains("Allow on firewall"), "{}", f.message);
    }

    /// The real-TV refusal of 2026-10-13 (DESIGN §40 §9.4 "Toward which TV"): the user's ufw
    /// allows 47800 from 192.168.1.0/24 on wlan0 (Tuner's tagged rule) under a DROP policy, and
    /// an agent's cast to the Apple TV at 192.168.1.5 was refused with `cast-firewall` because
    /// the cached detection had been made toward the mDNS group, before any cast. Only a
    /// detection made toward the TV itself may refuse it; toward the TV these rules allow it.
    #[test]
    fn the_firewall_is_judged_toward_the_tv_not_toward_another_address() {
        use crate::cast::firewall::{UnknownWhy, assess};
        // the tuples of the user's /etc/ufw/user.rules, verbatim
        let rules = "\
### tuple ### allow udp 53317 0.0.0.0/0 any 0.0.0.0/0 in
### tuple ### allow tcp 53317 0.0.0.0/0 any 0.0.0.0/0 in
### tuple ### allow udp 53 172.17.0.1 any 172.16.0.0/12 in comment=616c6c6f772d646f636b65722d646e73
### tuple ### allow udp 53 172.17.0.1 any 192.168.0.0/16 in comment=616c6c6f772d646f636b65722d646e73
### tuple ### allow tcp 47800 0.0.0.0/0 any 192.168.1.0/24 in_wlan0 comment=6f6d6174756e65722063617374
";
        let drop = crate::cast::firewall::input_policy_blocks("DEFAULT_INPUT_POLICY=\"DROP\"\n");
        assert!(drop);
        let paired = tv("Living Room", Pairing::Paired(crate::cast::PairVariant::Hkp5));
        let judged = |toward| {
            let (state, _) = assess(rules, drop, &wlan0(), toward, 47800);
            det(state, toward)
        };

        // toward the TV: Tuner's own rule covers it
        let at_tv = judged(LIVING_ROOM);
        assert_eq!(at_tv.state, FirewallState::Allowed { ours: true });
        assert_eq!(at_tv.state_for(LIVING_ROOM), Some(FirewallState::Allowed { ours: true }));
        assert_eq!(cast_gate(&paired, false, Some(&at_tv)), CastGate::Go);

        // toward the mDNS group (no TV known yet): can't tell, never "blocked"
        let mdns = judged(std::net::Ipv4Addr::new(224, 0, 0, 251));
        assert_eq!(mdns.state, FirewallState::Unknown { why: UnknownWhy::NoTv });
        assert_eq!(mdns.state_for(LIVING_ROOM), None);
        assert_eq!(cast_gate(&paired, false, Some(&mdns)), CastGate::Go);
        // …and a "blocked" made toward it (as before this fix) still doesn't refuse the TV
        let old = det(FirewallState::Blocked { shadowed: false }, std::net::Ipv4Addr::new(224, 0, 0, 251));
        assert_eq!(cast_gate(&paired, false, Some(&old)), CastGate::Go);

        // toward a TV on another subnet: blocked for that TV, which says nothing about this one
        let elsewhere = judged(std::net::Ipv4Addr::new(10, 0, 0, 7));
        assert_eq!(elsewhere.state, FirewallState::Blocked { shadowed: false });
        assert_eq!(elsewhere.state_for(LIVING_ROOM), None);
        assert_eq!(cast_gate(&paired, false, Some(&elsewhere)), CastGate::Go);

        // toward the TV with no network found (no route): nothing to judge by
        let no_route = Detection { net: None, ..det(FirewallState::Blocked { shadowed: false }, LIVING_ROOM) };
        assert_eq!(cast_gate(&paired, false, Some(&no_route)), CastGate::Go);
        // the same rules without Tuner's: blocked toward the TV itself, and that refuses
        let without: String = rules.lines().filter(|l| !l.contains("47800")).map(|l| format!("{l}\n")).collect();
        let (state, _) = assess(&without, drop, &wlan0(), LIVING_ROOM, 47800);
        assert_eq!(cast_gate(&paired, false, Some(&det(state, LIVING_ROOM))), CastGate::Refused("cast-firewall"));
    }

    /// `device` is a `device:<hex>` ref or an exact name; a name two TVs share is not guessed.
    #[test]
    fn a_tv_is_found_by_ref_or_exact_name() {
        let (a, b) = (tv("Living Room", Pairing::Required), tv("Bedroom", Pairing::Required));
        let twin = Device { id: DeviceId::derive("airplay", "twin"), ..tv("bedroom", Pairing::Required) };
        let all = [a.clone(), b.clone(), twin];
        assert_eq!(resolve_device(&all, &device_ref(a.id)).unwrap().id, a.id);
        assert_eq!(resolve_device(&all, " living ROOM ").unwrap().id, a.id);
        assert_eq!(resolve_device(&all, "Bedroom").unwrap_err().code, ErrorCode::BadRequest, "two TVs share it");
        assert_eq!(resolve_device(&all, &device_ref(b.id)).unwrap().id, b.id);
        for missing in ["Kitchen", "Living", &device_ref(DeviceId::derive("airplay", "nowhere"))] {
            assert_eq!(resolve_device(&all, missing).unwrap_err().code, ErrorCode::NotFound, "{missing}");
        }
        assert_eq!(resolve_device(&all, "device:xyz").unwrap_err().code, ErrorCode::BadRequest);
    }

    /// A cast plays in place; only the PIN prompt raises the window, and nothing says "casting"
    /// until the PIN is typed.
    #[test]
    fn a_play_for_a_tv_keeps_the_window_where_it_is() {
        let d = tv("Living Room", Pairing::Paired(crate::cast::PairVariant::Hkp3));
        let play = |outcome| {
            let out = PlayOut {
                outcome,
                item: "movie:1:m3u:x".into(),
                title: "Dune".into(),
                resume_at_min: Some(12),
                message: "Playing Dune from 12 min.".into(),
                opened_in: None,
                device: None,
            };
            let command = AgentCommand::Play(AgentPlay {
                target: PlayTarget::Media(MediaRef::Movie(crate::model::MovieId(1))),
                from_start: false,
                show: AgentShow::window(true),
                device: None,
            });
            (command, out)
        };
        let (mut command, mut out) = play(PlayOutcome::Playing);
        for_device(&mut command, &mut out, &d);
        let AgentCommand::Play(p) = &command else { panic!() };
        assert_eq!((p.device, p.show), (Some(d.id), AgentShow::IN_PLACE));
        assert_eq!((out.outcome, out.opened_in, out.device.as_deref()), (PlayOutcome::CastingStarted, Some(OpenedIn::Device), Some("Living Room")));
        assert_eq!(out.message, "Casting Dune to Living Room. It resumes from 12 min.");

        let (mut command, mut out) = play(PlayOutcome::PinRequired);
        for_device(&mut command, &mut out, &d);
        let AgentCommand::Play(p) = &command else { panic!() };
        assert_eq!((p.device, p.show), (Some(d.id), AgentShow::window(true)), "the PIN prompt raises the window");
        assert_eq!((out.outcome, out.opened_in), (PlayOutcome::PinRequired, None));
        assert!(out.message.ends_with("It will play on Living Room."), "{}", out.message);

        let asked = pairing_required(&out, &d);
        assert_eq!((asked.outcome, asked.opened_in), (PlayOutcome::PairingRequired, None));
        assert_eq!(asked.message, "Pair Living Room in Tuner first (Settings ▸ Cast).");
    }

    /// `status.casting` names the TV and the typed reason of a failure, also after a cast that
    /// ended without the user asking (stopped from the TV's remote) left nothing casting.
    #[test]
    fn status_casting_names_the_tv_and_the_typed_reason() {
        use crate::model::CastView;
        use crate::player::PlayerError;
        let id = DeviceId::derive("airplay", "Living Room");
        let view = |phase| CastView {
            device: id,
            name: "Living Room".into(),
            kind: ReceiverKind::AppleTv,
            phase,
            level: None,
            as_is: false,
            behind_live_s: Some(18),
            can_seek: false,
            volume: false,
        };
        let casting = |phase, state| PlayerSnapshot { cast: Some(view(phase)), state, ..Default::default() };
        let none = CastLast::default();
        let last = CastLast { error: Some(CastErrorKind::StoppedOnTv), device: Some((id, "Living Room".into(), ReceiverKind::AppleTv)) };

        let out = casting_out(&casting(CastPhase::Playing, PlayerState::Playing), &none).unwrap();
        assert_eq!(
            (out.device_ref.as_str(), out.device.as_str(), out.phase, out.behind_live_s, out.error),
            (&*device_ref(id), "Living Room", CastPhase::Playing, Some(18), None)
        );
        // an old failure doesn't stick to a cast that works again
        assert_eq!(casting_out(&casting(CastPhase::Starting, PlayerState::Playing), &last).unwrap().error, None);
        // the player's own cast error, and the service's once the session was lost
        let failed = PlayerState::Error(PlayerError { kind: PlayerErrorKind::Cast(CastErrorKind::Firewall), message: String::new() });
        let out = casting_out(&casting(CastPhase::Lost, failed), &none).unwrap();
        assert_eq!(out.error.as_deref(), Some("cast-firewall"));
        let out = casting_out(&casting(CastPhase::Lost, PlayerState::Idle), &last).unwrap();
        assert_eq!(out.error.as_deref(), Some("cast-stopped-on-tv"));
        // nothing casts and nothing plays here: the TV last cast to and why it stopped
        let out = casting_out(&PlayerSnapshot::default(), &last).unwrap();
        assert_eq!((out.phase, out.error.as_deref(), out.device.as_str()), (CastPhase::Lost, Some("cast-stopped-on-tv"), "Living Room"));
        // …but not once something plays here again, and not without a failure
        assert_eq!(casting_out(&PlayerSnapshot { state: PlayerState::Playing, ..Default::default() }, &last), None);
        assert_eq!(casting_out(&PlayerSnapshot::default(), &none), None);
    }

    /// `control` while casting (DESIGN §40 §12): no window to move, and AirPlay has no volume.
    #[test]
    fn control_while_casting_leaves_the_window_and_the_volume_alone() {
        use super::super::proto::ControlAction as A;
        use crate::model::{CastPhase, CastView};
        let view = |volume| CastView {
            device: DeviceId::derive("airplay", "tv"),
            name: "TV".into(),
            kind: ReceiverKind::AppleTv,
            phase: CastPhase::Playing,
            level: None,
            as_is: false,
            behind_live_s: None,
            can_seek: false,
            volume,
        };
        let airplay = view(false);
        let code = |a, v: Option<&CastView>| cast_control_guard(a, v, false).map(|f| f.code);
        for window in [A::Popout, A::Window] {
            assert_eq!(code(window, Some(&airplay)), Some(ErrorCode::Unavailable));
            assert_eq!(code(window, None), None, "locally they are allowed");
        }
        for volume in [A::Volume, A::Mute, A::Unmute] {
            assert_eq!(code(volume, Some(&airplay)), Some(ErrorCode::Unsupported));
            assert_eq!(code(volume, Some(&view(true))), None, "a receiver with a volume takes it");
            assert_eq!(code(volume, None), None);
        }
        for stop in [A::StopCasting, A::PlayHere] {
            assert_eq!(code(stop, None), Some(ErrorCode::Unavailable), "nothing is being cast");
            assert_eq!(code(stop, Some(&airplay)), None);
        }
        // after the TV stopped it: Play here plays the last item here; there is nothing to stop
        assert!(cast_control_guard(A::PlayHere, None, true).is_none());
        assert_eq!(cast_control_guard(A::StopCasting, None, true).map(|f| f.code), Some(ErrorCode::Unavailable));
        // the transport reaches the TV as it is
        for transport in [A::Pause, A::Resume, A::TogglePause, A::Stop, A::PreviousChannel] {
            assert_eq!(code(transport, Some(&airplay)), None);
        }
        let f = cast_control_guard(A::Window, Some(&airplay), false).unwrap();
        assert!(f.message.starts_with("casting: "), "unavailable: casting");
    }
}
