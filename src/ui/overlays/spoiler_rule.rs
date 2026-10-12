//! Add or edit a spoiler topic / blackout (DESIGN §34). OWNER: S-ui. Stub until the guard's UI
//! ships.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

/// The editor for one rule (`None`: a new one, prefilled with `initial`).
pub struct SpoilerRuleEditor {
    #[allow(dead_code)] // S-ui
    spec: SpoilerRuleSpec,
}

impl SpoilerRuleEditor {
    pub fn new(spec: SpoilerRuleSpec) -> Self {
        Self { spec }
    }
}

impl Overlay for SpoilerRuleEditor {
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

/// What the editor opens on.
#[derive(Debug, Clone, PartialEq)]
pub struct SpoilerRuleSpec {
    pub rule: Option<crate::model::SpoilerRuleId>,
    /// A new rule's terms (the programme's title, a team…).
    pub initial: String,
}
