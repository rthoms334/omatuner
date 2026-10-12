//! Multi-view in the UI (DESIGN §35): the state (layout, focus, sound), the pure geometry, the
//! pure copy planner, the requests screens make, the "Watch alongside" entry points and the
//! channel picker for the other picture. OWNER: MV-2.
//!
//! Two pictures at most, live TV or catch-up; with both accounts at one connection each, the
//! two must come from different playlists. Sound stays where it is until `M`; moving a playing
//! picture to its other playlist always asks (Enter = Move).
//!
//! * **Requests.** Screens never touch the second player: they send [`MvRequest`]s through
//!   [`UiCtx::mv_request`](crate::ui::UiCtx::mv_request) (and `UiCtx::play` turns a channel
//!   pick into one while multi-view is on), and the app applies them after the frame
//!   (`app/multiview.rs`): it starts picture B, plans the copies, asks before a move, and keeps
//!   focus and sound in step with `Services::tiles` and `AppShared::tiles`.
//! * **Geometry** ([`geometry`]): side by side (two 16:9 tiles with an info strip under each),
//!   big + small (2/3 and 1/3, the small one's strip under it), picture in picture (an inset of
//!   a quarter of the width in a corner). `auto` picks by width ([`Layout::for_width`]); the
//!   pop-out and the mini player always show picture in picture.
//! * **Planner** ([`plan`]): brute force over the pictures' copies (≤ 8 each), cheapest first —
//!   moving the playing picture +100 (only with consent), not the user's pick +10, a copy with
//!   recent failures +20, then quality fit (a small picture prefers the lowest quality at least
//!   its height, so a 300 pt inset takes an HD copy over UHD). Locked copies (while locked) and
//!   copies whose explicit flag differs from the pick are never chosen.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use eframe::egui::{self, Rect};

use crate::config::{MultiViewLayout, PopoutCorner};
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, MediaRef, PlayOrigin, Quality, SourceId, TileId, ZapList};
use crate::player::{PlayerCommand, PlayerSnapshot, PlayerState};
use crate::store::copies::ChannelCopy;
use crate::ui::UiCtx;
use crate::ui::item_menu::Target;
use crate::ui::overlays::{MenuItem, Overlay, OverlayOutcome};

/// Multi-view ships (Ctrl+M opens it, its keys and Settings rows show, MCP lists `multiview`).
pub const ENABLED: bool = true;

/// Glyphs of the multi-view chrome (Nerd Font, Material Design).
pub mod glyph {
    /// Sound from this picture.
    pub const SOUND: &str = crate::ui::widgets::glyphs::VOLUME;
    /// Swap places (mdi swap-horizontal).
    pub const SWAP: &str = "\u{f04e1}";
    /// Layout.
    pub const LAYOUT: &str = crate::ui::widgets::glyphs::MULTI_VIEW;
    /// Close this picture.
    pub const CLOSE: &str = crate::ui::widgets::glyphs::CLOSE;
}

/// Multi-view's state (UI-owned; only `layout` / `pip_corner` persist, in the config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiView {
    pub on: bool,
    /// The layout of the last frame the player drew (MCP `status`, the agent).
    pub layout: Layout,
    /// Which picture sits in which place (`W` swaps places, not pictures): `slots[0]` is the
    /// left one side by side and the big one otherwise.
    pub slots: [TileId; 2],
    pub focus: TileId,
    pub audible: TileId,
    pub pip_corner: PopoutCorner,
}

impl Default for MultiView {
    fn default() -> Self {
        Self {
            on: false,
            layout: Layout::SideBySide,
            slots: [TileId::A, TileId::B],
            focus: TileId::A,
            audible: TileId::A,
            pip_corner: PopoutCorner::BottomRight,
        }
    }
}

impl MultiView {
    /// The picture keys don't go to.
    pub fn unfocused(&self) -> TileId {
        self.focus.other()
    }

    /// The picture without the sound (outside plays go there).
    pub fn silent(&self) -> TileId {
        self.audible.other()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layout {
    SideBySide,
    BigSmall,
    Pip,
}

impl Layout {
    pub const ALL: [Layout; 3] = [Layout::SideBySide, Layout::BigSmall, Layout::Pip];

    /// The layout for `cfg` at this content width in pt ÷ rem: `auto` = side by side ≥ 1200,
    /// big + small ≥ 700, else picture in picture.
    pub fn for_width(cfg: MultiViewLayout, width: f32) -> Self {
        match cfg {
            MultiViewLayout::SideBySide => Layout::SideBySide,
            MultiViewLayout::BigSmall => Layout::BigSmall,
            MultiViewLayout::Pip => Layout::Pip,
            MultiViewLayout::Auto if width >= 1200.0 => Layout::SideBySide,
            MultiViewLayout::Auto if width >= 700.0 => Layout::BigSmall,
            MultiViewLayout::Auto => Layout::Pip,
        }
    }

    /// The layout shown: picture in picture in the popped-out window, else [`for_width`].
    ///
    /// [`for_width`]: Layout::for_width
    pub fn effective(cfg: MultiViewLayout, width_pt: f32, rem: f32, video_only: bool) -> Self {
        if video_only { Layout::Pip } else { Self::for_width(cfg, width_pt / rem.max(0.1)) }
    }

    /// `t`: side by side → big + small → picture in picture → side by side.
    pub fn next(self) -> Self {
        match self {
            Layout::SideBySide => Layout::BigSmall,
            Layout::BigSmall => Layout::Pip,
            Layout::Pip => Layout::SideBySide,
        }
    }

    /// As saved in `player.multiview.layout`.
    pub fn config(self) -> MultiViewLayout {
        match self {
            Layout::SideBySide => MultiViewLayout::SideBySide,
            Layout::BigSmall => MultiViewLayout::BigSmall,
            Layout::Pip => MultiViewLayout::Pip,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Layout::SideBySide => "Side by side",
            Layout::BigSmall => "Big + small",
            Layout::Pip => "Picture in picture",
        }
    }

    /// The MCP wire name (`multiview` `layout`).
    pub fn wire(self) -> &'static str {
        match self {
            Layout::SideBySide => "side_by_side",
            Layout::BigSmall => "big_small",
            Layout::Pip => "pip",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.wire() == s)
    }
}

/// One placed picture: its video rect and (side by side, the small picture) its info strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    pub tile: TileId,
    pub video: Rect,
    pub strip: Option<Rect>,
}

/// Height of a picture's info strip (three lines: channel, programme and time, next).
pub fn strip_height(rem: f32) -> f32 {
    (64.0 * rem).round()
}

/// The gap between pictures and around them.
fn gap(rem: f32) -> f32 {
    (12.0 * rem).round()
}

