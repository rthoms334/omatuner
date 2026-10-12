//! Settings ▸ Interface ▸ Playback: "Play the next item after" (Up next's countdown, DESIGN §32).
//! OWNER: Q-ui.

use super::*;

/// Rows in the Interface page's Playback section (nothing until Up next ships).
pub(super) fn rows(_s: &SettingsScreen, _ctx: &UiCtx<'_>, _v: &mut Vec<Row>) {}

/// Its `Act::Feature(Feature::Queue, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _n: u16) {}

/// Enter on its `Item::Feature(Feature::Queue, id)` rows.
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}
