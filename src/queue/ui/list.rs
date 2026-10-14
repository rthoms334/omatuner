//! The Up next list (DESIGN §32 §2.2–2.4), shared by the Up next screen and the player's `u`
//! panel: its lines (sections Played / Now / Next, the sleep marker), the cursor (never on a
//! section line), marks, type-to-filter, Alt+↑/↓ and drag reorder, and the row painter (theme
//! colours on the screen, the fixed OSD colours over the video).
//!
//! Rows come from the engine's snapshot only (≤ 200 rows, drawn virtualised); a move is a
//! [`QueueCommand::Move`] built by [`super::move_before`], so a reorder from the keyboard and
//! one from a drag end in the same stored order.

use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Rect};

use crate::keymap::Action;
use crate::model::{ItemKind, QueueId, Timestamp};
use crate::queue::{QueueCommand, QueueRow, QueueSnapshot, RowState};
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};
use crate::ui::UiCtx;
use crate::ui::video_host::osd_text;
use crate::ui::widgets::glyphs;
use crate::ui::widgets::list::{ListOutcome, ListState, virtual_list};

/// A pointer this close (pt) to the list's top or bottom edge scrolls it during a drag.
const EDGE: f32 = 24.0;

/// One line of the list.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// A section rule ("Played (2)", "Now", "Next", "✓ 2 played").
    Header(String),
    /// A row: index into the snapshot's rows.
    Row(usize),
    /// The sleep timer's marker; rows after it are "late".
    Sleep(String),
}

/// The sleep timer as the list marks it (`reminders::sleep`'s status, read by the screen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepMark {
    /// At a time (or the end of what plays now): before the first row expected to start after.
    At(Timestamp),
    /// After this many more items: Up next's rows while it plays them, the one on now first.
    Items(u32),
    /// When Up next runs out: after its last row.
    End,
}

/// Where the list is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The Up next screen; `compact` (under 700 pt): no ETA column, "Played" on one line.
    Screen { compact: bool },
    /// The player's `u` panel: what is still to play, no sections, OSD colours, no drag.
    Panel,
}

/// What a key or a click asks of the screen holding the list.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    None,
    /// Handled here (cursor, filter, marks).
    Handled,
    /// Enter / a click (`fullscreen`: Shift+Enter).
    Play {
        id: QueueId,
        fullscreen: bool,
    },
    Menu(QueueId),
    /// ← / Backspace with nothing to the left.
    Parent,
    Send(Vec<QueueCommand>),
    Remove(Vec<QueueId>),
    Toast(&'static str),
}

#[derive(Default)]
pub struct QueueList {
    pub list: ListState,
    snap: Arc<QueueSnapshot>,
    lines: Vec<Line>,
    /// Marked rows (Space / Insert): Delete and the menu's moves act on them all.
    pub marks: Vec<QueueId>,
    /// The row being dragged by its handle.
    drag: Option<QueueId>,
    /// What the lines were built from: (snapshot, filter, mode, sleep timer).
    built: Option<(Arc<QueueSnapshot>, String, Mode, Option<SleepMark>)>,
    /// The first late row (after the sleep marker).
    late_from: Option<usize>,
    /// The list's rect and the pointer's place last frame (drag auto-scroll).
    rect: Option<Rect>,
}

impl QueueList {
    /// Follow a new snapshot (cheap when nothing changed). `sleep`: the sleep timer, if set
    /// (its marker line).
    pub fn sync(&mut self, snap: Arc<QueueSnapshot>, mode: Mode, sleep: Option<SleepMark>) {
        let q = self.list.filter.query().to_owned();
        if self.built.as_ref().is_some_and(|(s, f, m, z)| (Arc::ptr_eq(s, &snap) || **s == *snap) && *f == q && *m == mode && *z == sleep) {
            return;
        }
        let keep = self.cursor_id();
        self.snap = snap.clone();
        self.built = Some((snap, q, mode, sleep));
        self.rebuild(mode, sleep);
        self.marks.retain(|id| self.snap.row(*id).is_some_and(|r| r.entry.state == RowState::Pending));
        if self.drag.is_some_and(|d| self.snap.row(d).is_none()) {
            self.drag = None;
        }
        let at = keep.and_then(|id| self.line_of(id)).or_else(|| self.first_row_from(0, true)).unwrap_or(0);
        self.list.cursor = at.min(self.lines.len().saturating_sub(1));
        self.list.set_len(self.lines.len());
    }

