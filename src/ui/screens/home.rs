//! Home (DESIGN §13.3): rails — Continue watching (posters w/ progress), Favourite channels
//! (live now/next tiles; one rail per non-empty favourites list), Recently watched channels,
//! Recently added movies / series. Key context `[PosterGrid]` (2-D: ↑/↓ between rails, ←/→
//! within a rail; letters do NOT filter on Home — the screen ignores `FilterInput`). Enter on a
//! channel plays FULLSCREEN (no preview pane here; DESIGN §13 "Enter rule"), zap collection =
//! the favourites list of the rail / the channel's primary group. Fewer than 2 rails → a
//! "Get started" rail (Browse Live TV · Open guide · Add favourites (Ctrl+F) · first group's
//! channels with now/next). Owner: WP-12.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct HomeScreen {
    // WP-12: screen state (private; free to reshape)
}

impl Screen for HomeScreen {
    fn id(&self) -> ScreenId { ScreenId::Home }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::PosterGrid] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::HOME, "Home", "WP-12") }
}