/// Where the pictures go inside `area` (pure), in drawing order (an inset after the picture it
/// sits on). `slots[0]` takes the first place: left side by side, big otherwise.
pub fn geometry(layout: Layout, area: Rect, slots: [TileId; 2], corner: PopoutCorner, rem: f32) -> Vec<Placed> {
    let g = gap(rem);
    let (pos, vec2) = (egui::pos2, egui::vec2);
    match layout {
        Layout::SideBySide => {
            let strip = strip_height(rem);
            let mut w = ((area.width() - 3.0 * g) / 2.0).max(0.0);
            let mut h = w * 9.0 / 16.0;
            let room = (area.height() - 2.0 * g - strip).max(0.0);
            if h > room {
                h = room;
                w = h * 16.0 / 9.0;
            }
            let x0 = area.center().x - (2.0 * w + g) / 2.0;
            let y0 = area.center().y - (h + strip) / 2.0;
            slots
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let video = Rect::from_min_size(pos(x0 + i as f32 * (w + g), y0), vec2(w, h));
                    Placed { tile: *t, video, strip: Some(Rect::from_min_size(video.left_bottom(), vec2(w, strip))) }
                })
                .collect()
        }
        Layout::BigSmall => {
            let strip = strip_height(rem);
            let avail = (area.width() - 3.0 * g).max(0.0);
            let (mut bw, mut sw) = (avail * 2.0 / 3.0, avail / 3.0);
            let mut bh = bw * 9.0 / 16.0;
            let room = (area.height() - 2.0 * g).max(0.0);
            if bh > room && bh > 0.0 {
                let k = room / bh;
                (bw, sw, bh) = (bw * k, sw * k, room);
            }
            let sh = sw * 9.0 / 16.0;
            let block_h = bh.max(sh + strip);
            let x0 = area.center().x - (bw + g + sw) / 2.0;
            let y0 = area.center().y - block_h / 2.0;
            let big = Rect::from_min_size(pos(x0, y0), vec2(bw, bh));
            let small = Rect::from_min_size(pos(big.right() + g, y0), vec2(sw, sh));
            vec![
                Placed { tile: slots[0], video: big, strip: None },
                Placed { tile: slots[1], video: small, strip: Some(Rect::from_min_size(small.left_bottom(), vec2(sw, strip))) },
            ]
        }
        Layout::Pip => {
            vec![Placed { tile: slots[0], video: area, strip: None }, Placed { tile: slots[1], video: inset_rect(area, corner, rem), strip: None }]
        }
    }
}

/// The picture-in-picture inset: a quarter of the width, 16:9, in `corner` with a 12 pt margin.
pub fn inset_rect(area: Rect, corner: PopoutCorner, rem: f32) -> Rect {
    let m = gap(rem).min(area.width() / 20.0);
    let w = (area.width() * 0.25).round();
    let size = egui::vec2(w, (w * 9.0 / 16.0).round());
    let (l, r, t, b) = (area.left() + m, area.right() - m - size.x, area.top() + m, area.bottom() - m - size.y);
    let min = match corner {
        PopoutCorner::TopLeft => egui::pos2(l, t),
        PopoutCorner::TopRight => egui::pos2(r, t),
        PopoutCorner::BottomLeft => egui::pos2(l, b),
        PopoutCorner::BottomRight => egui::pos2(r, b),
    };
    Rect::from_min_size(min, size)
}

/// The corner of `area` nearest to `p` (a dropped inset snaps there).
pub fn nearest_corner(area: Rect, p: egui::Pos2) -> PopoutCorner {
    match (p.x < area.center().x, p.y < area.center().y) {
        (true, true) => PopoutCorner::TopLeft,
        (false, true) => PopoutCorner::TopRight,
        (true, false) => PopoutCorner::BottomLeft,
        (false, false) => PopoutCorner::BottomRight,
    }
}

/// A direction for Ctrl+arrows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// The picture in direction `dir` of `from` (pure): the nearest whose centre lies that way.
pub fn neighbour(placed: &[Placed], from: TileId, dir: Dir) -> Option<TileId> {
    let c = placed.iter().find(|p| p.tile == from)?.video.center();
    placed
        .iter()
        .filter(|p| p.tile != from)
        .filter_map(|p| {
            let d = p.video.center() - c;
            let ahead = match dir {
                Dir::Left => -d.x,
                Dir::Right => d.x,
                Dir::Up => -d.y,
                Dir::Down => d.y,
            };
            (ahead > 0.5).then_some((p.tile, d.length()))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(t, _)| t)
}

// ------------------------------------------------------------------ the copy planner

/// One picture the planner should place: the user's pick and its copies
/// (`store::copies::channel_copies`); `candidates` includes the pick itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Wanted {
    pub tile: TileId,
    pub pick: ChannelId,
    pub candidates: Vec<ChannelCopy>,
    /// Target picture height in physical px (quality fit); 0 = unknown (best quality).
    pub target_px: u32,
}

/// The planner's input.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlaceRequest {
    pub wanted: Vec<Wanted>,
    /// The picture that keeps playing: (tile, what it plays, that channel and its copies — the
    /// first entry with that id is the playlist it holds now).
    pub keeps: Option<(TileId, ChannelId, Vec<ChannelCopy>)>,
    pub locked_session: bool,
    /// Channels with ≥ 2 failures in 24 h (`skip_on_zap`): a copy among them costs +20.
    pub trouble: HashSet<ChannelId>,
    /// Playlist names, for the reasons (never addresses).
    pub names: HashMap<SourceId, String>,
}

/// The planner's plan: per picture the channel to play (`from_copy` = not the user's pick) and,
/// when the playing picture must move playlists, which one (only with the user's consent).
#[derive(Debug, Clone, PartialEq)]
pub struct PlacementPlan {
    pub tiles: Vec<(TileId, ChannelId, bool)>,
    pub moves: Option<TileId>,
}

/// Why it can't be placed (plain English, names playlists only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unplaceable {
    pub reason: String,
}

/// The playlists' limits (`SharedBudget::usage`). Only `limit` is read: who holds what comes
/// from the request (the picture that keeps playing), so retuning a picture never counts its
/// own stream against itself.
pub type LimitsView = [crate::player::budget::StreamUse];

/// Nominal picture height of a quality class.
fn height(q: Quality) -> u32 {
    match q {
        Quality::Sd => 576,
        Quality::Hd => 720,
        Quality::Fhd => 1080,
        Quality::Uhd => 2160,
    }
}

fn class_index(q: Quality) -> u32 {
    match q {
        Quality::Sd => 0,
        Quality::Hd => 1,
        Quality::Fhd => 2,
        Quality::Uhd => 3,
    }
}

