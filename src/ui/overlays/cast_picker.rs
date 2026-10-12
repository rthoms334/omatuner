//! "Cast to…": the device picker (DESIGN §40 §3.1). OWNER: CAST-8.
//!
//! A right-edge panel in OSD colours, built like `tracks.rs` (`Row`, `cursor`, `place_cursor`,
//! `choose`): "This computer" first, then the devices from `CastHandle::devices()` (most recent
//! first) with their kind and state line, "Searching…" with a busy bar while
//! `CastHandle::browse()` runs, "No TVs found" + "Firewall help…" after 6 s, "Add by address…".
//! Enter on a paired device sends `PlayerCommand::CastTo`; on an unpaired one opens
//! `OverlayRequest::CastPair`; "This computer" while casting is Play here. No IP addresses
//! anywhere. In multi-view: one line, "Leave multi-view to cast".

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

/// CAST-8: the picker. Until then it closes at once (casting is off: `cast::ENABLED`).
#[derive(Debug, Default)]
pub struct CastPicker {}

impl CastPicker {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Overlay for CastPicker {
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
