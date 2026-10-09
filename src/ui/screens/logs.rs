//! Debug log viewer (UHF debug mode): type to filter text, Enter expands a line, Ctrl+S
//! (`SortMenu`) opens the view menu: minimum level + "Export…" (file picker; lines are already
//! redacted). Key context `[List|ListFiltering]`. Owner: WP-12.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct LogsScreen {
    // WP-12: screen state (private; free to reshape)
}

impl Screen for LogsScreen {
    fn id(&self) -> ScreenId { ScreenId::Logs }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::List] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::INFO, "Logs", "WP-12") }
}
