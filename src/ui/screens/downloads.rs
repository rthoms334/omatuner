//! Downloads (Alt+8): one cached snapshot for the five sections, keyboard and drag actions
//! (DESIGN §43 §2.1, §2.5). Delete waits for Confirm and a ten-second Undo before unlinking.
use crate::downloads::{DownloadCommand, Job, State};
use crate::keymap::{Action, KeyContext};
use crate::model::DownloadId;
use crate::ui::downloads as shared;
use crate::ui::overlays::{ConfirmSpec, ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::{OverlayRequest, Screen, ScreenId, UiCtx};
use eframe::egui;

const DELETE: u64 = 0x444f574e;

#[derive(Default)]
pub struct DownloadsScreen {
    cursor: usize,
    selected: Option<DownloadId>,
    delete: Vec<DownloadId>,
    drag: Option<DownloadId>,
}

fn section(job: &Job) -> u8 {
    match job.state {
        State::Running | State::Finishing => 0,
        State::Waiting | State::Paused => 1,
        State::Scheduled => 2,
        State::Done | State::Partial => 3,
        _ => 4,
    }
}

impl DownloadsScreen {
    fn rows(&self, ctx: &UiCtx<'_>) -> Vec<Job> {
        let mut rows = shared::snapshot(ctx).jobs.clone();
        rows.sort_by_key(|j| (section(j), if section(j) == 3 { j.title.clone() } else { String::new() }, j.position));
        rows
    }
    fn current(&self, ctx: &UiCtx<'_>) -> Option<Job> {
        let rows = self.rows(ctx);
        self.selected.and_then(|id| rows.iter().find(|j| j.id == id).cloned()).or_else(|| rows.get(self.cursor).cloned())
    }
    fn confirm(&mut self, ctx: &mut UiCtx<'_>, ids: Vec<DownloadId>) {
        self.delete = ids;
        ctx.open(OverlayRequest::Confirm(ConfirmSpec {
            title: "Delete download?".into(),
            body: "The file will be removed from this computer. Undo is available for 10 seconds.".into(),
            confirm_label: "Delete".into(),
            destructive: true,
            token: DELETE,
        }));
    }
    fn menu(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(job) = self.current(ctx) else { return };
        self.selected = Some(job.id);
        let entry = |label: &str, code| MenuItem { icon: "⤓", label: label.into(), action: Action::Digit(code), enabled: true, shortcut: None };
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec {
            title: job.title,
            items: vec![
                entry("Play", 200),
                entry("Pause / resume", 201),
                entry("Retry", 202),
                entry("Move to top", 203),
                entry("Download from another copy…", 204),
                entry("Delete…", 205),
                entry("Delete watched…", 206),
            ],
        }));
    }
}
impl Screen for DownloadsScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Downloads
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Downloads]
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        let rows = self.rows(ctx);
        let current = self.current(ctx);
        match action {
            Action::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                self.selected = rows.get(self.cursor).map(|j| j.id);
            }
            Action::Down => {
                self.cursor = (self.cursor + 1).min(rows.len().saturating_sub(1));
                self.selected = rows.get(self.cursor).map(|j| j.id);
            }
            Action::Activate => {
                if let Some(j) = current {
                    shared::play(ctx, j.id);
                }
            }
            Action::PlayPause => {
                if let Some(j) = current {
                    shared::send(
                        ctx,
                        if j.state == State::Paused { DownloadCommand::Resume(vec![j.id]) } else { DownloadCommand::Pause(vec![j.id]) },
                    );
                }
            }
            Action::MoveUp | Action::MoveDown => {
                if let Some(j) = current {
                    let to = if action == Action::MoveUp { j.position.saturating_sub(1).max(0) } else { j.position + 1 };
                    shared::send(ctx, DownloadCommand::Move { id: j.id, to: to as usize });
                }
            }
            Action::Remove => {
                if let Some(j) = current {
                    self.confirm(ctx, vec![j.id]);
                }
            }
            Action::ContextMenu => self.menu(ctx),
            _ => return false,
        }
        true
    }
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Confirmed(DELETE) => {
                for id in std::mem::take(&mut self.delete) {
                    shared::deleted(ctx, id);
                }
            }
            OverlayResult::Chosen(Action::Digit(code)) => {
                let Some(job) = self.current(ctx) else { return };
                match code {
                    200 => {
                        self.on_action(ctx, Action::Activate);
                    }
                    201 => {
                        self.on_action(ctx, Action::PlayPause);
                    }
                    202 => shared::send(ctx, DownloadCommand::Retry(vec![job.id])),
                    203 => shared::send(ctx, DownloadCommand::Move { id: job.id, to: 0 }),
                    204 => {
                        if let Ok(media) = crate::downloads::planning::media(ctx.db, &job) {
                            let target = match media {
                                crate::model::MediaRef::Movie(id) => Some(crate::ui::item_menu::Target::Movie(id)),
                                crate::model::MediaRef::Episode(episode) => crate::store::vod::episode(ctx.db, episode)
                                    .ok()
                                    .map(|e| crate::ui::item_menu::Target::Episode { series: e.series_id, episode }),
                                _ => None,
                            };
                            if let Some(target) = target {
                                ctx.open(OverlayRequest::Download(target));
                            }
                        }
                    }
                    205 => self.confirm(ctx, vec![job.id]),
                    206 => {
                        let ids = self.rows(ctx).iter().filter(|j| j.watched_at.is_some()).map(|j| j.id).collect();
                        self.confirm(ctx, ids);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let snap = shared::snapshot(ctx);
        let rows = self.rows(ctx);
        ui.heading("Downloads");
        ui.horizontal(|ui| {
            ui.label(format!(
                "{:.1} GB of {:.0} GB · {:.1} GB free",
                snap.used_bytes as f64 / 1e9,
                snap.cap_bytes as f64 / 1e9,
                snap.free_bytes as f64 / 1e9
            ));
            if ui.button(if snap.paused { "Resume all" } else { "Pause all" }).clicked() {
                shared::send(ctx, DownloadCommand::PauseAll(!snap.paused));
            }
        });
        if rows.is_empty() {
            ui.label("Download a film or episode, or record a programme with Ctrl+D.");
        }
        let mut play = None;
        let mut menu = None;
        let mut remove = None;
        let mut move_to = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut group = None;
            let mut series = String::new();
            for (i, job) in rows.iter().enumerate() {
                let s = section(job);
                if group != Some(s) {
                    ui.separator();
                    ui.heading(["Now", "Waiting", "Scheduled", "Downloaded", "Failed / Missed"][s as usize]);
                    group = Some(s);
                    series.clear();
                }
                if job.kind == crate::downloads::Kind::Episode && job.title != series {
                    let episodes: Vec<_> =
                        rows.iter().filter(|j| section(j) == s && j.kind == crate::downloads::Kind::Episode && j.title == job.title).collect();
                    let bytes: u64 =
                        episodes.iter().map(|j| if j.state.playable() { j.bytes_done } else { crate::store::downloads::estimate(j) }).sum();
                    let watched = episodes.iter().filter(|j| j.watched_at.is_some()).count();
                    ui.strong(format!("{} · {} episodes · {:.1} GB · {} watched", job.title, episodes.len(), bytes as f64 / 1e9, watched));
                    series = job.title.clone();
                }
                ui.horizontal(|ui| {
                    let grip = ui.add(egui::Label::new("⠿").sense(egui::Sense::drag()));
                    if grip.drag_started() {
                        self.drag = Some(job.id);
                    }
                    let name = job.subtitle.as_ref().filter(|_| job.kind == crate::downloads::Kind::Episode).unwrap_or(&job.title);
                    let r = ui.selectable_label(self.selected == Some(job.id) || self.selected.is_none() && self.cursor == i, name);
                    if r.clicked() {
                        self.cursor = i;
                        self.selected = Some(job.id);
                        play = Some(job.id);
                    }
                    if r.secondary_clicked() {
                        self.cursor = i;
                        self.selected = Some(job.id);
                        menu = Some(job.id);
                    }
                    if r.hovered()
                        && ui.input(|i| i.pointer.any_released())
                        && let Some(id) = self.drag.take()
                    {
                        move_to = Some((id, job.position as usize));
                    }
                    ui.label(shared::state_text(job));
                    if job.state.active() {
                        if let Some(p) = job.progress() {
                            ui.add(egui::ProgressBar::new(p as f32).desired_width(110.0));
                        } else {
                            ui.spinner();
                        }
                    }
                    if job.state.playable() {
                        ui.label(format!("{:.1} GB", job.bytes_done as f64 / 1e9));
                        if let Some(watched) = job.watched_at {
                            ui.label("✓ Watched");
                            let days = ctx.config.downloads.delete_after_watched_days;
                            if days > 0 {
                                let left = (watched + i64::from(days) * 86400 - ctx.now).max(0);
                                ui.label(format!("Deletes in {} days", (left + 86399) / 86400));
                            }
                        }
                    }
                    if job.state == State::Running
                        && job.kind.recording()
                        && let Some(stop) = job.stop()
                    {
                        ui.label(format!("{} min left", (stop - ctx.now).max(0) / 60));
                    }
                    if let Some(account) = job.source_id.and_then(|id| ctx.lib.source(id)) {
                        ui.label(&account.name);
                    } else {
                        ui.label("Playlist removed");
                    }
                    if job.state == State::Running {
                        let speed = job.fp.get("download_speed_bps").and_then(serde_json::Value::as_f64).unwrap_or(snap.speed_bps);
                        ui.label(format!("{:.1} MB/s", speed / 1e6));
                        if let Some(left) = job.bytes_total.map(|n| n.saturating_sub(job.bytes_done))
                            && speed > 0.0
                        {
                            ui.label(format!("~{} min", (left as f64 / speed / 60.0).ceil() as u64));
                        }
                    }
                    if job.state == State::Scheduled
                        && let Some(at) = job.prog_start
                    {
                        ui.label(crate::ui::widgets::time::day_time(at, ctx.config.interface.clock_24h));
                        if rows.iter().any(|other| {
                            other.id != job.id
                                && other.source_id == job.source_id
                                && (other.state == State::Running
                                    || other.state == State::Waiting
                                    || (other.state == State::Scheduled
                                        && other.start().zip(job.stop()).is_some_and(|(a, b)| a < b)
                                        && other.stop().zip(job.start()).is_some_and(|(a, b)| a > b)))
                        }) {
                            ui.label("⚠ Clashes with downloads on this account");
                        }
                    }
                    if ui.small_button("×").clicked() {
                        remove = Some(job.id);
                    }
                });
                if job.reason.as_deref() == Some("clash") {
                    ui.horizontal(|ui| {
                        for (label, record, alternate) in
                            [("Record (stops playback)", true, false), ("Use the other account", true, true), ("Skip this recording", false, false)]
                        {
                            if alternate && job.fp.get("download_alternate").and_then(|v| v.as_bool()) != Some(true) {
                                continue;
                            }
                            if ui.button(label).clicked() {
                                shared::send(ctx, DownloadCommand::Clash { id: job.id, record, alternate });
                            }
                        }
                    });
                }
            }
        });
        if let Some(id) = play {
            shared::play(ctx, id);
        }
        if menu.is_some() {
            self.menu(ctx);
        }
        if let Some(id) = remove {
            self.confirm(ctx, vec![id]);
        }
        if let Some((id, to)) = move_to {
            shared::send(ctx, DownloadCommand::Move { id, to });
        }
        if !ui.input(|i| i.pointer.any_down()) {
            self.drag = None;
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
}
