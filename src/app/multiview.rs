//! Multi-view in the app (DESIGN §35, MV-2): the pictures' focus and sound, picture B's lazy
//! start and surface, the copy planner's plays, the "move the playing picture?" card, the
//! outside-play rule (plays from outside go to the silent picture and take the sound), and
//! keeping single view whole when a picture goes away.
//!
//! * **One truth for focus and sound.** `Services::tiles` routes commands (`focused()`,
//!   `audible_handle()`); [`MvApp::view`] mirrors it for the UI and is published to
//!   `AppShared::tiles` every frame (MPRIS, reminders and the agent route with it while the
//!   window is hidden).
//! * **Lazy picture B.** The first play for B starts its player on the blocking pool
//!   (`Services::start_tile_b`); the plan waits in [`MvApp`] until `TileEvent::Ready`, then goes
//!   out, and B's surface is made in the next `logic` with GL current.
//! * **Single view stays whole.** A picture closed (`q`), stopped or failed to start ends
//!   multi-view: the other carries on, focused and audible. Single view may go on in picture
//!   B (no reload when A is closed) until something starts in picture A (a film, Up next,
//!   MPRIS), which stops B. Films, episodes and files always play in picture A.
//! * **Parental.** Every play goes through the same gates: a locked channel asks for the PIN
//!   "for picture B"; the planner never picks a locked copy while locked nor one whose
//!   explicit flag differs; relocking stops each picture that shows locked or explicit content.

use std::collections::HashSet;
use std::sync::Arc;

use eframe::egui;

use super::TunerApp;
use crate::keymap::Action;
use crate::model::{ChannelId, MediaRef, PlayOrigin, PlayableKind, SourceId, TileId};
use crate::msg::Toast;
use crate::playback::{Kind, PlayIntent, PlaySource, Route as PlayRoute, RouteState, TilesState};
use crate::player::{PlayRequest, PlayerCommand, PlayerSnapshot, PlayerState};
use crate::store::copies::ChannelCopy;
use crate::ui::multiview::{self as mv, Aim, Layout, MvRequest, PlaceIn, PlaceRequest, Wanted};
use crate::ui::overlays::{ConfirmSpec, OverlayResult};
use crate::ui::{NavRequest, OverlayRequest};

const A: TileId = TileId::A;
const B: TileId = TileId::B;

/// The "move the playing picture?" card's token.
const MOVE_TOKEN: u64 = 0x4d56_4d4f_5645_0001;

/// Multi-view's state in the app.
#[derive(Default)]
pub(super) struct MvApp {
    pub(super) view: mv::MultiView,
    /// Picture B's player is starting: this goes out once it is adopted.
    waiting: Option<Sending>,
    /// B was adopted: its surface is made in the next `logic` (GL current).
    make_surface: bool,
    /// A move waiting for the user's yes.
    consent: Option<Sending>,
    /// The plays waiting for a locked channel's PIN (one, or the agent's pair).
    pin: Vec<PlaceIn>,
    /// The load generation each picture had when it was sent a play: it counts as busy until
    /// its player took the command.
    sent: [Option<u64>; 2],
    /// Picture A's load generation when single view went on in picture B.
    a_gen: u64,
    /// The parental session was unlocked last frame (relocking stops locked pictures).
    unlocked: bool,
    /// Points per pixel of the last frame (the planner's quality fit).
    pub(super) ppp: f32,
}

/// A planned play: the commands per picture.
#[derive(Debug, Clone)]
struct Sending {
    target: TileId,
    cmds: Vec<(TileId, PlayerCommand)>,
    /// The picture moving to another playlist, and that playlist (only with the user's yes).
    mover: Option<(TileId, SourceId)>,
    take_sound: bool,
    slots: [TileId; 2],
    /// "Picture B plays it from TV 2 — TV is busy with picture A".
    note: Option<String>,
}

impl MvApp {
    /// A PIN prompt or a "move the playing picture?" card that went away unanswered (relock
    /// closed every dialog): their plays must never go out later.
    pub(super) fn forget_pending(&mut self) {
        self.pin.clear();
        self.consent = None;
    }
}

fn slot(t: TileId) -> usize {
    usize::from(t.0).min(1)
}

/// What a snapshot plays, for the router.
pub(super) fn kind_of(s: &PlayerSnapshot) -> Option<Kind> {
    if s.state == PlayerState::Idle {
        return None;
    }
    Some(match s.playable.as_ref().map(|p| &p.kind)? {
        PlayableKind::Live => Kind::Live,
        PlayableKind::Catchup { .. } => Kind::Catchup,
        PlayableKind::Movie | PlayableKind::Episode { .. } | PlayableKind::Url => Kind::Vod,
    })
}

fn media_kind(m: &MediaRef) -> Kind {
    match m {
        MediaRef::Channel(_) => Kind::Live,
        MediaRef::Catchup { .. } => Kind::Catchup,
        _ => Kind::Vod,
    }
}

/// The media a command plays, if it starts something.
pub(super) fn played_media(cmd: &PlayerCommand) -> Option<&MediaRef> {
    match cmd {
        PlayerCommand::Play(r) => Some(&r.media),
        PlayerCommand::PlayCopy(c) => Some(&c.req.media),
        _ => None,
    }
}

