//! The zap shield card (DESIGN §34, S4). OWNER: S-ui. Stub until the spoiler guard's UI ships:
//! a live zap onto a channel whose programme the guard shields shows this card instead of the
//! picture's text ("Guarded: Arsenal v Chelsea · Enter to watch anyway").

use super::*;

impl PlayerScreen {
    /// Draw the shield card over `rect` when the playing channel is shielded; `true` when drawn
    /// (the banner then stays hidden). Identity until S-ui.
    pub(super) fn draw_shield(&mut self, _ui: &egui::Ui, _ctx: &mut UiCtx<'_>, _rect: Rect, _snap: &PlayerSnapshot) -> bool {
        false
    }
}
