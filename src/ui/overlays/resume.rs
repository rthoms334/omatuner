//! "Resume from 41:20" / "Start over" prompt (Resume focused first). Owner: WP-12.
//! Also used for the end-of-episode card "Next episode in 10 s" (Enter = now, Esc = cancel).

use eframe::egui;

use super::{Overlay, OverlayOutcome, ResumeSpec};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

pub struct ResumeOverlay {
    pub spec: ResumeSpec,
    resume_focused: bool,
}

impl ResumeOverlay {
    pub fn new(spec: ResumeSpec) -> Self { Self { spec, resume_focused: true } }
}

impl Overlay for ResumeOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Resume", "WP-12") }
}