    fn rebuild(&mut self, mode: Mode, sleep: Option<SleepMark>) {
        let s = &self.snap;
        let mut lines = Vec::new();
        self.late_from = None;
        if self.list.filter.is_active() {
            let hay: Vec<String> = s.rows.iter().map(|r| format!("{} {}", r.entry.title, r.entry.subtitle.as_deref().unwrap_or(""))).collect();
            let mut hits = self.list.filter.apply(hay.iter().map(String::as_str));
            hits.sort_unstable(); // list order, not best first: it is a play order
            lines.extend(hits.into_iter().map(|i| Line::Row(i as usize)).filter(|l| mode != Mode::Panel || self.to_play(l)));
            self.lines = lines;
            return;
        }
        let idx = |want: fn(RowState) -> bool| s.rows.iter().enumerate().filter(move |(_, r)| want(r.entry.state)).map(|(i, _)| i);
        let done: Vec<usize> = idx(RowState::is_done).collect();
        let current: Vec<usize> = idx(|st| st == RowState::Current).collect();
        let pending: Vec<usize> = idx(|st| st == RowState::Pending).collect();
        match mode {
            Mode::Screen { compact } if !done.is_empty() => {
                if compact {
                    lines.push(Line::Header(format!("✓ {} played", done.len())));
                } else {
                    lines.push(Line::Header(format!("Played ({})", done.len())));
                    lines.extend(done.iter().map(|i| Line::Row(*i)));
                }
            }
            _ => {}
        }
        let panel = mode == Mode::Panel;
        if !current.is_empty() {
            if !panel {
                lines.push(Line::Header("Now".into()));
            }
            lines.extend(current.iter().map(|i| Line::Row(*i)));
        }
        if !pending.is_empty() && !panel {
            lines.push(Line::Header("Next".into()));
        }
        // the sleep marker: after the row it falls after (what plays now: right under it), else
        // before the first row expected to start after a fixed-time timer
        let after = sleep.and_then(|z| sleep_after(s, z));
        let sleep_at = match sleep {
            Some(SleepMark::At(t)) => Some(t),
            _ => None,
        };
        let marker = || Line::Sleep(format!("{} Sleep timer", glyphs::SLEEP));
        let mut marked = false;
        if let Some(a) = after.filter(|a| s.current == Some(*a)) {
            let cur = lines.iter().position(|l| matches!(l, Line::Row(i) if s.rows[*i].entry.id == a));
            let at = lines.iter().position(|l| *l == Line::Header("Next".into())).or(cur).map_or(lines.len(), |p| p + 1);
            lines.insert(at, marker());
            marked = true;
            self.late_from = Some(at + 1);
        }
        for i in pending {
            let r = &s.rows[i];
            if !marked && sleep_at.is_some_and(|at| r.eta.is_some_and(|e| e >= at)) {
                lines.push(marker());
                marked = true;
                self.late_from = Some(lines.len());
            }
            lines.push(Line::Row(i));
            if !marked && after == Some(r.entry.id) {
                lines.push(marker());
                marked = true;
                self.late_from = Some(lines.len());
            }
        }
        self.lines = lines;
    }

