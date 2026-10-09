//! Guide programme popover: title, time range, channel, description, episode, categories;
//! actions Watch (live now) / Catch up (past + archive) / Remind me (future) / Assign EPG.
//! Owner: WP-11.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, ProgrammeId};
use crate::ui::UiCtx;

pub struct ProgrammeOverlay {
    pub programme: ProgrammeId,
    pub channel: ChannelId,
    cursor: usize,
}

impl ProgrammeOverlay {
    pub fn new(programme: ProgrammeId, channel: ChannelId) -> Self { Self { programme, channel, cursor: 0 } }
}

impl Overlay for ProgrammeOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Programme", "WP-11") }
}
