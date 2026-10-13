//! `queue` (Up next, DESIGN §32 §7): one action-shaped tool over the Up next engine's
//! [`QueueHandle`](crate::queue::QueueHandle). OWNER: agent track.
//!
//! The engine does the guarding: an agent add of a locked item is refused ("locked: needs your
//! PIN in Tuner"), an explicit one while hidden is `not_found` (indistinguishable from a
//! missing one), and concealed rows are never in the snapshot this lists. `play` replaces what
//! plays (the skill calls `status` first) and shows where the `play` tool would (the pop-out by
//! default, `open_in`; DESIGN §30.13); a locked next item waits for the PIN on screen.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use super::params::{Queue, QueueAction};
use super::proto::{AgentRef, ErrorCode, fmt_time, item_ref};
use super::query::{Fail, Out, Q, parse_ref, text};
use super::server::{Inner, to_value};
use crate::model::{ItemKind, QueueId};
use crate::queue::{AddTarget, AddedBy, Blocked, Insert, PlayReply, QueueCommand, QueueReply, QueueSnapshot, Slot};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueueListOut {
    pub items: Vec<QueueItemOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_id: Option<i64>,
    /// When Up next ends (`exact` false: some lengths are guesses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_end: Option<String>,
    pub exact: bool,
    /// The next item starts by itself then (a countdown is showing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub countdown_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_after_id: Option<i64>,
    /// The user played something else: Up next stepped aside ("Alt+6 to carry on").
    pub interrupted: bool,
    /// Why it waits: `needs_pin`, `not_started`, `failed_streak`, `multiview`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<&'static str>,
    /// Up next drives the picture now.
    pub playing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueueItemOut {
    pub id: i64,
    /// The movie, episode or channel ref.
    #[serde(rename = "ref")]
    pub item: String,
    /// `movie`, `episode`, `series` or `programme`.
    pub kind: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// `pending`, `current`, `played`, `skipped`, `failed`.
    pub state: &'static str,
    /// Needs the PIN now (it waits for it on screen).
    pub locked: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub from_start: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_min: Option<u32>,
    /// Expected start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta: Option<String>,
    /// A live programme slot: its programme ref and times.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub programme: Option<SlotOut>,
    /// Why it was skipped or failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub by_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SlotOut {
    #[serde(rename = "ref")]
    pub item: String,
    pub start: String,
    pub end: String,
}

fn blocked_name(b: Blocked) -> &'static str {
    match b {
        Blocked::NeedsPin => "needs_pin",
        Blocked::NotStarted => "not_started",
        Blocked::FailedStreak => "failed_streak",
        Blocked::MultiView => "multiview",
    }
}

/// The snapshot as `list` shows it (concealed rows are never in it).
pub fn list_out(s: &QueueSnapshot, include_played: bool) -> QueueListOut {
    let items = s
        .rows
        .iter()
        .filter(|r| include_played || !r.entry.state.is_done())
        .map(|r| {
            let e = &r.entry;
            let kind = match (e.item.kind, &e.programme) {
                (_, Some(_)) => "programme",
                (ItemKind::Movie, _) => "movie",
                (ItemKind::Series, _) => "series",
                _ => "episode",
            };
            QueueItemOut {
                id: e.id.0,
                item: item_ref(&e.item),
                kind,
                title: super::query::clean(&e.title),
                subtitle: e.subtitle.as_deref().map(super::query::clean),
                state: e.state.as_str(),
                locked: r.locked,
                from_start: e.from_start,
                duration_min: e.duration_s.filter(|d| *d > 0.0).map(|d| (d / 60.0).round() as u32),
                eta: r.eta.map(fmt_time),
                programme: e.programme.as_ref().map(|p| SlotOut {
                    item: AgentRef::programme(e.item.clone(), p.start).to_string(),
                    start: fmt_time(p.start),
                    end: fmt_time(p.stop),
                }),
                note: r.note.clone(),
                by_agent: e.by_agent,
            }
        })
        .collect();
    QueueListOut {
        items,
        current_id: s.current.map(|i| i.0),
        next_id: s.next.map(|i| i.0),
        eta_end: s.eta_end.map(|(t, _)| fmt_time(t)),
        exact: s.eta_end.is_some_and(|(_, exact)| exact),
        countdown_at: s.countdown.as_ref().and_then(|c| c.at).map(fmt_time),
        stop_after_id: s.stop_after.map(|i| i.0),
        interrupted: s.interrupted,
        waiting: s.blocked.map(blocked_name),
        playing: s.driving,
    }
}

