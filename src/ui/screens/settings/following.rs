//! Settings ▸ Following (DESIGN §33). OWNER: F-C. Listed once `follow::ENABLED`: the rules,
//! the rule editor and Upcoming (Skipped included).

use super::*;

/// The page's state (loaded rows, the editor…), kept in `SettingsScreen`.
#[derive(Default)]
pub(super) struct State {}

/// The Following page's rows.
pub(super) fn rows(_s: &SettingsScreen, _ctx: &UiCtx<'_>, _v: &mut Vec<Row>) {}

/// Its `Act::Feature(Feature::Following, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _n: u16) {}

/// Enter on its `Item::Feature(Feature::Following, id)` rows.
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}

/// The page became current (or its data changed): start loading.
pub(super) fn enter(_s: &mut SettingsScreen, _ctx: &UiCtx<'_>) {}
