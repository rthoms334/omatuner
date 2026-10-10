//! Background tasks panel (Ctrl+J): running jobs with phase + progress bar; `Delete` (Remove)
//! cancels the highlighted job; last 20 finished with errors expandable (Enter; redacted).
//! Owner: WP-10a. Key contexts `[List, Modal]`.
//!
//! Running jobs come from `JobManager::active()` every frame. Finished jobs with errors come
//! from the log ring: the job system logs every failure at WARN/ERROR under `tuner::jobs`, and
//! log lines are redacted when recorded, so the panel needs no extra job-history state.

use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim};
use super::{Overlay, OverlayOutcome};
use crate::jobs::{JobPhase, JobStatus};
use crate::keymap::{Action, KeyContext};
use crate::logging::LogLine;
use crate::model::{JobId, Timestamp};
use crate::msg::Toast;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::progress::{busy_bar, progress_bar};
use crate::ui::widgets::section;

/// Problems listed (newest first).
const MAX_PROBLEMS: usize = 20;
/// Log target prefix of the job system.
const JOBS_TARGET: &str = "tuner::jobs";
/// How often the problem list is re-read from the log ring while the panel is open.
const PROBLEMS_REFRESH: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct JobsOverlay {
    /// Index into running jobs, then problems (one cursor over both sections).
    cursor: usize,
    running: Vec<JobStatus>,
    problems: Vec<LogLine>,
    /// Problem rows showing their full message (by content: the list is re-read newest first,
    /// so positions shift whenever a new problem is logged).
    expanded: Vec<ProblemKey>,
    problems_read: Option<Instant>,
}

/// Identity of a problem row across re-reads.
type ProblemKey = (Timestamp, String);

fn problem_key(line: &LogLine) -> ProblemKey { (line.at, line.message.clone()) }

/// What the cursor is on, so it stays on that row when the lists above it change.
enum CursorRow {
    Job(JobId),
    Problem(ProblemKey),
}

impl JobsOverlay {
    fn rows(&self) -> usize { self.running.len() + self.problems.len() }

    fn cursor_row(&self) -> Option<CursorRow> {
        match self.cursor.checked_sub(self.running.len()) {
            None => self.running.get(self.cursor).map(|j| CursorRow::Job(j.id)),
            Some(p) => self.problems.get(p).map(|l| CursorRow::Problem(problem_key(l))),
        }
    }

    /// Re-read running jobs (every frame) and problems (throttled) from the services.
    fn refresh(&mut self, ctx: &UiCtx<'_>) {
        let on = self.cursor_row();
        self.running = ctx.services.jobs.active();
        self.running.sort_by_key(|j| (j.started_at, j.id));
        if self.problems_read.is_none_or(|t| t.elapsed() >= PROBLEMS_REFRESH) {
            self.problems = job_problems(&ctx.services.logs.snapshot(tracing::Level::WARN));
            self.problems_read = Some(Instant::now());
            let problems = &self.problems;
            self.expanded.retain(|k| problems.iter().any(|l| l.at == k.0 && l.message == k.1));
        }
        // follow the row (a finished job or a new problem shifts the positions)
        let found = on.and_then(|on| match on {
            CursorRow::Job(id) => self.running.iter().position(|j| j.id == id),
            CursorRow::Problem((at, msg)) => self.problems.iter().position(|l| l.at == at && l.message == msg).map(|p| self.running.len() + p),
        });
        self.cursor = found.unwrap_or(self.cursor).min(self.rows().saturating_sub(1));
    }
}

/// The newest [`MAX_PROBLEMS`] warning/error lines of the job system, newest first.
fn job_problems(lines: &[LogLine]) -> Vec<LogLine> {
    lines.iter().rev().filter(|l| l.target.starts_with(JOBS_TARGET) && l.level <= tracing::Level::WARN).take(MAX_PROBLEMS).cloned().collect()
}

/// "Downloading 12.3 MB of 40.0 MB", "Parsing · 12 345 items"…
fn phase_text(p: &JobPhase) -> String {
    fn mb(b: u64) -> String { format!("{:.1} MB", b as f64 / 1_000_000.0) }
    match p {
        JobPhase::Queued => "Waiting".into(),
        JobPhase::Connecting => "Connecting".into(),
        JobPhase::Downloading { done, total: Some(t) } => format!("Downloading {} of {}", mb(*done), mb(*t)),
        JobPhase::Downloading { done, total: None } => format!("Downloading {}", mb(*done)),
        JobPhase::Parsing { items } => format!("Reading · {items} items"),
        JobPhase::Writing { done, total: Some(t) } => format!("Saving {done} of {t}"),
        JobPhase::Writing { done, total: None } => format!("Saving {done}"),
        JobPhase::Matching => "Matching guide channels".into(),
        JobPhase::Indexing => "Indexing".into(),
        JobPhase::Finishing => "Finishing".into(),
    }
}