/// The quality-fit cost of a copy for a picture `target_px` tall (physical px; 0 = unknown).
/// A full-size picture prefers the best; a small one the lowest quality at least its height,
/// and a UHD stream in a small picture costs more than not being the pick (+10), since it
/// takes most of a core in software HEVC for pixels nobody sees.
pub fn quality_cost(q: Option<Quality>, target_px: u32) -> u32 {
    let Some(q) = q else { return 4 };
    if target_px == 0 || target_px >= 1080 {
        return 2 * (3 - class_index(q));
    }
    let fit = [Quality::Sd, Quality::Hd, Quality::Fhd, Quality::Uhd].into_iter().find(|c| height(*c) >= target_px).unwrap_or(Quality::Uhd);
    let (qi, fi) = (class_index(q), class_index(fit));
    if qi >= fi { 4 * (qi - fi) + if q == Quality::Uhd { 12 } else { 0 } } else { 6 * (fi - qi) }
}

/// At most this many options per picture (≤ 8 × 8 combinations).
const MAX_OPTIONS: usize = 8;
const COST_MOVE: u32 = 100;
const COST_NOT_PICK: u32 = 10;
const COST_TROUBLE: u32 = 20;

/// Choose copies for every wanted picture (pure; module docs for the costs). A plan that
/// moves the playing picture (`moves`) is returned only when nothing else fits: the caller
/// asks before sending it.
pub fn plan(req: &PlaceRequest, limits: &LimitsView) -> Result<PlacementPlan, Unplaceable> {
    let limit = |s: SourceId| limits.iter().find(|u| u.source == s).and_then(|u| u.limit);
    let trouble = |c: &ChannelCopy| if req.trouble.contains(&c.id) { COST_TROUBLE } else { 0 };
    let allowed = |c: &ChannelCopy, base: &ChannelCopy| c.explicit == base.explicit && (c.id == base.id || !(req.locked_session && c.locked));
    let mut options: Vec<Vec<(ChannelCopy, u32)>> = Vec::new();
    for w in &req.wanted {
        let Some(pick) = w.candidates.iter().find(|c| c.id == w.pick) else {
            return Err(Unplaceable { reason: "That channel isn't available".into() });
        };
        let o = w
            .candidates
            .iter()
            .filter(|c| allowed(c, pick))
            .take(MAX_OPTIONS)
            .map(|c| (*c, if c.id == pick.id { 0 } else { COST_NOT_PICK } + trouble(c) + quality_cost(c.quality, w.target_px)))
            .collect();
        options.push(o);
    }
    // the picture that keeps playing: where it is (free), else one of its copies (a move)
    let keeper: Option<(TileId, Vec<(ChannelCopy, u32)>)> = req.keeps.as_ref().and_then(|(t, cur, copies)| {
        let now = copies.iter().find(|c| c.id == *cur)?;
        let mut v = vec![(*now, 0)];
        v.extend(copies.iter().filter(|c| c.id != *cur && allowed(c, now)).take(MAX_OPTIONS - 1).map(|c| (*c, COST_MOVE + trouble(c))));
        Some((*t, v))
    });
    let keeper_opts: Vec<Option<(ChannelCopy, u32)>> = match &keeper {
        Some((_, v)) => v.iter().copied().map(Some).collect(),
        None => vec![None],
    };
    let mut best: Option<(u32, Vec<ChannelCopy>, Option<ChannelCopy>)> = None;
    let mut idx = vec![0usize; options.len()];
    'combos: loop {
        for k in &keeper_opts {
            let chosen: Vec<(ChannelCopy, u32)> = idx.iter().zip(&options).map(|(i, o)| o[*i]).collect();
            let mut used: HashMap<SourceId, u32> = HashMap::new();
            for (c, _) in chosen.iter().chain(k.iter()) {
                *used.entry(c.source).or_default() += 1;
            }
            let fits = used.iter().all(|(s, n)| limit(*s).is_none_or(|l| *n <= l.max(1)));
            let cost = chosen.iter().map(|(_, c)| c).sum::<u32>() + k.map_or(0, |(_, c)| c);
            if fits && best.as_ref().is_none_or(|(b, ..)| cost < *b) {
                best = Some((cost, chosen.iter().map(|(c, _)| *c).collect(), k.map(|(c, _)| c)));
            }
        }
        // next combination of the wanted pictures' options
        for (i, o) in idx.iter_mut().zip(&options) {
            *i += 1;
            if *i < o.len() {
                continue 'combos;
            }
            *i = 0;
        }
        break;
    }
    let Some((_, chosen, kept)) = best else { return Err(Unplaceable { reason: reason(req, &options, keeper.as_ref(), &limit) }) };
    let mut tiles: Vec<(TileId, ChannelId, bool)> = req.wanted.iter().zip(&chosen).map(|(w, c)| (w.tile, c.id, c.id != w.pick)).collect();
    let mut moves = None;
    if let (Some((t, opts)), Some(k)) = (&keeper, kept)
        && k.id != opts[0].0.id
    {
        tiles.push((*t, k.id, true));
        moves = Some(*t);
    }
    Ok(PlacementPlan { tiles, moves })
}

/// Why nothing fits, naming playlists only.
fn reason(
    req: &PlaceRequest,
    options: &[Vec<(ChannelCopy, u32)>],
    keeper: Option<&(TileId, Vec<(ChannelCopy, u32)>)>,
    limit: &dyn Fn(SourceId) -> Option<u32>,
) -> String {
    let name = |s: SourceId| req.names.get(&s).cloned().unwrap_or_else(|| "This playlist".into());
    let streams = |s: SourceId| match limit(s).unwrap_or(1) {
        1 => "one stream at a time".to_owned(),
        n => format!("{n} streams at a time"),
    };
    let wanted: BTreeSet<SourceId> = options.iter().flatten().map(|(c, _)| c.source).collect();
    if let Some((t, opts)) = keeper
        && let Some((held, _)) = opts.first()
    {
        if wanted.len() == 1 && wanted.contains(&held.source) && opts.len() == 1 {
            return format!("Both are only on {}, which allows {}.", name(held.source), streams(held.source));
        }
        return format!("{} is in use by picture {t}, and this channel has no copy on a free playlist.", name(held.source));
    }
    match wanted.iter().next() {
        Some(s) if wanted.len() == 1 => format!("Both are only on {}, which allows {}.", name(*s), streams(*s)),
        _ => "There aren't enough free streams on your playlists for both.".into(),
    }
}

// ------------------------------------------------------------------ requests

/// Which picture a play is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aim {
    /// The focused picture (Enter and play buttons while multi-view is on).
    Focused,
    /// Ctrl+M "watch alongside": the other picture (from single view it opens multi-view).
    Alongside,
    /// A play from outside (reminder, Following, MCP, IPC, a notification's Watch): the silent
    /// picture, which takes the sound.
    Silent,
    Tile(TileId),
}

/// A live channel or catch-up for a picture.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaceIn {
    pub aim: Aim,
    pub media: MediaRef,
    /// The list ↑/↓ zap through (the opener's collection).
    pub zap: Option<ZapList>,
    pub origin: PlayOrigin,
    /// Move the sound there once it plays (outside plays).
    pub take_sound: bool,
    /// Moving the playing picture to another playlist was agreed already (MCP `rearrange`).
    pub rearrange: bool,
}