/// `zap` with `from` replaced by `to` (a copy keeps the zap-list position of the pick).
fn zap_with(zap: Option<&crate::model::ZapList>, from: ChannelId, to: ChannelId) -> Option<crate::model::ZapList> {
    let z = zap?;
    if from == to {
        return Some(z.clone());
    }
    let channels: Arc<[ChannelId]> = z.channels.iter().map(|c| if *c == from { to } else { *c }).collect();
    Some(crate::model::ZapList { collection: z.collection.clone(), channels })
}

impl TunerApp {
    fn services_mut(&mut self) -> &mut crate::services::Services {
        self.services.as_mut().expect("services alive")
    }

    /// `tile`'s snapshot, if its player exists.
    pub(super) fn mv_snap(&self, tile: TileId) -> Option<Arc<PlayerSnapshot>> {
        self.services().tiles.player(tile).map(|p| p.handle().snapshot())
    }

    /// Multi-view as this frame's screens see it.
    pub(super) fn mv_frame(&self) -> mv::MvFrame {
        let starting = self.mv.waiting.is_some();
        mv::MvFrame { view: self.mv.view, snaps: [self.mv_snap(A), self.mv_snap(B)], starting }
    }

    /// The pictures as the routers see them (`AppShared::tiles`).
    pub(super) fn tiles_state(&self) -> TilesState {
        let kind = |t| self.mv_snap(t).and_then(|s| kind_of(&s));
        TilesState { on: self.mv.view.on, focus: self.mv.view.focus, audible: self.mv.view.audible, kinds: [kind(A), kind(B)] }
    }

    /// A command to one picture (tests record it).
    pub(super) fn send_to(&self, tile: TileId, cmd: PlayerCommand) {
        #[cfg(test)]
        self.log.borrow_mut().tile_commands.push((tile, cmd.clone()));
        if let Some(p) = self.services().tiles.player(tile) {
            p.handle().send(cmd);
        }
    }

    /// A film, an episode or a file while multi-view is on (or single view went on in picture
    /// B): it plays in picture A, where resume and the next-episode card live, and picture B
    /// stops (multi-view ends with the next frame). True when it was sent that way.
    pub(super) fn mv_vod_to_a(&self, cmd: &PlayerCommand) -> bool {
        let vod = played_media(cmd).is_some_and(|m| media_kind(m) == Kind::Vod);
        if !vod || (!self.mv.view.on && self.mv.view.focus == A) {
            return false;
        }
        if self.services().tiles.b().is_some() {
            self.send_to(B, PlayerCommand::Stop);
        }
        self.services().tiles.handle(A).send(cmd.clone());
        true
    }

    fn mv_focus(&mut self, t: TileId) {
        let tiles = &mut self.services_mut().tiles;
        tiles.set_focus(t);
        let focus = tiles.focus();
        self.mv.view.focus = focus;
        self.snapshot = self.services().tiles.focused().snapshot();
    }

    fn mv_sound(&mut self, t: TileId) {
        let tiles = &mut self.services_mut().tiles;
        tiles.move_sound(t);
        let audible = tiles.audible();
        self.mv.view.audible = audible;
    }

    /// Single view on `t` (the other picture closed, stopped or never started).
    fn mv_single(&mut self, t: TileId) {
        self.mv.view.on = false;
        self.mv.view.slots = [A, B];
        self.mv_focus(t);
        self.mv_sound(t);
        if t == B {
            self.mv.a_gen = self.mv_snap(A).map_or(0, |s| s.load_generation);
        }
    }

    /// Apply a multi-view request (after the frame that made it).
    pub(super) fn apply_mv(&mut self, r: MvRequest) {
        let on = self.mv.view.on;
        match r {
            MvRequest::Place(p) => self.mv_place(p),
            MvRequest::Focus(t) if on => self.mv_focus(t),
            MvRequest::FocusNext if on => self.mv_focus(self.mv.view.unfocused()),
            MvRequest::Sound(t) if on => self.mv_sound(t.unwrap_or(self.mv.view.focus)),
            MvRequest::Swap if on => self.mv.view.slots.swap(0, 1),
            MvRequest::CycleLayout if on => {
                let cfg = self.services().config();
                let next = Layout::effective(cfg.player.multiview.layout, self.window_size.x, self.theme.rem(), self.video_only).next();
                self.config_edits.push(Box::new(move |c: &mut crate::config::Config| c.player.multiview.layout = next.config()));
            }
            MvRequest::SetLayout(l) => self.config_edits.push(Box::new(move |c: &mut crate::config::Config| c.player.multiview.layout = l.config())),
            MvRequest::SetCorner(k) => self.config_edits.push(Box::new(move |c: &mut crate::config::Config| c.player.multiview.pip_corner = k)),
            MvRequest::Close(t) => {
                let t = t.unwrap_or(self.mv.view.focus);
                self.send_to(t, PlayerCommand::Stop);
                self.mv.sent[slot(t)] = None;
                if on {
                    self.mv_single(t.other());
                }
            }
            MvRequest::PauseAll => self.mv_pause_all(),
            MvRequest::StopAll => self.mv_stop_all(),
            MvRequest::SingleView(cmd) => self.send_player(*cmd),
            MvRequest::Focus(_) | MvRequest::FocusNext | MvRequest::Sound(_) | MvRequest::Swap | MvRequest::CycleLayout => {}
        }
    }