/// Where an add or a move goes: `end`, `next`, `top`, `bottom` or an index.
fn insert(s: Option<&str>) -> Out<Insert> {
    let Some(s) = s else { return Ok(Insert::End) };
    Ok(match text("at", s)? {
        "end" | "bottom" => Insert::End,
        "next" => Insert::Next,
        "top" => Insert::Index(0),
        n => Insert::Index(n.parse::<usize>().map_err(|_| Fail::bad("give end, next, top, bottom or an index"))?),
    })
}

/// The add targets of `refs` (refs that aren't items or programmes are refused here; the
/// engine decides visibility, locks and duplicates).
/// (asked ref, target) to add, and (asked ref, refusal) refused here.
type Targets = (Vec<(String, AddTarget)>, Vec<(String, &'static str)>);

fn targets(q: &Q, p: &Queue) -> Out<Targets> {
    let refs = p.refs.as_deref().filter(|r| !r.is_empty()).ok_or_else(|| Fail::bad("add needs refs"))?;
    if refs.len() > crate::store::queue::MAX_ADD {
        return Err(Fail::bad(format!("at most {} refs per add", crate::store::queue::MAX_ADD)));
    }
    let any_copy = match p.copy.as_deref() {
        None | Some("best") => true,
        Some("this") => false,
        Some(_) => return Err(Fail::bad("copy is best or this")),
    };
    let from_start = p.from_start.unwrap_or(false);
    let (mut out, mut refused) = (Vec::new(), Vec::new());
    for s in refs {
        match parse_ref(s)? {
            AgentRef::Item(r) if r.kind == ItemKind::Channel => refused.push((s.clone(), crate::queue::Refusal::NoGuide.code())),
            AgentRef::Item(r) => out.push((s.clone(), AddTarget { item: r, programme: None, from_start, any_copy })),
            AgentRef::Programme { channel, start } => {
                let prog = q.channel_by_ref(&channel).map(|c| q.programme_at(c, start)).transpose()?.flatten();
                let Some(prog) = prog else {
                    refused.push((s.clone(), crate::queue::Refusal::NotFound.code()));
                    continue;
                };
                let catchup_id = if prog.flags.has_catchup_id { crate::store::epg::programme(q.conn, prog.id)?.catchup_id } else { None };
                out.push((
                    s.clone(),
                    AddTarget { item: channel, programme: Some(Slot { start: prog.start, stop: prog.stop, catchup_id }), from_start, any_copy },
                ));
            }
        }
    }
    Ok((out, refused))
}

fn ids(p: &Queue) -> Out<Vec<QueueId>> {
    let ids = p.ids.as_deref().filter(|i| !i.is_empty()).ok_or_else(|| Fail::bad("give ids (from list)"))?;
    Ok(ids.iter().map(|i| QueueId(*i)).collect())
}

fn engine_fail(m: String) -> Fail {
    Fail::new(ErrorCode::Unavailable, m)
}

/// What an agent's `queue play` / `skip` does with the window (DESIGN §30.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shows {
    /// Show it where `play` would; `pin`: Up next waits for the PIN, typed in the window.
    Place { pin: bool },
    /// A skip inside a running Up next: it stays where it plays (like a zap).
    Stays,
    /// Up next waits (multi-view, a future slot): the window stays as it is.
    Waits,
    /// Nothing plays.
    Nothing,
}

/// `moves`: whether a play may move the window (not a skip inside a running Up next).
fn shows(reply: &QueueReply, moves: bool) -> Shows {
    match reply {
        QueueReply::Play(PlayReply::WaitingForPin(_)) => Shows::Place { pin: true },
        QueueReply::Play(PlayReply::Playing(_)) if moves => Shows::Place { pin: false },
        QueueReply::Play(PlayReply::Playing(_)) => Shows::Stays,
        QueueReply::Play(PlayReply::Waiting(..)) => Shows::Waits,
        _ => Shows::Nothing,
    }
}

/// What Up next started for an agent shows where `play` would (DESIGN §30.13): the engine sent
/// it to the main picture, which the app never hears about; `AgentCommand::Show` opens the
/// player there (without it the window stayed on Home with the corner mini player). A wait
/// for the PIN brings the window forward (the PIN is typed there); nothing played puts an
/// empty pop-out back in. `opened_in` for the result; `None` when nothing started.
async fn show(h: &Inner, reply: &QueueReply, asked: Option<crate::model::OpenIn>, moves: bool) -> Option<super::proto::OpenedIn> {
    match shows(reply, moves) {
        // casting, or Play on a TV: it plays on the TV and the window stays where it is
        // (DESIGN §40 §3.8, §3.9)
        Shows::Place { pin: false } | Shows::Stays if h.lands_on(&h.player.snapshot()).is_some() => Some(super::proto::OpenedIn::Device),
        Shows::Place { pin } => {
            let (show, w) = h.show(asked, pin).await;
            h.bus.send(crate::msg::AppEvent::Agent(super::AgentCommand::Show(show)));
            Some(Inner::raise_for(show, w))
        }
        Shows::Stays => Some(super::act::opened_in(super::act::AgentShow::IN_PLACE, h.window())),
        Shows::Waits => None,
        Shows::Nothing => {
            h.undo_empty_pop_out();
            None
        }
    }
}

pub(super) async fn handle(h: &Arc<Inner>, p: Queue) -> Out<Value> {
    let Some(engine) = h.shared.queue_handle() else {
        return Err(Fail::new(ErrorCode::Unavailable, "Up next isn't running in this Tuner"));
    };
    let ask = |cmd: QueueCommand| {
        let engine = engine.clone();
        async move {
            match engine.request(cmd).await {
                QueueReply::Error(m) => Err(engine_fail(m)),
                r => Ok(r),
            }
        }
    };
    let action = p.action;
    let asked = super::act::parse_open_in(p.open_in.as_deref())?;
    if asked.is_some() && !matches!(action, QueueAction::Play | QueueAction::Skip) {
        return Err(Fail::bad("open_in is for play and skip"));
    }
    match action {
        QueueAction::List => to_value(&list_out(&engine.snapshot(), p.include_played == Some(true))),
        QueueAction::Add => {
            let at = insert(p.at.as_deref())?;
            let (targets, mut refused) = h.run(true, move |q| targets(q, &p)).await?;
            let mut added = Vec::new();
            if !targets.is_empty() {
                let asked: Vec<(String, crate::model::ItemRef)> = targets.iter().map(|(s, t)| (s.clone(), t.item.clone())).collect();
                let cmd = QueueCommand::Add { targets: targets.into_iter().map(|(_, t)| t).collect(), at, by: AddedBy::Agent };
                if let QueueReply::Added { added: a, refused: r } = ask(cmd).await? {
                    let name =
                        |item: &crate::model::ItemRef| asked.iter().find(|(_, i)| i == item).map_or_else(|| item_ref(item), |(s, _)| s.clone());
                    added = a.iter().map(|(id, item)| json!({ "id": id.0, "ref": name(item) })).collect();
                    refused.extend(r.iter().map(|(item, why)| (name(item), why.code())));
                }
            }
            tracing::info!("agent: queue add → {} added, {} refused", added.len(), refused.len());
            let refused: Vec<Value> = refused.into_iter().map(|(r, e)| json!({ "ref": r, "error": e })).collect();
            to_value(&json!({ "added": added, "refused": refused }))
        }
        QueueAction::Move => {
            let ids = ids(&p)?;
            let to = insert(Some(p.to.as_deref().ok_or_else(|| Fail::bad("move needs to"))?))?;
            ask(QueueCommand::Move { ids, to }).await?;
            tracing::info!("agent: queue move");
            to_value(&list_out(&engine.snapshot(), false))
        }
        QueueAction::Remove => {
            let removed = match ask(QueueCommand::Remove(ids(&p)?)).await? {
                QueueReply::Removed(n) => n,
                _ => 0,
            };
            tracing::info!("agent: queue remove → {removed}");
            to_value(&json!({ "removed": removed }))
        }
        QueueAction::Clear => {
            let played_only = p.played_only.unwrap_or(true);
            let removed = match ask(QueueCommand::Clear { played_only }).await? {
                QueueReply::Removed(n) => n,
                _ => 0,
            };
            tracing::info!("agent: queue clear (played only: {played_only}) → {removed}");
            to_value(&json!({ "removed": removed }))
        }
        QueueAction::Play => {
            let snap = engine.snapshot();
            let id = match p.id {
                Some(i) => QueueId(i),
                None => snap.next.or(snap.current).ok_or_else(|| Fail::new(ErrorCode::Unavailable, "Up next is empty"))?,
            };
            if snap.row(id).is_none() {
                return Err(Fail::not_found("Up next item"));
            }
            let reply = ask(QueueCommand::PlayAt(id)).await?;
            let opened = show(h, &reply, asked, true).await;
            let mut out = match reply {
                QueueReply::Play(PlayReply::Playing(i)) => json!({ "playing": i.0 }),
                QueueReply::Play(PlayReply::WaitingForPin(i)) => json!({ "waiting_for_pin": true, "id": i.0,
                    "message": "Tuner is asking for the parental PIN on its screen; it plays once it is typed there." }),
                QueueReply::Play(PlayReply::Waiting(i, why)) => json!({ "waiting": blocked_name(why), "id": i.0 }),
                _ => json!({ "playing": Value::Null }),
            };
            if let Some(o) = opened {
                out["opened_in"] = serde_json::to_value(o).unwrap_or_default();
            }
            tracing::info!("agent: queue play #{} in {opened:?}", id.0);
            to_value(&out)
        }
        QueueAction::Skip => {
            // a skip inside a running Up next keeps it where it plays (like a zap)
            let was_driving = engine.snapshot().driving;
            let reply = ask(QueueCommand::Next).await?;
            let opened = show(h, &reply, asked, !was_driving || asked.is_some()).await;
            tracing::info!("agent: queue skip in {opened:?}");
            let s = engine.snapshot();
            let mut out = json!({ "current_id": s.current.map(|i| i.0), "next_id": s.next.map(|i| i.0) });
            if let Some(o) = opened {
                out["opened_in"] = serde_json::to_value(o).unwrap_or_default();
            }
            to_value(&out)
        }
        QueueAction::StopAfter => {
            let snap = engine.snapshot();
            let id = p.id.map(QueueId).or(snap.current).ok_or_else(|| Fail::new(ErrorCode::Unavailable, "Up next isn't playing; give id"))?;
            if snap.row(id).is_none_or(|r| r.entry.state.is_done()) {
                return Err(Fail::not_found("Up next item"));
            }
            ask(QueueCommand::StopAfter(Some(id))).await?;
            tracing::info!("agent: queue stop_after #{}", id.0);
            to_value(&json!({ "stop_after_id": id.0 }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DESIGN §30.13: `queue play` (and a skip that starts Up next) shows where `play` would;
    /// a skip inside a running Up next stays where it plays; the PIN shows the window even
    /// then; a wait leaves the window; nothing played is a failure (an empty pop-out goes in).
    #[test]
    fn up_next_shows_where_play_would_and_a_running_one_stays() {
        let id = QueueId(3);
        let playing = QueueReply::Play(PlayReply::Playing(id));
        let pin = QueueReply::Play(PlayReply::WaitingForPin(id));
        assert_eq!(shows(&playing, true), Shows::Place { pin: false });
        assert_eq!(shows(&playing, false), Shows::Stays, "a skip while Up next drives the picture");
        assert_eq!(shows(&pin, false), Shows::Place { pin: true });
        assert_eq!(shows(&QueueReply::Play(PlayReply::Waiting(id, Blocked::NotStarted)), true), Shows::Waits);
        assert_eq!(shows(&QueueReply::Play(PlayReply::Nothing), true), Shows::Nothing);
    }
}
