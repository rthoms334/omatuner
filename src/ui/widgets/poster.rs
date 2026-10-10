//! Poster cards and the 2-D virtualized poster grid (Movies / Series / Home rails).
//! Owner: WP-12 (VOD screens are its main user).
//!
//! Card: 2:3 poster (Poster image kind), title below (2 lines max, body), year + rating
//! (body-small @0.52), progress bar along the bottom edge (accent) when partially watched,
//! lock glyph when locked. Cursor = 2 px accent outline (posters have no row background).
//! The item that is playing carries the accent 󰐊 now-playing marker left of its title
//! (DESIGN §13.0, §13.1 "Now playing marker").
//!
//! Keyboard: [`GridState::handle`] turns `PosterGrid` actions into cursor moves (↑/↓ by a row,
//! ←/→ by one card, ← on the first column = back to the categories pane) and filter edits —
//! the launcher idiom of `list::ListState`, in two dimensions. Mouse: hover moves the cursor
//! only on real pointer motion (Omarchy pointer gate), click selects, double-click activates,
//! right-click opens the context menu.

use eframe::egui::{self, Align2, Rect};

use super::list::ListOutcome;
use super::{glyphs, paint};
use crate::images::{ImageCache, ImageKind, ImageState};
use crate::keymap::{Action, KeyContext};
use crate::model::VodSummary;
use crate::search::ListFilter;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};

/// Minimum card width (pt × rem) in Medium / Wide layouts (DESIGN §13.5).
const MIN_CARD_MEDIUM: f32 = 132.0;
const MIN_CARD_WIDE: f32 = 150.0;
/// Available grid width (pt × rem) from which cards use the Wide minimum.
const WIDE_GRID_FROM: f32 = 900.0;
/// Rail card width (pt × rem).
const RAIL_CARD: f32 = 132.0;
/// PageUp/PageDown step (rows) before the first frame measured the viewport.
const DEFAULT_PAGE_ROWS: usize = 3;

#[derive(Default)]
pub struct GridState {
    pub cursor: usize,
    pub len: usize,
    pub columns: usize,
    pub filter: crate::search::ListFilter,
    scroll_to_cursor: bool,
    /// Whole rows visible in the last frame (PageUp/PageDown step).
    rows_visible: usize,
    /// Scroll offset / viewport height of the last frame (scroll-to-cursor math).
    offset: f32,
    viewport: f32,
}

