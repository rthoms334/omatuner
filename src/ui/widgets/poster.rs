//! Poster cards and the 2-D virtualized poster grid (Movies / Series / Home rails).
//! Owner: WP-12 (VOD screens are its main user).
//!
//! Card: 2:3 poster (Poster image kind), title below (2 lines max, body), year + rating
//! (body-small @0.52), progress bar along the bottom edge (accent) when partially watched,
//! lock glyph when locked. Cursor = 2 px accent outline (posters have no row background).
//! With several playlists the card says which one it comes from: the playlist's short code
//! ("NW") in its colour in a top-left corner chip (lock / watched chips are top-right; Settings
//! ▸ Interface ▸ Playlist on cards, DESIGN §13.0 "Playlist codes"). The full name is the chip's
//! tooltip, part of the accessible label and, while the card has the cursor or the pointer, in
//! the status bar ("Northwind · 2019 · ★ 7.9"); the meta line stays "year · ★ rating".
//! The item that is playing carries the accent 󰐊 now-playing marker left of its title
//! (DESIGN §13.0, §13.1 "Now playing marker").
//! A **stack card** (copies of one title in "All playlists", DESIGN §29.14) has two offset card
//! edges behind its poster and a copy-count badge in the poster's bottom-right corner (a click
//! on it fans the stack out in the grid, or folds it); its corner shows the code chips of up to
//! three of its playlists, then "+n" ([TV][TV2][NW] +1). A copy shown fanned out names its
//! language and version instead of year and rating ("EN · 4K"). Inside one playlist a title that is
//! also elsewhere gets one edge and "+1" (tooltip "Also in TV"). Locked copies and all-locked
//! stacks are drawn disabled (dimmed, with the lock) while the session is locked.
//!
//! Keyboard: [`GridState::handle`] turns `PosterGrid` actions into cursor moves (↑/↓ by a row,
//! ←/→ by one card, ← on the first column = back to the categories pane) and filter edits —
//! the launcher idiom of `list::ListState`, in two dimensions. Mouse: hover moves the cursor
//! only on real pointer motion (Omarchy pointer gate), a click does what Enter does (a
//! double-click activates once), right-click opens the context menu.

use eframe::egui::{self, Align2, Rect};

use super::badge::CodeChip;
use super::list::ListOutcome;
use super::{glyphs, paint};
use crate::images::{ImageCache, ImageKind, ImageState};
use crate::keymap::{Action, KeyContext};
use crate::model::VodSummary;
use crate::search::ListFilter;
use crate::store::library::SourceMeta;
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
/// Code chips a stack card shows before "+n" (its own playlist's first).
const STACK_CHIPS: usize = 3;

#[derive(Default)]
pub struct GridState {
    /// A click on a stack card's copy-count badge in the last frame (the card's index): the
    /// screen fans the stack out or folds it ([`GridState::take_badge_click`]).
    badge_clicked: Option<usize>,
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
    /// ← on the first column which reports `Parent` (the categories pane on the left, DESIGN
    /// §15.1 "Spatial panes") — also while filtering (the filter is kept) and on an empty grid —
    /// like Backspace on an empty filter.
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
            Action::Left if self.cursor.is_multiple_of(cols) => ListOutcome::Parent,
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

