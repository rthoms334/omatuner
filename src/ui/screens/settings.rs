//! Settings (DESIGN §13.9): page list (Playlists, EPG, Player, Interface, Favourites,
//! Reminders, Parental, Backup, Hidden, Orphans, About) | page content. Panel idiom; reorder
//! rows (playlists, EPG priority, favourites lists) with Alt+↑/↓ or K/J or the drag grip.
//! Playlists page: sources with status, account info (expiry, connections, formats),
//! refresh/edit/delete/reorder, "+ Add" (→ `Route::Onboarding(AddSource)`); EPG page: ordered
//! EPG sources with offsets + "EPG mapping…"; Reminders: pending list (Delete cancels);
//! About: version, paths, log viewer link. Text fields push `TextInput`. Edits go through
//! `ctx.edit_config` (applied live to the player). Owner: WP-12.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct SettingsScreen {
    // WP-12: screen state (private; free to reshape)
}

impl Screen for SettingsScreen {
    fn id(&self) -> ScreenId { ScreenId::Settings }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Panel] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::SETTINGS, "Settings", "WP-12") }
}
