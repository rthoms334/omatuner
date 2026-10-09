//! Component catalogue mapped to Omarchy tokens (DESIGN §13.1). Owner: WP-10a (except where a
//! file says otherwise). Every widget takes `&Theme` and paints with `theme.tokens` only —
//! no hard-coded colours except the video OSD scrim.

pub mod badge;
pub mod button;
pub mod card;
pub mod empty_state;
pub mod glyphs;
pub mod list;
pub mod logo;
pub mod poster;
pub mod progress;
pub mod rows;
pub mod section;
pub mod sidebar;
pub mod slider;
pub mod status_bar;
pub mod text_field;
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