    /// A still-to-play row (the panel lists only those).
    fn to_play(&self, l: &Line) -> bool {
        matches!(l, Line::Row(i) if matches!(self.snap.rows[*i].entry.state, RowState::Current | RowState::Pending))
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn snapshot(&self) -> &Arc<QueueSnapshot> {
        &self.snap
    }

    fn line_of(&self, id: QueueId) -> Option<usize> {
        self.lines.iter().position(|l| matches!(l, Line::Row(i) if self.snap.rows[*i].entry.id == id))
    }

    fn row_at(&self, line: usize) -> Option<&QueueRow> {
        match self.lines.get(line)? {
            Line::Row(i) => self.snap.rows.get(*i),
            _ => None,
        }
    }

    /// The first row line at or after `from` (`down`) / at or before it.
    fn first_row_from(&self, from: usize, down: bool) -> Option<usize> {
        let is_row = |i: &usize| matches!(self.lines.get(*i), Some(Line::Row(_)));
        if down { (from..self.lines.len()).find(is_row) } else { (0..=from.min(self.lines.len().saturating_sub(1))).rev().find(is_row) }
    }

    /// The row under the cursor.
    pub fn cursor_row(&self) -> Option<&QueueRow> {
        self.row_at(self.list.cursor)
    }

    pub fn cursor_id(&self) -> Option<QueueId> {
        self.cursor_row().map(|r| r.entry.id)
    }

    /// Put the cursor on a row (the current one when the panel opens).
    pub fn select(&mut self, id: QueueId) {
        if let Some(i) = self.line_of(id) {
            self.list.select(i);
        }
    }

    /// The rows an action covers: the marked ones, else the cursor's.
    pub fn targets(&self) -> Vec<QueueId> {
        if self.marks.is_empty() { self.cursor_id().into_iter().collect() } else { self.marks.clone() }
    }

    /// Esc during a drag: cancel it (the Esc ladder's step before leaving).
    pub fn cancel_drag(&mut self) -> bool {
        self.drag.take().is_some()
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The cursor left a section line: step on in the same direction (or back).
    fn skip_lines(&mut self, down: bool) {
        if matches!(self.lines.get(self.list.cursor), Some(Line::Row(_)) | None) {
            return;
        }
        let at = self.first_row_from(self.list.cursor, down).or_else(|| self.first_row_from(self.list.cursor, !down));
        if let Some(i) = at {
            self.list.select(i);
        }
    }

    /// A key on the list.
    pub fn handle(&mut self, action: Action) -> Out {
        let before = self.list.cursor;
        match self.list.handle(action) {
            ListOutcome::Moved => {
                self.skip_lines(self.list.cursor >= before);
                return Out::Handled;
            }
            ListOutcome::FilterChanged => {
                // build again for the new filter, in the mode and with the timer of the last build
                let (snap, mode, z) = (self.snap.clone(), self.mode(), self.sleep_mark());
                self.built = None;
                self.sync(snap, mode, z);
                self.list.select(self.first_row_from(0, true).unwrap_or(0));
                return Out::Handled;
            }
            ListOutcome::Activated(_) | ListOutcome::ActivatedAlt(_) => {
                return match self.cursor_id() {
                    Some(id) => Out::Play { id, fullscreen: action == Action::ActivateAlt },
                    None => Out::Handled,
                };
            }
            ListOutcome::ContextMenu(_) => return self.cursor_id().map_or(Out::Handled, Out::Menu),
            ListOutcome::Parent => return Out::Parent,
            ListOutcome::None => {}
        }
        match action {
            Action::ToggleMark => {
                if let Some(r) = self.cursor_row().filter(|r| r.entry.state == RowState::Pending) {
                    let id = r.entry.id;
                    match self.marks.iter().position(|m| *m == id) {
                        Some(i) => {
                            self.marks.remove(i);
                        }
                        None => self.marks.push(id),
                    }
                    if self.list.handle(Action::Down) == ListOutcome::Moved {
                        self.skip_lines(true);
                    }
                }
                Out::Handled
            }
            Action::Remove => {
                let ids = self.targets();
                self.marks.clear();
                if ids.is_empty() { Out::Handled } else { Out::Remove(ids) }
            }
            Action::MoveUp | Action::MoveDown => self.reorder(action == Action::MoveDown),
            Action::QueuePlayNext => {
                let ids: Vec<QueueId> = self.targets().into_iter().filter(|id| self.pending(*id)).collect();
                self.marks.clear();
                if ids.is_empty() {
                    return Out::Toast("Only what is still to play moves");
                }
                Out::Send(vec![QueueCommand::Move { ids, to: crate::queue::Insert::Next }])
            }
            _ => Out::None,
        }
    }

    fn mode(&self) -> Mode {
        self.built.as_ref().map_or(Mode::Screen { compact: false }, |b| b.2)
    }

    fn sleep_mark(&self) -> Option<SleepMark> {
        self.built.as_ref().and_then(|b| b.3)
    }

    fn pending(&self, id: QueueId) -> bool {
        self.snap.row(id).is_some_and(|r| r.entry.state == RowState::Pending)
    }

    /// The pending rows in list order.
    fn pending_ids(&self) -> Vec<QueueId> {
        self.snap.rows.iter().filter(|r| r.entry.state == RowState::Pending).map(|r| r.entry.id).collect()
    }

    /// Alt+↑/↓: the cursor's row one place among the rows still to play.
    fn reorder(&mut self, down: bool) -> Out {
        if self.list.filter.is_active() {
            return Out::Toast("Clear the filter to reorder");
        }
        let Some(id) = self.cursor_id() else { return Out::Handled };
        if !self.pending(id) {
            return Out::Toast("Only what is still to play moves");
        }
        let order = self.pending_ids();
        let Some(i) = order.iter().position(|x| *x == id) else { return Out::Handled };
        let before = if down {
            if i + 1 >= order.len() {
                return Out::Handled;
            }
            order.get(i + 2).copied()
        } else {
            if i == 0 {
                return Out::Handled;
            }
            Some(order[i - 1])
        };
        Out::Send(super::move_before(&self.snap, &[id], before))
    }

    /// Where a row dropped at `y` goes: before the first pending row (not `moving`) whose middle
    /// is below `y`, else at the end.
    fn drop_target(&self, drawn: &[(Rect, QueueId)], moving: QueueId, y: f32) -> Option<QueueId> {
        let candidates: Vec<&(Rect, QueueId)> = drawn.iter().filter(|(_, id)| *id != moving && self.pending(*id)).collect();
        if let Some((_, id)) = candidates.iter().find(|(r, _)| r.center().y > y) {
            return Some(*id);
        }
        // below every drawn row: before the first pending row after the last one drawn
        let last = candidates.last().map(|(_, id)| *id)?;
        let order = self.pending_ids();
        let at = order.iter().position(|x| *x == last)?;
        order[at + 1..].iter().copied().find(|x| *x != moving)
    }

    /// Draw the list in `ui` (all of it); returns what was asked (activations, menus, a drop,
    /// a row's ✕).
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, look: &Look, row_h: f32) -> Out {
        let rect = ui.available_rect_before_wrap();
        self.rect = Some(rect);
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let theme = ctx.theme;
        let mode = self.mode();
        let filtering = self.list.filter.is_active();
        let can_drag = mode != Mode::Panel && !filtering;
        let h24 = ctx.config.interface.clock_24h;
        let mut drawn: Vec<(Rect, QueueId)> = Vec::new();
        let mut remove: Option<QueueId> = None;
        let mut drag_start: Option<QueueId> = None;
        let mut scroll = 0.0;
        if let (Some(_), Some(p)) = (self.drag, pointer) {
            if p.y < rect.top() + EDGE {
                scroll = row_h / 4.0;
            } else if p.y > rect.bottom() - EDGE {
                scroll = -row_h / 4.0;
            }
        }
        let lines = std::mem::take(&mut self.lines);
        let snap = self.snap.clone();
        let (marks, dragging, late_from) = (self.marks.clone(), self.drag, self.late_from);
        let playing_pos = playing_position(ctx, &snap);
        let font = FontId::proportional(theme.font(TypeToken::Body));
        let small = FontId::proportional(theme.font(TypeToken::BodySmall));
        let icon = FontId::proportional(theme.font(TypeToken::Icon));
        let pad = theme.space(SpaceToken::RowPaddingX).min(row_h / 2.0);
        let mut first = true;
        let then_h = snap.watchlist.as_ref().map_or(0.0, |w| (w.then.len().min(3) as f32 + 2.2) * row_h).min(rect.height() * 0.45);
        let rows_rect = Rect::from_min_max(rect.min, egui::pos2(rect.right(), rect.bottom() - then_h));
        let mut rows_ui = ui.new_child(egui::UiBuilder::new().max_rect(rows_rect));
        let outcome = virtual_list(&mut rows_ui, theme, ("queue-list", mode == Mode::Panel), &mut self.list, row_h, |ui, rc| {
            if std::mem::take(&mut first) && scroll != 0.0 {
                ui.scroll_with_delta(egui::vec2(0.0, scroll));
                ui.ctx().request_repaint();
            }
            let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
            let p = ui.painter().clone();
            match &lines[rc.index] {
                Line::Header(text) | Line::Sleep(text) => {
                    let sleep = matches!(lines[rc.index], Line::Sleep(_));
                    let color = if sleep { look.warn } else { look.dim };
                    let g = osd_text(
                        &p,
                        egui::pos2(r.left() + pad, r.center().y),
                        Align2::LEFT_CENTER,
                        text,
                        small.clone(),
                        color,
                        r.width() - 2.0 * pad,
                    );
                    let y = r.center().y.round() + 0.5;
                    if g.right() + pad < r.right() - pad {
                        p.hline(g.right() + pad..=r.right() - pad, y, egui::Stroke::new(1.0, with_alpha(color, 0.5)));
                    }
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text.clone()));
                    resp
                }
                Line::Row(i) => {
                    let row = &snap.rows[*i];
                    let e = &row.entry;
                    let cursor = rc.state.is_cursor();
                    let hovered = resp.hovered() || ui.rect_contains_pointer(r);
                    let marked = marks.contains(&e.id);
                    if cursor || marked {
                        p.rect_filled(r, theme.tokens.rounding, if marked { look.marked_bg } else { look.cursor_bg });
                    }
                    if dragging == Some(e.id) {
                        p.rect_stroke(r, theme.tokens.rounding, egui::Stroke::new(1.0, look.accent), egui::StrokeKind::Inside);
                    }
                    let done = e.state.is_done();
                    let dim_row = done || (row.locked && crate::ui::widgets::session_locked(ui.ctx())) || row.note.is_some();
                    let fg = if cursor {
                        look.accent
                    } else if dim_row {
                        look.dim
                    } else {
                        look.fg
                    };
                    let label = super::row_title(e);
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label.clone()));
                    // left: the drag handle (hover / cursor) or the state mark
                    let lead = Rect::from_min_size(r.min, egui::vec2(pad + icon.size + pad / 2.0, row_h));
                    let pending = e.state == RowState::Pending;
                    let mut x = lead.right();
                    if can_drag && pending && (hovered || cursor || dragging == Some(e.id)) {
                        let h = ui.interact(lead, resp.id.with("drag"), egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::Grab);
                        h.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Drag {label}")));
                        p.text(lead.center(), Align2::CENTER_CENTER, glyphs::REORDER, icon.clone(), if h.hovered() { look.accent } else { look.dim });
                        if h.drag_started() {
                            drag_start = Some(e.id);
                        }
                    } else {
                        let mark = if marked {
                            Some((glyphs::CHECKBOX_ON, look.accent))
                        } else {
                            match e.state {
                                RowState::Current => Some((glyphs::PLAY, look.accent)),
                                RowState::Played => Some((glyphs::CHECK, look.dim)),
                                RowState::Skipped => Some((glyphs::NEXT, look.dim)),
                                RowState::Failed => Some((glyphs::WARNING, look.warn)),
                                RowState::Pending => None,
                            }
                        };
                        if let Some((g, c)) = mark {
                            p.text(lead.center(), Align2::CENTER_CENTER, g, icon.clone(), c);
                        }
                    }
                    // right, from the edge: ✕ on hover, note / late, ETA, duration, playlist
                    let mut right = r.right() - pad / 2.0;
                    if hovered && mode != Mode::Panel {
                        let xr = Rect::from_center_size(egui::pos2(right - row_h * 0.3, r.center().y), egui::vec2(row_h * 0.6, row_h * 0.6));
                        let xb = ui.interact(xr, resp.id.with("remove"), egui::Sense::click()).on_hover_text("Remove (Delete)");
                        xb.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Remove {label}")));
                        p.text(xr.center(), Align2::CENTER_CENTER, glyphs::CLOSE, icon.clone(), if xb.hovered() { look.danger } else { look.dim });
                        // the next row's ✕ moves up under the pointer: a double-click removes one
                        if crate::ui::widgets::fresh_click(&xb) {
                            remove = Some(e.id);
                        }
                        right = xr.left() - pad / 2.0;
                    }
                    if let Some(chip) = e.watchlist_item_id.and_then(|id| snap.watchlist_chips.get(&id)) {
                        let label = format!("{} · {}/{}", chip.name, chip.position, chip.total);
                        let g =
                            osd_text(&p, egui::pos2(right, r.center().y), Align2::RIGHT_CENTER, &label, small.clone(), look.accent, r.width() / 3.0);
                        right = g.left() - pad;
                    }
                    let late = late_from.is_some_and(|l| rc.index >= l) && pending;
                    let note = row.note.clone().or_else(|| late.then(|| format!("{} late", glyphs::WARNING)));
                    if let Some(n) = note {
                        let g = osd_text(&p, egui::pos2(right, r.center().y), Align2::RIGHT_CENTER, &n, small.clone(), look.warn, r.width() / 3.0);
                        right = g.left() - pad;
                    }
                    let compact = matches!(mode, Mode::Screen { compact: true });
                    if !compact
                        && mode != Mode::Panel
                        && let Some(at) = row.eta
                    {
                        let g = osd_text(
                            &p,
                            egui::pos2(right, r.center().y),
                            Align2::RIGHT_CENTER,
                            &super::eta(at, h24),
                            small.clone(),
                            look.dim,
                            f32::INFINITY,
                        );
                        right = g.left() - pad;
                    }
                    let length = match (e.state, playing_pos) {
                        (RowState::Current, Some((pos, Some(d)))) => {
                            format!("{} / {}", crate::ui::screens::home::kit::clock(pos), crate::ui::screens::home::kit::clock(d))
                        }
                        _ => super::duration(e.duration_s),
                    };
                    let g = osd_text(&p, egui::pos2(right, r.center().y), Align2::RIGHT_CENTER, &length, small.clone(), look.dim, f32::INFINITY);
                    right = g.left() - pad;
                    let kind = if e.item.kind == ItemKind::Episode { ItemKind::Series } else { e.item.kind };
                    if mode != Mode::Panel
                        && let Some(src) = ctx.source_of(kind, e.item.source)
                    {
                        let chip = crate::ui::widgets::badge::CodeChip::new(&p, theme, src, small.clone(), look.chip_bg, look.fg, 0.0);
                        let m = chip.paint(&p, theme, egui::pos2(right, r.center().y), Align2::RIGHT_CENTER, None);
                        if !e.any_copy {
                            // a chosen copy: its chip is underlined
                            p.hline(m.x_range(), m.bottom() + 1.5, egui::Stroke::new(1.5, look.fg));
                        }
                        right = m.left() - pad;
                    }
                    // the title, then its marks
                    let mut tail = String::new();
                    if row.locked {
                        tail.push_str(glyphs::LOCK);
                    }
                    if e.programme.is_some() {
                        if !tail.is_empty() {
                            tail.push(' ');
                        }
                        tail.push_str(glyphs::CATCHUP);
                    }
                    let tail_w = if tail.is_empty() { 0.0 } else { p.layout_no_wrap(tail.clone(), icon.clone(), look.dim).size().x + pad / 2.0 };
                    let title_w = (right - x - tail_w).max(0.0);
                    let g = osd_text(&p, egui::pos2(x, r.center().y), Align2::LEFT_CENTER, &label, font.clone(), fg, title_w);
                    x = g.right() + pad / 2.0;
                    if !tail.is_empty() {
                        osd_text(&p, egui::pos2(x, r.center().y), Align2::LEFT_CENTER, &tail, icon.clone(), look.dim, f32::INFINITY);
                    }
                    drawn.push((r, e.id));
                    resp
                }
            }
        });
        self.lines = lines;
        let mut out = match outcome {
            // a click on a section or sleep line: the cursor steps on to a row, as keys would
            ListOutcome::Activated(_) | ListOutcome::ContextMenu(_) if self.cursor_id().is_none() => {
                self.skip_lines(true);
                Out::Handled
            }
            ListOutcome::Activated(_) => self.cursor_id().map_or(Out::Handled, |id| Out::Play { id, fullscreen: false }),
            ListOutcome::ContextMenu(_) => self.cursor_id().map_or(Out::Handled, Out::Menu),
            ListOutcome::Moved => {
                self.skip_lines(true);
                Out::Handled
            }
            _ => Out::None,
        };
        if let Some(id) = remove {
            out = Out::Remove(vec![id]);
        }
        if let Some(id) = drag_start {
            self.drag = Some(id);
            self.marks.clear();
        }
        // a drag: the insertion line follows the pointer; a release inside the list drops
        if let (Some(moving), Some(p)) = (self.drag, pointer) {
            let target = self.drop_target(&drawn, moving, p.y);
            let y = match target.and_then(|t| drawn.iter().find(|(_, id)| *id == t)) {
                Some((r, _)) => r.top(),
                None => drawn.iter().filter(|(_, id)| self.pending(*id)).map(|(r, _)| r.bottom()).fold(rect.top(), f32::max),
            };
            if rect.contains(p) {
                ui.painter().hline(rect.x_range(), y, egui::Stroke::new(2.0, look.accent));
            }
            if ui.input(|i| i.pointer.any_released()) {
                self.drag = None;
                if rect.contains(p) {
                    out = Out::Send(super::move_before(&self.snap, &[moving], target));
                }
            }
            ui.ctx().request_repaint();
        } else if self.drag.is_some() && ui.input(|i| !i.pointer.any_down()) {
            self.drag = None;
        }
        if let Some(w) = &snap.watchlist {
            let then_rect = Rect::from_min_max(egui::pos2(rect.left(), rows_rect.bottom()), rect.max);
            ui.scope_builder(egui::UiBuilder::new().max_rect(then_rect), |ui| {
                ui.separator();
                ui.label(egui::RichText::new(format!("Then from {}", w.name)).color(look.accent));
                for e in w.then.iter().take(3) {
                    let episode = e.season.zip(e.episode).map(|(s, n)| format!(" · S{s}E{n}")).unwrap_or_default();
                    ui.label(egui::RichText::new(format!("{}{}{episode}", if e.locked { "󰌾 " } else { "" }, e.title)).color(look.dim));
                }
                if ui.button("Stop after the queue").clicked() {
                    out = Out::Send(vec![QueueCommand::StopList]);
                }
            });
        }
        out
    }
}

