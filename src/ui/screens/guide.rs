//! TV Guide (DESIGN §13.4): header (clickable collection picker "Favourites ▾", ‹ day ›, Now) +
//! `epg_grid` + optional preview strip (Wide, config `guide_preview`). Default collection: the
//! first non-empty favourites list, else the last-used group. Enter → programme popover; `r`
//! toggles a reminder; digits jump to a channel number (`[NumericEntry, Guide]`); click on the
//! channel column = watch; returning from the player focuses the channel just watched
//! (UHF 1.96). Keeps the focused cell clear of `ctx.mini_rect`. Owner: WP-11.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct GuideScreen {
    // WP-11: screen state (private; free to reshape)
}

impl Screen for GuideScreen {
    fn id(&self) -> ScreenId { ScreenId::Guide }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Guide] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::GUIDE, "TV Guide", "WP-11") }
}