    /// The card whose copy-count badge was clicked in the last frame (once).
    pub(crate) fn take_badge_click(&mut self) -> Option<usize> {
        self.badge_clicked.take()
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

/// A stack card's stack (DESIGN §29.14).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct StackDecor<'a> {
    /// Copies in the stack (the badge).
    pub count: usize,
    /// The stack is fanned out in the grid (the badge folds it).
    pub open: bool,
    /// A click on the badge fans the stack out (grids; Home's rails only show the count).
    pub fan: bool,
    /// The stack's next playlists after the card's own, chipped beside its chip, when cards
    /// show playlists.
    pub next: [Option<&'a SourceMeta>; STACK_CHIPS - 1],
    /// Playlists of the stack besides the card's own (those past the chips are "+n").
    pub others: usize,
}

/// Inside one playlist: the other playlists a title is also in ("+1").
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Elsewhere<'a> {
    pub count: usize,
    /// The first of them, named in the tooltip (`None` while cards don't name playlists).
    pub first: Option<&'a SourceMeta>,
}

/// Per-card decorations a screen knows about (the frozen `poster_card` has none).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct TileDecor<'a> {
    /// Watch progress 0..1 (shown while partially watched).
    pub progress: Option<f32>,
    /// Now-playing marker.
    pub playing: bool,
    /// Finished: a check glyph on the poster.
    pub watched: bool,
    /// The current card of a pane without keyboard focus ("selected": 1 px outline, no
    /// accent — only the focused pane shows the cursor, DESIGN §13.0).
    pub selected: bool,
    /// The playlist the item comes from, when cards show it (`UiCtx::source_of`).
    pub source: Option<&'a SourceMeta>,
    /// The card stands for a stack of copies.
    pub stack: Option<StackDecor<'a>>,
    /// A copy shown in a fanned-out stack: its language and version ("EN · 4K"), on the meta
    /// line instead of year and rating.
    pub copy: Option<&'a str>,
    /// Inside one playlist: the title is also in other playlists.
    pub elsewhere: Option<Elsewhere<'a>>,
    /// Drawn disabled: a locked copy, or a stack whose every copy is locked, while the session
    /// is locked.
    pub dimmed: bool,
}

/// The playlists a card names in full (accessible label, status bar): "Northwind", "TV + TV 2",
/// "TV + TV 2 + Aurora +1"; `None` while cards don't show playlists.
fn playlist_names(decor: &TileDecor<'_>) -> Option<String> {
    let mut out = decor.source?.name.clone();
    if let Some(st) = decor.stack.filter(|s| !s.open) {
        let next: Vec<&SourceMeta> = st.next.iter().flatten().copied().collect();
        for s in &next {
            out = format!("{out} + {}", s.name);
        }
        if st.others > next.len() {
            out = format!("{out} +{}", st.others - next.len());
        }
    }
    Some(out)
}

/// The accessible label of a card: "Dune · TV + TV 2 · 3 copies", "Dune · TV 2 · EN · 4K".
pub(crate) fn tile_label(item: &VodSummary, decor: &TileDecor<'_>) -> String {
    let mut label = match playlist_names(decor) {
        Some(names) => format!("{} · {names}", item.name),
        None => item.name.to_string(),
    };
    if let Some(s) = decor.stack {
        label = format!("{label} · {} copies", s.count);
    }
    if let Some(c) = decor.copy {
        label = format!("{label} · {c}");
    }
    if let Some(e) = decor.elsewhere {
        label = format!("{label} · +{}", e.count);
    }
    if decor.dimmed {
        label = format!("{label} · locked");
    }
    label
}

/// Card height for a card `width` (poster 2:3 + two title lines + meta line).
pub(crate) fn card_height(theme: &Theme, width: f32) -> f32 {
    let text = 2.0 * theme.font(TypeToken::Body) * 1.25 + theme.font(TypeToken::BodySmall) * 1.25;
    (width * 1.5 + theme.space(SpaceToken::Md) + text + theme.space(SpaceToken::Xs)).ceil()
}

/// Columns and card width for an available grid width.
pub(crate) fn grid_metrics(theme: &Theme, avail: f32) -> (usize, f32, f32) {
    let rem = theme.rem().max(0.5);
    // Lg keeps DESIGN §12.1's density: 4–5 columns at a tiled half, 7+ at full screen
    let gap = theme.space(SpaceToken::Lg);
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
    decor: TileDecor<'_>,
) -> egui::Response {
    poster_tile_ex(ui, theme, images, item, width, cursor, decor).0
}

