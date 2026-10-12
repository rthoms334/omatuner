//! Following's UI pieces (DESIGN §33). OWNER: F-C. base-B stubs: `R` and "Follow…" in item
//! menus (they open `OverlayRequest::FollowSheet`), the guide tooltip's "from Following" text.

use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;
use crate::ui::item_menu::Target;
use crate::ui::overlays::MenuItem;

/// Its entries in an item menu (`ui::item_menu::extend`); none while the feature is off.
pub fn menu_items(_target: &Target, _ctx: &UiCtx<'_>, _keys: KeyContext) -> Vec<MenuItem> {
    Vec::new()
}

/// Its primary button in the programme popover ("Follow…"); none while the feature is off.
pub fn popover_button(_target: &Target, _ctx: &UiCtx<'_>) -> Option<MenuItem> {
    None
}

/// The guide tooltip's line for a programme a rule matched ("From Following: Arsenal"); none
/// while Following is off.
pub fn tooltip_line(_p: &crate::model::ProgrammeSlim) -> Option<String> {
    None
}

/// Its keys and menu choices on an item (`ui::item_menu::dispatch`); false while off.
pub fn on_action(_ctx: &mut UiCtx<'_>, _action: Action, _target: &Target) -> bool {
    false
}