    /// The `multiview` MCP tool (validated by the handler; the PIN and copy rules apply here
    /// as for the user's own plays). One target plays alongside what plays; two fill A and B.
    pub(super) fn on_agent_multiview(&mut self, r: &crate::agent::AgentMultiView) {
        use crate::agent::{AgentMultiView as M, MultiViewKeep as Keep, MultiViewSwap as Swap};
        let set_layout = |app: &mut Self, l: crate::config::MultiViewLayout| {
            app.config_edits.push(Box::new(move |c: &mut crate::config::Config| c.player.multiview.layout = l));
        };
        match r {
            M::Open { targets, layout, sound, rearrange } => {
                if let Some(l) = layout {
                    set_layout(self, *l);
                }
                let place = |i: usize, aim: Aim, media: &MediaRef| PlaceIn {
                    take_sound: *sound == Some(i),
                    rearrange: *rearrange,
                    ..PlaceIn::new(aim, media.clone(), None)
                };
                match targets.as_slice() {
                    [one] => self.mv_place(place(0, Aim::Alongside, one)),
                    // one plan for both (A and B; from single view the focused picture and the
                    // other), so the second never plans against what the first replaces
                    [a, b, ..] if self.mv.view.on => self.mv_place_pair(vec![place(0, Aim::Tile(A), a), place(1, Aim::Tile(B), b)]),
                    [a, b, ..] => self.mv_place_pair(vec![place(0, Aim::Focused, a), place(1, Aim::Alongside, b)]),
                    [] => {}
                }
            }
            M::Swap(Swap::Sound) => self.apply_mv(MvRequest::Sound(Some(self.mv.view.audible.other()))),
            M::Swap(Swap::Places) => self.apply_mv(MvRequest::Swap),
            M::Swap(Swap::Focus) => self.apply_mv(MvRequest::FocusNext),
            M::Swap(Swap::Layout(l)) => set_layout(self, *l),
            M::Close { keep } => {
                let keep = match keep {
                    Keep::A => A,
                    Keep::B => B,
                    Keep::Sound => self.mv.view.audible,
                };
                self.apply_mv(MvRequest::Close(Some(keep.other())));
            }
        }
    }

    /// Ctrl+Shift+P with two pictures: pause both, or play both when both are paused.
    pub(super) fn mv_pause_all(&mut self) {
        let playing = [A, B].into_iter().filter_map(|t| self.mv_snap(t)).any(|s| s.state.is_active() && !s.paused);
        for t in [A, B] {
            self.send_to(t, PlayerCommand::SetPause(playing));
        }
    }

    /// Ctrl+Shift+S, the mini player's ✕ with two pictures: stop both.
    pub(super) fn mv_stop_all(&mut self) {
        for t in [A, B] {
            self.send_to(t, PlayerCommand::Stop);
        }
        self.mv.sent = [None, None];
        self.mv.waiting = None;
        self.mv_single(A);
    }

