//! Debug log viewer (UHF debug mode): type to filter text, Enter expands a line, Ctrl+S
//! (`SortMenu`) opens the view menu: minimum level + "Export…" (file picker; lines are already
//! redacted). Key context `[List|ListFiltering]`. Owner: WP-12.
//!
//! The lines are a snapshot of the in-memory ring (`Services::logs`), refreshed about once a
//! second while the screen shows (new lines follow when the cursor sits on the last line).
//! "Export…" only opens the save dialog (`PickPurpose::ExportLog`); the app writes the file on
//! the blocking pool when the picker answers and toasts the outcome — this screen never does
//! file I/O.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, RichText};
use tracing::Level;

use crate::keymap::{Action, KeyContext};
use crate::logging::LogLine;
use crate::platform::PickPurpose;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::list::{ListOutcome, ListState, paint_row_state, virtual_list};
use crate::ui::widgets::text_field::search_header;
use crate::ui::widgets::{empty_state, glyphs, section};
use crate::ui::{OverlayRequest, Route, Screen, ScreenId, UiCtx};

/// (minimum level, menu label, button label).
const LEVELS: [(Level, &str, &str); 5] = [
    (Level::ERROR, "Errors only", "Errors"),
    (Level::WARN, "Warnings and errors", "Warnings"),
    (Level::INFO, "Info and above", "Info"),
    (Level::DEBUG, "Debug and above", "Debug"),
    (Level::TRACE, "Everything", "Everything"),
];
/// View-menu code of "Export…" (levels use their index).
const MENU_EXPORT: u8 = 10;
const REFRESH: Duration = Duration::from_secs(1);

pub struct LogsScreen {
    min: Level,
    lines: Vec<LogLine>,
    /// Displayed lines (indices into `lines`) for the filter.
    rows: Vec<u32>,
    list: ListState,
    /// The line shown in full below the list, by identity: the ring buffer rotates and the
    /// view refreshes every second, so a row index would point at another line.
    expanded: Option<LineId>,
    refreshed: Option<Instant>,
}

/// What identifies a log line across refreshes (lines carry no id).
#[derive(Debug, Clone, PartialEq)]
struct LineId {
    at: crate::model::Timestamp,
    level: Level,
    target: String,
    message: String,
}

impl LineId {
    fn of(l: &LogLine) -> Self {
        Self { at: l.at, level: l.level, target: l.target.clone(), message: l.message.clone() }
    }
    fn is(&self, l: &LogLine) -> bool {
        self.at == l.at && self.level == l.level && self.target == l.target && self.message == l.message
    }
}

impl Default for LogsScreen {
    fn default() -> Self {
        Self { min: Level::INFO, lines: Vec::new(), rows: Vec::new(), list: ListState::default(), expanded: None, refreshed: None }
    }
}

impl LogsScreen {
    /// Re-read the ring buffer. The cursor stays on its line (or follows new lines when it sat
    /// on the last one); the expanded line stays open while it is still there.
    fn refresh(&mut self, ctx: &UiCtx<'_>) {
        let follow = self.rows.is_empty() || self.list.cursor + 1 >= self.rows.len();
        let at_cursor = self.line_at(self.list.cursor).map(LineId::of);
        self.lines = ctx.services.logs.snapshot(self.min);
        self.refilter();
        if follow {
            self.list.select(self.rows.len().saturating_sub(1));
        } else if let Some(i) = at_cursor.and_then(|id| self.row_of(&id)) {
            self.list.select(i);
        }
        if self.expanded.as_ref().is_some_and(|id| self.row_of(id).is_none()) {
            self.expanded = None;
        }
        self.refreshed = Some(Instant::now());
    }

    fn refilter(&mut self) {
        let texts: Vec<String> = self.lines.iter().map(|l| format!("{} {} {}", l.level, l.target, l.message)).collect();
        self.rows = self.list.filter.apply(texts.iter().map(String::as_str));
        self.list.set_len(self.rows.len());
    }

    /// The line of displayed row `i`.
    fn line_at(&self, i: usize) -> Option<&LogLine> {
        self.rows.get(i).and_then(|l| self.lines.get(*l as usize))
    }

