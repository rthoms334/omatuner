//! The spoiler guard menu (Ctrl+Shift+B; DESIGN §34): pause for 15 min, I'll watch it later,
//! guard a topic, settings. OWNER: S-ui. Stub until the guard's UI ships.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

/// The menu.
#[derive(Default)]
pub struct SpoilerMenu;

impl Overlay for SpoilerMenu {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }
    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, _action: Action) -> OverlayOutcome {
        OverlayOutcome::Close
    }
    fn ui(&mut self, _egui_ctx: &egui::Context, _ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        OverlayOutcome::Close
    }
}
