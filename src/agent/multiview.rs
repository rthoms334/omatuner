//! `multiview` (DESIGN §35 §6): two pictures of live TV or catch-up. OWNER: agent track; the
//! app side (`AgentCommand::MultiView`) is MV-2's.
//!
//! The handler validates and checks the connection budget the way the user's setup needs it
//! (each playlist carries as many streams as its account allows, usually one): two pictures on
//! one single-stream playlist need a copy in another playlist. When the only way is to move the
//! picture that plays now to its copy, the answer is `needs_rearrange` and the skill asks the
//! user before sending `rearrange=true`. Movies and episodes stay single view. The app applies
//! the PIN rules on screen.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};

use super::params::{MultiView, MultiViewAction};
use super::proto::{AgentRef, Airing, ErrorCode};
use super::query::{Fail, Out, Q, clean, parse_ref};
use super::server::{Inner, to_value};
use super::{AgentCommand, AgentMultiView, MultiViewKeep, MultiViewSwap};
use crate::config::MultiViewLayout;
use crate::model::{ChannelId, ItemKind, MediaRef, SourceId};
use crate::msg::AppEvent;

fn layout(s: Option<&str>) -> Out<Option<MultiViewLayout>> {
    Ok(match s {
        None => None,
        Some("side_by_side") => Some(MultiViewLayout::SideBySide),
        Some("big_small") => Some(MultiViewLayout::BigSmall),
        Some("pip") => Some(MultiViewLayout::Pip),
        Some(_) => return Err(Fail::bad("layout is side_by_side, big_small or pip")),
    })
}

/// One picture asked for: what plays and the playlists that could carry it (its own first).
struct Want {
    media: MediaRef,
    name: String,
    sources: Vec<SourceId>,
    locked: bool,
}

fn want(q: &Q, s: &str, from_start: bool) -> Out<Want> {
    let (channel, media) = match parse_ref(s)? {
        AgentRef::Item(r) if r.kind == ItemKind::Channel => {
            let c = q.channel_by_ref(&r).filter(|_| !q.conceals(&r)).ok_or_else(|| Fail::not_found("channel"))?;
            (c, MediaRef::Channel(c.id))
        }
        AgentRef::Item(_) => {
            return Err(Fail::new(ErrorCode::Unsupported, "Movies and episodes play in single view; multi-view is for live TV and catch-up"));
        }
        AgentRef::Programme { channel, start } => {
            let c = q.channel_by_ref(&channel).filter(|_| !q.conceals(&channel)).ok_or_else(|| Fail::not_found("programme"))?;
            let prog = q.programme_at(c, start)?.ok_or_else(|| Fail::not_found("programme"))?;
            let archive = || -> Out<MediaRef> {
                let catchup_id = if prog.flags.has_catchup_id { crate::store::epg::programme(q.conn, prog.id)?.catchup_id } else { None };
                Ok(MediaRef::Catchup { channel: c.id, start: prog.start, stop: prog.stop, catchup_id })
            };
            let media = match q.airing(&prog) {
                Airing::Upcoming => {
                    return Err(Fail::new(ErrorCode::NotStarted, clean(&format!("{} hasn't started: use reminders or schedule_switch", prog.title))));
                }
                Airing::Past if q.catchup(c, &prog) => archive()?,
                Airing::Past => return Err(Fail::new(ErrorCode::Unavailable, clean(&format!("{} has ended and has no catch-up", prog.title)))),
                Airing::Live if from_start && q.catchup(c, &prog) => archive()?,
                Airing::Live => MediaRef::Channel(c.id),
            };
            (c, media)
        }
    };
    // live: the channel's copies in other playlists can carry it too (catch-up stays on its own)
    let mut sources = vec![channel.source_id];
    if matches!(media, MediaRef::Channel(_)) {
        for copy in crate::store::copies::channel_copies(q.lib, q.conn, channel.id)? {
            if !(copy.explicit && q.view.hides_explicit) && q.channel(copy.id).is_some() && !sources.contains(&copy.source) {
                sources.push(copy.source);
            }
        }
    }
    Ok(Want { media, name: channel.name.to_string(), sources, locked: q.needs_pin(channel.locked) })
}