    /// The displayed row of a line.
    fn row_of(&self, id: &LineId) -> Option<usize> {
        self.rows.iter().position(|l| self.lines.get(*l as usize).is_some_and(|x| id.is(x)))
    }

    /// Enter on row `i`: expand it, or collapse it when it is the expanded one.
    fn toggle_expanded(&mut self, i: usize) {
        let id = self.line_at(i).map(LineId::of);
        self.expanded = if self.expanded == id { None } else { id };
    }

    fn view_menu(&self, ctx: &mut UiCtx<'_>) {
        let mut items: Vec<MenuItem> = LEVELS
            .iter()
            .enumerate()
            .map(|(i, (l, label, _))| MenuItem {
                icon: if *l == self.min { glyphs::CHECK } else { "" },
                label: (*label).to_owned(),
                action: Action::Digit(i as u8),
                enabled: true,
                shortcut: None,
            })
            .collect();
        items.push(MenuItem { icon: glyphs::BACKUP, label: "Export…".into(), action: Action::Digit(MENU_EXPORT), enabled: true, shortcut: None });
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Log view".into(), items }));
    }

    /// Open the save dialog; the app exports when it answers (`PickPurpose::ExportLog`).
    fn export(&self, ctx: &mut UiCtx<'_>) {
        super::home::kit::pick_file(ctx, PickPurpose::ExportLog);
    }
}

