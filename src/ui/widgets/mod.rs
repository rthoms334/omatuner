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

pub mod badge;
pub mod button;
pub mod card;
pub mod empty_state;
pub mod glyphs;
pub mod list;
pub mod logo;
mod paint;
pub mod poster;
pub mod progress;
pub mod rows;
pub mod section;
pub mod sidebar;
pub mod slider;
pub mod status_bar;
pub mod text_field;
pub mod time;
pub mod toast;
pub mod toggle;

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
