//! Settings ▸ Reminders: open in (pop-out / full window / where I am), while Tuner is closed,
//! the sleep timer's warning, the systemd arming status (B6). OWNER: rui.

use super::*;

/// Rows after the pending reminders (nothing until the reminder UI ships).
pub(super) fn rows(_s: &SettingsScreen, _ctx: &UiCtx<'_>, _v: &mut Vec<Row>) {}

/// Its `Act::Feature(Feature::Reminders, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _n: u16) {}

/// Enter on its `Item::Feature(Feature::Reminders, id)` rows.
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}
