//! Audio / subtitle track menu over the video (fixed dark scrim). Subtitle menu also has delay
//! (−/+ 100 ms) and size rows. Owner: WP-11.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

pub struct TracksOverlay {
    pub audio: bool,
    cursor: usize,
}

impl TracksOverlay {
    pub fn new(audio: bool) -> Self { Self { audio, cursor: 0 } }
}

impl Overlay for TracksOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Tracks", "WP-11") }
}
