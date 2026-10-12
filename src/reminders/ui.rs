//! The reminder engine's UI pieces (B6). OWNER: rui. base-B stubs: the sleep timer menu
//! (Ctrl+Shift+T, `OverlayRequest::SleepTimer`) and ending a news flash (Delete in the pop-out).

use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;
use crate::ui::overlays::{Overlay, OverlayOutcome};

/// The sleep timer menu: in N min, end of programme, after N items, end of Up next, off.
pub struct SleepTimerMenu;

impl Overlay for SleepTimerMenu {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }
    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, _action: Action) -> OverlayOutcome {
        OverlayOutcome::Close
    }
    fn ui(&mut self, _egui_ctx: &eframe::egui::Context, _ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        OverlayOutcome::Close
    }
}

/// Delete in the pop-out during a news flash: end it and put back what was playing. False
/// when no flash is on (the key then does nothing).
pub fn end_news_flash(_services: &crate::services::Services) -> bool {
    false
}
