//! Ctrl+I "Edit information" (UHF 1.100.1): rename, per-channel catch-up offset (± minutes,
//! stored as `item_override.catchup_offset_s`), EPG assignment (opens the picker with
//! `epg_match::suggest` candidates), lock, hide. Writes through `store::user::set_item_override`
//! / `store::epg::set_manual_mapping` (fingerprints computed by the store). Owner: WP-11.
//! Key contexts: `[Modal]`, `[TextInput, Modal]` while a field is focused.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::model::ItemRef;
use crate::ui::UiCtx;

pub struct EditItemOverlay {
    pub item: ItemRef,
    cursor: usize,
}

impl EditItemOverlay {
    pub fn new(item: ItemRef) -> Self { Self { item, cursor: 0 } }
}

impl Overlay for EditItemOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        super::placeholder_ui(egui_ctx, ctx, "Edit information", "WP-11")
    }
}