/// The row the sleep timer falls after: the n-th still to play (the row on now first) while Up
/// next plays or is between its items, or the last one for "end of Up next".
fn sleep_after(s: &QueueSnapshot, z: SleepMark) -> Option<QueueId> {
    let mut pending = s.rows.iter().filter(|r| r.eta.is_some()).map(|r| r.entry.id);
    match z {
        SleepMark::Items(n) if s.driving || s.between_items() => {
            let on = s.current.filter(|_| s.driving);
            on.into_iter().chain(pending).nth((n as usize).checked_sub(1)?)
        }
        SleepMark::End => pending.next_back(),
        SleepMark::Items(_) | SleepMark::At(_) => None,
    }
}

/// Position and duration of what plays, when it is Up next's current row.
fn playing_position(ctx: &UiCtx<'_>, s: &QueueSnapshot) -> Option<(f64, Option<f64>)> {
    if !s.driving {
        return None;
    }
    let p = ctx.player.position?;
    Some((p, ctx.player.duration))
}

/// The list's colours: the theme's on the screen, the fixed OSD ones over the video.
#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub fg: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub cursor_bg: Color32,
    pub marked_bg: Color32,
    pub chip_bg: Color32,
}

impl Look {
    pub fn theme(theme: &Theme) -> Self {
        let t = &theme.tokens;
        Self {
            fg: t.fg,
            dim: t.text_secondary,
            accent: t.list_cursor_text,
            warn: t.warning,
            danger: t.urgent,
            cursor_bg: t.list_cursor_bg,
            marked_bg: t.fill_selected,
            chip_bg: t.bg,
        }
    }

