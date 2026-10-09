//! Onboarding wizard (DESIGN §13.10): one centred card, steps: Welcome (Add playlist /
//! Restore backup) → Type (Xtream / M3U URL / M3U file — pasted get.php URLs auto-detected) →
//! Details (name, icon, colour, credentials) → Test (ProbeSource: account info, warnings) →
//! Guide (only for M3U without `url-tvg`: XMLTV URL/file or Skip) → Import progress
//! (AddSource job phases) → Done. Details (DESIGN §13.10): "Open Live TV" is enabled as soon as
//! the channels phase finishes; Test failures route back to the offending field (auth) or offer
//! Retry / Add anyway (unreachable); the Xtream server field is normalized with
//! `xtream::urls::normalize_server`. The same card serves `Route::Onboarding(start)` for
//! Settings ▸ Add, playlist files and restored sources needing credentials. Key contexts:
//! `[Modal]` on choice steps, `[TextInput, Modal]` while a form field is focused. No sidebar,
//! status bar or mini-player while onboarding. Owner: WP-12.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct OnboardingScreen {
    // WP-12: screen state (private; free to reshape)
}

impl Screen for OnboardingScreen {
    fn id(&self) -> ScreenId { ScreenId::Onboarding }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }
    fn allows_mini_player(&self) -> bool { false }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::PLAYLIST, "Welcome", "WP-12") }
}