/// [`poster_tile`], and whether its stack's copy-count badge was clicked.
pub(crate) fn poster_tile_ex(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    item: &VodSummary,
    width: f32,
    cursor: bool,
    decor: TileDecor<'_>,
) -> (egui::Response, bool) {
    let t = &theme.tokens;
    let width = width.max(40.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, card_height(theme, width)), egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tile_label(item, &decor)));
    if !ui.is_rect_visible(rect) {
        return (resp, false);
    }
    let full = Rect::from_min_size(rect.min, egui::vec2(width, (width * 1.5).round()));
    // a stack: two card edges behind the poster, offset down and right (one for "+1"); the
    // card keeps its footprint, so titles stay in line
    let edges: u8 = if decor.stack.is_some() {
        2
    } else if decor.elsewhere.is_some() {
        1
    } else {
        0
    };
    let off = (3.0 * theme.rem()).round().max(2.0);
    let poster = Rect::from_min_max(full.min, full.max - egui::Vec2::splat(off * f32::from(edges)));
    // drawn disabled: a locked copy or an all-locked stack (`decor.dimmed`), and any locked
    // title, until the PIN opens the session (DESIGN §20.3) — its card edges too
    let dim = decor.dimmed || (item.locked && paint::session_locked(ui.ctx()));
    for k in (1..=edges).rev() {
        let edge = poster.translate(egui::Vec2::splat(off * f32::from(k)));
        let (fill, stroke) = if dim { (with_alpha(t.raised_bg, 0.5), with_alpha(t.border_hover, 0.5)) } else { (t.raised_bg, t.border_hover) };
        ui.painter().rect_filled(edge, t.rounding, fill);
        ui.painter().rect_stroke(edge, t.rounding, egui::Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    }
    paint_poster_image(ui, theme, images, item.poster.as_deref(), &item.name, poster, ImageKind::Poster);
    if dim {
        ui.painter().rect_filled(poster, t.rounding, with_alpha(t.bg, 0.6));
    }
    let chip = theme.font(TypeToken::Icon) + 2.0 * theme.space(SpaceToken::Xs);
    if let Some(src) = decor.source {
        // top-left: the lock / watched chips grow from the top-right corner
        let right_chips = f32::from(u8::from(item.locked) + u8::from(decor.watched));
        let limit = poster.right() - right_chips * (chip + theme.space(SpaceToken::Xs));
        paint_playlist_chips(ui, theme, &resp, poster.left_top(), limit, chip, src, decor.stack.filter(|s| !s.open));
    }
    let p = ui.painter();
    let corner = |p: &egui::Painter, glyph: &str, color: egui::Color32, slot: f32| {
        let size = chip;
        let r = Rect::from_min_size(
            egui::pos2(poster.right() - size * (slot + 1.0) - theme.space(SpaceToken::Xs) * slot, poster.top()),
            egui::vec2(size, size),
        );
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
    let bar_h = (3.0 * theme.rem()).round().max(2.0);
    if let Some(f) = decor.progress.filter(|f| *f > 0.0 && *f < 1.0 && !decor.watched) {
        let bar = Rect::from_min_max(egui::pos2(poster.left(), poster.bottom() - bar_h), poster.right_bottom());
        super::progress::paint_progress(p, bar, theme, f, true);
    }
    // the copy count, bottom-right above the progress bar; a click fans the stack out / folds it
    let mut badge_clicked = false;
    if let Some(st) = decor.stack {
        let ink = if st.open { t.bg } else { t.fg };
        let g = paint::galley(p, st.count.to_string(), paint::font(theme, TypeToken::BodySmall), ink, f32::INFINITY);
        let size = egui::vec2((g.size().x + 2.0 * theme.space(SpaceToken::Sm)).max(chip), chip);
        let at = poster.right_bottom() - size - egui::vec2(theme.space(SpaceToken::Xs), bar_h + theme.space(SpaceToken::Xs));
        let r = Rect::from_min_size(at, size);
        p.rect_filled(r, t.rounding, if st.open { t.accent } else { with_alpha(t.bg, 0.85) });
        p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, if st.open { t.accent } else { t.border_field }), egui::StrokeKind::Inside);
        p.galley(r.center() - g.size() / 2.0, g, ink);
        if st.fan {
            let what = if st.open { "fold them" } else { "show each" };
            let b = ui.interact(r, resp.id.with("stack-badge"), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            b.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("{} copies — {what}", st.count)));
            let key = crate::keymap::tables(crate::keymap::KeyContext::PosterGrid)
                .iter()
                .find_map(|t| t.iter().find(|b| b.action == crate::keymap::Action::FanOut))
                .map(|b| format!(" ({})", b.chord.label()))
                .unwrap_or_default();
            let tip = egui::RichText::new(format!("{} copies — click to {what}{key}", st.count)).size(theme.font(TypeToken::BodySmall));
            badge_clicked = super::fresh_click(&b.on_hover_text(tip));
        }
    }
    let p = ui.painter();
    if cursor || decor.selected || resp.hovered() {
        // inside the poster: a scroll area clips nothing of the first row / column
        let stroke = if cursor { egui::Stroke::new(2.0, t.accent) } else { egui::Stroke::new(1.0, t.border_hover) };
        p.rect_stroke(poster, t.rounding, stroke, egui::StrokeKind::Inside);
    }

    if let Some(label) = crate::ui::downloads::cached_badge(ui.ctx(), &crate::store::copies::vod_ref(item)) {
        let font = egui::FontId::proportional(theme.font(TypeToken::BodySmall));
        let galley = p.layout_no_wrap(label, font, t.fg);
        let at = poster.left_top() + egui::vec2(5.0, 5.0);
        p.rect_filled(egui::Rect::from_min_size(at - egui::vec2(3.0, 2.0), galley.size() + egui::vec2(6.0, 4.0)), t.rounding, t.osd_scrim);
        p.galley(at, galley, t.fg);
    }
    // title (≤ 2 lines, now-playing marker first), then the meta line
    let mut y = full.bottom() + theme.space(SpaceToken::Md);
    let mut left = rect.left();
    let title_color = if cursor {
        t.list_cursor_text
    } else if dim {
        t.text_disabled
    } else {
        t.fg
    };
    if decor.playing {
        let r = paint::text(p, egui::pos2(left, y), Align2::LEFT_TOP, glyphs::PLAY, paint::font(theme, TypeToken::Body), t.accent, f32::INFINITY);
        left = r.right() + theme.space(SpaceToken::Sm);
    }
    let mut job =
        egui::text::LayoutJob::simple(item.name.to_string(), paint::font(theme, TypeToken::Body), title_color, (rect.right() - left).max(1.0));
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    let g = p.layout_job(job);
    let h = g.size().y;
    p.galley(egui::pos2(left, y), g, title_color);
    y += h + theme.space(SpaceToken::Xxs);
    // a fanned-out copy says which version it is; "+1" inside one playlist
    let mut meta = match decor.copy {
        Some(c) => c.to_owned(),
        None => meta_line(item),
    };
    if let Some(e) = decor.elsewhere {
        meta = if meta.is_empty() { format!("+{}", e.count) } else { format!("{meta} · +{}", e.count) };
    }
    // the full playlist names go to the status bar while the card has the cursor or the pointer
    if (cursor || resp.hovered())
        && let Some(names) = playlist_names(&decor)
    {
        let line = if meta.is_empty() { names } else { format!("{names} · {meta}") };
        super::status_bar::note_focus(ui.ctx(), line, resp.hovered());
    }
    let font = paint::font(theme, TypeToken::BodySmall);
    let left = rect.left();
    if !meta.is_empty() {
        let r = paint::text(p, egui::pos2(left, y), Align2::LEFT_TOP, meta, font, t.text_secondary, (rect.right() - left).max(0.0));
        if let Some(e) = decor.elsewhere {
            let also = match (e.first, e.count) {
                (Some(s), 1) => format!("Also in {}", s.name),
                (Some(s), n) => format!("Also in {} and {} more", s.name, n - 1),
                (None, 1) => "Also in another playlist".to_owned(),
                (None, n) => format!("Also in {n} other playlists"),
            };
            ui.interact(r, resp.id.with("elsewhere"), egui::Sense::hover())
                .on_hover_text(egui::RichText::new(also).size(theme.font(TypeToken::BodySmall)));
        }
    }
    (resp, badge_clicked)
}