impl GridState {
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
            self.scroll_to_cursor = true;
        }
    }

    /// Apply a `PosterGrid(Filtering)` action. Moves past an edge are `None` (no wrap), except
    /// ← on the first column which reports `Parent` (back to the categories pane) like
    /// Backspace on an empty filter.
    pub fn handle(&mut self, action: Action) -> ListOutcome {
        let cols = self.columns.max(1);
        let last = self.len.saturating_sub(1);
        let page = if self.rows_visible == 0 { DEFAULT_PAGE_ROWS } else { self.rows_visible } * cols;
        match action {
            Action::Up if self.cursor >= cols => self.move_to(self.cursor - cols),
            Action::Up => ListOutcome::None,
            Action::Down => {
                let row = self.cursor / cols;
                let last_row = last / cols;
                if self.len == 0 || row >= last_row {
                    ListOutcome::None
                } else {
                    // the last row may be partial: land on its last card
                    self.move_to((self.cursor + cols).min(last))
                }
            }
            Action::Left if self.len > 0 && self.cursor.is_multiple_of(cols) => {
                if self.filter.is_active() { ListOutcome::None } else { ListOutcome::Parent }
            }
            Action::Left => self.move_to(self.cursor.saturating_sub(1)),
            Action::Right if self.cursor + 1 < self.len && !(self.cursor + 1).is_multiple_of(cols) => self.move_to(self.cursor + 1),
            Action::Right => ListOutcome::None,
            Action::PageUp => self.move_to(self.cursor.saturating_sub(page) / cols * cols + self.cursor % cols),
            Action::PageDown => {
                let target = (self.cursor + page).min(last);
                self.move_to(target)
            }
            Action::Home => self.move_to(0),
            Action::End => self.move_to(last),
            Action::Activate if self.len > 0 => ListOutcome::Activated(self.cursor),
            Action::ActivateAlt if self.len > 0 => ListOutcome::ActivatedAlt(self.cursor),
            Action::ContextMenu if self.len > 0 => ListOutcome::ContextMenu(self.cursor),
            Action::Parent if !self.filter.is_active() => ListOutcome::Parent,
            Action::FilterInput(c) => self.edit_filter(|f| f.push(c)),
            Action::FilterBackspace => self.edit_filter(ListFilter::backspace),
            Action::FilterDeleteWord => self.edit_filter(ListFilter::delete_word),
            Action::FilterClear => self.edit_filter(ListFilter::clear),
            _ => ListOutcome::None,
        }
    }

    /// `PosterGrid` or `PosterGridFiltering` depending on the filter.
    pub(crate) fn key_context(&self) -> KeyContext {
        if self.filter.is_active() { KeyContext::PosterGridFiltering } else { KeyContext::PosterGrid }
    }

    /// Move the cursor programmatically and scroll it into view.
    pub(crate) fn select(&mut self, index: usize) {
        self.cursor = index.min(self.len.saturating_sub(1));
        self.scroll_to_cursor = true;
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

    /// Scroll offset that brings the cursor's row fully into view (`None` = keep).
    fn scroll_target(&self, row_h: f32) -> Option<f32> {
        if !self.scroll_to_cursor || self.viewport <= 0.0 {
            return None;
        }
        let top = (self.cursor / self.columns.max(1)) as f32 * row_h;
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

/// Per-card decorations a screen knows about (the frozen `poster_card` has none).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct TileDecor {
    /// Watch progress 0..1 (shown while partially watched).
    pub progress: Option<f32>,
    /// Now-playing marker.
    pub playing: bool,
    /// Finished: a check glyph on the poster.
    pub watched: bool,
}

/// Card height for a card `width` (poster 2:3 + two title lines + meta line).
pub(crate) fn card_height(theme: &Theme, width: f32) -> f32 {
    let text = 2.0 * theme.font(TypeToken::Body) * 1.25 + theme.font(TypeToken::BodySmall) * 1.25;
    (width * 1.5 + theme.space(SpaceToken::Md) + text + theme.space(SpaceToken::Xs)).ceil()
}

/// Columns and card width for an available grid width.
pub(crate) fn grid_metrics(theme: &Theme, avail: f32) -> (usize, f32, f32) {
    let rem = theme.rem().max(0.5);
    let gap = theme.space(SpaceToken::Xxl);
    let min_w = if avail >= WIDE_GRID_FROM * rem { MIN_CARD_WIDE } else { MIN_CARD_MEDIUM } * rem;
    let cols = (((avail + gap) / (min_w + gap)).floor() as usize).max(1);
    let card_w = ((avail - gap * (cols as f32 - 1.0)) / cols as f32).max(40.0);
    (cols, card_w, gap)
}

/// "1999 · ★ 8.7".
fn meta_line(item: &VodSummary) -> String {
    let mut parts = Vec::with_capacity(2);
    if let Some(y) = item.year {
        parts.push(y.to_string());
    }
    if let Some(r) = item.rating.filter(|r| *r > 0.0) {
        parts.push(format!("★ {r:.1}"));
    }
    parts.join(" · ")
}

/// Paint a poster image filling `rect` (cropped to its aspect), or the placeholder.
pub(crate) fn paint_poster_image(ui: &egui::Ui, theme: &Theme, images: &mut ImageCache, url: Option<&str>, name: &str, rect: Rect, kind: ImageKind) {
    let t = &theme.tokens;
    let p = ui.painter();
    match url.filter(|u| !u.trim().is_empty()).map(|u| images.get(u, kind)) {
        Some(ImageState::Ready(tex)) => {
            // cover: crop the texture to the rect's aspect
            let s = tex.size_vec2();
            let (tw, th) = (s.x.max(1.0), s.y.max(1.0));
            let want = rect.width() / rect.height().max(1.0);
            let have = tw / th;
            let uv = if have > want {
                let w = want / have;
                Rect::from_min_max(egui::pos2((1.0 - w) / 2.0, 0.0), egui::pos2((1.0 + w) / 2.0, 1.0))
            } else {
                let h = have / want;
                Rect::from_min_max(egui::pos2(0.0, (1.0 - h) / 2.0), egui::pos2(1.0, (1.0 + h) / 2.0))
            };
            p.rect_filled(rect, t.rounding, t.fill_field);
            p.image(tex.id(), rect, uv, egui::Color32::WHITE);
        }
        state => {
            p.rect_filled(rect, t.rounding, t.fill_field);
            if !matches!(state, Some(ImageState::Loading)) {
                let font = egui::FontId::proportional((rect.width() * 0.22).round().clamp(10.0, 40.0));
                paint::text(p, rect.center(), Align2::CENTER_CENTER, super::logo::monogram(name), font, t.text_secondary, rect.width());
            }
        }
    }
}

/// The card body shared by grids, rails and the frozen [`poster_card`].
pub(crate) fn poster_tile(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    item: &VodSummary,
    width: f32,
    cursor: bool,
    decor: TileDecor,
) -> egui::Response {
    let t = &theme.tokens;
    let width = width.max(40.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, card_height(theme, width)), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &*item.name));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let poster = Rect::from_min_size(rect.min, egui::vec2(width, (width * 1.5).round()));
    paint_poster_image(ui, theme, images, item.poster.as_deref(), &item.name, poster, ImageKind::Poster);
    let p = ui.painter();
    let corner = |p: &egui::Painter, glyph: &str, color: egui::Color32, slot: f32| {
        let size = theme.font(TypeToken::Icon) + 2.0 * theme.space(SpaceToken::Xs);
        let r = Rect::from_min_size(egui::pos2(poster.right() - size * (slot + 1.0) - theme.space(SpaceToken::Xs) * slot, poster.top()), egui::vec2(size, size));
        p.rect_filled(r, t.rounding, with_alpha(t.bg, 0.85));
        p.text(r.center(), Align2::CENTER_CENTER, glyph, paint::font(theme, TypeToken::Icon), color);
    };
    let mut slot = 0.0;
    if item.locked {
        corner(p, glyphs::LOCK, t.fg, slot);
        slot += 1.0;
    }
    if decor.watched {
        corner(p, glyphs::CHECK, t.accent, slot);
    }
    if let Some(f) = decor.progress.filter(|f| *f > 0.0 && *f < 1.0 && !decor.watched) {
        let h = (3.0 * theme.rem()).round().max(2.0);
        let bar = Rect::from_min_max(egui::pos2(poster.left(), poster.bottom() - h), poster.right_bottom());
        super::progress::paint_progress(p, bar, theme, f, true);
    }
    if cursor || resp.hovered() {
        let stroke = if cursor { egui::Stroke::new(2.0, t.accent) } else { egui::Stroke::new(1.0, t.border_hover) };
        p.rect_stroke(poster, t.rounding, stroke, egui::StrokeKind::Outside);
    }

    // title (≤ 2 lines, now-playing marker first), then the meta line
    let mut y = poster.bottom() + theme.space(SpaceToken::Md);
    let mut left = rect.left();
    let title_color = if cursor { t.list_cursor_text } else { t.fg };
    if decor.playing {
        let r = paint::text(p, egui::pos2(left, y), Align2::LEFT_TOP, glyphs::PLAY, paint::font(theme, TypeToken::Body), t.accent, f32::INFINITY);
        left = r.right() + theme.space(SpaceToken::Sm);
    }
    let mut job = egui::text::LayoutJob::simple(item.name.to_string(), paint::font(theme, TypeToken::Body), title_color, (rect.right() - left).max(1.0));
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    let g = p.layout_job(job);
    let h = g.size().y;
    p.galley(egui::pos2(left, y), g, title_color);
    y += h + theme.space(SpaceToken::Xxs);
    let meta = meta_line(item);
    if !meta.is_empty() {
        paint::text(p, egui::pos2(rect.left(), y), Align2::LEFT_TOP, meta, paint::font(theme, TypeToken::BodySmall), t.text_secondary, rect.width());
    }
    resp
}

