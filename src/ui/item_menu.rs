//! The batch features' share of item menus and item keys (base-B, plan §2.2). OWNER: the
//! integrator; each feature fills its own `menu_items` / `on_action` in its `ui` module:
//! Up next (`queue::ui`), Following (`follow::ui`), the spoiler guard (`spoiler::ui`), Guide
//! fixes (`guide_fix::ui`), multi-view (`ui::multiview`) and reminders (`reminders::ui`: "Switch to
//! this when it starts…" on a programme).
//!
//! A screen with an item menu calls [`extend`] when it builds the menu for the item under the
//! cursor, and [`dispatch`] from its item action handler (keys such as Ctrl+N / Ctrl+M / `R`,
//! and the menu's choices, which come back as the same actions). Every hook is a no-op while
//! its feature is off, so the screens behave as before.
//!
//! Screens dispatch before their own handling, so a feature takes only its own actions — the
//! one shared key is Ctrl+W (`MarkWatched`), which the spoiler guard takes on a `Programme`
//! only ("Mark seen"); on films and episodes it stays "Mark watched".

use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, EpisodeId, MovieId, ProgrammeId, SeriesId};
use crate::ui::UiCtx;
use crate::ui::overlays::MenuItem;

/// What an item menu or an item key is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Channel(ChannelId),
    Movie(MovieId),
    Series(SeriesId),
    Episode {
        series: SeriesId,
        episode: EpisodeId,
    },
    /// A guide programme on a channel (past, airing or future).
    Programme {
        programme: ProgrammeId,
        channel: ChannelId,
    },
}

impl Target {
    /// A movie or series card.
    pub fn vod(kind: crate::model::VodKind) -> Self {
        match kind {
            crate::model::VodKind::Movie(id) => Target::Movie(id),
            crate::model::VodKind::Series(id) => Target::Series(id),
        }
    }
}

/// Add the features' entries for `target` to a menu (`keys`: the context the shortcuts are
/// shown for).
pub fn extend(items: &mut Vec<MenuItem>, target: &Target, ctx: &UiCtx<'_>, keys: KeyContext) {
    items.extend(crate::queue::ui::menu_items(target, ctx, keys));
    items.extend(crate::follow::ui::menu_items(target, ctx, keys));
    items.extend(crate::spoiler::ui::menu_items(target, ctx, keys));
    items.extend(crate::ui::multiview::menu_items(target, ctx, keys));
    items.extend(crate::guide_fix::ui::menu_items(target, ctx, keys));
    items.extend(crate::reminders::ui::menu_items(target, ctx, keys));
}

/// The programme popover's primary buttons after Watch / Catch up and Remind me: Up next, then
/// Follow… (plan §2.2: at most 4 in all; the rest go under "More…" through [`extend`]).
pub fn popover_buttons(target: &Target, ctx: &UiCtx<'_>) -> Vec<MenuItem> {
    [crate::queue::ui::popover_button(target, ctx), crate::follow::ui::popover_button(target, ctx)].into_iter().flatten().collect()
}

/// A feature's key or menu choice on `target`; true when a feature took it.
pub fn dispatch(ctx: &mut UiCtx<'_>, action: Action, target: &Target) -> bool {
    crate::queue::ui::on_action(ctx, action, target)
        || crate::follow::ui::on_action(ctx, action, target)
        || crate::spoiler::ui::on_action(ctx, action, target)
        || crate::ui::multiview::on_action(ctx, action, target)
        || crate::guide_fix::ui::on_action(ctx, action, target)
        || crate::reminders::ui::on_action(ctx, action, target)
}

/// A menu entry with its shortcut from `keys` (the screens' `menu_item`).
pub fn entry(icon: &'static str, label: impl Into<String>, action: Action, keys: KeyContext) -> MenuItem {
    crate::ui::screens::live::menu_item(icon, label, action, true, keys)
}