/// The code chips of a card's playlists from `at` (the poster's top-left corner) rightwards, no
/// further than `limit`: the card's own, then (a folded stack) the next ones up to
/// [`STACK_CHIPS`], then "+n" for the rest. Each chip's tooltip is the full name.
#[allow(clippy::too_many_arguments)]
fn paint_playlist_chips(
    ui: &egui::Ui,
    theme: &Theme,
    card: &egui::Response,
    at: egui::Pos2,
    limit: f32,
    chip: f32,
    own: &SourceMeta,
    stack: Option<StackDecor<'_>>,
) {
    let t = &theme.tokens;
    let p = ui.painter();
    let (font, fill, gap) = (paint::font(theme, TypeToken::Caption), with_alpha(t.bg, 0.85), theme.space(SpaceToken::Xs));
    let mut list = vec![own];
    list.extend(stack.iter().flat_map(|s| s.next.iter().flatten().copied()));
    let total = 1 + stack.map_or(0, |s| s.others);
    let more_chip = |n: usize| CodeChip::text(p, theme, &format!("+{n}"), t.fg, font.clone(), t.bg, t.fg, chip);
    let mut x = at.x;
    let mut shown = 0;
    for (i, src) in list.iter().enumerate() {
        let c = CodeChip::new(p, theme, src, font.clone(), t.bg, t.fg, chip);
        let rest = total.saturating_sub(i + 1);
        let need = c.size().x + if rest > 0 { gap + more_chip(rest).size().x } else { 0.0 };
        // the card's own chip always shows
        if i > 0 && x + need > limit {
            break;
        }
        let r = c.paint(p, theme, egui::pos2(x, at.y), Align2::LEFT_TOP, Some(fill));
        ui.interact(r, card.id.with(("source-chip", i)), egui::Sense::hover())
            .on_hover_text(egui::RichText::new(&src.name).size(theme.font(TypeToken::BodySmall)));
        x = r.right() + gap;
        shown += 1;
    }
    if total > shown {
        let n = total - shown;
        let r = more_chip(n).paint(p, theme, egui::pos2(x, at.y), Align2::LEFT_TOP, Some(fill));
        let tip = if n == 1 { "Also in 1 more playlist".to_owned() } else { format!("Also in {n} more playlists") };
        ui.interact(r, card.id.with("source-more"), egui::Sense::hover())
            .on_hover_text(egui::RichText::new(tip).size(theme.font(TypeToken::BodySmall)));
    }
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
    poster_grid_with(ui, theme, images, state, all, items, true, |_, _| TileDecor::default())
}