pub fn poster_card(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    item: &VodSummary,
    width: f32,
    cursor: bool,
    progress: Option<f32>,
) -> egui::Response {
    poster_tile(ui, theme, images, item, width, cursor, TileDecor { progress, ..Default::default() })
}

/// Grid of `items` (indices into `all`), column count from available width (min card 132 pt
/// Medium, 150 pt Wide), rows virtualized with `show_rows`.
pub fn poster_grid(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    state: &mut GridState,
    all: &[VodSummary],
    items: &[u32],
) -> ListOutcome {
    poster_grid_with(ui, theme, images, state, all, items, |_| TileDecor::default())
}

/// [`poster_grid`] with per-card decorations (progress, now playing, watched).
pub(crate) fn poster_grid_with(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    state: &mut GridState,
    all: &[VodSummary],
    items: &[u32],
    decor: impl Fn(&VodSummary) -> TileDecor,
) -> ListOutcome {
    state.set_len(items.len());
    let (cols, card_w, gap) = grid_metrics(theme, ui.available_width());
    if cols != state.columns {
        state.columns = cols;
        state.scroll_to_cursor = true;
    }
    let row_h = card_height(theme, card_w) + gap;
    let rows = items.len().div_ceil(cols);
    let pointer_moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
    let mut area = egui::ScrollArea::vertical().id_salt("poster-grid").auto_shrink([false, false]);
    if let Some(off) = state.scroll_target(row_h) {
        area = area.vertical_scroll_offset(off);
    }
    if state.viewport > 0.0 {
        state.scroll_to_cursor = false;
    }
    let mut outcome = ListOutcome::None;
    let cursor = state.cursor;
    let out = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            area.show_rows(ui, row_h, rows, |ui, range| {
                for row in range {
                    let (row_rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
                    for col in 0..cols {
                        let i = row * cols + col;
                        let Some(&idx) = items.get(i) else { break };
                        let Some(item) = all.get(idx as usize) else { continue };
                        let x = row_rect.left() + col as f32 * (card_w + gap);
                        let cell = Rect::from_min_size(egui::pos2(x, row_rect.top()), egui::vec2(card_w, row_h - gap));
                        let mut child = ui.new_child(egui::UiBuilder::new().id_salt(("poster", i)).max_rect(cell).layout(egui::Layout::top_down(egui::Align::Min)));
                        let resp = poster_tile(&mut child, theme, images, item, card_w, i == cursor, decor(item));
                        if resp.double_clicked() {
                            state.cursor = i;
                            outcome = ListOutcome::Activated(i);
                        } else if resp.secondary_clicked() {
                            state.cursor = i;
                            outcome = ListOutcome::ContextMenu(i);
                        } else if (resp.clicked() || (pointer_moved && resp.hovered())) && i != state.cursor {
                            state.cursor = i;
                            outcome = ListOutcome::Moved;
                        }
                    }
                }
            })
        })
        .inner;
    state.offset = out.state.offset.y;
    state.viewport = out.inner_rect.height();
    state.rows_visible = ((state.viewport / row_h).floor() as usize).max(1);
    outcome
}