/// "12 s", "3 min", "1 h 05 min" since `started`.
fn elapsed_text(now: Timestamp, started: Timestamp) -> String {
    let s = (now - started).max(0);
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        _ => format!("{} h {:02} min", s / 3600, (s / 60) % 60),
    }
}

impl Overlay for JobsOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::List, KeyContext::Modal] }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        let rows = self.rows();
        match action {
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => self.cursor = (self.cursor + 1).min(rows.saturating_sub(1)),
            Action::PageUp | Action::Home => self.cursor = 0,
            Action::PageDown | Action::End => self.cursor = rows.saturating_sub(1),
            Action::Remove => match self.running.get(self.cursor) {
                Some(job) => {
                    ctx.services.jobs.cancel(job.id);
                    ctx.toast(Toast::info(format!("Cancelling “{}”…", job.label)));
                }
                None => return OverlayOutcome::Keep,
            },
            Action::Activate => {
                if let Some(line) = self.cursor.checked_sub(self.running.len()).and_then(|p| self.problems.get(p)) {
                    let key = problem_key(line);
                    match self.expanded.iter().position(|e| *e == key) {
                        Some(i) => {
                            self.expanded.remove(i);
                        }
                        None => self.expanded.push(key),
                    }
                }
            }
            Action::Back | Action::Parent | Action::ShowJobs => return OverlayOutcome::Close,
            // typing doesn't filter this short panel; swallow it rather than reach the globals
            Action::FilterInput(_) => {}
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.refresh(ctx);
        let theme = ctx.theme;
        if scrim(egui_ctx, theme, "jobs") {
            return OverlayOutcome::Close;
        }
        let now = ctx.now;
        let h24 = ctx.config.interface.clock_24h;
        let mut clicked = None;
        card_area(egui_ctx, theme, "jobs", CardKind::Menu, 560.0 * theme.rem(), None, |ui| {
            let t = &theme.tokens;
            ui.label(RichText::new("Background tasks").size(theme.font(TypeToken::Heading)).color(t.fg));
            ui.add_space(theme.space(SpaceToken::Md));
            egui::ScrollArea::vertical().max_height(egui_ctx.content_rect().height() * 0.65).show(ui, |ui| {
                section::section_header(ui, theme, "Running");
                if self.running.is_empty() {
                    ui.label(RichText::new("Nothing is running").size(theme.font(TypeToken::Body)).color(t.text_secondary));
                }
                for (i, job) in self.running.iter().enumerate() {
                    if running_row(ui, theme, job, now, i == self.cursor).clicked() {
                        clicked = Some(i);
                    }
                }
                ui.add_space(theme.space(SpaceToken::Lg));
                section::section_header(ui, theme, "Recent problems");
                if self.problems.is_empty() {
                    ui.label(RichText::new("No problems").size(theme.font(TypeToken::Body)).color(t.text_secondary));
                }
                for (p, line) in self.problems.iter().enumerate() {
                    let row = self.running.len() + p;
                    let expanded = self.expanded.iter().any(|(at, msg)| *at == line.at && *msg == line.message);
                    if problem_row(ui, theme, line, p, expanded, row == self.cursor, h24).clicked() {
                        clicked = Some(row);
                    }
                }
            });
            ui.add_space(theme.space(SpaceToken::Md));
            ui.label(RichText::new("Delete cancel · Enter show details · Esc close").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        });
        if let Some(row) = clicked {
            self.cursor = row;
            if row >= self.running.len() {
                return self.on_action(ctx, Action::Activate);
            }
        }
        OverlayOutcome::Keep
    }
}

