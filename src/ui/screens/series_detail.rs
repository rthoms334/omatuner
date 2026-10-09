//! Series detail (DESIGN §13.6): header (backdrop, title, plot) + season tabs (`[` / `]`,
//! Tab moves between buttons / seasons / episodes) + episode list opened at
//! `store::vod::next_episode`; Enter plays, Shift+Enter from the start, Ctrl+W marks watched,
//! bulk "mark season watched" in the context menu. Key context `[Detail]`. End of an episode:
//! "Next episode in 10 s" card (Enter = now, Esc = cancel). Owner: WP-12.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct SeriesDetailScreen {
    // WP-12: screen state (private; free to reshape)
}

impl Screen for SeriesDetailScreen {
    fn id(&self) -> ScreenId { ScreenId::SeriesDetail }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Detail] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::SERIES, "Series", "WP-12") }
}
