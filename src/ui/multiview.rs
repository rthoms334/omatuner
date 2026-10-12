//! Multi-view in the UI (DESIGN §35): the state (layout, focus, sound), the pure geometry and
//! the pure copy planner. Batch base: the types and signatures; MV-2 implements them (and the
//! chrome, drag, info strips). OWNER: MV-2.
//!
//! Two pictures at most, live TV or catch-up; with both accounts at one connection each, the
//! two must come from different playlists. Sound stays where it is until `M`; moving a playing
//! picture to its other playlist always asks (Enter = Move).

use eframe::egui::{self, Rect};

use crate::config::{MultiViewLayout, PopoutCorner};
use crate::model::{ChannelId, SourceId, TileId};

/// Multi-view's state (UI-owned; only `layout` / `pip_corner` persist, in the config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiView {
    pub on: bool,
    pub layout: Layout,
    /// Which picture sits in which place (`W` swaps places, not pictures).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layout {
    SideBySide,
    BigSmall,
    Pip,
}

impl Layout {
    /// The layout for `cfg` at this content width (pt × rem): `auto` = side by side ≥ 1200,
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
}

/// One placed picture: its video rect and (side by side) its info strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    pub tile: TileId,
    pub video: Rect,
    pub strip: Option<Rect>,
}

/// Where the pictures go inside `area` (pure). Placeholder until MV-2: the first slot fills the
/// area.
pub fn geometry(_layout: Layout, area: Rect, slots: [TileId; 2], _corner: PopoutCorner, _rem: f32) -> Vec<Placed> {
    vec![Placed { tile: slots[0], video: area, strip: None }]
}

/// One picture the planner should place: the user's pick and its copies (`store::copies::channel_copies`).
#[derive(Debug, Clone, PartialEq)]
pub struct Wanted {
    pub tile: TileId,
    pub pick: ChannelId,
    pub candidates: Vec<crate::store::copies::ChannelCopy>,
    /// Target picture height in physical px (quality fit).
    pub target_px: u32,
}

/// The planner's input.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaceRequest {
    pub wanted: Vec<Wanted>,
    /// The picture that keeps playing: (tile, its current copy, its copies).
    pub keeps: Option<(TileId, ChannelId, Vec<crate::store::copies::ChannelCopy>)>,
    pub locked_session: bool,
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

/// The playlists' limits and current use (`SharedBudget::usage`).
pub type LimitsView = [crate::player::budget::StreamUse];

/// Choose copies for every wanted picture (pure; ≤ 8 × 8 combinations). Placeholder until
/// MV-2: each wanted picture plays its pick when its playlist admits it.
pub fn plan(req: &PlaceRequest, limits: &LimitsView) -> Result<PlacementPlan, Unplaceable> {
    let used = |s: SourceId| limits.iter().find(|u| u.source == s).is_some_and(|u| u.limit.is_some_and(|l| u.in_use >= l));
    let mut tiles = Vec::new();
    for w in &req.wanted {
        let Some(c) = w.candidates.iter().find(|c| c.id == w.pick) else {
            return Err(Unplaceable { reason: "That channel isn't available".into() });
        };
        if used(c.source) {
            return Err(Unplaceable { reason: "Its playlist allows one stream at a time".into() });
        }
        tiles.push((w.tile, w.pick, false));
    }
    Ok(PlacementPlan { tiles, moves: None })
}

/// MV-2 flips this in its last commit (Ctrl+M opens multi-view, MCP lists `multiview`).
pub const ENABLED: bool = false;

// ------------------------------------------------------------------ base-B hooks (MV-2's)

/// Its entries in an item menu ("Watch alongside", Ctrl+M; `ui::item_menu::extend`); none
/// while multi-view is off.
pub fn menu_items(
    _target: &crate::ui::item_menu::Target,
    _ctx: &crate::ui::UiCtx<'_>,
    _keys: crate::keymap::KeyContext,
) -> Vec<crate::ui::overlays::MenuItem> {
    Vec::new()
}

/// Ctrl+M on an item (`ui::item_menu::dispatch`); false while multi-view is off.
pub fn on_action(_ctx: &mut crate::ui::UiCtx<'_>, _action: crate::keymap::Action, _target: &crate::ui::item_menu::Target) -> bool {
    false
}

/// "Watch alongside…" (`OverlayRequest::WatchAlongside`): choose the channel for the other
/// picture.
pub struct AlongsidePicker;

impl crate::ui::overlays::Overlay for AlongsidePicker {
    fn key_contexts(&self) -> Vec<crate::keymap::KeyContext> {
        vec![crate::keymap::KeyContext::Modal]
    }
    fn on_action(&mut self, _ctx: &mut crate::ui::UiCtx<'_>, _action: crate::keymap::Action) -> crate::ui::overlays::OverlayOutcome {
        crate::ui::overlays::OverlayOutcome::Close
    }
    fn ui(&mut self, _egui_ctx: &egui::Context, _ctx: &mut crate::ui::UiCtx<'_>) -> crate::ui::overlays::OverlayOutcome {
        crate::ui::overlays::OverlayOutcome::Close
    }
}
