//! Up next (Alt+6, DESIGN §32): the list, drag to reorder, the detail pane. OWNER: Q-ui. Stub
//! until Up next ships (the route is reachable only once `queue::ENABLED`).

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::{empty_state, glyphs};
use crate::ui::{Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct QueueScreen {}

impl Screen for QueueScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Queue
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::List]
    }
    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, _action: Action) -> bool {
        false
    }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        empty_state::empty_state(
            ui,
            ctx.theme,
            glyphs::UP_NEXT,
            "Up next is empty",
            Some("Ctrl+N on a film, an episode or a programme adds it."),
            None,
        );
    }
}
