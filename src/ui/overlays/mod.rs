//! Overlay layer (modal stack above screens). FROZEN (skeleton): this file, including the
//! fully wired [`build`] factory. Owners of the overlay files: help/jobs/dialogs = WP-10a,
//! search/resume = WP-12, programme/tracks/edit_item = WP-11. Until an owner lands, its overlay
//! uses [`placeholder_action`] / [`placeholder_ui`] (Esc closes) — never `todo!()`.
//!
//! Visuals: scrim = background @ 0.5 (clicking it cancels); card per Omarchy menu/popup
//! (2 px border; popups use the accent border); opens centred, grows downward only once
//! typing starts (launcher idiom).

pub mod dialogs;
pub mod edit_item;
pub mod help;
pub mod jobs;
pub mod programme;
pub mod resume;
pub mod search;
pub mod tracks;

use eframe::egui;

use super::{OverlayRequest, UiCtx};
use crate::keymap::{Action, KeyContext};
use crate::model::{FavListId, GroupKey, ItemKind, ItemRef, MediaRef, SourceId};

#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    /// Nerd Font glyph (see `widgets::glyphs`).
    pub icon: &'static str,
    pub label: String,
    /// Dispatched to the screen that opened the menu (via `OverlayResult::Chosen`).
    pub action: Action,
    pub enabled: bool,
    /// Shortcut hint shown right-aligned (from the keymap, e.g. "Ctrl+F").
    pub shortcut: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextMenuSpec {
    pub title: String,
    pub items: Vec<MenuItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmSpec {
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    /// Destructive actions render the confirm button in `urgent` and default focus to Cancel.
    pub destructive: bool,
    /// Echoed back in `OverlayResult::Confirmed` / `Cancelled`.
    pub token: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenameTarget {
    /// The store computes the fingerprint inside the write.
    Item { item: ItemRef },
    Group { source: SourceId, kind: ItemKind, key: GroupKey },
    FavList(FavListId),
    Source(SourceId),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenameSpec {
    pub title: String,
    pub initial: String,
    /// The overlay performs the write itself (store writer) and emits the DataScope.
    pub target: RenameTarget,
}

#[derive(Debug, Clone, PartialEq)]
/// Checklist of favourites lists + a "New list…" row.
pub struct ListPickerSpec {
    pub item: ItemRef,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResumeSpec {
    pub media: MediaRef,
    pub title: String,
    pub position_s: f64,
    pub duration_s: Option<f64>,
}

/// What an overlay reports back to the screen that opened it.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayResult {
    Chosen(Action),
    Confirmed(u64),
    Cancelled(u64),
    /// PIN accepted: retry the action that required it.
    Unlocked(Action),
}

pub enum OverlayOutcome {
    /// Handled; stay open.
    Keep,
    /// Not handled: the app falls back to global actions (Ctrl+Q, Alt+digits, F1… keep working
    /// while an overlay is open).
    Ignored,
    Close,
    /// Close and deliver a result to the screen below.
    CloseWith(OverlayResult),
}

pub trait Overlay {
    fn key_contexts(&self) -> Vec<KeyContext>;
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome;
    /// Draw (scrim + card) using `egui::Area`s on `egui_ctx`.
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome;
}

/// Build an overlay for a request (every variant wired; frozen).
pub fn build(req: OverlayRequest, _ctx: &mut UiCtx<'_>) -> Box<dyn Overlay> {
    match req {
        OverlayRequest::Search { initial } => Box::new(search::SearchOverlay::new(initial)),
        OverlayRequest::Help => Box::new(help::HelpOverlay::new()),
        OverlayRequest::Jobs => Box::new(jobs::JobsOverlay::default()),
        OverlayRequest::ContextMenu(spec) => Box::new(dialogs::ContextMenuOverlay::new(spec)),
        OverlayRequest::Confirm(spec) => Box::new(dialogs::ConfirmOverlay::new(spec)),
        OverlayRequest::Pin { then, reason } => Box::new(dialogs::PinOverlay::new(*then, reason)),
        OverlayRequest::Rename(spec) => Box::new(dialogs::RenameOverlay::new(spec)),
        OverlayRequest::ListPicker(spec) => Box::new(dialogs::ListPickerOverlay::new(spec)),
        OverlayRequest::Programme { programme, channel } => Box::new(programme::ProgrammeOverlay::new(programme, channel)),
        OverlayRequest::Resume(spec) => Box::new(resume::ResumeOverlay::new(spec)),
        OverlayRequest::Tracks { audio } => Box::new(tracks::TracksOverlay::new(audio)),
        OverlayRequest::EditItem(item) => Box::new(edit_item::EditItemOverlay::new(item)),
    }
}

/// Default key handling of a not-yet-implemented overlay: Esc/Enter close, everything else is
/// `Ignored` (so globals keep working).
pub fn placeholder_action(action: Action) -> OverlayOutcome {
    if matches!(action, Action::Back | Action::Activate) { OverlayOutcome::Close } else { OverlayOutcome::Ignored }
}

/// Default drawing of a not-yet-implemented overlay: scrim + card "Not implemented yet".
pub fn placeholder_ui(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>, title: &str, owner: &str) -> OverlayOutcome {
    let t = &ctx.theme.tokens;
    let screen = egui_ctx.content_rect();
    let mut close = false;
    egui::Area::new(egui::Id::new("placeholder-scrim")).order(egui::Order::Foreground).fixed_pos(screen.min).show(egui_ctx, |ui| {
        let r = ui.allocate_rect(screen, egui::Sense::click());
        ui.painter().rect_filled(screen, 0.0, t.scrim);
        close = r.clicked();
    });
    egui::Area::new(egui::Id::new("placeholder-overlay"))
        .order(egui::Order::Tooltip)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(egui_ctx, |ui| {
            egui::Frame::NONE.fill(t.card_bg).stroke(egui::Stroke::new(t.card_border_width, t.popup_border)).inner_margin(18.0).show(ui, |ui| {
                ui.label(egui::RichText::new(title).color(t.fg));
                ui.label(egui::RichText::new(format!("Not implemented yet ({owner}) — Esc to close")).color(t.text_secondary));
            });
        });
    if close { OverlayOutcome::Close } else { OverlayOutcome::Keep }
}