/// [`poster_grid`] with per-card decorations (progress, now playing, watched; `decor` gets the
/// index into `all` and the item). `focused`: the grid has the keyboard (else its current card
/// shows the "selected" outline instead of the cursor).
#[allow(clippy::too_many_arguments)]
pub(crate) fn poster_grid_with<'s>(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    state: &mut GridState,
    all: &[VodSummary],
    items: &[u32],
    focused: bool,
    decor: impl Fn(u32, &VodSummary) -> TileDecor<'s>,
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
                        let mut child = ui
                            .new_child(egui::UiBuilder::new().id_salt(("poster", i)).max_rect(cell).layout(egui::Layout::top_down(egui::Align::Min)));
                        let current = i == cursor;
                        let d = TileDecor { selected: current && !focused, ..decor(idx, item) };
                        let (resp, badge) = poster_tile_ex(&mut child, theme, images, item, card_w, current && focused, d);
                        if badge {
                            state.cursor = i;
                            state.badge_clicked = Some(i);
                        } else if super::fresh_click(&resp) {
                            state.cursor = i;
                            outcome = ListOutcome::Activated(i);
                        } else if resp.secondary_clicked() {
                            state.cursor = i;
                            outcome = ListOutcome::ContextMenu(i);
                        } else if resp.clicked() {
                            // the 2nd/3rd click of a multi-click: already activated
                        } else if pointer_moved && resp.hovered() && i != state.cursor {
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
        assert_eq!(empty.handle(Action::Left), ListOutcome::Parent, "an empty grid still leads back to the categories");
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
        assert_eq!(g.handle(Action::Left), ListOutcome::Parent, "← on the first column steps out while filtering too");
        assert_eq!(g.filter.query(), "m", "and keeps the filter");
        g.set_len(5);
        g.select(1);
        assert_eq!(g.handle(Action::Left), ListOutcome::Moved, "inside the row ← moves between posters");
        assert_eq!(g.cursor, 0);
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

    fn movie() -> VodSummary {
        VodSummary {
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
        }
    }

    fn playlist(id: i64, name: &str, code: &str) -> SourceMeta {
        SourceMeta {
            id: crate::model::SourceId(id),
            name: name.into(),
            icon: String::new(),
            color: Default::default(),
            kind: crate::model::SourceKind::M3uUrl,
            enabled: true,
            last_refresh: None,
            has_vod: true,
            has_series: false,
            has_live: false,
            in_all: true,
            category_filter: None,
            own_category_filter: false,
            language_hidden: 0,
            code: code.into(),
        }
    }

    /// (painted texts, status bar line) of one card `width` wide with the cursor on it.
    fn draw(t: &mut crate::ui::testing::TestUi, width: f32, decor: TileDecor<'_>) -> (Vec<String>, Option<String>) {
        let theme = Theme::fallback();
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            poster_tile(ui, &theme, &mut t.images, &movie(), width, true, decor);
        });
        out.textures_delta.clear();
        let texts = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_owned()),
                _ => None,
            })
            .collect();
        (texts, crate::ui::widgets::status_bar::take_focus(&ctx))
    }

    /// DESIGN §13.0 "Playlist codes": the corner shows codes — a stack's first three, then
    /// "+n", fewer on a narrow card — the meta line stays "year · ★ rating", and the full names
    /// go to the accessible label and the status bar.
    #[test]
    fn cards_chip_playlist_codes_and_keep_the_meta_line() {
        let mut t = crate::ui::testing::TestUi::new().unwrap();
        let (a, b, c) = (playlist(1, "Alpha", "AL"), playlist(2, "Bravo", "BR"), playlist(3, "Charlie", "CH"));
        let single = TileDecor { source: Some(&a), ..Default::default() };
        let (texts, status) = draw(&mut t, 150.0, single);
        assert!(texts.iter().any(|t| t == "AL") && texts.iter().any(|t| t == "1999 · ★ 8.7"), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("Alpha")), "no name on the card: {texts:?}");
        assert_eq!(status.as_deref(), Some("Alpha · 1999 · ★ 8.7"));
        assert_eq!(tile_label(&movie(), &single), "The Matrix · Alpha");

        // four playlists: three chips, then "+1"
        let stack = StackDecor { count: 5, open: false, fan: true, next: [Some(&b), Some(&c)], others: 3 };
        let decor = TileDecor { source: Some(&a), stack: Some(stack), ..Default::default() };
        let (texts, status) = draw(&mut t, 150.0, decor);
        for code in ["AL", "BR", "CH", "+1"] {
            assert!(texts.iter().any(|t| t == code), "{code}: {texts:?}");
        }
        assert!(texts.iter().any(|t| t == "1999 · ★ 8.7"), "{texts:?}");
        assert_eq!(status.as_deref(), Some("Alpha + Bravo + Charlie +1 · 1999 · ★ 8.7"));
        assert_eq!(tile_label(&movie(), &decor), "The Matrix · Alpha + Bravo + Charlie +1 · 5 copies");
        // a narrow card: its own chip, the rest counted
        let (texts, _) = draw(&mut t, 60.0, decor);
        assert!(texts.iter().any(|t| t == "AL") && texts.iter().any(|t| t == "+3") && !texts.iter().any(|t| t == "BR"), "{texts:?}");
        // fanned out: the head card is one copy, its own chip only
        let open = TileDecor { stack: Some(StackDecor { open: true, ..stack }), ..decor };
        let (texts, _) = draw(&mut t, 150.0, open);
        assert!(texts.iter().any(|t| t == "AL") && !texts.iter().any(|t| t == "BR" || t.starts_with('+')), "{texts:?}");
        // cards without playlists: nothing in the corner, nothing in the status bar
        let (texts, status) = draw(&mut t, 150.0, TileDecor::default());
        assert!(!texts.iter().any(|t| t == "AL") && status.is_none());
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
