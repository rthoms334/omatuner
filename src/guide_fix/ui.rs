//! Guide fixes' UI pieces (DESIGN §31). OWNER: G-ui. base-B stubs: "Fix this channel's guide…"
//! in channel menus.

use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;
use crate::ui::item_menu::Target;
use crate::ui::overlays::MenuItem;

/// Its entries in an item menu (`ui::item_menu::extend`); none while the feature is off.
pub fn menu_items(_target: &Target, _ctx: &UiCtx<'_>, _keys: KeyContext) -> Vec<MenuItem> {
    Vec::new()
}

/// Its keys and menu choices on an item (`ui::item_menu::dispatch`); false while off.
pub fn on_action(_ctx: &mut UiCtx<'_>, _action: Action, _target: &Target) -> bool {
    false
}
