//! Settings ▸ EPG: "Fix guide data… (N to fix)" (DESIGN §31). OWNER: G-ui.
//!
//! The rules ("+1/+2 follow their base", "Copies share a guide", the playlist catch-up offset)
//! are changed in Guide fixes itself (its Rules card and Ctrl+S menu), where the card says what
//! a change affects before it is made.

use super::*;

/// The "EPG mapping…" row becomes "Fix guide data… (N to fix)" (counted off the UI thread per
/// library build; the count of shown channels, like the coverage above it).
pub(super) fn rows(_s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut [Row]) {
    if !crate::guide_fix::ENABLED {
        return;
    }
    if let Some(Row::Act { act: Act::EpgMapping, label, .. }) = v.last_mut() {
        *label = match crate::guide_fix::ui::to_fix(ctx) {
            Some(0) => "Fix guide data… (nothing to fix)".into(),
            Some(n) => format!("Fix guide data… ({} to fix)", grouped(n)),
            None => "Fix guide data…".into(),
        };
    }
}

/// Its `Act::Feature(Feature::GuideFix, n)` buttons (none).
pub(super) fn act(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _n: u16) {}

/// Enter on its `Item::Feature(Feature::GuideFix, id)` rows (none).
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}