    /// Play a live channel or catch-up in a picture (module docs).
    pub(super) fn mv_place(&mut self, p: PlaceIn) {
        let view = self.mv.view;
        let kind = media_kind(&p.media);
        let channel = match &p.media {
            MediaRef::Channel(c) | MediaRef::Catchup { channel: c, .. } => Some(*c),
            _ => None,
        };
        let plain = |p: &PlaceIn| {
            PlayerCommand::Play(PlayRequest { media: p.media.clone(), zap: p.zap.clone(), origin: p.origin, resume: false, immediate: true })
        };
        // the parental gate before anything plays, whichever picture it is for
        let ch = match channel {
            Some(id) => match self.mv_gate(id, std::slice::from_ref(&p)) {
                Some(ch) => Some(ch),
                None => return,
            },
            None => None,
        };
        let tile = match p.aim {
            Aim::Focused | Aim::Silent if !view.on => return self.send_player(plain(&p)),
            Aim::Focused => view.focus,
            Aim::Silent => view.silent(),
            Aim::Tile(t) => t,
            Aim::Alongside => {
                let main_kind = kind_of(&self.snapshot);
                let st = RouteState { tiles: self.tiles_state(), main_kind, ..Default::default() };
                let intent = PlayIntent { source: PlaySource::Alongside, kind, live_channel: channel };
                match crate::playback::route(&intent, &st, &self.services().agent.guard(), crate::model::now()) {
                    PlayRoute::Refuse(why) => return self.toast(Toast::info(why)),
                    PlayRoute::Tile { tile, .. } if view.on => tile,
                    // nothing plays: it simply plays
                    PlayRoute::Tile { .. } if main_kind.is_none() => return self.send_player(plain(&p)),
                    PlayRoute::Tile { .. } => view.focus.other(),
                    _ => return,
                }
            }
        };
        if kind == Kind::Vod || (!view.on && tile == view.focus) {
            return self.send_player(plain(&p));
        }
        let take_sound = p.take_sound || p.aim == Aim::Silent;
        let (Some(id), Some(ch)) = (channel, ch) else { return };
        let cfg = self.services().config();
        let unlocked = self.parental.is_unlocked(&cfg.parental);
        // already on screen: that picture is chosen (Enter) or gets the sound (outside plays)
        if matches!(p.media, MediaRef::Channel(_))
            && let Some(t) = [A, B].into_iter().find(|t| {
                (view.on || *t == view.focus)
                    && self.mv_snap(*t).is_some_and(|s| s.state != PlayerState::Idle && crate::ui::screens::live::playing_channel(&s) == Some(id))
            })
        {
            if view.on && take_sound {
                self.mv_sound(t);
            } else if view.on && p.aim == Aim::Focused {
                self.mv_focus(t);
            }
            return;
        }
        let keeper = tile.other();
        let keeper_snap = self.mv_snap(keeper).filter(|s| s.state != PlayerState::Idle && (view.on || keeper == view.focus));
        let slots = if view.on { view.slots } else { [view.focus, tile] };
        let layout = Layout::effective(cfg.player.multiview.layout, self.window_size.x, self.theme.rem(), self.video_only);
        let area = egui::Rect::from_min_size(egui::Pos2::ZERO, self.window_size);
        let ppp = if self.mv.ppp > 0.0 { self.mv.ppp } else { 1.0 };
        let target_px = mv::target_px(layout, area, slots, cfg.player.multiview.pip_corner, self.theme.rem(), ppp, tile);
        let candidates = match p.media {
            MediaRef::Channel(_) => self.copies_of(id),
            _ => vec![self.copy_of(&ch)],
        };
        let all: Vec<ChannelId> = candidates.iter().map(|c| c.id).collect();
        let keeps = keeper_snap.as_ref().and_then(|s| self.keeper_copies(keeper, s));
        let mut trouble = self.trouble(&all);
        if let Some((_, _, k)) = &keeps {
            trouble.extend(self.trouble(&k.iter().map(|c| c.id).collect::<Vec<_>>()));
        }
        let names = self.lib.sources.iter().map(|s| (s.id, s.name.clone())).collect();
        let req = PlaceRequest { wanted: vec![Wanted { tile, pick: id, candidates, target_px }], keeps, locked_session: !unlocked, trouble, names };
        let plan = match mv::plan(&req, &self.services().tiles.budget().usage()) {
            Ok(plan) => plan,
            Err(u) => return self.toast(Toast::info(u.reason)),
        };
        let light = cfg.player.multiview.light_decode_small.0;
        let small = |t: TileId| layout != Layout::SideBySide && slots[1] == t;
        let mut cmds = Vec::new();
        let mut note = None;
        let mut mover = None;
        for (t, chosen, from_copy) in plan.tiles {
            if light {
                cmds.push((t, PlayerCommand::SetLightDecode(small(t))));
            }
            let source = self.lib.live.get(chosen).map(|c| c.source_id);
            let cmd = if t == tile {
                if from_copy && let (Some(to), Some(from)) = (source, keeper_snap.as_ref().and_then(|s| s.claim)) {
                    note = Some(format!(
                        "Picture {tile} plays it from {} — {} is busy with picture {keeper}",
                        self.source_name(to),
                        self.source_name(from)
                    ));
                }
                let media = match &p.media {
                    MediaRef::Catchup { .. } => p.media.clone(),
                    _ => MediaRef::Channel(chosen),
                };
                PlayRequest { media, zap: zap_with(p.zap.as_ref(), id, chosen), origin: p.origin, resume: false, immediate: true }
            } else {
                // the playing picture moves to its copy (only after the user's yes)
                let s = keeper_snap.as_ref();
                let cur = s.and_then(|s| crate::ui::screens::live::playing_channel(s)).unwrap_or(chosen);
                mover = source.map(|src| (t, src));
                let origin = s.and_then(|s| s.origin).unwrap_or(PlayOrigin::MultiView);
                PlayRequest {
                    media: MediaRef::Channel(chosen),
                    zap: zap_with(s.and_then(|s| s.zap.as_ref()), cur, chosen),
                    origin,
                    resume: false,
                    immediate: true,
                }
            };
            cmds.push((t, PlayerCommand::Play(cmd)));
        }
        let sending = Sending { target: tile, cmds, mover, take_sound, slots, note };
        match plan.moves {
            Some(m) if !p.rearrange => {
                let to = sending.mover.map(|(_, s)| self.source_name(s)).unwrap_or_default();
                let from = keeper_snap.as_ref().and_then(|s| s.claim).map(|s| self.source_name(s)).unwrap_or_default();
                self.mv.consent = Some(sending);
                self.nav.push(NavRequest::Open(OverlayRequest::Confirm(ConfirmSpec {
                    title: format!("Move picture {m}?"),
                    body: format!(
                        "Picture {tile}'s channel is only on {from}, which picture {m} uses. Move picture {m} to its {to} copy so {tile} can use {from}? It can take up to 30 s while {from} frees up."
                    ),
                    confirm_label: "Move".into(),
                    destructive: false,
                    token: MOVE_TOKEN,
                })));
            }
            _ => self.mv_send(sending),
        }
    }

