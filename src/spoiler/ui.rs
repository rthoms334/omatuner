//! The spoiler guard's UI pieces (DESIGN §34). OWNER: S-ui. base-B stubs: Ctrl+B on an item
//! (`RevealSet`), "Mark seen" (Ctrl+W in the guide), "Guard spoilers about…" in item menus, the
//! status bar chip.

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

/// The status bar's chip ("Spoilers shown · 14 min ✕"); none while off.
pub fn status_chip(_shared: &crate::shared::AppShared, _now: crate::model::Timestamp) -> Option<crate::ui::widgets::status_bar::StatusChip> {
    None
}
