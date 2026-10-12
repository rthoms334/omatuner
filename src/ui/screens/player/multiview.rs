//! The multi-view chrome of the player screen (DESIGN §35): per-picture info strips, the
//! focus ring, the OSD's Layout / Sound here / Swap / Close group. OWNER: MV-2. Stub until
//! multi-view ships: single view draws nothing here.

use super::*;

impl PlayerScreen {
    /// Draw the multi-view chrome over the pictures; nothing in single view.
    pub(super) fn draw_multiview(&mut self, _ui: &egui::Ui, _ctx: &mut UiCtx<'_>, _rect: Rect) {}

    /// A multi-view key (`KeyContext::MultiView`); `None` when not one (single view: always).
    pub(super) fn multiview_action(&mut self, _ctx: &mut UiCtx<'_>, _action: Action) -> Option<bool> {
        None
    }
}
