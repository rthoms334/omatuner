//! The Follow sheet (`R` in the guide, "Follow…" in menus; DESIGN §33): suggestions, then the
//! rule editor with its live preview. OWNER: F-C. Stub until Following ships.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

/// The sheet for one programme (or a rule to edit).
pub struct FollowSheet {
    #[allow(dead_code)] // F-C
    spec: FollowSheetSpec,
}

impl FollowSheet {
    pub fn new(spec: FollowSheetSpec) -> Self {
        Self { spec }
    }
}

impl Overlay for FollowSheet {
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

/// What the sheet opens on.
#[derive(Debug, Clone, PartialEq)]
pub struct FollowSheetSpec {
    /// The programme it was opened on (`None`: a blank rule, Settings ▸ Following ▸ New).
    pub programme: Option<(crate::model::ProgrammeId, crate::model::ChannelId)>,
    /// An existing rule to edit.
    pub rule: Option<crate::model::FollowRuleId>,
}
