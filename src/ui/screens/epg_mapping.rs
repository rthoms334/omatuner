//! EPG mapping (DESIGN §9.4): channel list filtered to Unmapped / Fuzzy suggestions / All;
//! right pane shows suggestions (`epg_match::suggest`, computed lazily for VISIBLE rows on a
//! worker with a token prefilter — never for all unmapped channels at once) + searchable list
//! of EPG channels; Enter assigns (Manual), Delete clears. Key contexts
//! `[EpgMapping, List|ListFiltering]`. Owner: WP-12.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct EpgMappingScreen {
    // WP-12: screen state (private; free to reshape)
}

impl Screen for EpgMappingScreen {
    fn id(&self) -> ScreenId { ScreenId::EpgMapping }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::EpgMapping, KeyContext::List] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::GUIDE, "EPG mapping", "WP-12") }
}
