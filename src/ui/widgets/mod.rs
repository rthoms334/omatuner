//! Component catalogue mapped to Omarchy tokens (DESIGN §13.1). Owner: WP-10a (except where a
//! file says otherwise). Every widget takes `&Theme` and paints with `theme.tokens` only —
//! no hard-coded colours except the video OSD scrim.
//!
//! Conventions shared by every widget here:
//! * Widgets never read raw keys (DESIGN §15.1): keyboard state arrives as a `focused` flag or a
//!   [`RowState`]; the screen turns `keymap::Action`s into value changes.
//! * Every interactive widget reports `WidgetInfo` (role + label), so accesskit — and the
//!   `egui_kittest` tests — can find it by its label.
//! * Row renderers (`rows::*`) paint their own state background from the `RowState` they get, so
//!   they look right inside [`list::virtual_list`] and anywhere else (menus, side panels).

use eframe::egui;

pub mod badge;
pub mod button;
pub mod card;
pub mod copy_card;
pub mod empty_state;
pub mod glyphs;
pub mod list;
pub mod logo;
mod paint;
pub mod poster;
pub mod progress;
pub mod rows;
pub mod scope_bar;
pub mod section;
pub mod sidebar;
pub mod slider;
pub mod spoiler;
pub mod status_bar;
pub mod text_field;
pub mod time;
pub mod toast;
pub mod toggle;

pub use paint::{session_locked, set_session_locked};

/// A primary click that is not the 2nd/3rd click of a double/triple-click (egui reports
/// `clicked()` on every release of a multi-click). One click does what Enter does, so a widget
/// acting on `clicked()` acts on this instead: a double-click acts once — a tick isn't flipped
/// back, a row isn't played twice (DESIGN §15.1 "Double-click safety"). Keyboard/accesskit
/// clicks are always fresh, and so is a whole double-click that arrives in one frame (a busy
/// frame batches input): it still acts once.
pub fn fresh_click(resp: &egui::Response) -> bool {
    let releases = || {
        resp.ctx.input(|i| {
            i.raw
                .events
                .iter()
                .filter(|e| matches!(e, egui::Event::PointerButton { button: egui::PointerButton::Primary, pressed: false, .. }))
                .count()
        })
    };
    resp.clicked() && ((!resp.double_clicked() && !resp.triple_clicked()) || releases() > 1)
}

/// This frame's click swapped what is under the pointer without changing the route or overlay
/// stack (a category opening its grid in the same pane, the Compact page list opening a page):
/// the app drops the second click of the double-click so it can't act on what appeared there
/// ([`crate::ui::click_guard`]). Route and overlay changes are noticed without this.
pub fn swapped_under_pointer(ctx: &egui::Context) {
    crate::ui::click_guard::mark_swapped(ctx);
}

/// Visual state of a row/control (Omarchy paint priority: pressed > focus > hover/cursor >
/// selected > active > idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowState {
    #[default]
    Idle,
    /// Keyboard cursor or mouse hover: fg@0.08 fill, text accent.
    Cursor,
    /// Current selection in a non-focused pane (e.g. the open group): fg@0.18 fill.
    Selected,
    /// Cursor on the selected item.
    CursorSelected,
}

impl RowState {
    /// Cursor (keyboard or hover) is on this row: text turns accent.
    pub fn is_cursor(self) -> bool {
        matches!(self, RowState::Cursor | RowState::CursorSelected)
    }

    /// The row is the pane's current selection.
    pub fn is_selected(self) -> bool {
        matches!(self, RowState::Selected | RowState::CursorSelected)
    }

    /// State for a row from the two flags.
    pub fn from_flags(cursor: bool, selected: bool) -> Self {
        match (cursor, selected) {
            (true, true) => RowState::CursorSelected,
            (true, false) => RowState::Cursor,
            (false, true) => RowState::Selected,
            (false, false) => RowState::Idle,
        }
    }
}