/// Label + phase + elapsed, with a progress (or busy) bar below.
fn running_row(ui: &mut egui::Ui, theme: &Theme, job: &JobStatus, now: Timestamp, cursor: bool) -> egui::Response {
    let t = &theme.tokens;
    let pad = theme.space(SpaceToken::Lg);
    let inner = ui.scope(|ui| {
        let top = ui.cursor().min;
        let bg = ui.painter().add(egui::Shape::Noop);
        ui.add_space(theme.space(SpaceToken::Sm));
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.label(RichText::new(&job.label).size(theme.font(TypeToken::Title)).color(if cursor { t.list_cursor_text } else { t.fg }));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(pad);
                ui.label(RichText::new(elapsed_text(now, job.started_at)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            });
        });
        ui.horizontal(|ui| {
            ui.add_space(pad);
            let pct = job.fraction.filter(|f| f.is_finite()).map(|f| format!(" · {:.0} %", f.clamp(0.0, 1.0) * 100.0)).unwrap_or_default();
            ui.label(RichText::new(format!("{}{pct}", phase_text(&job.phase))).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        });
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.set_width(ui.available_width() - pad);
            let h = (3.0 * theme.rem()).round().max(2.0);
            match job.fraction {
                Some(f) => progress_bar(ui, theme, f, h),
                None => busy_bar(ui, theme, h),
            };
        });
        ui.add_space(theme.space(SpaceToken::Sm));
        let rect = egui::Rect::from_min_max(top, egui::pos2(ui.max_rect().right(), ui.cursor().min.y));
        ui.painter().set(bg, row_bg(theme, rect, cursor));
        rect
    });
    // registered after the labels, so it gets the clicks; the labels carry the accesskit text
    ui.interact(inner.inner, ui.id().with(("job", job.id)), egui::Sense::click())
}

/// "20:14  Couldn't refresh My IPTV: HTTP 404" (one line, or the full message when expanded;
/// "8:14 PM" on the 12-hour clock). `index` salts the row id: the same failure can be logged
/// twice within a second.
fn problem_row(ui: &mut egui::Ui, theme: &Theme, line: &LogLine, index: usize, expanded: bool, cursor: bool, h24: bool) -> egui::Response {
    let t = &theme.tokens;
    let pad = theme.space(SpaceToken::Lg);
    let color = if cursor { t.list_cursor_text } else { t.fg };
    let time = crate::ui::widgets::time::hhmm(line.at, h24);
    let width = ui.available_width();
    let inner = ui.scope(|ui| {
        let top = ui.cursor().min;
        let bg = ui.painter().add(egui::Shape::Noop);
        ui.add_space(theme.space(SpaceToken::Sm));
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.label(RichText::new(time).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            ui.add_space(theme.space(SpaceToken::Md));
            let text = RichText::new(&line.message).size(theme.font(TypeToken::Body)).color(color);
            ui.set_max_width(width - 2.0 * pad);
            if expanded {
                ui.add(egui::Label::new(text).wrap());
            } else {
                ui.add(egui::Label::new(text).truncate());
            }
        });
        ui.add_space(theme.space(SpaceToken::Sm));
        let rect = egui::Rect::from_min_max(top, egui::pos2(top.x + width, ui.cursor().min.y));
        ui.painter().set(bg, row_bg(theme, rect, cursor));
        rect
    });
    ui.interact(inner.inner, ui.id().with(("problem", index)), egui::Sense::click())
}

/// Cursor background for a row whose height is only known after its content was laid out
/// (painted into a placeholder shape reserved before the content).
fn row_bg(theme: &Theme, rect: egui::Rect, cursor: bool) -> egui::Shape {
    if cursor { egui::Shape::rect_filled(rect, theme.tokens.rounding, theme.tokens.list_cursor_bg) } else { egui::Shape::Noop }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(target: &str, level: tracing::Level, msg: &str) -> LogLine { LogLine { at: 0, level, target: target.into(), message: msg.into() } }

    #[test]
    fn problems_are_job_warnings_newest_first() {
        let mut lines = vec![
            line("tuner::jobs::import_source", tracing::Level::ERROR, "old"),
            line("tuner::player", tracing::Level::ERROR, "player"),
            line("tuner::jobs", tracing::Level::INFO, "info"),
            line("tuner::jobs", tracing::Level::WARN, "new"),
        ];
        let p = job_problems(&lines);
        assert_eq!(p.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(), ["new", "old"]);
        lines = (0..30).map(|i| line("tuner::jobs", tracing::Level::WARN, &i.to_string())).collect();
        let p = job_problems(&lines);
        assert_eq!(p.len(), MAX_PROBLEMS);
        assert_eq!(p[0].message, "29");
    }

    #[test]
    fn phase_and_elapsed_copy() {
        assert_eq!(phase_text(&JobPhase::Downloading { done: 12_300_000, total: Some(40_000_000) }), "Downloading 12.3 MB of 40.0 MB");
        assert_eq!(phase_text(&JobPhase::Writing { done: 2000, total: None }), "Saving 2000");
        assert_eq!(elapsed_text(100, 88), "12 s");
        assert_eq!(elapsed_text(1000, 800), "3 min");
        assert_eq!(elapsed_text(4000, 100), "1 h 05 min");
        assert_eq!(elapsed_text(0, 50), "0 s");
    }
}
