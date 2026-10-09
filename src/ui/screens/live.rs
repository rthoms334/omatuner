//! Live TV (DESIGN §13.2): groups pane | channel list (logo, number, name, now/next +
//! progress via `ctx.now_next`) | preview pane (video via VideoHost Preview + programme
//! details + upcoming). Group list has virtual groups on top: each favourites list, History,
//! Reminders, All channels.
//! * Enter rule: Enter on the channel that is already playing → fullscreen player; otherwise
//!   play in the preview (Shift+Enter always fullscreen). Returning from the player moves the
//!   cursor to the channel that was zapped to.
//! * Preview autoplay (`live_preview_autoplay`, OFF by default): only after the KEYBOARD cursor
//!   rested `preview_delay_ms` (1200), only if nothing plays or the current stream was itself
//!   started by autoplay (`snapshot.origin == LivePreview`); never from hover.
//! * Hover moves the cursor only after real pointer motion (Omarchy `pointerGate`).
//! * Layouts: Wide 3 panes; Medium = channels + preview on the right, groups in a drawer
//!   (← / Backspace or the breadcrumb); Compact = stack, Enter plays fullscreen.
//! * Key contexts: `[List|ListFiltering]` of the focused pane. Owner: WP-11.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct LiveScreen {
    // WP-11: screen state (private; free to reshape)
}

impl Screen for LiveScreen {
    fn id(&self) -> ScreenId { ScreenId::Live }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::List] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::LIVE_TV, "Live TV", "WP-11") }
}