    /// The parental gate of a picture's channel (DESIGN §20.3): `None` when it can't play now —
    /// gone, DRM, or locked while the session is (the PIN prompt opens, and `plays` go out once
    /// it is typed).
    fn mv_gate(&mut self, id: ChannelId, plays: &[PlaceIn]) -> Option<crate::model::ChannelSummary> {
        let Some(ch) = self.lib.live.get(id).cloned() else {
            self.toast(Toast::info("That channel is no longer in your playlists"));
            return None;
        };
        if ch.unsupported {
            self.toast(Toast::info("DRM-protected — can't be played"));
            return None;
        }
        if ch.locked && !self.parental.is_unlocked(&self.services().config().parental) {
            self.pending_locked = None;
            self.pending_agent = None;
            let view = self.mv.view;
            let picture = match plays.first().map(|p| p.aim) {
                _ if plays.len() > 1 => None,
                Some(Aim::Tile(t)) => Some(t),
                Some(Aim::Focused) if view.on => Some(view.focus),
                Some(Aim::Silent) if view.on => Some(view.silent()),
                Some(Aim::Alongside) if view.on || kind_of(&self.snapshot).is_some() => Some(view.focus.other()),
                _ => None,
            };
            let reason = match picture {
                Some(t) => format!("“{}” is locked — for picture {t}", ch.name),
                None => format!("“{}” is locked", ch.name),
            };
            self.nav.push(NavRequest::Open(OverlayRequest::Pin { then: Box::new(Action::OpenPlayer), reason }));
            self.mv.pin = plays.to_vec();
            return None;
        }
        Some(ch)
    }

    /// Two live channels or catch-ups (the agent's `multiview open`): from single view the
    /// focused picture and the other, with multi-view on A and B (the layout's places stay),
    /// planned together on the playlists' limits (nothing is kept: both pictures play anew).
    /// A picture that already shows one of the two keeps it, and the other one goes in the
    /// other picture, planned against it: a move of the picture that stays is asked first.
    /// Both pass the parental gate before either plays.
    pub(super) fn mv_place_pair(&mut self, pair: Vec<PlaceIn>) {
        let channel = |p: &PlaceIn| match &p.media {
            MediaRef::Channel(c) | MediaRef::Catchup { channel: c, .. } => Some(*c),
            _ => None,
        };
        let (Some(first), Some(second)) = (pair.first(), pair.get(1)) else { return };
        let (Some(id0), Some(id1)) = (channel(first), channel(second)) else {
            return self.toast(Toast::info("Films and episodes play on their own"));
        };
        let Some(ch0) = self.mv_gate(id0, &pair) else { return };
        let Some(ch1) = self.mv_gate(id1, &pair) else { return };
        let view = self.mv.view;
        let focus = view.focus;
        let shows = |app: &Self, t: TileId, id: ChannelId| app.mv_snap(t).is_some_and(|s| crate::ui::screens::live::playing_channel(&s) == Some(id));
        if view.on {
            // a picture already shows one of them (a live channel): it stays, the other one
            // goes in the other picture
            let shown = |p: &PlaceIn, id: ChannelId| match p.media {
                MediaRef::Channel(_) => [A, B].into_iter().find(|t| shows(self, *t, id)),
                _ => None,
            };
            match (shown(first, id0), shown(second, id1)) {
                // both on screen already: at most the sound moves
                (Some(t0), Some(t1)) if t0 != t1 => {
                    if let Some(t) = [(first, t0), (second, t1)].into_iter().find_map(|(p, t)| p.take_sound.then_some(t)) {
                        self.mv_sound(t);
                    }
                    return;
                }
                (Some(t), _) => return self.mv_keep_one(first, t, second),
                (None, Some(t)) => return self.mv_keep_one(second, t, first),
                (None, None) => {}
            }
        } else if matches!(first.media, MediaRef::Channel(_)) && shows(self, focus, id0) {
            // the focused picture already shows the first: only the second needs a picture
            let p = PlaceIn { aim: Aim::Alongside, ..second.clone() };
            return self.mv_place(p);
        }
        let cfg = self.services().config();
        let unlocked = self.parental.is_unlocked(&cfg.parental);
        // the pictures they go to, and the layout's places
        let (tiles, slots) = if view.on { ([A, B], view.slots) } else { ([focus, focus.other()], [focus, focus.other()]) };
        let layout = Layout::effective(cfg.player.multiview.layout, self.window_size.x, self.theme.rem(), self.video_only);
        let area = egui::Rect::from_min_size(egui::Pos2::ZERO, self.window_size);
        let ppp = if self.mv.ppp > 0.0 { self.mv.ppp } else { 1.0 };
        let mut wanted = Vec::new();
        for (p, id, ch, tile) in [(first, id0, &ch0, tiles[0]), (second, id1, &ch1, tiles[1])] {
            let candidates = match p.media {
                MediaRef::Channel(_) => self.copies_of(id),
                _ => vec![self.copy_of(ch)],
            };
            let target_px = mv::target_px(layout, area, slots, cfg.player.multiview.pip_corner, self.theme.rem(), ppp, tile);
            wanted.push(Wanted { tile, pick: id, candidates, target_px });
        }
        let all: Vec<ChannelId> = wanted.iter().flat_map(|w| w.candidates.iter().map(|c| c.id)).collect();
        let trouble = self.trouble(&all);
        let names = self.lib.sources.iter().map(|s| (s.id, s.name.clone())).collect();
        let req = PlaceRequest { wanted, keeps: None, locked_session: !unlocked, trouble, names };
        let plan = match mv::plan(&req, &self.services().tiles.budget().usage()) {
            Ok(plan) => plan,
            Err(u) => return self.toast(Toast::info(u.reason)),
        };
        let light = cfg.player.multiview.light_decode_small.0;
        let mut cmds = Vec::new();
        for ((t, chosen, _), (p, id)) in plan.tiles.into_iter().zip([(first, id0), (second, id1)]) {
            if light {
                cmds.push((t, PlayerCommand::SetLightDecode(layout != Layout::SideBySide && slots[1] == t)));
            }
            let media = match &p.media {
                MediaRef::Catchup { .. } => p.media.clone(),
                _ => MediaRef::Channel(chosen),
            };
            let req = PlayRequest { media, zap: zap_with(p.zap.as_ref(), id, chosen), origin: p.origin, resume: false, immediate: true };
            cmds.push((t, PlayerCommand::Play(req)));
        }
        // the sound stays where it is unless one of them takes it
        let (target, take_sound) = match pair.iter().position(|p| p.take_sound) {
            Some(i) => (tiles[i.min(1)], true),
            None => (tiles[1], false),
        };
        self.mv_send(Sending { target, cmds, mover: None, take_sound, slots, note: None });
    }