/// Whether `wants` fit the playlists' stream limits next to `held` (what the other picture
/// holds): `Ok(false)`: they fit only if the playing picture moves to a copy.
fn fits(wants: &[Want], held: Option<(SourceId, Vec<SourceId>)>, limits: &HashMap<SourceId, u32>) -> Result<bool, String> {
    let limit = |s: &SourceId| limits.get(s).copied().unwrap_or(u32::MAX);
    let assign = |fixed: &[SourceId]| -> bool {
        // try every choice of playlists (≤ 2 pictures, a handful of copies each)
        let choices: Vec<&Vec<SourceId>> = wants.iter().map(|w| &w.sources).collect();
        let mut stack = vec![(0usize, fixed.to_vec())];
        while let Some((i, used)) = stack.pop() {
            if i == choices.len() {
                return true;
            }
            for s in choices[i] {
                if used.iter().filter(|u| *u == s).count() < limit(s) as usize {
                    let mut next = used.clone();
                    next.push(*s);
                    stack.push((i + 1, next));
                }
            }
        }
        false
    };
    match held {
        None => assign(&[]).then_some(true).ok_or_else(|| "they are only on playlists that allow one stream at a time".to_owned()),
        Some((src, alts)) => {
            if assign(&[src]) {
                return Ok(true);
            }
            if alts.iter().any(|a| *a != src && assign(&[*a])) {
                return Ok(false);
            }
            Err("the playing picture and this one share a playlist that allows one stream at a time, and neither has a copy elsewhere".to_owned())
        }
    }
}

/// What the picture that stays (the focused one) plays when one ref goes alongside it.
enum Staying {
    Nothing,
    /// A live channel on a playlist: it may move to one of its copies.
    Live(ChannelId, SourceId),
    /// Catch-up holds its playlist's stream and stays on it.
    Catchup(SourceId),
    /// A film, an episode or a file: multi-view doesn't open next to it.
    Film,
}

fn staying(snap: &crate::player::PlayerSnapshot, q: &Q) -> Staying {
    use crate::model::PlayableKind as K;
    let Some(pl) = snap.playable.as_deref().filter(|_| snap.state != crate::player::PlayerState::Idle) else { return Staying::Nothing };
    match (&pl.kind, snap.claim) {
        (K::Live, Some(src)) => pl.item.as_ref().and_then(|i| q.lib.live.by_ref(i)).map_or(Staying::Nothing, |id| Staying::Live(id, src)),
        (K::Catchup { .. }, Some(src)) => Staying::Catchup(src),
        (K::Live | K::Catchup { .. }, None) => Staying::Nothing,
        (K::Movie | K::Episode { .. } | K::Url, _) => Staying::Film,
    }
}

fn plan_open(q: &Q, p: &MultiView, playing: Staying, multiview_on: bool) -> Out<(Option<AgentMultiView>, Value)> {
    let refs = p.refs.as_deref().filter(|r| (1..=2).contains(&r.len())).ok_or_else(|| Fail::bad("open takes 1–2 refs"))?;
    let from_start = p.from_start == Some(true);
    let wants: Vec<Want> = refs.iter().map(|s| want(q, s, from_start)).collect::<Out<_>>()?;
    let sound = p.sound.map(|s| s as usize);
    if sound.is_some_and(|s| s >= wants.len()) {
        return Err(Fail::bad("sound is an index into refs"));
    }
    let limits: HashMap<SourceId, u32> = crate::store::sources::list_sources(q.conn)?
        .into_iter()
        .filter_map(|s| Some((s.id, s.status.account?.max_connections.filter(|m| *m > 0)?)))
        .collect();
    // one ref goes alongside what plays (which keeps its playlist, or moves to a copy)
    let held = match (wants.len(), playing) {
        (1, Staying::Live(ch, src)) => {
            let alts: Vec<SourceId> = crate::store::copies::channel_copies(q.lib, q.conn, ch)?
                .iter()
                .filter(|c| !(c.explicit && q.view.hides_explicit) && q.channel(c.id).is_some())
                .map(|c| c.source)
                .collect();
            Some((src, alts))
        }
        (1, Staying::Catchup(src)) => Some((src, vec![])),
        (1, Staying::Film) if !multiview_on => {
            return Err(Fail::new(ErrorCode::Unavailable, "A film or episode is playing: multi-view is for live TV and catch-up"));
        }
        (1, Staying::Nothing) if !multiview_on => {
            return Err(Fail::new(ErrorCode::Unavailable, "Nothing is playing: give two refs, or play one first"));
        }
        _ => None,
    };
    let names: Vec<String> = wants.iter().map(|w| clean(&w.name)).collect();
    let fits = fits(&wants, held, &limits).map_err(|why| Fail::new(ErrorCode::Unavailable, format!("Can't show both: {why}.")))?;
    if !fits && p.rearrange != Some(true) {
        let unique: HashSet<&String> = names.iter().collect();
        return Ok((
            None,
            json!({
                "outcome": "needs_rearrange",
                "message": format!("To add {} the playing picture has to move to its copy in another playlist (it pauses up to 30 s). Ask the user, then send rearrange=true.", unique.into_iter().cloned().collect::<Vec<_>>().join(" and ")),
            }),
        ));
    }
    let locked = wants.iter().any(|w| w.locked);
    let cmd = AgentMultiView::Open {
        targets: wants.into_iter().map(|w| w.media).collect(),
        layout: layout(p.layout.as_deref())?,
        sound,
        rearrange: p.rearrange == Some(true),
    };
    let out = json!({
        "outcome": if locked { "pin_required" } else { "opening" },
        "message": if locked {
            "Tuner is asking for the parental PIN on its screen; the locked picture opens once it is typed there.".to_owned()
        } else {
            format!("Opening {} in multi-view.", names.join(" and "))
        },
    });
    Ok((Some(cmd), out))
}