    pub fn osd(theme: &Theme) -> Self {
        let t = &theme.tokens;
        Self {
            fg: t.osd_text,
            dim: with_alpha(t.osd_text, 0.7),
            accent: t.osd_accent,
            warn: t.osd_accent,
            danger: t.osd_accent,
            cursor_bg: with_alpha(t.osd_text, 0.12),
            marked_bg: with_alpha(t.osd_text, 0.2),
            chip_bg: t.osd_scrim.to_opaque(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Insert;
    use crate::queue::ui::tests::entry;

    fn snap(rows: &[(i64, RowState)]) -> Arc<QueueSnapshot> {
        let rows: Vec<QueueRow> = rows.iter().map(|(id, s)| QueueRow { entry: entry(*id, *s), locked: false, eta: None, note: None }).collect();
        Arc::new(QueueSnapshot { rows: rows.into(), ..Default::default() })
    }

    fn row_ids(l: &QueueList) -> Vec<Option<i64>> {
        l.lines().iter().map(|x| if let Line::Row(i) = x { Some(l.snapshot().rows[*i].entry.id.0) } else { None }).collect()
    }

    /// Played / Now / Next sections with the sleep marker after its row (after 2 items: the one
    /// on now and the next; between items, the next two); the cursor never rests on a section
    /// line; the panel keeps what is still to play.
    #[test]
    fn sections_keep_the_play_order_and_the_cursor_skips_their_rules() {
        let mut s = (*snap(&[(1, RowState::Played), (2, RowState::Current), (3, RowState::Pending), (4, RowState::Pending)])).clone();
        let mut rows = s.rows.to_vec();
        rows[2].eta = Some(100);
        rows[3].eta = Some(200);
        s.rows = rows.into();
        let z = Some(SleepMark::Items(2));
        let mut l = QueueList::default();
        l.sync(Arc::new(s.clone()), Mode::Screen { compact: false }, z);
        assert!(!l.lines().iter().any(|x| matches!(x, Line::Sleep(_))), "not while Up next is stopped");
        (s.current, s.driving) = (Some(QueueId(2)), true);
        let s = Arc::new(s);
        l.sync(s.clone(), Mode::Screen { compact: false }, z);
        assert_eq!(row_ids(&l), vec![None, Some(1), None, Some(2), None, Some(3), None, Some(4)]);
        assert!(matches!(&l.lines()[6], Line::Sleep(_)));
        let mut between = (*s).clone();
        (between.driving, between.countdown) =
            (false, Some(crate::queue::Countdown { id: Some(QueueId(3)), at: Some(100), why: crate::queue::CountdownWhy::Ended }));
        assert_eq!(sleep_after(&between, SleepMark::Items(2)), Some(QueueId(4)), "row 2 has ended: the next two");
        assert_eq!(sleep_after(&between, SleepMark::End), Some(QueueId(4)));
        l.list.select(1);
        assert_eq!(l.handle(Action::Down), Out::Handled);
        assert_eq!(l.cursor_id(), Some(QueueId(2)), "the Now rule is stepped over");
        assert_eq!(l.handle(Action::Down), Out::Handled);
        assert_eq!(l.handle(Action::Down), Out::Handled);
        assert_eq!(l.cursor_id(), Some(QueueId(4)), "and the sleep marker too");
        l.sync(s.clone(), Mode::Screen { compact: true }, z);
        assert_eq!(l.lines()[0], Line::Header("✓ 1 played".into()), "compact: played on one line");
        assert_eq!(l.cursor_id(), Some(QueueId(4)), "the cursor stays on its row");
        l.sync(s, Mode::Panel, z);
        assert_eq!(row_ids(&l).into_iter().flatten().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    /// Alt+↓ moves the row past the next one still to play; reordering is refused while
    /// filtering and for rows already played; marks then Ctrl+Shift+N move them all next.
    #[test]
    fn keyboard_reorder_and_marks_send_moves() {
        let s = snap(&[(1, RowState::Played), (2, RowState::Pending), (3, RowState::Pending), (4, RowState::Pending)]);
        let mut l = QueueList::default();
        l.sync(s, Mode::Screen { compact: false }, None);
        l.select(QueueId(2));
        assert_eq!(
            l.handle(Action::MoveDown),
            Out::Send(vec![
                QueueCommand::Move { ids: vec![QueueId(2)], to: Insert::End },
                QueueCommand::Move { ids: vec![QueueId(4)], to: Insert::End }
            ])
        );
        l.select(QueueId(1));
        assert_eq!(l.handle(Action::MoveUp), Out::Toast("Only what is still to play moves"));
        l.select(QueueId(3));
        l.handle(Action::ToggleMark);
        assert_eq!(l.cursor_id(), Some(QueueId(4)), "a mark steps down");
        l.handle(Action::ToggleMark);
        assert_eq!(l.handle(Action::QueuePlayNext), Out::Send(vec![QueueCommand::Move { ids: vec![QueueId(3), QueueId(4)], to: Insert::Next }]));
        assert!(l.marks.is_empty());
        l.handle(Action::FilterInput('4'));
        assert_eq!(l.handle(Action::MoveUp), Out::Toast("Clear the filter to reorder"));
    }
}