    /// [`Self::mv_place_pair`] with picture `at` already showing `kept`: it stays (with the
    /// sound when `kept` takes it) and `other` goes in the other picture, planned against the
    /// one that stays (moving it is asked first, `mv_place`).
    fn mv_keep_one(&mut self, kept: &PlaceIn, at: TileId, other: &PlaceIn) {
        if kept.take_sound {
            self.mv_sound(at);
        }
        self.mv_place(PlaceIn { aim: Aim::Tile(at.other()), ..other.clone() });
    }

    /// Send a plan, starting picture B first when it doesn't exist yet.
    fn mv_send(&mut self, s: Sending) {
        if let Some(n) = &s.note {
            self.toast(Toast::info(n.clone()));
        }
        self.mv.view.slots = s.slots;
        let needs_b = s.cmds.iter().any(|(t, _)| *t == B) && self.services().tiles.b().is_none();
        if needs_b {
            // the pictures it is for count as busy while B starts (an idle A must not end
            // multi-view before its play goes out)
            for (t, _) in &s.cmds {
                self.mv.sent[slot(*t)] = Some(self.mv_snap(*t).map_or(0, |x| x.load_generation));
            }
            self.services_mut().start_tile_b();
            self.mv.waiting = Some(s);
            self.mv.view.on = true;
            return;
        }
        self.mv_dispatch(s);
    }

    fn mv_dispatch(&mut self, s: Sending) {
        for (t, _) in &s.cmds {
            self.mv.sent[slot(*t)] = Some(self.mv_snap(*t).map_or(0, |x| x.load_generation));
        }
        match s.mover {
            Some((mover, to)) => {
                let play = |t: TileId| s.cmds.iter().rev().find(|(x, c)| *x == t && matches!(c, PlayerCommand::Play(_))).map(|(_, c)| c.clone());
                for (t, c) in s.cmds.iter().filter(|(_, c)| !matches!(c, PlayerCommand::Play(_))) {
                    self.send_to(*t, c.clone());
                }
                if let (Some(mc), Some(tc)) = (play(mover), play(s.target)) {
                    #[cfg(test)]
                    self.log.borrow_mut().tile_commands.extend([(mover, mc.clone()), (s.target, tc.clone())]);
                    // the target's playlist is claimed ahead too: the mover's load then waits
                    // for the target's old stream instead of being refused (a swap)
                    let target_to = played_media(&tc)
                        .and_then(|m| match m {
                            MediaRef::Channel(c) | MediaRef::Catchup { channel: c, .. } => self.lib.live.get(*c),
                            _ => None,
                        })
                        .map(|c| c.source_id);
                    self.services().tiles.rearrange(mover, to, mc, s.target, target_to, tc);
                }
            }
            None => {
                // both pictures play anew (the agent's two): both claims move first, as in
                // `Tiles::rearrange`, so pictures that trade playlists each wait for the other's
                // old stream to close instead of being refused, whichever loads first
                let claims: Vec<(TileId, SourceId)> = s
                    .cmds
                    .iter()
                    .filter_map(|(t, c)| match played_media(c)? {
                        MediaRef::Channel(ch) | MediaRef::Catchup { channel: ch, .. } => Some((*t, self.lib.live.get(*ch)?.source_id)),
                        _ => None,
                    })
                    .collect();
                if claims.len() > 1 {
                    for (t, src) in claims {
                        self.services().tiles.budget().claim(t, Some(src));
                    }
                }
                for (t, c) in s.cmds {
                    self.send_to(t, c);
                }
            }
        }
        self.mv.view.on = true;
        if s.take_sound {
            self.mv_sound(s.target);
        }
    }