impl PlaceIn {
    /// A user's play of `media` in the picture `aim` names.
    pub fn new(aim: Aim, media: MediaRef, zap: Option<ZapList>) -> Self {
        Self { aim, media, zap, origin: PlayOrigin::MultiView, take_sound: false, rearrange: false }
    }
}

/// What screens (and the agent) ask multi-view to do; the app applies it after the frame.
#[derive(Debug, Clone, PartialEq)]
pub enum MvRequest {
    Place(PlaceIn),
    Focus(TileId),
    /// `w`.
    FocusNext,
    /// `M`: the sound from this picture (`None` = the focused one).
    Sound(Option<TileId>),
    /// `W`.
    Swap,
    /// `t`.
    CycleLayout,
    SetLayout(Layout),
    /// The inset was dropped in a corner.
    SetCorner(PopoutCorner),
    /// `q`: close a picture (`None` = the focused one); the other carries on.
    Close(Option<TileId>),
    /// Ctrl+Shift+P: pause or play both.
    PauseAll,
    /// Ctrl+Shift+S, the mini player's ✕: stop both.
    StopAll,
    /// A film, an episode or a file while multi-view is on: it ends, and this plays in picture A.
    SingleView(Box<PlayerCommand>),
}

/// Multi-view as a frame's screens see it (set by the app on `UiCtx::multiview`; off in tests).
#[derive(Debug, Clone, Default)]
pub struct MvFrame {
    pub view: MultiView,
    /// Each picture's snapshot (A first; B once it exists).
    pub snaps: [Option<Arc<PlayerSnapshot>>; 2],
    /// Picture B is starting (its player or surface isn't there yet).
    pub starting: bool,
}

impl MvFrame {
    pub fn on(&self) -> bool {
        self.view.on
    }

    /// `tile`'s snapshot, if its player exists.
    pub fn snap(&self, tile: TileId) -> Option<&Arc<PlayerSnapshot>> {
        self.snaps[usize::from(tile.0).min(1)].as_ref()
    }

    /// The picture showing `ch` (Live's A / B marks), focused first.
    pub fn tile_playing(&self, ch: ChannelId) -> Option<TileId> {
        if !self.on() {
            return None;
        }
        [self.view.focus, self.view.unfocused()]
            .into_iter()
            .find(|t| self.snap(*t).is_some_and(|s| s.state != PlayerState::Idle && crate::ui::screens::live::playing_channel(s) == Some(ch)))
    }
}

/// Where `UiCtx::play` sends a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Routed {
    /// The focused picture (single view: the only one).
    Focused(PlayerCommand),
    /// The picture with the sound (one listening volume).
    Audible(PlayerCommand),
    /// Through the app (a channel pick to plan, a film that ends multi-view).
    Request(MvRequest),
}

/// The media a command plays, if it starts something.
fn played_media(cmd: &PlayerCommand) -> Option<&MediaRef> {
    match cmd {
        PlayerCommand::Play(r) => Some(&r.media),
        PlayerCommand::PlayCopy(c) => Some(&c.req.media),
        _ => None,
    }
}

/// Route one of a screen's player commands (pure): unchanged in single view on picture A;
/// with multi-view on, volume and mute go to the audible picture, a live or catch-up pick goes
/// through the planner for the focused picture, and a film / episode / file ends multi-view
/// (they play in picture A only, where resume and the next-episode card live).
pub fn route_command(mv: &MultiView, cmd: PlayerCommand) -> Routed {
    let live = |m: &MediaRef| matches!(m, MediaRef::Channel(_) | MediaRef::Catchup { .. });
    match played_media(&cmd) {
        Some(m) if mv.on && live(m) => {
            let (media, zap, origin) = match &cmd {
                PlayerCommand::Play(r) => (r.media.clone(), r.zap.clone(), r.origin),
                PlayerCommand::PlayCopy(c) => (c.req.media.clone(), c.req.zap.clone(), c.req.origin),
                _ => unreachable!("played_media"),
            };
            return Routed::Request(MvRequest::Place(PlaceIn { origin, ..PlaceIn::new(Aim::Focused, media, zap) }));
        }
        Some(m) if !live(m) && (mv.on || mv.focus != TileId::A) => return Routed::Request(MvRequest::SingleView(Box::new(cmd))),
        _ => {}
    }
    match cmd {
        PlayerCommand::SetVolume(_) | PlayerCommand::AdjustVolume(_) | PlayerCommand::ToggleMute | PlayerCommand::SetMute(_) if mv.on => {
            Routed::Audible(cmd)
        }
        cmd => Routed::Focused(cmd),
    }
}

/// Picture height in physical px a picture will get (quality fit): `area` in pt.
pub fn target_px(layout: Layout, area: Rect, slots: [TileId; 2], corner: PopoutCorner, rem: f32, ppp: f32, tile: TileId) -> u32 {
    geometry(layout, area, slots, corner, rem).iter().find(|p| p.tile == tile).map_or(0, |p| (p.video.height() * ppp).round() as u32)
}

// ------------------------------------------------------------------ copies and Live's preview

/// A channel as the planner sees it.
pub fn as_copy(lib: &crate::store::library::Library, ch: &crate::model::ChannelSummary) -> ChannelCopy {
    ChannelCopy {
        id: ch.id,
        source: ch.source_id,
        quality: ch.quality.map(|q| q.class),
        locked: ch.locked,
        explicit: lib.explicit.channels.contains(&ch.id),
    }
}

/// `ch` (first) and its copies in other playlists (`store::copies::channel_copies_with`;
/// `tvg` reads tvg-ids, `None` = no frame budget now).
pub fn copies_with_pick(
    lib: &crate::store::library::Library,
    ch: &crate::model::ChannelSummary,
    tvg: impl FnOnce(&[ChannelId]) -> Option<HashMap<ChannelId, String>>,
) -> Option<Vec<ChannelCopy>> {
    let copies = crate::store::copies::channel_copies_with(lib, ch, tvg)?;
    Some(std::iter::once(as_copy(lib, ch)).chain(copies).collect())
}

