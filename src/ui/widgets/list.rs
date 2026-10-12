//! Virtualized, keyboard-first list with launcher-style type-to-filter (DESIGN §13.1).
//! Rows are drawn by the caller; this owns cursor, paging, scroll-to-cursor, hover-moves-cursor
//! (Omarchy menus), clicks, and the filter text (drawn by `text_field::search_header`).
//! Uses `ScrollArea::show_rows` so 100k rows cost only the visible ones.
//!
//! Usage (the screen owns the filtered rows; the list only knows their count):
//! ```ignore
//! match state.handle(action) {
//!     ListOutcome::FilterChanged => { rows = state.filter.apply(names()); state.set_len(rows.len()) }
//!     ListOutcome::Activated(i) => play(rows[i]),
//!     _ => {}
//! }
//! let h = rows::row_height(theme, density, true);
//! virtual_list(ui, theme, "channels", &mut state, h, |ui, rc| rows::channel_row(ui, …, rc.state));
//! ```
//! Mouse rules (Omarchy `Menu.qml` pointer gate): hover moves the cursor only on frames with
//! real pointer motion — never because rows scroll under a still pointer — and never activates;
//! a click does what Enter does (cursor there + `Activated`), right-click = context menu. The
//! later clicks of a double/triple-click are ignored ([`super::fresh_click`]): a double-click
//! activates once (DESIGN §15.1 "Double-click safety").

use eframe::egui;

use super::RowState;
use crate::keymap::{Action, KeyContext};
use crate::search::ListFilter;
use crate::theme::Theme;

/// PageUp/PageDown step before the first frame told us how many rows fit.
const DEFAULT_PAGE: usize = 10;

#[derive(Default)]
pub struct ListState {
    pub cursor: usize,
    pub len: usize,
    pub filter: ListFilter,
    /// Rows visible in the last frame (PageUp/PageDown step).
    pub page: usize,
    scroll_to_cursor: bool,
    /// Scroll offset and viewport height of the last frame (scroll-to-cursor math).
    offset: f32,
    viewport: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOutcome {
    None,
    Moved,
    /// Enter / Right / click on row `usize` (index into the *displayed* rows).
    Activated(usize),
    /// Shift+Enter.
    ActivatedAlt(usize),
    /// ← (with or without a filter; the filter is kept) or Backspace on an empty filter: the
    /// pane on the left (DESIGN §15.1 "Spatial panes").
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
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
            self.scroll_to_cursor = true;
        }
    }
    /// Move the cursor programmatically and scroll it into view.
    pub fn select(&mut self, index: usize) {
        self.cursor = index.min(self.len.saturating_sub(1));
        self.scroll_to_cursor = true;
    }
    /// `List` or `ListFiltering` depending on the filter.
    pub fn key_context(&self) -> KeyContext {
        if self.filter.is_active() { KeyContext::ListFiltering } else { KeyContext::List }
    }
    /// Apply a key action (navigation + filter editing). Unrelated actions → `None`.
    ///
    /// Moves at either end are `None` (no wrap-around), as are activations of an empty list and
    /// filter edits that change nothing (a leading space). Any filter change puts the cursor on
    /// the first (best) row; the caller re-filters and calls [`set_len`](Self::set_len).
    pub fn handle(&mut self, action: Action) -> ListOutcome {
        let last = self.len.saturating_sub(1);
        let page = if self.page == 0 { DEFAULT_PAGE } else { self.page };
        match action {
            Action::Up => self.move_to(self.cursor.saturating_sub(1)),
            Action::Down => self.move_to((self.cursor + 1).min(last)),
            Action::PageUp => self.move_to(self.cursor.saturating_sub(page)),
            Action::PageDown => self.move_to((self.cursor + page).min(last)),
            Action::Home => self.move_to(0),
            Action::End => self.move_to(last),
            Action::Activate if self.len > 0 => ListOutcome::Activated(self.cursor),
            Action::ActivateAlt if self.len > 0 => ListOutcome::ActivatedAlt(self.cursor),
            Action::ContextMenu if self.len > 0 => ListOutcome::ContextMenu(self.cursor),
            // `Parent` (← / Backspace in `List`) only on an empty filter: the stack is resolved
            // once per frame, so a typed "a" + Backspace in one frame must not also step out;
            // ← while filtering arrives as `Left` and steps out (keeping the filter)
            Action::Parent if !self.filter.is_active() => ListOutcome::Parent,
            Action::Left => ListOutcome::Parent,
            Action::FilterInput(c) => self.edit_filter(|f| f.push(c)),
            Action::FilterBackspace => self.edit_filter(ListFilter::backspace),
            Action::FilterDeleteWord => self.edit_filter(ListFilter::delete_word),
            Action::FilterClear => self.edit_filter(ListFilter::clear),
            _ => ListOutcome::None,
        }
    }

    fn move_to(&mut self, index: usize) -> ListOutcome {
        if self.len == 0 || index == self.cursor {
            return ListOutcome::None;
        }
        self.cursor = index;
        self.scroll_to_cursor = true;
        ListOutcome::Moved
    }

    fn edit_filter(&mut self, edit: impl FnOnce(&mut ListFilter)) -> ListOutcome {
        let before = self.filter.query().to_owned();
        edit(&mut self.filter);
        if self.filter.query() == before {
            return ListOutcome::None;
        }
        self.cursor = 0;
        self.scroll_to_cursor = true;
        ListOutcome::FilterChanged
    }

    /// Scroll offset to apply this frame so the cursor row is fully visible (`None` = keep).
    fn scroll_target(&self, row_h: f32) -> Option<f32> {
        if !self.scroll_to_cursor || self.viewport <= 0.0 {
            return None;
        }
        let top = self.cursor as f32 * row_h;
        let bottom = top + row_h;
        if top < self.offset {
            Some(top)
        } else if bottom > self.offset + self.viewport {
            Some((bottom - self.viewport).max(0.0))
        } else {
            None
        }
    }
}

