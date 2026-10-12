//! Overlay layer (modal stack above screens). FROZEN (skeleton): this file, including the
//! fully wired [`build`] factory. Owners of the overlay files: help/jobs/dialogs = WP-10a,
//! search/resume = WP-12, programme/tracks/edit_item = WP-11.
//!
//! Visuals: scrim = background @ 0.5 (a click or right-click on it cancels, except while the
//! overlay holds an unsaved edit: [`Overlay::is_dirty`]); card per Omarchy menu/popup
//! (2 px border; popups use the accent border); opens centred, grows downward only once
//! typing starts (launcher idiom).

pub mod categories;
pub mod delete_playlist;
pub mod dialogs;
pub mod edit_item;
pub mod follow_sheet;
pub mod help;
pub mod jobs;
pub mod playlists;
pub mod programme;
pub mod reminders;
pub mod resume;
pub mod same_title;
pub mod search;
pub mod spoiler_menu;
pub mod spoiler_rule;
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
    Item {
        item: ItemRef,
    },
    /// Every copy of a combined title (DESIGN §29.15): copies with a name of their own keep it
    /// unless the user confirms; the toast offers Undo.
    Copies {
        items: Vec<ItemRef>,
    },
    Group {
        source: SourceId,
        kind: ItemKind,
        key: GroupKey,
    },
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
    /// A combined title (DESIGN §29.15): `media` is the copy that plays, this its fallback and
    /// the position to start at when the latest progress is another copy's.
    pub copy: Option<Box<crate::player::CopyPlay>>,
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
    /// Holds an edit that a stray click outside the card must not throw away: the scrim then
    /// ignores the click (and flashes the card) instead of closing.
    fn is_dirty(&self) -> bool {
        false
    }
    /// Opened by a right-click: show the card at the pointer (menus) instead of centred.
    fn anchor_at(&mut self, _pos: egui::Pos2) {}
    /// Result of a menu THIS overlay opened (e.g. search's item menu); the outcome applies to
    /// this overlay.
    fn on_overlay_result(&mut self, _ctx: &mut UiCtx<'_>, _result: OverlayResult) -> OverlayOutcome {
        OverlayOutcome::Keep
    }
}

/// Build an overlay for a request (every variant wired; frozen).
pub fn build(req: OverlayRequest, ctx: &mut UiCtx<'_>) -> Box<dyn Overlay> {
    match req {
        OverlayRequest::Search { initial } => Box::new(search::SearchOverlay::new(initial)),
        OverlayRequest::Help => Box::new(help::HelpOverlay::new()),
        OverlayRequest::Jobs => Box::new(jobs::JobsOverlay::default()),
        OverlayRequest::ContextMenu(spec) => Box::new(dialogs::ContextMenuOverlay::new(spec)),
        OverlayRequest::Confirm(spec) => Box::new(dialogs::ConfirmOverlay::new(spec)),
        OverlayRequest::Pin { then, reason } => Box::new(dialogs::PinOverlay::new(*then, reason)),
        OverlayRequest::Rename(spec) => Box::new(dialogs::RenameOverlay::new(spec)),
        OverlayRequest::ListPicker(spec) => Box::new(dialogs::ListPickerOverlay::new(spec)),
        OverlayRequest::Programme { programme, channel, zap } => Box::new(programme::ProgrammeOverlay::new(programme, channel, zap)),
        OverlayRequest::Resume(spec) => Box::new(resume::ResumeOverlay::new(spec)),
        OverlayRequest::Tracks { audio } => Box::new(tracks::TracksOverlay::new(audio)),
        OverlayRequest::EditItem(item) => Box::new(edit_item::EditItemOverlay::new(item)),
        OverlayRequest::Categories(source) => Box::new(categories::CategoriesOverlay::new(source)),
        OverlayRequest::PlaylistPicker(kind) => Box::new(playlists::PlaylistPickerOverlay::new(kind)),
        OverlayRequest::PlaylistMenu(source, kind) => Box::new(playlists::PlaylistMenuOverlay::new(source, kind, ctx)),
        OverlayRequest::SameTitle(spec) => Box::new(same_title::SameTitleOverlay::new(spec)),
        OverlayRequest::DeletePlaylist { source, token } => Box::new(delete_playlist::DeletePlaylistOverlay::new(source, token)),
        // the batch features (base-B; each overlay is its track's)
        OverlayRequest::FollowSheet(spec) => Box::new(follow_sheet::FollowSheet::new(spec)),
        OverlayRequest::SpoilerMenu => Box::<spoiler_menu::SpoilerMenu>::default(),
        OverlayRequest::SpoilerRule(spec) => Box::new(spoiler_rule::SpoilerRuleEditor::new(spec)),
        OverlayRequest::QueueSeries { series, play_next } => Box::new(crate::queue::ui::SeriesPicker::new(series, play_next)),
        OverlayRequest::SleepTimer => Box::<reminders::SleepTimerMenu>::default(),
        OverlayRequest::ReminderSheet(spec) => Box::new(reminders::ReminderSheet::new(spec)),
        OverlayRequest::WatchAlongside => Box::new(crate::ui::multiview::AlongsidePicker),
    }
}