fn level_label(l: Level) -> &'static str {
    match l {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

impl Screen for LogsScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Logs
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![self.list.key_context()]
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, _route: &Route) {
        if ctx.config.general.debug_log && self.refreshed.is_none() {
            self.min = Level::DEBUG;
        }
        self.list = ListState::default();
        self.rows.clear();
        self.refresh(ctx);
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        if action == Action::SortMenu {
            self.view_menu(ctx);
            return true;
        }
        match self.list.handle(action) {
            ListOutcome::FilterChanged => {
                self.refilter();
                // a new filter starts from a clean view
                self.expanded = None;
                true
            }
            ListOutcome::Activated(i) | ListOutcome::ActivatedAlt(i) => {
                self.toggle_expanded(i);
                true
            }
            ListOutcome::Moved => true,
            ListOutcome::ContextMenu(_) => {
                self.view_menu(ctx);
                true
            }
            // ←/Backspace on an empty filter: back to Settings (the app's Back)
            ListOutcome::Parent => false,
            ListOutcome::None => matches!(
                action,
                Action::Up
                    | Action::Down
                    | Action::PageUp
                    | Action::PageDown
                    | Action::Home
                    | Action::End
                    | Action::FilterInput(_)
                    | Action::Activate
            ),
        }
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Chosen(Action::Digit(MENU_EXPORT)) => self.export(ctx),
            OverlayResult::Chosen(Action::Digit(i)) if (i as usize) < LEVELS.len() => {
                self.min = LEVELS[i as usize].0;
                self.refresh(ctx);
            }
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        if self.refreshed.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh(ctx);
        }
        ui.ctx().request_repaint_after(REFRESH);
        let theme = ctx.theme;
        let t = &theme.tokens;
        let mut menu = false;
        let mut export = false;
        section::page_header(ui, ctx, "Logs", |_| {});
        ui.add_space(theme.space(SpaceToken::Sm));
        // buttons first (right to left), the filter header gets the rest of the row
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                export = button(ui, theme, Some(glyphs::BACKUP), "Export…", ButtonStyle { bordered: true, ..Default::default() }).clicked();
                let label = LEVELS.iter().find(|(l, ..)| *l == self.min).map(|(.., s)| *s).unwrap_or("Info");
                menu = button(ui, theme, None, &format!("{label} ▾"), ButtonStyle::default()).clicked();
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    search_header(ui, theme, self.list.filter.query(), "Type to filter the log", Some(glyphs::SEARCH));
                });
            });
        });
        if std::mem::take(&mut menu) {
            self.view_menu(ctx);
        }
        if export {
            self.export(ctx);
        }
        ui.add_space(theme.space(SpaceToken::Md));
        if self.rows.is_empty() {
            let q = self.list.filter.query().to_owned();
            if q.is_empty() {
                empty_state::empty_state(
                    ui,
                    theme,
                    glyphs::INFO,
                    "The log is empty",
                    Some("Turn on debug logging in Settings ▸ About & logs for more detail."),
                    None,
                );
            } else {
                empty_state::empty_state(ui, theme, glyphs::SEARCH, &format!("No matches for “{q}”"), None, None);
            }
            return;
        }
        let expanded_row = self.expanded.as_ref().and_then(|id| self.row_of(id));
        let detail_h = if expanded_row.is_some() { (ui.available_height() * 0.3).max(80.0) } else { 0.0 };
        let h24 = ctx.config.interface.clock_24h;
        let list_h = (ui.available_height() - detail_h).max(60.0);
        let row_h = (theme.font(TypeToken::BodySmall) + 2.0 * theme.space(SpaceToken::Md)).round();
        let (lines, rows) = (&self.lines, &self.rows);
        let mut activated = None;
        ui.allocate_ui(egui::vec2(ui.available_width(), list_h), |ui| {
            let out = virtual_list(ui, theme, "log-lines", &mut self.list, row_h, |ui, rc| {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                let Some(line) = rows.get(rc.index).and_then(|i| lines.get(*i as usize)) else { return resp };
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &line.message));
                paint_row_state(ui.painter(), rect, theme, rc.state);
                let font = egui::FontId::monospace(theme.font(TypeToken::BodySmall));
                let lc = match line.level {
                    Level::ERROR => t.urgent,
                    Level::WARN => t.warning,
                    _ => t.text_secondary,
                };
                let p = ui.painter();
                let x = rect.left() + theme.space(SpaceToken::Md);
                let r =
                    p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, super::home::kit::hhmm(line.at, h24), font.clone(), t.text_secondary);
                let r = p.text(
                    egui::pos2(r.right() + theme.space(SpaceToken::Lg), rect.center().y),
                    Align2::LEFT_CENTER,
                    format!("{:>5}", level_label(line.level)),
                    font.clone(),
                    lc,
                );
                let color = if rc.state.is_cursor() { t.list_cursor_text } else { t.fg };
                let text = format!("{}: {}", line.target, line.message);
                let mut job = egui::text::LayoutJob::simple_singleline(text, font, color);
                job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - r.right() - 2.0 * theme.space(SpaceToken::Lg)).max(0.0));
                let g = p.layout_job(job);
                p.galley(egui::pos2(r.right() + theme.space(SpaceToken::Lg), rect.center().y - g.size().y / 2.0), g, color);
                resp
            });
            match out {
                ListOutcome::Activated(i) => activated = Some(i),
                // right-click: the menu key's view menu
                ListOutcome::ContextMenu(_) => menu = true,
                _ => {}
            }
        });
        // a click does what Enter does: open or close the line's detail
        if let Some(i) = activated {
            self.toggle_expanded(i);
        }
        if menu {
            self.view_menu(ctx);
        }
        if let Some(line) = self.expanded.as_ref().and_then(|id| self.row_of(id)).and_then(|i| self.line_at(i)) {
            ui.add_space(theme.space(SpaceToken::Md));
            crate::ui::widgets::card::card(ui, theme, crate::ui::widgets::card::CardKind::Pane, |ui| {
                egui::ScrollArea::vertical().id_salt("log-detail").max_height(detail_h).show(ui, |ui| {
                    let heading = format!("{} {} {}", super::home::kit::date(line.at), level_label(line.level), line.target);
                    if button(ui, theme, Some(glyphs::COPY), "Copy line", ButtonStyle::default()).clicked() {
                        ui.ctx().copy_text(format!("{heading}\n{}", line.message));
                    }
                    ui.label(RichText::new(heading).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                    ui.add(egui::Label::new(RichText::new(&line.message).monospace().size(theme.font(TypeToken::Body)).color(t.fg)).wrap());
                });
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::home::kit;
    use crate::ui::screens::home::testkit::{harness, has_label, rig};

    fn line(level: Level, msg: &str) -> LogLine {
        LogLine { at: 1_791_576_000, level, target: "tuner::test".into(), message: msg.into() }
    }

    #[test]
    fn clicking_log_text_expands_it_and_copy_line_copies_details() {
        use crate::ui::testing::clicks;
        let (t, one) = rig();
        t.services().logs.push(line(Level::ERROR, "Couldn't refresh: HTTP 404"));
        let mut h = harness(t, one, LogsScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::Logs);
        h.run_steps(2);
        clicks::click_text(&mut h, "tuner::test: Couldn't refresh: HTTP 404");
        assert!(h.state().s.expanded.is_some());
        h.run_steps(2);
        clicks::click_text(&mut h, "Copy line");
        let copied = clicks::copied_text(&h).expect("clipboard command");
        assert!(copied.contains("ERROR tuner::test\nCouldn't refresh: HTTP 404"), "{copied}");
        assert!(h.state().s.expanded.is_some());
    }

    #[test]
    fn filter_expand_level_and_export_without_file_io() {
        let (t, one) = rig();
        t.services().logs.push(line(Level::INFO, "imported 120 channels"));
        t.services().logs.push(line(Level::WARN, "Couldn't refresh My IPTV: HTTP 404"));
        t.services().logs.push(line(Level::ERROR, "player error: boom"));
        let dir = t.services().paths.state_dir.clone();
        let mut h = harness(t, one, LogsScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::Logs);
        h.run_steps(2);
        assert!(has_label(&h, "imported 120 channels") && has_label(&h, "player error: boom"));
        // type to filter
        for c in "refresh".chars() {
            h.state_mut().act(Action::FilterInput(c));
        }
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::ListFiltering]);
        h.run_steps(1);
        assert_eq!(h.state().s.rows.len(), 1);
        // Enter expands the line
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert!(h.state().s.expanded.is_some());
        h.state_mut().act(Action::FilterClear);
        // Ctrl+S: the view menu — errors only, and Export…
        h.state_mut().act(Action::SortMenu);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [crate::ui::NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items.iter().any(|i| i.label == "Export…")),
            "{nav:?}"
        );
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(0)));
        assert_eq!(h.state().s.rows.len(), 1, "errors only");
        let before: Vec<_> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_EXPORT)));
        assert_eq!(kit::PICKED.with_borrow_mut(std::mem::take), vec![PickPurpose::ExportLog], "only the save dialog");
        let after: Vec<_> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        assert_eq!(before, after, "the screen writes no file (the app exports when the dialog answers)");
    }

    /// The expanded line and the cursor stay on THEIR line across the once-a-second refresh
    /// while new lines arrive (correctness review M3; they are tracked by identity, so a
    /// rotating ring can't move them to another line either).
    #[test]
    fn the_expanded_line_survives_refreshes() {
        use egui_kittest::kittest::Queryable;
        let (t, one) = rig();
        for i in 0..5 {
            t.services().logs.push(line(Level::INFO, &format!("line {i}")));
        }
        let mut h = harness(t, one, LogsScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::Logs);
        h.run_steps(2);
        // Enter on "line 2"
        h.state_mut().act(Action::Home);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        let count = |h: &egui_kittest::Harness<'static, crate::ui::screens::home::testkit::App<LogsScreen>>| h.query_all_by_label("line 2").count();
        assert_eq!(count(&h), 2, "the row and the expanded card");
        // a new line arrives; the timed refresh runs twice
        let refresh = |h: &mut egui_kittest::Harness<'static, crate::ui::screens::home::testkit::App<LogsScreen>>| {
            let app = h.state_mut();
            app.s.refreshed = Instant::now().checked_sub(REFRESH * 2);
            h.run_steps(1);
        };
        h.state().t.services().logs.push(line(Level::INFO, "line 5"));
        refresh(&mut h);
        refresh(&mut h);
        assert_eq!(count(&h), 2, "still expanded after the timed refreshes");
        let s = &h.state().s;
        assert_eq!(s.line_at(s.list.cursor).map(|l| l.message.as_str()), Some("line 2"), "the cursor stays on its line");
        // a new filter starts from a clean view
        h.state_mut().act(Action::FilterInput('4'));
        h.run_steps(1);
        assert!(h.state().s.expanded.is_none());
    }

    #[test]
    fn levels_cover_the_menu() {
        assert_eq!(LEVELS.len(), 5);
        assert!((MENU_EXPORT as usize) >= LEVELS.len(), "menu codes don't collide");
        assert_eq!(level_label(Level::WARN), "WARN");
    }
}
