//! Series (DESIGN §13.5): categories pane | poster grid. Enter → SeriesDetail. Owner: WP-12.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct SeriesScreen {
    // WP-12: screen state (private; free to reshape)
}

impl Screen for SeriesScreen {
    fn id(&self) -> ScreenId { ScreenId::Series }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::PosterGrid] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::SERIES, "Series", "WP-12") }
}
