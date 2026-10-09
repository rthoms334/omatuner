//! Fullscreen player + OSD (DESIGN §13.8): VideoHost Fullscreen, fixed dark scrim, OSD type
//! scale `layout::osd_scale`, accent = `tokens.osd_accent`.
//! * INFO BANNER (passive): shown on zap / any key / mouse move, hides after
//!   `osd_timeout_ms`; NEVER changes the key context (arrows keep zapping/seeking).
//! * CONTROLS FOCUS MODE: entered only with Enter / `i` / Tab (`FocusOsdControls`) or a click
//!   on a control; pushes `PlayerOsd`; Esc (`LeaveOsdControls`) leaves it, banner stays.
//! * Esc ladder (no focus mode): hide banner → leave window fullscreen → leave the player
//!   (mini-player keeps playing). Esc never SHOWS the banner.
//! * Side panels (`c` channels, `g` mini guide, `h`/→ history): stack
//!   `[List|ListFiltering, PlayerLive]`; ↑/↓ move the cursor (no zap), Enter tunes, ←/Esc close.
//!   Stream info `I`.
//! * Numeric entry (digits): stack `[NumericEntry, PlayerLive]`, 1.5 s auto-commit.
//! * End of playback: episode → "Next episode in 10 s" card; movie → marked watched, back to
//!   its detail page.
//! * Mouse: double-click toggles window fullscreen; wheel = volume (only here).
//!
//! Owner: WP-11.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct PlayerScreen {
    // WP-11: screen state (private; free to reshape)
}

impl Screen for PlayerScreen {
    fn id(&self) -> ScreenId { ScreenId::Player }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::PlayerLive] }
    fn allows_mini_player(&self) -> bool { false }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::NO_VIDEO, "Player", "WP-11") }
}
