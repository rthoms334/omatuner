//! Settings ▸ EPG: "Fix guide data… (N to fix)" and the +1 / copies rule toggles (DESIGN §31).
//! OWNER: G-ui.

use super::*;

/// Rows after "EPG mapping…" on the EPG page (nothing until Guide fixes ships).
pub(super) fn rows(_s: &SettingsScreen, _ctx: &UiCtx<'_>, _v: &mut Vec<Row>) {}

/// Its `Act::Feature(Feature::GuideFix, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _n: u16) {}

/// Enter on its `Item::Feature(Feature::GuideFix, id)` rows.
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}
