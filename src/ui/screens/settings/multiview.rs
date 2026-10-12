//! Settings ▸ Player ▸ Multi-view: layout, picture-in-picture corner, light decoding
//! (DESIGN §35). OWNER: MV-2.

use super::*;

/// Rows at the end of the Player page (nothing until multi-view ships).
pub(super) fn rows(_s: &SettingsScreen, _ctx: &UiCtx<'_>, _v: &mut Vec<Row>) {}

/// Its `Act::Feature(Feature::MultiView, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _n: u16) {}

/// Enter on its `Item::Feature(Feature::MultiView, id)` rows.
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}