/// Live's note under the preview for the cursor channel while two pictures show: what Ctrl+M
/// would do with it in the other picture ("B would play it from TV 2 (TV is busy with picture
/// A)", "Needs picture A to move to TV 2", or why it can't). `None`: it plays as picked, or no
/// frame budget now (asked again next frame).
pub fn preview_note(ctx: &mut UiCtx<'_>, id: ChannelId) -> Option<String> {
    let view = ctx.multiview.view;
    if !view.on {
        return None;
    }
    let (target, keeper) = (view.unfocused(), view.focus);
    let lib = ctx.lib.clone();
    let ch = lib.live.get(id)?;
    let (db, budget) = (ctx.db, &mut *ctx.budget);
    let mut tvg = |ids: &[ChannelId]| budget.run(|| crate::store::copies::tvg_ids(db, ids)).and_then(Result::ok);
    let candidates = copies_with_pick(&lib, ch, &mut tvg)?;
    let keeper_snap = ctx.multiview.snap(keeper).cloned().filter(|s| s.state != PlayerState::Idle);
    let keeps = match keeper_snap.as_ref().and_then(|s| crate::ui::screens::live::playing_channel(s)).and_then(|c| lib.live.get(c)) {
        Some(k) => Some((keeper, k.id, copies_with_pick(&lib, k, &mut tvg)?)),
        None => keeper_snap.as_ref().and_then(|s| s.claim).map(|src| {
            (keeper, ChannelId(i64::MIN), vec![ChannelCopy { id: ChannelId(i64::MIN), source: src, quality: None, locked: false, explicit: false }])
        }),
    };
    let names: HashMap<SourceId, String> = lib.sources.iter().map(|s| (s.id, s.name.clone())).collect();
    let name = |s: SourceId| names.get(&s).cloned().unwrap_or_default();
    let held = keeps.as_ref().and_then(|(_, cur, v)| v.iter().find(|c| c.id == *cur)).map(|c| c.source);
    let req = PlaceRequest {
        wanted: vec![Wanted { tile: target, pick: id, candidates, target_px: 0 }],
        keeps,
        locked_session: !ctx.parental.is_unlocked(&ctx.config.parental),
        names: names.clone(),
        ..Default::default()
    };
    match plan(&req, &ctx.services.tiles.budget().usage()) {
        Err(u) => Some(u.reason),
        Ok(p) => {
            let source_of = |c: ChannelId| lib.live.get(c).map(|c| c.source_id);
            if let Some(m) = p.moves {
                let to = p.tiles.iter().find(|(t, ..)| *t == m).and_then(|(_, c, _)| source_of(*c)).map(name).unwrap_or_default();
                return Some(format!("Needs picture {m} to move to {to} (Ctrl+M asks first)"));
            }
            let (_, chosen, from_copy) = *p.tiles.first()?;
            from_copy.then(|| {
                let to = source_of(chosen).map(name).unwrap_or_default();
                match held {
                    Some(h) => format!("{target} would play it from {to} ({} is busy with picture {keeper})", name(h)),
                    None => format!("{target} would play it from {to}"),
                }
            })
        }
    }
}

/// Live's picture chips over the preview while two pictures show (`[A 󰕾 BBC One] [B ITV]`):
/// a click focuses that picture, so Enter targets it.
pub fn chips(ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
    use crate::theme::{SpaceToken, TypeToken};
    if !ctx.multiview.on() {
        return;
    }
    let theme = ctx.theme;
    let t = &theme.tokens;
    let view = ctx.multiview.view;
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(theme.space(SpaceToken::Sm), theme.space(SpaceToken::Xs));
        for tile in [TileId::A, TileId::B] {
            let snap = ctx.multiview.snap(tile).cloned();
            let title = snap.as_ref().and_then(|s| s.playable.as_ref().map(|p| p.title.clone())).unwrap_or_else(|| "Starting…".into());
            let sound = if tile == view.audible { format!(" {}", glyph::SOUND) } else { String::new() };
            let text = format!("{tile}{sound}  {title}");
            let focused = tile == view.focus;
            let rich = egui::RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(if focused { t.accent } else { t.fg });
            let b = egui::Button::new(rich).selected(focused).truncate();
            let resp = ui
                .add_sized(
                    egui::vec2((ui.available_width() / 2.0 - theme.space(SpaceToken::Sm)).max(60.0), theme.space(SpaceToken::ControlHeight)),
                    b,
                )
                .on_hover_text(format!("Picture {tile}: Enter plays here when it is chosen"))
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                clicked = Some(tile);
            }
        }
    });
    if let Some(t) = clicked {
        ctx.mv_request(MvRequest::Focus(t));
    }
    ui.add_space(theme.space(SpaceToken::Sm));
}

// ------------------------------------------------------------------ item menus (base-B hooks)

/// Something live or catch-up plays (or multi-view is on): "Watch alongside" makes sense.
fn alongside_possible(ctx: &UiCtx<'_>) -> bool {
    ctx.multiview.on()
        || (ctx.player.state != PlayerState::Idle
            && ctx
                .player
                .playable
                .as_ref()
                .is_some_and(|p| matches!(p.kind, crate::model::PlayableKind::Live | crate::model::PlayableKind::Catchup { .. })))
}

/// Its entries in an item menu ("Watch alongside", Ctrl+M; `ui::item_menu::extend`): on
/// channels and programmes while a live channel or catch-up plays.
pub fn menu_items(target: &Target, ctx: &UiCtx<'_>, keys: KeyContext) -> Vec<MenuItem> {
    if !ENABLED || !matches!(target, Target::Channel(_) | Target::Programme { .. }) || !alongside_possible(ctx) {
        return Vec::new();
    }
    let label = if ctx.multiview.on() { format!("Watch in picture {}", ctx.multiview.view.unfocused()) } else { "Watch alongside".to_owned() };
    vec![crate::ui::item_menu::entry(glyph::LAYOUT, label, Action::WatchAlongside, keys)]
}

/// Ctrl+M on an item (`ui::item_menu::dispatch`): the channel, the airing programme's channel
/// or a past programme's catch-up in the other picture. Films and episodes stay single view.
pub fn on_action(ctx: &mut UiCtx<'_>, action: Action, target: &Target) -> bool {
    if !ENABLED || action != Action::WatchAlongside {
        return false;
    }
    match *target {
        Target::Channel(id) => {
            let zap = crate::ui::screens::live::primary_zap(ctx, id);
            ctx.mv_request(MvRequest::Place(PlaceIn::new(Aim::Alongside, MediaRef::Channel(id), zap)));
        }
        Target::Programme { programme, channel } => {
            let Some(ch) = ctx.lib.live.get(channel).cloned() else { return true };
            let db = ctx.db;
            let Some(p) = ctx.budget.run(|| crate::store::epg::programme(db, programme)).and_then(Result::ok) else { return true };
            let shift = i64::from(ch.tvg_shift_min) * 60;
            let (start, stop) = (p.start + shift, p.stop + shift);
            if start > ctx.now {
                ctx.toast(crate::msg::Toast::info("It hasn't started yet — Remind me (r) can switch to it"));
            } else if stop > ctx.now {
                let zap = crate::ui::screens::live::primary_zap(ctx, channel);
                ctx.mv_request(MvRequest::Place(PlaceIn::new(Aim::Alongside, MediaRef::Channel(channel), zap)));
            } else if ch.has_catchup {
                let media = MediaRef::Catchup { channel, start, stop, catchup_id: p.catchup_id };
                ctx.mv_request(MvRequest::Place(PlaceIn::new(Aim::Alongside, media, None)));
            } else {
                ctx.toast(crate::msg::Toast::info("This programme isn't in the channel's archive"));
            }
        }
        Target::Movie(_) | Target::Series(_) | Target::Episode { .. } => {
            ctx.toast(crate::msg::Toast::info(crate::playback::route::MULTIVIEW_LIVE_ONLY))
        }
    }
    true
}

