//! Generic dialogs: context menu, confirm, PIN prompt, rename, favourites-list picker.
//! Owner: WP-10a. All keyboard-first (Modal / TextInput contexts), Omarchy menu card visuals.

use eframe::egui;

use super::{ConfirmSpec, ContextMenuSpec, ListPickerSpec, Overlay, OverlayOutcome, RenameSpec};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

/// Launcher-style menu: rows (icon, label, shortcut hint), type-to-filter, Enter chooses.
pub struct ContextMenuOverlay {
    pub spec: ContextMenuSpec,
    cursor: usize,
    filter: crate::search::ListFilter,
}

impl ContextMenuOverlay {
    pub fn new(spec: ContextMenuSpec) -> Self { Self { spec, cursor: 0, filter: Default::default() } }
}

impl Overlay for ContextMenuOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![if self.filter.is_active() { KeyContext::ListFiltering } else { KeyContext::List }, KeyContext::Modal]
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Menu", "WP-10a") }
}

/// Yes/No dialog; destructive variant focuses Cancel and paints confirm in `urgent`.
pub struct ConfirmOverlay {
    pub spec: ConfirmSpec,
    focus_confirm: bool,
}

impl ConfirmOverlay {
    pub fn new(spec: ConfirmSpec) -> Self { let f = !spec.destructive; Self { spec, focus_confirm: f } }
}

impl Overlay for ConfirmOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Confirm", "WP-10a") }
}

/// 4–8 digit PIN entry (dots), 3 wrong attempts → 30 s cooldown. Verifies with
/// `ctx.parental.unlock` and replies `OverlayResult::Unlocked(then)`.
pub struct PinOverlay {
    pub then: Action,
    pub reason: String,
    entered: String,
    failures: u32,
}

impl PinOverlay {
    pub fn new(then: Action, reason: String) -> Self { Self { then, reason, entered: String::new(), failures: 0 } }
}

impl Overlay for PinOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::TextInput, KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "PIN", "WP-10a") }
}

/// Single text field; Enter saves via the store writer (empty = remove the rename).
pub struct RenameOverlay {
    pub spec: RenameSpec,
    text: String,
}

impl RenameOverlay {
    pub fn new(spec: RenameSpec) -> Self { let t = spec.initial.clone(); Self { spec, text: t } }
}

impl Overlay for RenameOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::TextInput, KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Rename", "WP-10a") }
}

/// Checklist of favourites lists (toggle membership with Enter/Space) + a "New list…" row
/// (opens an inline name field: `[TextInput, Modal]` while typing).
pub struct ListPickerOverlay {
    pub spec: ListPickerSpec,
    cursor: usize,
}

impl ListPickerOverlay {
    pub fn new(spec: ListPickerSpec) -> Self { Self { spec, cursor: 0 } }
}

impl Overlay for ListPickerOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Favourites lists", "WP-10a") }
}