/// Draw the visible rows. `row` paints one row inside a `Ui` sized to the row rect and returns
/// its response (used for hover/click). The row renderer paints the background from
/// `RowCtx::state` (the `rows::*` widgets do; custom rows call [`paint_row_state`]). The list
/// only reports `Cursor`/`Idle`; a screen showing an unfocused pane maps the state itself (e.g.
/// `RowState::from_flags(false, rc.index == open_group)`).
pub fn virtual_list(
    ui: &mut egui::Ui,
    _theme: &Theme,
    id_salt: impl std::hash::Hash,
    state: &mut ListState,
    row_height: f32,
    mut row: impl FnMut(&mut egui::Ui, RowCtx) -> egui::Response,
) -> ListOutcome {
    let row_h = row_height.max(1.0);
    let len = state.len;
    // Omarchy pointer gate: only real pointer motion moves the cursor by hovering.
    let pointer_moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
    // egui ids want `Debug + Hash`; the frozen signature only promises `Hash`
    let salt = {
        use std::hash::Hasher;
        let mut h = std::hash::DefaultHasher::new();
        id_salt.hash(&mut h);
        h.finish()
    };
    let mut area = egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]);
    if let Some(off) = state.scroll_target(row_h) {
        area = area.vertical_scroll_offset(off);
    }
    if state.viewport > 0.0 {
        state.scroll_to_cursor = false;
    }
    let mut outcome = ListOutcome::None;
    let out = ui
        .scope(|ui| {
            // show_rows assumes uniform rows: no spacing between them
            ui.spacing_mut().item_spacing.y = 0.0;
            area.show_rows(ui, row_h, len, |ui, range| {
                for index in range {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
                    let row_state = if index == state.cursor { RowState::Cursor } else { RowState::Idle };
                    let mut child =
                        ui.new_child(egui::UiBuilder::new().id_salt(index).max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
                    let resp = row(&mut child, RowCtx { index, state: row_state }).on_hover_cursor(egui::CursorIcon::PointingHand);
                    if super::fresh_click(&resp) {
                        state.cursor = index;
                        outcome = ListOutcome::Activated(index);
                    } else if resp.secondary_clicked() {
                        state.cursor = index;
                        outcome = ListOutcome::ContextMenu(index);
                    } else if resp.clicked() {
                        // the 2nd/3rd click of a multi-click: already activated
                    } else if pointer_moved && resp.hovered() && index != state.cursor {
                        state.cursor = index;
                        outcome = ListOutcome::Moved;
                    }
                }
            })
        })
        .inner;
    state.offset = out.state.offset.y;
    state.viewport = out.inner_rect.height();
    state.page = ((state.viewport / row_h).floor() as usize).max(1);
    outcome
}