// ------------------------------------------------------------------ the channel picker

/// "Watch alongside…" (`OverlayRequest::WatchAlongside`, Ctrl+M in the player): choose the
/// channel for the other picture. The focused picture's zap list (else every channel); typing
/// filters by name or number; Enter / a click plays it there.
///
/// The overlay factory builds it as a unit value, so its list lives in [`PICKER`]: one picker
/// is open at a time, and it starts afresh when it wasn't drawn in the previous frame.
pub struct AlongsidePicker;

thread_local! {
    /// The open picker's list (and the pass it was last drawn in).
    static PICKER: std::cell::RefCell<(u64, PickerState)> = std::cell::RefCell::new((0, PickerState::default()));
}

impl AlongsidePicker {
    fn with<R>(f: impl FnOnce(&mut PickerState) -> R) -> R {
        PICKER.with_borrow_mut(|(_, s)| f(s))
    }

    /// Done with it: the next one starts afresh.
    fn reset(outcome: OverlayOutcome) -> OverlayOutcome {
        if matches!(outcome, OverlayOutcome::Close | OverlayOutcome::CloseWith(_)) {
            PICKER.with_borrow_mut(|p| *p = (0, PickerState::default()));
        }
        outcome
    }
}

impl Overlay for AlongsidePicker {
    fn key_contexts(&self) -> Vec<KeyContext> {
        Self::with(|s| s.key_contexts())
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        Self::reset(Self::with(|s| s.on_action(ctx, action)))
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let pass = egui_ctx.cumulative_pass_nr();
        PICKER.with_borrow_mut(|(last, s)| {
            if *last + 1 < pass {
                *s = PickerState::default();
            }
            *last = pass;
        });
        Self::reset(Self::with(|s| s.ui(egui_ctx, ctx)))
    }
}

/// The picker's list.
#[derive(Default)]
struct PickerState {
    list: crate::ui::widgets::list::ListState,
    ids: Vec<ChannelId>,
    names: Vec<String>,
    rows: Vec<usize>,
    zap: Option<ZapList>,
    started: bool,
}

impl PickerState {
    fn start(&mut self, ctx: &UiCtx<'_>) {
        if self.started {
            return;
        }
        self.started = true;
        let live = &ctx.lib.live;
        self.zap = ctx.player.zap.clone().filter(|z| !z.channels.is_empty());
        self.ids = match &self.zap {
            Some(z) => z.channels.iter().copied().filter(|id| live.get(*id).is_some_and(|c| !c.unsupported)).collect(),
            None => live.channels.iter().filter(|c| !c.unsupported && !live.hidden.contains(&c.id)).map(|c| c.id).collect(),
        };
        self.names = self
            .ids
            .iter()
            .map(|id| match live.get(*id) {
                Some(c) => c.number.map_or_else(|| c.name.to_string(), |n| format!("{n} {}", c.name)),
                None => String::new(),
            })
            .collect();
        self.refilter();
    }

    fn refilter(&mut self) {
        self.rows = self.list.filter.apply(self.names.iter().map(String::as_str)).into_iter().map(|i| i as usize).collect();
        self.list.set_len(self.rows.len());
    }

    fn choose(&self, ctx: &mut UiCtx<'_>, row: usize) -> OverlayOutcome {
        let Some(&id) = self.rows.get(row).and_then(|i| self.ids.get(*i)) else { return OverlayOutcome::Keep };
        let zap = self.zap.clone().or_else(|| crate::ui::screens::live::primary_zap(ctx, id));
        ctx.mv_request(MvRequest::Place(PlaceIn::new(Aim::Alongside, MediaRef::Channel(id), zap)));
        OverlayOutcome::Close
    }

    /// The picture the choice goes to.
    fn target(ctx: &UiCtx<'_>) -> TileId {
        if ctx.multiview.on() { ctx.multiview.view.unfocused() } else { ctx.multiview.view.focus.other() }
    }
}

