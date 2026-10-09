//! Virtualized, keyboard-first list with launcher-style type-to-filter (DESIGN §13.1).
//! Rows are drawn by the caller; this owns cursor, paging, scroll-to-cursor, hover-moves-cursor
//! (Omarchy menus), click/double-click, and the filter text (drawn by `text_field::search_header`).
//! Uses `ScrollArea::show_rows` so 100k rows cost only the visible ones.

use eframe::egui;

use super::RowState;
use crate::keymap::{Action, KeyContext};
use crate::search::ListFilter;
use crate::theme::Theme;

#[derive(Default)]
pub struct ListState {
    pub cursor: usize,
    pub len: usize,
    pub filter: ListFilter,
    /// Rows visible in the last frame (PageUp/PageDown step).
    pub page: usize,
    scroll_to_cursor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOutcome {
    None,
    Moved,
    /// Enter / Right / double-click on row `usize` (index into the *displayed* rows).
    Activated(usize),
    /// Shift+Enter.
    ActivatedAlt(usize),
    /// Left / Backspace on an empty filter.
    Parent,
    FilterChanged,
    /// Right-click / Shift+F10 / Ctrl+Enter.
    ContextMenu(usize),
}

pub struct RowCtx {
    pub index: usize,
    pub state: RowState,
}

impl ListState {
    /// Update the row count (after filtering); clamps the cursor.
    pub fn set_len(&mut self, len: usize) { todo!("WP-10a") }
    /// Move the cursor programmatically and scroll it into view.
    pub fn select(&mut self, index: usize) { todo!("WP-10a") }
    /// `List` or `ListFiltering` depending on the filter.
    pub fn key_context(&self) -> KeyContext {
        if self.filter.is_active() { KeyContext::ListFiltering } else { KeyContext::List }
    }
    /// Apply a key action (navigation + filter editing). Unrelated actions → `None`.
    pub fn handle(&mut self, action: Action) -> ListOutcome { todo!("WP-10a") }
}

/// Draw the visible rows. `row` paints one row inside the given rect-sized `Ui` and returns its
/// response (used for hover/click). Row background/cursor painting is done here from `RowState`.
pub fn virtual_list(
    ui: &mut egui::Ui,
    theme: &Theme,
    id_salt: impl std::hash::Hash,
    state: &mut ListState,
    row_height: f32,
    row: impl FnMut(&mut egui::Ui, RowCtx) -> egui::Response,
) -> ListOutcome {
    todo!("WP-10a")
}

/// Paint the list-cursor/selection background for a row rect (shared with grids).
pub fn paint_row_state(painter: &egui::Painter, rect: egui::Rect, theme: &Theme, state: RowState) { todo!("WP-10a") }
