//! Background tasks panel (Ctrl+J): running jobs with phase + progress bar; `Delete` (Remove)
//! cancels the highlighted job; last 20 finished with errors expandable (Enter; redacted).
//! Owner: WP-10a. Key contexts `[List, Modal]`.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

#[derive(Default)]
pub struct JobsOverlay {
    cursor: usize,
}

impl Overlay for JobsOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::List, KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Background tasks", "WP-10a") }
}