impl PickerState {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![self.list.key_context(), KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        use crate::ui::widgets::list::ListOutcome;
        self.start(ctx);
        match self.list.handle(action) {
            ListOutcome::Moved | ListOutcome::ContextMenu(_) => OverlayOutcome::Keep,
            ListOutcome::FilterChanged => {
                self.refilter();
                OverlayOutcome::Keep
            }
            ListOutcome::Activated(r) | ListOutcome::ActivatedAlt(r) => self.choose(ctx, r),
            ListOutcome::Parent => OverlayOutcome::Close,
            ListOutcome::None => match action {
                Action::Back | Action::WatchAlongside => OverlayOutcome::Close,
                Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) => {
                    OverlayOutcome::Keep
                }
                Action::Activate | Action::ActivateAlt | Action::Right => OverlayOutcome::Keep,
                _ => OverlayOutcome::Ignored,
            },
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        use crate::theme::{SpaceToken, TypeToken};
        use crate::ui::widgets::list::{ListOutcome, virtual_list};
        self.start(ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        let screen = egui_ctx.content_rect();
        // scrim: a click outside closes
        let outside = egui::Area::new(egui::Id::new("alongside-scrim"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(egui_ctx, |ui| {
                let r = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter().rect_filled(screen, 0.0, t.scrim);
                r.clicked() || r.secondary_clicked()
            })
            .inner;
        if outside {
            return OverlayOutcome::Close;
        }
        let frame = crate::ui::widgets::card::card_frame(theme, crate::ui::widgets::card::CardKind::Menu);
        let gap = theme.space(SpaceToken::PanelPadding);
        let width = (420.0 * theme.rem()).min(screen.width() - 2.0 * gap).max(120.0);
        let height = (screen.height() * 0.7).min(520.0 * theme.rem()).max(160.0);
        let row_h = theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Sm);
        let tile = Self::target(ctx);
        let lib = ctx.lib.clone();
        let mut picked: Option<usize> = None;
        egui::Area::new(egui::Id::new("alongside-card")).order(egui::Order::Tooltip).anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO).show(
            egui_ctx,
            |ui| {
                frame.show(ui, |ui| {
                    ui.set_width(width - frame.total_margin().sum().x);
                    ui.horizontal(|ui| {
                        crate::ui::widgets::text_field::search_header(
                            ui,
                            theme,
                            self.list.filter.query(),
                            &format!("Watch in picture {tile} — type to filter"),
                            None,
                        );
                    });
                    ui.add_space(theme.space(SpaceToken::Sm));
                    if self.rows.is_empty() {
                        let text = if self.list.filter.is_active() {
                            format!("No matches for “{}”", self.list.filter.query())
                        } else {
                            "No channels".into()
                        };
                        ui.label(egui::RichText::new(text).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                        return;
                    }
                    ui.allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
                        let (rows, ids, names) = (&self.rows, &self.ids, &self.names);
                        let out = virtual_list(ui, theme, "alongside-list", &mut self.list, row_h, |ui, rc| {
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                            crate::ui::widgets::list::paint_row_state(ui.painter(), rect, theme, rc.state);
                            let i = rows[rc.index];
                            let name = names.get(i).map_or("", String::as_str);
                            let locked = ids.get(i).and_then(|id| lib.live.get(*id)).is_some_and(|c| c.locked);
                            let text = if locked { format!("{name}  {}", crate::ui::widgets::glyphs::LOCK) } else { name.to_owned() };
                            crate::ui::video_host::osd_text(
                                ui.painter(),
                                egui::pos2(rect.left() + theme.space(SpaceToken::Md), rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                &text,
                                egui::FontId::proportional(theme.font(TypeToken::Body)),
                                t.fg,
                                rect.width() - 2.0 * theme.space(SpaceToken::Md),
                            );
                            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name));
                            resp
                        });
                        // Enter or one click plays, like the player's side panels
                        if let ListOutcome::Activated(r) = out {
                            picked = Some(r);
                        }
                    });
                });
            },
        );
        match picked {
            Some(r) => self.choose(ctx, r),
            None => OverlayOutcome::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::budget::StreamUse;

    const A: TileId = TileId::A;
    const B: TileId = TileId::B;
    const TV: SourceId = SourceId(1);
    const TV2: SourceId = SourceId(2);

    fn area(w: f32, h: f32) -> Rect {
        Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))
    }

    /// Both pictures inside the area, apart, 16:9; side by side has a strip under each, big +
    /// small under the small one only; the inset sits in its corner inside the big picture.
    #[test]
    fn layouts_fit_at_half_and_full_screen() {
        for (w, h) in [(790.0, 600.0), (950.0, 1000.0), (1600.0, 900.0)] {
            let a = area(w, h);
            for layout in Layout::ALL {
                let p = geometry(layout, a, [A, B], PopoutCorner::BottomRight, 1.0);
                assert_eq!(p.len(), 2, "{layout:?}");
                for t in &p {
                    assert!(a.contains_rect(t.video), "{layout:?} at {w}: {:?}", t.video);
                    assert!((t.video.width() / t.video.height() - 16.0 / 9.0).abs() < 0.02 || layout == Layout::Pip && t.tile == A);
                    if let Some(s) = t.strip {
                        assert!(a.contains_rect(s) && s.top() >= t.video.bottom() - 0.5, "{layout:?} strip");
                    }
                }
                match layout {
                    Layout::SideBySide => {
                        assert!(p[0].video.right() < p[1].video.left() && p.iter().all(|t| t.strip.is_some()));
                        assert_eq!(p[0].video.size(), p[1].video.size());
                    }
                    Layout::BigSmall => {
                        assert!(p[0].video.right() < p[1].video.left());
                        assert!((p[0].video.width() / p[1].video.width() - 2.0).abs() < 0.01, "2/3 and 1/3");
                        assert!(p[0].strip.is_none() && p[1].strip.is_some());
                    }
                    Layout::Pip => {
                        assert_eq!(p[0].video, a);
                        assert!(p[0].video.contains_rect(p[1].video));
                        assert_eq!(p[1].video.width(), (w * 0.25).round());
                        assert!(p[1].video.center().x > a.center().x && p[1].video.center().y > a.center().y, "bottom right");
                    }
                }
            }
        }
        // full screen: two 16:9 tiles of about 780 pt
        let p = geometry(Layout::SideBySide, area(1600.0, 900.0), [A, B], PopoutCorner::BottomRight, 1.0);
        assert!(p[0].video.width() > 770.0, "{:?}", p[0].video);
        // W swaps places, not pictures
        let swapped = geometry(Layout::BigSmall, area(1600.0, 900.0), [B, A], PopoutCorner::BottomRight, 1.0);
        assert_eq!(swapped[0].tile, B);
        assert!(swapped[0].video.width() > swapped[1].video.width());
    }

    #[test]
    fn auto_layout_by_width_and_popped_out_is_pip() {
        assert_eq!(Layout::effective(MultiViewLayout::Auto, 1600.0, 1.0, false), Layout::SideBySide);
        assert_eq!(Layout::effective(MultiViewLayout::Auto, 950.0, 1.0, false), Layout::BigSmall);
        // at text size 16 (rem 1.33) a 1500 pt window is "medium"
        assert_eq!(Layout::effective(MultiViewLayout::Auto, 1500.0, 1.33, false), Layout::BigSmall);
        assert_eq!(Layout::effective(MultiViewLayout::Auto, 600.0, 1.0, false), Layout::Pip);
        assert_eq!(Layout::effective(MultiViewLayout::SideBySide, 1600.0, 1.0, true), Layout::Pip);
        assert_eq!(Layout::SideBySide.next().next().next(), Layout::SideBySide);
    }

    #[test]
    fn ctrl_arrows_find_the_picture_that_way_and_a_dropped_inset_snaps_to_its_corner() {
        let a = area(1600.0, 900.0);
        let p = geometry(Layout::SideBySide, a, [A, B], PopoutCorner::BottomRight, 1.0);
        assert_eq!(neighbour(&p, A, Dir::Right), Some(B));
        assert_eq!(neighbour(&p, A, Dir::Left), None);
        assert_eq!(neighbour(&p, B, Dir::Left), Some(A));
        let pip = geometry(Layout::Pip, a, [A, B], PopoutCorner::TopLeft, 1.0);
        assert_eq!(neighbour(&pip, A, Dir::Up), Some(B));
        assert_eq!(nearest_corner(a, egui::pos2(100.0, 800.0)), PopoutCorner::BottomLeft);
    }

    fn copy(id: i64, source: SourceId, q: Option<Quality>) -> ChannelCopy {
        ChannelCopy { id: ChannelId(id), source, quality: q, locked: false, explicit: false }
    }

    fn one_each() -> Vec<StreamUse> {
        vec![StreamUse { source: TV, limit: Some(1), in_use: 0 }, StreamUse { source: TV2, limit: Some(1), in_use: 0 }]
    }

    fn names() -> HashMap<SourceId, String> {
        HashMap::from([(TV, "TV".to_owned()), (TV2, "TV 2".to_owned())])
    }

    fn want(tile: TileId, pick: i64, candidates: Vec<ChannelCopy>, target_px: u32) -> Wanted {
        Wanted { tile, pick: ChannelId(pick), candidates, target_px }
    }

    /// The user's setup: "TV" and "TV 2", one stream each (decision table of §4.4).
    #[test]
    fn the_planner_on_two_one_stream_playlists() {
        let limits = one_each();
        // A plays 10 on TV; B wants 20, which is on TV 2: plays as picked
        let keeps = Some((A, ChannelId(10), vec![copy(10, TV, None)]));
        let req =
            PlaceRequest { wanted: vec![want(B, 20, vec![copy(20, TV2, None)], 0)], keeps: keeps.clone(), names: names(), ..Default::default() };
        assert_eq!(plan(&req, &limits), Ok(PlacementPlan { tiles: vec![(B, ChannelId(20), false)], moves: None }));
        // B wants 21 on TV (held by A), which has a copy 22 on TV 2: the copy, no question
        let req = PlaceRequest { wanted: vec![want(B, 21, vec![copy(21, TV, None), copy(22, TV2, None)], 0)], keeps: keeps.clone(), ..req };
        assert_eq!(plan(&req, &limits), Ok(PlacementPlan { tiles: vec![(B, ChannelId(22), true)], moves: None }));
        // only on TV, and A has a copy on TV 2: the move is offered, never chosen silently
        let keeps_movable = Some((A, ChannelId(10), vec![copy(10, TV, None), copy(11, TV2, None)]));
        let req = PlaceRequest { wanted: vec![want(B, 21, vec![copy(21, TV, None)], 0)], keeps: keeps_movable, ..req };
        assert_eq!(plan(&req, &limits), Ok(PlacementPlan { tiles: vec![(B, ChannelId(21), false), (A, ChannelId(11), true)], moves: Some(A) }));
        // both only on TV: unplaceable, naming the playlist
        let req = PlaceRequest { wanted: vec![want(B, 21, vec![copy(21, TV, None)], 0)], keeps, ..req };
        assert_eq!(plan(&req, &limits).unwrap_err().reason, "Both are only on TV, which allows one stream at a time.");
        // two at once (MCP open): each on its own playlist
        let req = PlaceRequest {
            wanted: vec![want(A, 1, vec![copy(1, TV, None), copy(2, TV2, None)], 0), want(B, 3, vec![copy(3, TV, None), copy(4, TV2, None)], 0)],
            names: names(),
            ..Default::default()
        };
        let p = plan(&req, &limits).unwrap();
        let sources: HashSet<i64> = p.tiles.iter().map(|(_, c, _)| c.0).collect();
        assert_eq!(p.tiles.len(), 2);
        assert!(sources == HashSet::from([1, 4]) || sources == HashSet::from([2, 3]), "{p:?}");
        // an unknown limit lets both share the playlist
        let free = [StreamUse { source: TV, limit: None, in_use: 1 }];
        let req = PlaceRequest {
            wanted: vec![want(B, 21, vec![copy(21, TV, None)], 0)],
            keeps: Some((A, ChannelId(10), vec![copy(10, TV, None)])),
            ..Default::default()
        };
        assert_eq!(plan(&req, &free).unwrap().tiles, vec![(B, ChannelId(21), false)]);
    }

    /// Never escalate: a locked copy while locked, a copy whose explicit flag differs; a copy
    /// with recent failures loses to a healthy one; the small picture fits its quality.
    #[test]
    fn the_planner_never_escalates_and_prefers_healthy_fitting_copies() {
        let limits = one_each();
        let keeps = Some((A, ChannelId(10), vec![copy(10, TV, None)]));
        let locked = ChannelCopy { locked: true, ..copy(22, TV2, None) };
        let req = PlaceRequest {
            wanted: vec![want(B, 21, vec![copy(21, TV, None), locked], 0)],
            keeps: keeps.clone(),
            locked_session: true,
            names: names(),
            ..Default::default()
        };
        assert!(plan(&req, &limits).is_err(), "a locked copy while locked");
        assert_eq!(plan(&PlaceRequest { locked_session: false, ..req.clone() }, &limits).unwrap().tiles, vec![(B, ChannelId(22), true)]);
        let explicit = ChannelCopy { explicit: true, ..copy(22, TV2, None) };
        let req = PlaceRequest { wanted: vec![want(B, 21, vec![copy(21, TV, None), explicit], 0)], locked_session: false, ..req };
        assert!(plan(&req, &limits).is_err(), "an explicit copy of a channel that isn't");
        // a copy with failures loses to the pick and to a healthy copy
        let both = [StreamUse { source: TV, limit: None, in_use: 0 }, StreamUse { source: TV2, limit: None, in_use: 0 }];
        let req = PlaceRequest {
            wanted: vec![want(B, 21, vec![copy(21, TV, Some(Quality::Hd)), copy(22, TV2, Some(Quality::Hd))], 0)],
            trouble: HashSet::from([ChannelId(21)]),
            ..Default::default()
        };
        assert_eq!(plan(&req, &both).unwrap().tiles, vec![(B, ChannelId(22), true)]);
        // a 300 px inset takes the HD copy over the UHD pick; full size keeps the pick
        let cands = vec![copy(30, TV, Some(Quality::Uhd)), copy(31, TV2, Some(Quality::Hd))];
        let small = PlaceRequest { wanted: vec![want(B, 30, cands.clone(), 300)], ..Default::default() };
        assert_eq!(plan(&small, &both).unwrap().tiles, vec![(B, ChannelId(31), true)]);
        let big = PlaceRequest { wanted: vec![want(B, 30, cands, 1440)], ..Default::default() };
        assert_eq!(plan(&big, &both).unwrap().tiles, vec![(B, ChannelId(30), false)]);
    }

    /// Single view on picture A is untouched; in multi-view volume goes to the audible picture,
    /// a channel pick to the planner and a film ends multi-view.
    #[test]
    fn screen_commands_route_by_picture() {
        let single = MultiView::default();
        let on = MultiView { on: true, focus: B, ..Default::default() };
        let play = |m: MediaRef| {
            PlayerCommand::Play(crate::player::PlayRequest { media: m, zap: None, origin: PlayOrigin::Guide, resume: false, immediate: true })
        };
        assert_eq!(route_command(&single, PlayerCommand::AdjustVolume(5.0)), Routed::Focused(PlayerCommand::AdjustVolume(5.0)));
        assert_eq!(route_command(&single, play(MediaRef::Channel(ChannelId(1)))), Routed::Focused(play(MediaRef::Channel(ChannelId(1)))));
        assert_eq!(route_command(&on, PlayerCommand::ToggleMute), Routed::Audible(PlayerCommand::ToggleMute));
        assert_eq!(route_command(&on, PlayerCommand::TogglePause), Routed::Focused(PlayerCommand::TogglePause));
        match route_command(&on, play(MediaRef::Channel(ChannelId(1)))) {
            Routed::Request(MvRequest::Place(p)) => assert_eq!((p.aim, p.origin), (Aim::Focused, PlayOrigin::Guide)),
            r => panic!("{r:?}"),
        }
        let film = play(MediaRef::Movie(crate::model::MovieId(3)));
        assert_eq!(route_command(&on, film.clone()), Routed::Request(MvRequest::SingleView(Box::new(film.clone()))));
        // single view left on picture B (A was closed): a film still plays in picture A
        let b_alone = MultiView { focus: B, audible: B, ..Default::default() };
        assert_eq!(route_command(&b_alone, film.clone()), Routed::Request(MvRequest::SingleView(Box::new(film))));
    }
}
