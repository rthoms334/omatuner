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
use crate::ui::widgets::{empty_state, glyphs};
use crate::ui::{OverlayRequest, Route, Screen, ScreenId, UiCtx};

const LEVELS: [(Level, &str); 5] = [(Level::ERROR, "Errors"), (Level::WARN, "Warnings"), (Level::INFO, "Info"), (Level::DEBUG, "Debug"), (Level::TRACE, "Everything")];
/// View-menu code of "Export…" (levels use their index).
const MENU_EXPORT: u8 = 10;
const REFRESH: Duration = Duration::from_secs(1);

pub struct LogsScreen {
    min: Level,
    lines: Vec<LogLine>,
    /// Displayed lines (indices into `lines`) for the filter.
    rows: Vec<u32>,
    list: ListState,
    /// The line shown in full below the list.
    expanded: Option<usize>,
    refreshed: Option<Instant>,
}

impl Default for LogsScreen {
    fn default() -> Self {
        Self { min: Level::INFO, lines: Vec::new(), rows: Vec::new(), list: ListState::default(), expanded: None, refreshed: None }
    }
}

impl LogsScreen {
    fn refresh(&mut self, ctx: &UiCtx<'_>) {
        let follow = self.rows.is_empty() || self.list.cursor + 1 >= self.rows.len();
        self.lines = ctx.services.logs.snapshot(self.min);
        self.refilter();
        if follow {
            self.list.select(self.rows.len().saturating_sub(1));
        }
        self.refreshed = Some(Instant::now());
    }

    fn refilter(&mut self) {
        let texts: Vec<String> = self.lines.iter().map(|l| format!("{} {} {}", l.level, l.target, l.message)).collect();
        self.rows = self.list.filter.apply(texts.iter().map(String::as_str));
        self.list.set_len(self.rows.len());
        self.expanded = None;
    }

    fn view_menu(&self, ctx: &mut UiCtx<'_>) {
        let mut items: Vec<MenuItem> = LEVELS
            .iter()
            .enumerate()
            .map(|(i, (l, label))| MenuItem {
                icon: if *l == self.min { glyphs::CHECK } else { "" },
                label: format!("Show {label}"),
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
    fn id(&self) -> ScreenId { ScreenId::Logs }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![self.list.key_context()] }

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
                true
            }
            ListOutcome::Activated(i) | ListOutcome::ActivatedAlt(i) => {
                self.expanded = if self.expanded == Some(i) { None } else { Some(i) };
                true
            }
            ListOutcome::Moved => true,
            ListOutcome::ContextMenu(_) => {
                self.view_menu(ctx);
                true
            }
            // ←/Backspace on an empty filter: back to Settings (the app's Back)
            ListOutcome::Parent => false,
            ListOutcome::None => matches!(action, Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) | Action::Activate),
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
        ui.horizontal(|ui| {
            let w = ui.available_width();
            let side = (260.0 * theme.rem()).min(w * 0.5);
            ui.allocate_ui(egui::vec2(w - side, ui.available_height()), |ui| {
                search_header(ui, theme, self.list.filter.query(), "Filter the log (type)", Some(glyphs::SEARCH));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                export = button(ui, theme, Some(glyphs::BACKUP), "Export…", ButtonStyle { bordered: true, ..Default::default() }).clicked();
                let label = LEVELS.iter().find(|(l, _)| *l == self.min).map(|(_, s)| *s).unwrap_or("Info");
                menu = button(ui, theme, None, &format!("{label} ▾ (Ctrl+S)"), ButtonStyle::default()).clicked();
            });
        });
        if menu {
            self.view_menu(ctx);
        }
        if export {
            self.export(ctx);
        }
        ui.add_space(theme.space(SpaceToken::Md));
        if self.rows.is_empty() {
            let q = self.list.filter.query().to_owned();
            if q.is_empty() {
                empty_state::empty_state(ui, theme, glyphs::INFO, "The log is empty", Some("Turn on debug logging in Settings ▸ About & logs for more detail."), None);
            } else {
                empty_state::empty_state(ui, theme, glyphs::SEARCH, &format!("No matches for “{q}”"), None, None);
            }
            return;
        }
        let detail_h = if self.expanded.is_some() { (ui.available_height() * 0.3).max(80.0) } else { 0.0 };
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
                let r = p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, super::home::kit::hhmm(line.at), font.clone(), t.text_secondary);
                let r = p.text(egui::pos2(r.right() + theme.space(SpaceToken::Lg), rect.center().y), Align2::LEFT_CENTER, format!("{:>5}", level_label(line.level)), font.clone(), lc);
                let color = if rc.state.is_cursor() { t.list_cursor_text } else { t.fg };
                let text = format!("{}: {}", line.target, line.message);
                let mut job = egui::text::LayoutJob::simple_singleline(text, font, color);
                job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - r.right() - 2.0 * theme.space(SpaceToken::Lg)).max(0.0));
                let g = p.layout_job(job);
                p.galley(egui::pos2(r.right() + theme.space(SpaceToken::Lg), rect.center().y - g.size().y / 2.0), g, color);
                resp
            });
            if let ListOutcome::Activated(i) = out {
                activated = Some(i);
            }
        });
        if let Some(i) = activated {
            self.expanded = Some(i);
        }
        if let Some(line) = self.expanded.and_then(|i| self.rows.get(i)).and_then(|i| self.lines.get(*i as usize)) {
            ui.add_space(theme.space(SpaceToken::Md));
            crate::ui::widgets::card::card(ui, theme, crate::ui::widgets::card::CardKind::Pane, |ui| {
                egui::ScrollArea::vertical().id_salt("log-detail").max_height(detail_h).show(ui, |ui| {
                    ui.label(RichText::new(format!("{} {} {}", super::home::kit::date(line.at), level_label(line.level), line.target)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
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

    fn line(level: Level, msg: &str) -> LogLine { LogLine { at: 1_791_576_000, level, target: "tuner::test".into(), message: msg.into() } }

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
        assert!(matches!(nav.as_slice(), [crate::ui::NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items.iter().any(|i| i.label == "Export…")), "{nav:?}");
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(0)));
        assert_eq!(h.state().s.rows.len(), 1, "errors only");
        let before: Vec<_> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_EXPORT)));
        assert_eq!(kit::PICKED.with_borrow_mut(std::mem::take), vec![PickPurpose::ExportLog], "only the save dialog");
        let after: Vec<_> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        assert_eq!(before, after, "the screen writes no file (the app exports when the dialog answers)");
    }

    #[test]
    fn levels_cover_the_menu() {
        assert_eq!(LEVELS.len(), 5);
        assert!((MENU_EXPORT as usize) >= LEVELS.len(), "menu codes don't collide");
        assert_eq!(level_label(Level::WARN), "WARN");
    }
}