/// Horizontal rail (Home sections): ←/→ move, Enter opens.
pub fn poster_rail(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    id_salt: impl std::hash::Hash,
    all: &[VodSummary],
    cursor: Option<usize>,
) -> Option<usize> {
    let salt = {
        use std::hash::Hasher;
        let mut h = std::hash::DefaultHasher::new();
        id_salt.hash(&mut h);
        h.finish()
    };
    let card_w = (RAIL_CARD * theme.rem()).round();
    let gap = theme.space(SpaceToken::Xxl);
    let mut clicked = None;
    egui::ScrollArea::horizontal().id_salt(("poster-rail", salt)).auto_shrink([false, true]).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (i, item) in all.iter().enumerate() {
                let is_cursor = cursor == Some(i);
                let resp = poster_tile(ui, theme, images, item, card_w, is_cursor, TileDecor::default());
                if is_cursor {
                    scroll_into_view_once(ui, ("poster-rail-cursor", salt), i, resp.rect);
                }
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
        });
    });
    clicked
}

/// Scroll `rect` into view when the cursor `index` changed since the last frame (so a rail
/// the user scrolls by hand isn't pulled back every frame).
pub(crate) fn scroll_into_view_once(ui: &mut egui::Ui, key: impl std::hash::Hash, index: usize, rect: Rect) {
    let id = {
        use std::hash::Hasher;
        let mut h = std::hash::DefaultHasher::new();
        key.hash(&mut h);
        egui::Id::new(("scroll-into-view", h.finish()))
    };
    let last: Option<usize> = ui.data(|d| d.get_temp(id));
    if last != Some(index) {
        ui.data_mut(|d| d.insert_temp(id, index));
        ui.scroll_to_rect(rect, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(len: usize, cols: usize) -> GridState {
        let mut g = GridState { columns: cols, ..Default::default() };
        g.set_len(len);
        g
    }

    #[test]
    fn arrows_move_in_two_dimensions_without_wrapping() {
        let mut g = grid(10, 4); // rows: 0-3, 4-7, 8-9
        assert_eq!(g.handle(Action::Up), ListOutcome::None);
        assert_eq!(g.handle(Action::Right), ListOutcome::Moved);
        assert_eq!(g.cursor, 1);
        assert_eq!(g.handle(Action::Down), ListOutcome::Moved);
        assert_eq!(g.cursor, 5);
        assert_eq!(g.handle(Action::Down), ListOutcome::Moved, "partial last row: land on its last card");
        assert_eq!(g.cursor, 9);
        assert_eq!(g.handle(Action::Down), ListOutcome::None);
        assert_eq!(g.handle(Action::Right), ListOutcome::None, "no wrap past the end");
        assert_eq!(g.handle(Action::Up), ListOutcome::Moved);
        assert_eq!(g.cursor, 5);
        g.select(3);
        assert_eq!(g.handle(Action::Right), ListOutcome::None, "no wrap to the next row");
        g.select(4);
        assert_eq!(g.handle(Action::Left), ListOutcome::Parent, "first column → categories pane");
        assert_eq!(g.handle(Action::End), ListOutcome::Moved);
        assert_eq!(g.cursor, 9);
        assert_eq!(g.handle(Action::Home), ListOutcome::Moved);
        assert_eq!(g.cursor, 0);
    }

    #[test]
    fn paging_moves_whole_rows_and_activation_reports_the_cursor() {
        let mut g = grid(100, 5);
        g.rows_visible = 2;
        assert_eq!(g.handle(Action::PageDown), ListOutcome::Moved);
        assert_eq!(g.cursor, 10);
        g.select(12);
        assert_eq!(g.handle(Action::PageUp), ListOutcome::Moved);
        assert_eq!(g.cursor, 2, "same column one page up");
        assert_eq!(g.handle(Action::Activate), ListOutcome::Activated(2));
        assert_eq!(g.handle(Action::ActivateAlt), ListOutcome::ActivatedAlt(2));
        assert_eq!(g.handle(Action::ContextMenu), ListOutcome::ContextMenu(2));
        let mut empty = grid(0, 5);
        for a in [Action::Down, Action::Right, Action::End, Action::Activate, Action::ContextMenu] {
            assert_eq!(empty.handle(a), ListOutcome::None, "{a:?}");
        }
    }

    #[test]
    fn typing_filters_and_backspace_steps_out_only_when_empty() {
        let mut g = grid(30, 3);
        g.select(7);
        assert_eq!(g.key_context(), KeyContext::PosterGrid);
        assert_eq!(g.handle(Action::FilterInput(' ')), ListOutcome::None, "leading space ignored");
        assert_eq!(g.handle(Action::FilterInput('m')), ListOutcome::FilterChanged);
        assert_eq!(g.cursor, 0);
        assert_eq!(g.key_context(), KeyContext::PosterGridFiltering);
        assert_eq!(g.handle(Action::Parent), ListOutcome::None);
        assert_eq!(g.handle(Action::Left), ListOutcome::None, "← at the edge keeps the filter");
        assert_eq!(g.handle(Action::FilterBackspace), ListOutcome::FilterChanged);
        assert_eq!(g.handle(Action::Parent), ListOutcome::Parent);
        g.set_len(2);
        g.select(10);
        assert_eq!(g.cursor, 1);
    }

    #[test]
    fn metrics_follow_the_minimum_card_width() {
        let th = Theme::fallback();
        let (cols, w, gap) = grid_metrics(&th, 600.0);
        assert!(w >= MIN_CARD_MEDIUM && cols >= 3, "{cols} × {w}");
        assert!((cols as f32 * w + (cols as f32 - 1.0) * gap - 600.0).abs() < 1.0);
        let (wide_cols, wide_w, _) = grid_metrics(&th, 1400.0);
        assert!(wide_w >= MIN_CARD_WIDE && (7..=9).contains(&wide_cols), "{wide_cols} × {wide_w}");
        assert_eq!(grid_metrics(&th, 50.0).0, 1);
        assert!(card_height(&th, 150.0) > 225.0);
    }

    #[test]
    fn meta_lines() {
        let mut v = VodSummary {
            kind: crate::model::VodKind::Movie(crate::model::MovieId(1)),
            source_id: crate::model::SourceId(1),
            key: crate::model::ItemKey("k".into()),
            name: "The Matrix".into(),
            year: Some(1999),
            poster: None,
            rating: Some(8.7),
            group_ids: vec![],
            added_at: None,
            provider_order: 0,
            locked: false,
        };
        assert_eq!(meta_line(&v), "1999 · ★ 8.7");
        v.rating = Some(0.0);
        assert_eq!(meta_line(&v), "1999");
        v.year = None;
        assert_eq!(meta_line(&v), "");
    }
}
