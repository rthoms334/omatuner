//! Settings ▸ Player ▸ Multi-view: layout, picture-in-picture corner, light decoding
//! (DESIGN §35). OWNER: MV-2.

use super::*;
use crate::config::{LightDecode, MultiViewLayout};

const LAYOUTS: [MultiViewLayout; 4] = [MultiViewLayout::Auto, MultiViewLayout::SideBySide, MultiViewLayout::BigSmall, MultiViewLayout::Pip];

static MV_CHOICES: [CfgChoice; 2] = [
    CfgChoice {
        label: "Multi-view layout",
        options: &["Automatic (by window width)", "Side by side", "Big + small", "Picture in picture"],
        get: |c| pos(&LAYOUTS, &c.player.multiview.layout),
        set: |c, i| c.player.multiview.layout = LAYOUTS[i],
    },
    CfgChoice {
        label: "Picture-in-picture corner",
        options: &["Bottom right", "Bottom left", "Top right", "Top left"],
        get: |c| pos(&POPOUT_CORNERS, &c.player.multiview.pip_corner),
        set: |c, i| c.player.multiview.pip_corner = POPOUT_CORNERS[i],
    },
];

static MV_LIGHT: CfgToggle = CfgToggle {
    label: "Lighter decoding for the small picture",
    get: |c| c.player.multiview.light_decode_small.0,
    set: |c, on| c.player.multiview.light_decode_small = LightDecode(on),
};

/// Rows at the end of the Player page.
pub(super) fn rows(_s: &SettingsScreen, _ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    if !crate::ui::multiview::ENABLED {
        return;
    }
    v.push(Row::Section("Multi-view".into()));
    v.extend(MV_CHOICES.iter().map(Row::Choice));
    v.push(Row::Toggle(&MV_LIGHT));
    v.push(Row::Note(
        "Ctrl+M watches a channel in a second picture. Each playlist plays as many streams as its account allows (usually one), so two pictures \
         come from two playlists — Tuner picks a copy on a free one and asks before moving a playing picture. t changes the layout, M moves the \
         sound, W swaps."
            .into(),
        false,
    ));
}

/// Its `Act::Feature(Feature::MultiView, n)` buttons (none).
pub(super) fn act(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _n: u16) {}

/// Enter on its `Item::Feature(Feature::MultiView, id)` rows (none).
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}