    /// `TileEvent`s: B's start, and its player's events (a focused picture's reach the screens
    /// as the player's: returned).
    pub(super) fn on_tile_event(&mut self, ev: crate::player::tiles::TileEvent) -> Option<crate::player::PlayerEvent> {
        use crate::player::tiles::TileEvent;
        match ev {
            TileEvent::Ready(_) | TileEvent::Failed(..) => {
                match self.services_mut().adopt_tile_b() {
                    Some(Ok(())) => {
                        self.mv.make_surface = true;
                        if let Some(s) = self.mv.waiting.take() {
                            self.mv_dispatch(s);
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!("multi-view: picture B didn't start: {e}");
                        self.toast(Toast::error("Couldn't start the second picture (see Logs)"));
                        self.mv.waiting = None;
                        self.mv_single(self.mv.view.focus);
                    }
                    None => {}
                }
                None
            }
            TileEvent::Player(t, pev) => {
                self.on_picture_event(t, &pev);
                (t == self.mv.view.focus).then_some(pev)
            }
        }
    }

    /// A picture's player event the app tells the user about: a skipped zap; a copy fallback
    /// of a picture that isn't the focused one (the focused one's goes the usual way).
    pub(super) fn on_picture_event(&mut self, t: TileId, ev: &crate::player::PlayerEvent) {
        use crate::player::PlayerEvent;
        match ev {
            PlayerEvent::ZapSkipped { channel, source, holder } => {
                let name = self.lib.live.get(*channel).map_or_else(|| "A channel".to_owned(), |c| c.name.to_string());
                let by = holder.map(|h| format!(" by picture {h}")).unwrap_or_default();
                self.toast(Toast::info(format!("{name} skipped — {} is in use{by}", self.source_name(*source))));
            }
            PlayerEvent::FellBack { title, from, to, .. } if t != self.mv.view.focus => {
                self.toast(crate::ui::copies::fell_back_toast(&self.lib, title, *from, *to));
            }
            _ => {}
        }
    }

    /// The confirm card's answer (true when it was multi-view's), and the PIN typed for a
    /// picture's locked channel.
    pub(super) fn mv_overlay_result(&mut self, result: &OverlayResult) -> bool {
        match result {
            OverlayResult::Confirmed(MOVE_TOKEN) => {
                if let Some(s) = self.mv.consent.take() {
                    self.mv_send(s);
                }
                true
            }
            OverlayResult::Cancelled(MOVE_TOKEN) => {
                self.mv.consent = None;
                true
            }
            // a newer prompt of `play_channel` / the agent's is theirs
            OverlayResult::Unlocked(Action::OpenPlayer) if self.pending_locked.is_some() || self.pending_agent.is_some() => {
                self.mv.pin.clear();
                false
            }
            OverlayResult::Unlocked(Action::OpenPlayer) => {
                let mut plays = std::mem::take(&mut self.mv.pin);
                match plays.len() {
                    0 => return false,
                    1 => self.mv_place(plays.remove(0)),
                    _ => self.mv_place_pair(plays),
                }
                true
            }
            _ => false,
        }
    }

    /// An outside play (reminder, Following, MCP, `--play`, a notification's Watch) while two
    /// pictures show: the silent picture, which takes the sound. False in single view.
    pub(super) fn mv_outside(&mut self, media: MediaRef, zap: Option<crate::model::ZapList>, origin: PlayOrigin) -> bool {
        if !self.mv.view.on || media_kind(&media) == Kind::Vod {
            return false;
        }
        self.mv_place(PlaceIn { aim: Aim::Silent, media, zap, origin, take_sound: true, rearrange: false });
        true
    }

    /// Every frame, after the events: multi-view ends when a picture went away, single view
    /// on picture B ends when picture A starts something, relocking stops locked pictures, and
    /// the routers get the pictures.
    pub(super) fn mv_reconcile(&mut self) {
        let snaps = [self.mv_snap(A), self.mv_snap(B)];
        let generation = |t: TileId| snaps[slot(t)].as_ref().map_or(0, |s| s.load_generation);
        for t in [A, B] {
            let took = snaps[slot(t)].as_ref().is_some_and(|s| s.state != PlayerState::Idle);
            if self.mv.sent[slot(t)].is_some_and(|g| g != generation(t) || took) {
                self.mv.sent[slot(t)] = None;
            }
        }
        let busy = |t: TileId| self.mv.sent[slot(t)].is_some() || snaps[slot(t)].as_ref().is_some_and(|s| s.state != PlayerState::Idle);
        let (a_busy, b_busy) = (busy(A), self.mv.waiting.is_some() || busy(B));
        if self.mv.view.on {
            if !b_busy {
                self.mv_single(A);
            } else if !a_busy {
                self.mv_single(B);
            }
        } else if self.mv.view.focus == B {
            if !b_busy {
                self.mv_single(A);
            } else if a_busy && generation(A) != self.mv.a_gen {
                // picture A started something (a film, Up next, MPRIS): it takes over
                self.send_to(B, PlayerCommand::Stop);
                self.mv_single(A);
            }
        }
        // the sound went to a picture that then failed while the other plays: it comes back
        // (a play that was just sent still counts as busy, `sent`)
        let audible = self.mv.view.audible;
        let state = |t: TileId| snaps[slot(t)].as_ref().map(|s| &s.state);
        if self.mv.view.on
            && self.mv.sent[slot(audible)].is_none()
            && matches!(state(audible), Some(PlayerState::Error(_)))
            && state(audible.other()) == Some(&PlayerState::Playing)
        {
            self.mv_sound(audible.other());
        }
        // relocking stops each picture that shows a locked channel (explicit content goes in
        // `hide_explicit_now`)
        let unlocked = self.parental.is_unlocked(&self.services().config().parental);
        if self.mv.view.on && self.mv.unlocked && !unlocked {
            for t in [A, B] {
                let locked = snaps[slot(t)]
                    .as_ref()
                    .filter(|s| s.state != PlayerState::Idle)
                    .and_then(|s| crate::ui::screens::live::playing_channel(s))
                    .and_then(|c| self.lib.live.get(c))
                    .is_some_and(|c| c.locked);
                if locked {
                    self.send_to(t, PlayerCommand::Stop);
                    self.toast(Toast::info(format!("Picture {t} stopped — parental control is on")));
                }
            }
        }
        self.mv.unlocked = unlocked;
        self.services().agent.publish_tiles(self.tiles_state());
    }

    /// Stop each picture that shows explicit content (relock); true when one did.
    pub(super) fn mv_stop_explicit(&self, shows: impl Fn(&PlayerSnapshot) -> bool) -> bool {
        let mut stopped = false;
        for t in [A, B] {
            if t == self.mv.view.focus || !self.mv.view.on {
                continue;
            }
            if self.mv_snap(t).is_some_and(|s| s.state != PlayerState::Idle && shows(&s)) {
                self.send_to(t, PlayerCommand::Stop);
                stopped = true;
            }
        }
        stopped
    }

    /// B's surface, once its player was adopted (GL current).
    pub(super) fn mv_make_surface(&mut self) {
        if !std::mem::take(&mut self.mv.make_surface) {
            return;
        }
        let Some(core) = self.services().tiles.b().and_then(|p| p.render_core()) else { return };
        if let Err(e) = self.video.make_surface(B, core) {
            tracing::warn!("multi-view: picture B's video surface: {e}");
        }
    }

    /// A channel as the planner sees it.
    fn copy_of(&self, ch: &crate::model::ChannelSummary) -> ChannelCopy {
        mv::as_copy(&self.lib, ch)
    }

    /// `id` and its copies in other playlists (a few indexed reads).
    fn copies_of(&self, id: ChannelId) -> Vec<ChannelCopy> {
        let Some(ch) = self.lib.live.get(id) else { return Vec::new() };
        mv::copies_with_pick(&self.lib, ch, |ids| crate::store::copies::tvg_ids(&self.db, ids).ok()).unwrap_or_else(|| vec![self.copy_of(ch)])
    }

    /// What the picture that keeps playing holds, and where it could move (a channel only).
    fn keeper_copies(&self, tile: TileId, s: &PlayerSnapshot) -> Option<(TileId, ChannelId, Vec<ChannelCopy>)> {
        let held = |src: SourceId| {
            (tile, ChannelId(i64::MIN), vec![ChannelCopy { id: ChannelId(i64::MIN), source: src, quality: None, locked: false, explicit: false }])
        };
        match s.playable.as_ref().map(|p| &p.media) {
            Some(MediaRef::Channel(c)) => {
                let copies = self.copies_of(*c);
                if copies.is_empty() { s.claim.map(held) } else { Some((tile, *c, copies)) }
            }
            Some(MediaRef::Catchup { channel, .. }) => match self.lib.live.get(*channel) {
                Some(ch) => Some((tile, *channel, vec![self.copy_of(ch)])),
                None => s.claim.map(held),
            },
            _ => s.claim.map(held),
        }
    }

    /// Which of `ids` failed twice or more in the last 24 h (`skip_on_zap`).
    fn trouble(&self, ids: &[ChannelId]) -> HashSet<ChannelId> {
        let Ok(failures) = crate::store::live::failures(&self.db, crate::model::now() - 86_400) else { return HashSet::new() };
        ids.iter()
            .copied()
            .filter(|id| {
                self.lib.live.get(*id).is_some_and(|c| failures.get(&crate::ui::screens::live::channel_ref(c)).is_some_and(|f| f.count >= 2))
            })
            .collect()
    }
}
