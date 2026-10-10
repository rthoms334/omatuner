//! Series (DESIGN §13.5): categories pane | poster grid. Enter → SeriesDetail. Owner: WP-12.
//! Same browser as Movies (`movies::VodBrowser`) over series posters; Shift+Enter plays the
//! episode the series page would open at (resume prompt when it has progress).

use eframe::egui;

use super::movies::VodBrowser;
use crate::keymap::{Action, KeyContext};
use crate::msg::AppEvent;
use crate::ui::overlays::OverlayResult;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct SeriesScreen {
    browser: VodBrowser,
}

impl Screen for SeriesScreen {
    fn id(&self) -> ScreenId { ScreenId::Series }
    fn key_contexts(&self) -> Vec<KeyContext> { self.browser.key_contexts() }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let group = match route {
            Route::Series { group } => *group,
            _ => None,
        };
        self.browser.enter(ctx, false, group);
    }
    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) { self.browser.event(ctx, ev) }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { self.browser.action(ctx, action) }
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) { self.browser.overlay_result(ctx, result) }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { self.browser.ui(ui, ctx) }
}