pub(super) async fn handle(h: &Arc<Inner>, p: MultiView) -> Out<Value> {
    let tiles = h.shared.tiles();
    let send = |m: AgentMultiView| h.bus.send(AppEvent::Agent(AgentCommand::MultiView(m)));
    match p.action {
        MultiViewAction::Open => {
            // one ref replaces the picture that isn't focused: the focused one stays
            let snap = match h.shared.tile_player(tiles.focus) {
                Some(rx) => rx.borrow().clone(),
                None => h.player.snapshot(),
            };
            let on = tiles.on;
            let (cmd, out) = h.run(true, move |q| plan_open(q, &p, staying(&snap, q), on)).await?;
            if let Some(cmd) = cmd {
                send(cmd);
                tracing::info!("agent: multiview open");
            }
            to_value(&out)
        }
        MultiViewAction::Swap => {
            if !tiles.on {
                return Err(Fail::new(ErrorCode::Unavailable, "Multi-view is off"));
            }
            let what = match p.what.as_deref() {
                Some("sound") | None => MultiViewSwap::Sound,
                Some("places") => MultiViewSwap::Places,
                Some("focus") => MultiViewSwap::Focus,
                Some("layout") => MultiViewSwap::Layout(layout(p.layout.as_deref())?.ok_or_else(|| Fail::bad("what=layout needs layout"))?),
                Some(_) => return Err(Fail::bad("what is sound, places, focus or layout")),
            };
            send(AgentMultiView::Swap(what));
            tracing::info!("agent: multiview swap {what:?}");
            to_value(&json!({ "done": true }))
        }
        MultiViewAction::Close => {
            if !tiles.on {
                return Err(Fail::new(ErrorCode::Unavailable, "Multi-view is off"));
            }
            let keep = match p.keep.as_deref() {
                Some("a") => MultiViewKeep::A,
                Some("b") => MultiViewKeep::B,
                Some("sound") | None => MultiViewKeep::Sound,
                Some(_) => return Err(Fail::bad("keep is a, b or sound")),
            };
            send(AgentMultiView::Close { keep });
            tracing::info!("agent: multiview close");
            to_value(&json!({ "done": true }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(sources: &[i64]) -> Want {
        Want { media: MediaRef::Channel(ChannelId(1)), name: String::new(), sources: sources.iter().map(|s| SourceId(*s)).collect(), locked: false }
    }

    /// The user's setup: both accounts carry one stream. Two pictures need two playlists; one
    /// alongside a playing picture on the same playlist needs a copy, or the playing one moves.
    #[test]
    fn two_pictures_fit_one_stream_per_playlist() {
        let one = HashMap::from([(SourceId(1), 1), (SourceId(2), 1)]);
        assert_eq!(fits(&[w(&[1]), w(&[2])], None, &one), Ok(true));
        assert_eq!(fits(&[w(&[1]), w(&[1, 2])], None, &one), Ok(true), "the second uses its copy");
        assert!(fits(&[w(&[1]), w(&[1])], None, &one).is_err(), "both only on playlist 1");
        assert_eq!(fits(&[w(&[1, 2])], Some((SourceId(1), vec![])), &one), Ok(true));
        assert_eq!(fits(&[w(&[1])], Some((SourceId(1), vec![SourceId(2)])), &one), Ok(false), "the playing picture must move");
        assert!(fits(&[w(&[1])], Some((SourceId(1), vec![])), &one).is_err());
        assert_eq!(fits(&[w(&[1]), w(&[1])], None, &HashMap::new()), Ok(true), "no known limit");
    }
}
