//! Up next's UI pieces (DESIGN §32). OWNER: Q-ui. base-B stubs: the item menus' "Add to Up next"
//! / "Play next", the series episode-count picker overlay, the mini player's chip and the
//! pop-out's strip.

use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;
use crate::ui::item_menu::Target;
use crate::ui::overlays::MenuItem;

/// Its entries in an item menu (`ui::item_menu::extend`); none while the feature is off.
pub fn menu_items(_target: &Target, _ctx: &UiCtx<'_>, _keys: KeyContext) -> Vec<MenuItem> {
    Vec::new()
}

/// Its primary button in the programme popover ("Up next"); none while the feature is off.
pub fn popover_button(_target: &Target, _ctx: &UiCtx<'_>) -> Option<MenuItem> {
    None
}

/// Its keys and menu choices on an item (`ui::item_menu::dispatch`); false while off.
pub fn on_action(_ctx: &mut UiCtx<'_>, _action: Action, _target: &Target) -> bool {
    false
}

/// The series picker (`OverlayRequest::QueueSeries`): how many episodes to add.
pub struct SeriesPicker;

impl SeriesPicker {
    pub fn new(_series: crate::model::SeriesId, _play_next: bool) -> Self {
        Self
    }
}

impl crate::ui::overlays::Overlay for SeriesPicker {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }
    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, _action: Action) -> crate::ui::overlays::OverlayOutcome {
        crate::ui::overlays::OverlayOutcome::Close
    }
    fn ui(&mut self, _egui_ctx: &eframe::egui::Context, _ctx: &mut UiCtx<'_>) -> crate::ui::overlays::OverlayOutcome {
        crate::ui::overlays::OverlayOutcome::Close
    }
}

/// The Up next chip on the mini player (`VideoHost::mini_player` calls it over the video's
/// rect); nothing while Up next is off.
pub fn mini_chip(_ui: &eframe::egui::Ui, _theme: &crate::theme::Theme, _queue: &crate::queue::QueueSnapshot, _video: eframe::egui::Rect) {}