/// Paint the list-cursor/selection background for a row rect (shared with grids).
pub fn paint_row_state(painter: &egui::Painter, rect: egui::Rect, theme: &Theme, state: RowState) {
    let t = &theme.tokens;
    let fill = match state {
        RowState::Idle => return,
        RowState::Cursor => t.list_cursor_bg,
        RowState::Selected | RowState::CursorSelected => t.fill_selected,
    };
    painter.rect_filled(rect, t.rounding, fill);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(len: usize) -> ListState {
        let mut s = ListState::default();
        s.set_len(len);
        s
    }

    #[test]
    fn moves_clamp_without_wrapping() {
        let mut s = list(3);
        assert_eq!(s.handle(Action::Up), ListOutcome::None);
        assert_eq!(s.handle(Action::Down), ListOutcome::Moved);
        assert_eq!(s.handle(Action::End), ListOutcome::Moved);
        assert_eq!(s.cursor, 2);
        assert_eq!(s.handle(Action::Down), ListOutcome::None);
        assert_eq!(s.handle(Action::Home), ListOutcome::Moved);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn paging_uses_the_last_frame_or_a_default() {
        let mut s = list(100);
        assert_eq!(s.handle(Action::PageDown), ListOutcome::Moved);
        assert_eq!(s.cursor, DEFAULT_PAGE);
        s.page = 7;
        s.handle(Action::PageDown);
        assert_eq!(s.cursor, DEFAULT_PAGE + 7);
        s.handle(Action::PageUp);
        s.handle(Action::PageUp);
        s.handle(Action::PageUp);
        assert_eq!(s.cursor, 0);
        s.select(98);
        s.handle(Action::PageDown);
        assert_eq!(s.cursor, 99);
    }

    #[test]
    fn empty_list_does_nothing() {
        let mut s = list(0);
        for a in [Action::Down, Action::End, Action::Activate, Action::ActivateAlt, Action::ContextMenu] {
            assert_eq!(s.handle(a), ListOutcome::None, "{a:?}");
        }
        assert_eq!(s.handle(Action::Parent), ListOutcome::Parent);
        assert_eq!(s.handle(Action::Left), ListOutcome::Parent);
    }

    #[test]
    fn filter_edits_reset_the_cursor_and_report_changes() {
        let mut s = list(50);
        s.select(20);
        assert_eq!(s.key_context(), KeyContext::List);
        // a leading space changes nothing
        assert_eq!(s.handle(Action::FilterInput(' ')), ListOutcome::None);
        assert_eq!(s.handle(Action::FilterInput('b')), ListOutcome::FilterChanged);
        assert_eq!(s.cursor, 0);
        assert_eq!(s.key_context(), KeyContext::ListFiltering);
        assert_eq!(s.handle(Action::FilterInput(' ')), ListOutcome::FilterChanged);
        assert_eq!(s.handle(Action::FilterInput('c')), ListOutcome::FilterChanged);
        assert_eq!(s.filter.query(), "b c");
        // Parent (Backspace's) only on an empty filter; ← (`Left` while filtering) steps out
        // and keeps the filter and the cursor
        assert_eq!(s.handle(Action::Parent), ListOutcome::None);
        s.select(1);
        assert_eq!(s.handle(Action::Left), ListOutcome::Parent);
        assert_eq!((s.filter.query(), s.cursor), ("b c", 1));
        assert_eq!(s.handle(Action::FilterDeleteWord), ListOutcome::FilterChanged);
        assert_eq!(s.filter.query(), "b ");
        assert_eq!(s.handle(Action::FilterClear), ListOutcome::FilterChanged);
        assert_eq!(s.handle(Action::FilterClear), ListOutcome::None);
        assert_eq!(s.handle(Action::FilterBackspace), ListOutcome::None);
        assert_eq!(s.handle(Action::Parent), ListOutcome::Parent);
    }

    #[test]
    fn set_len_clamps_and_activation_reports_the_cursor() {
        let mut s = list(10);
        s.select(9);
        s.set_len(4);
        assert_eq!(s.cursor, 3);
        assert_eq!(s.handle(Action::Activate), ListOutcome::Activated(3));
        assert_eq!(s.handle(Action::ActivateAlt), ListOutcome::ActivatedAlt(3));
        assert_eq!(s.handle(Action::ContextMenu), ListOutcome::ContextMenu(3));
        assert_eq!(s.handle(Action::Back), ListOutcome::None);
        s.select(1000);
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn scroll_target_keeps_the_cursor_visible() {
        let mut s = list(100);
        s.viewport = 100.0;
        s.offset = 0.0;
        s.select(3);
        assert_eq!(s.scroll_target(50.0), Some(100.0)); // row 3 = 150..200 → offset 100
        s.offset = 100.0;
        s.select(1);
        assert_eq!(s.scroll_target(50.0), Some(50.0));
        s.offset = 50.0;
        s.select(2);
        assert_eq!(s.scroll_target(50.0), None);
    }
}
