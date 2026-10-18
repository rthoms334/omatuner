//! Downloads: a compact queue above the title library and each series' saved episodes
//! (DESIGN §43 §2.2, §2.5). Delete waits for Confirm and ten seconds of Undo before unlinking.
use crate::downloads::{DownloadCommand, DownloadsSnapshot, Job, KeepRule, SeriesScope, State};
use crate::keymap::{Action, KeyContext};
use crate::model::{DownloadId, VodKind};
use crate::ui::downloads::{
    self as shared,
    grouping::{self, Episode, Title, TitleKey, Titles},
};
use crate::ui::overlays::{ConfirmSpec, ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, UiCtx};
use eframe::egui;

const DELETE: u64 = 0x444f574e;

#[derive(Clone, PartialEq)]
enum RowKey {
    Job(DownloadId),
    Title(TitleKey),
    Rule(i64),
}

#[derive(Clone)]
enum Row {
    Job { episode: Box<Episode>, title: String, queue: bool },
    Title(Title),
    Rule(KeepRule),
}
impl Row {
    fn key(&self) -> RowKey {
        match self {
            Self::Job { episode, .. } => RowKey::Job(episode.job.id),
            Self::Title(t) => RowKey::Title(t.key.clone()),
            Self::Rule(r) => RowKey::Rule(r.id),
        }
    }
    fn job(&self) -> Option<&Job> {
        match self {
            Self::Job { episode, .. } => Some(&episode.job),
            Self::Title(t) => t.jobs.iter().max_by_key(|j| (j.state == State::Done, j.state.playable(), j.done_at, j.id.0)),
            Self::Rule(_) => None,
        }
    }
    fn ids(&self) -> Vec<DownloadId> {
        match self {
            Self::Title(t) => t.jobs.iter().map(|j| j.id).collect(),
            _ => self.job().map(|j| vec![j.id]).unwrap_or_default(),
        }
    }
    fn section(&self) -> &'static str {
        match self {
            Self::Job { queue: true, .. } => "Downloading",
            Self::Job { .. } => "Failed / Missed",
            Self::Title(_) => "Downloaded",
            Self::Rule(_) => "Kept topped up",
        }
    }
}

#[derive(Default)]
pub struct DownloadsScreen {
    cursor: usize,
    selected: Option<RowKey>,
    page: Option<Title>,
    back: Option<(usize, RowKey)>,
    titles: Titles,
    delete: Vec<DownloadId>,
    drag: Option<DownloadId>,
    keep: u32,
    scroll: bool,
}

impl DownloadsScreen {
    fn rows(&mut self, ctx: &UiCtx<'_>, snap: &DownloadsSnapshot) -> Vec<Row> {
        if self.page.as_ref().is_some_and(|p| {
            matches!(&p.key, TitleKey::Item(i) if ctx.lib.conceals(i)
            || (ctx.parental.hides_explicit(&ctx.config.parental) && ctx.lib.explicit.contains(i)))
        }) {
            self.leave();
        }
        let titles = self.titles.list(ctx.lib, ctx.parental.hides_explicit(&ctx.config.parental), &snap.jobs);
        if let Some(page) = &mut self.page {
            if let Some(current) = titles.iter().find(|t| t.key == page.key) {
                *page = current.clone();
            } else {
                page.jobs.clear();
            }
            return page
                .episodes()
                .into_iter()
                .map(|episode| Row::Job { episode: Box::new(episode), title: page.name.clone(), queue: false })
                .collect();
        }
        let mut queue = Vec::new();
        let mut failed = Vec::new();
        for title in &titles {
            for job in &title.jobs {
                let waiting = grouping::downloading(job);
                if waiting || (!job.state.playable() && !title.saved()) {
                    let row = Row::Job {
                        episode: Box::new(Episode { job: job.clone(), others: vec![], watched: job.watched_at.is_some() }),
                        title: title.name.clone(),
                        queue: waiting,
                    };
                    if waiting {
                        queue.push(row);
                    } else {
                        failed.push(row);
                    }
                }
            }
        }
        queue.sort_by_key(|r| r.job().map(|j| j.position));
        failed.sort_by_key(|r| r.job().map(|j| j.position));
        queue.extend(titles.into_iter().filter(Title::saved).map(Row::Title));
        queue.extend(failed);
        queue.extend(snap.rules.iter().cloned().map(Row::Rule));
        queue
    }
    fn select(&mut self, rows: &[Row]) {
        self.cursor =
            self.selected.as_ref().and_then(|key| rows.iter().position(|r| &r.key() == key)).unwrap_or(self.cursor).min(rows.len().saturating_sub(1));
        self.selected = rows.get(self.cursor).map(Row::key);
    }
    fn activate(&mut self, ctx: &mut UiCtx<'_>, row: &Row) {
        if let Row::Title(title) = row
            && title.series
        {
            self.back = Some((self.cursor, row.key()));
            self.page = Some(title.clone());
            self.cursor = 0;
            self.selected = None;
            self.keep = 3;
            self.scroll = true;
        } else if let Some(job) = row.job() {
            shared::play(ctx, job.id);
        }
    }
    fn leave(&mut self) -> bool {
        if self.page.take().is_none() {
            return false;
        }
        if let Some((cursor, selected)) = self.back.take() {
            self.cursor = cursor;
            self.selected = Some(selected);
        }
        self.scroll = true;
        true
    }
    fn confirm(&mut self, ctx: &mut UiCtx<'_>, ids: Vec<DownloadId>) {
        if ids.is_empty() {
            return;
        }
        self.delete = ids;
        ctx.open(OverlayRequest::Confirm(ConfirmSpec {
            title: "Delete download?".into(),
            body: "The file will be removed from this computer. Undo is available for 10 seconds.".into(),
            confirm_label: "Delete".into(),
            destructive: true,
            token: DELETE,
        }));
    }
    fn menu(&mut self, ctx: &mut UiCtx<'_>, row: &Row) {
        let Some(job) = row.job() else { return };
        let entry = |label: &str, code| MenuItem { icon: "⤓", label: label.into(), action: Action::Digit(code), enabled: true, shortcut: None };
        let series = matches!(row, Row::Title(t) if t.series);
        let mut items = vec![entry(if series { "Episodes" } else { "Play" }, 200)];
        if !series && grouping::downloading(job) {
            items.extend([entry("Pause / resume", 201), entry("Cancel", 208)]);
        }
        if matches!(row, Row::Job { queue: true, .. }) {
            items.push(entry("Move to top", 203));
        }
        if !series {
            items.extend([entry("Retry", 202), entry("Download from another copy…", 204)]);
        }
        items.extend([entry("Delete…", 205), entry("Delete watched…", 206)]);
        if matches!(row, Row::Job { episode, .. } if episode.others.iter().any(|j| j.state.playable())) {
            items.push(entry("Delete the other copy's file", 207));
        }
        let title = match row {
            Row::Title(t) => t.name.clone(),
            Row::Job { title, .. } => title.clone(),
            _ => job.title.clone(),
        };
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
    }
    fn pause(ctx: &UiCtx<'_>, job: &Job) {
        shared::send(ctx, if job.state == State::Paused { DownloadCommand::Resume(vec![job.id]) } else { DownloadCommand::Pause(vec![job.id]) });
    }
    fn footer(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, snap: &DownloadsSnapshot) {
        let Some(page) = &self.page else { return };
        let Some(VodKind::Series(id)) = page.kind else { return };
        let Some(summary) = ctx.lib.vod.find(VodKind::Series(id)) else { return };
        let item = crate::ui::stacks::item_ref(summary);
        if ctx.lib.conceals(&item) {
            return;
        }
        ui.separator();
        if ui.button("Download more episodes").clicked() {
            ctx.push(Route::SeriesDetail(id));
        }
        let rules: Vec<_> = snap.rules.iter().filter(|r| self.titles.key(&r.item) == page.key).collect();
        let mut on = !rules.is_empty();
        if let Some(rule) = rules.first() {
            self.keep = rule.keep;
        }
        ui.horizontal_wrapped(|ui| {
            let changed = ui.checkbox(&mut on, "Keep the next").changed();
            let number = ui.add(egui::DragValue::new(&mut self.keep).range(1..=10)).changed();
            ui.label("unwatched downloaded");
            if changed && !on {
                for rule in &rules {
                    shared::deleted_rule(ctx, rule.id);
                }
            } else if on && (changed || number) {
                let item = rules.first().map_or(item, |r| r.item.clone());
                shared::send(ctx, DownloadCommand::Series { item, scope: SeriesScope::Keep(self.keep), agent: false });
            }
        });
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
        if matches!(action, Action::Back | Action::Parent | Action::Left) {
            return self.leave();
        }
        let snap = shared::snapshot(ctx);
        let rows = self.rows(ctx, &snap);
        self.select(&rows);
        let current = rows.get(self.cursor);
        match action {
            Action::Up | Action::Down => {
                self.cursor = if action == Action::Up { self.cursor.saturating_sub(1) } else { (self.cursor + 1).min(rows.len().saturating_sub(1)) };
                self.selected = rows.get(self.cursor).map(Row::key);
                self.scroll = true;
            }
            Action::Activate => {
                if let Some(row) = current {
                    self.activate(ctx, row);
                }
            }
            Action::PlayPause => {
                if let Some(Row::Job { episode, .. }) = current {
                    Self::pause(ctx, &episode.job);
                }
            }
            Action::MoveUp | Action::MoveDown => {
                if let Some(Row::Job { episode, queue: true, .. }) = current {
                    let j = &episode.job;
                    let next = if action == Action::MoveUp { self.cursor.checked_sub(1) } else { Some(self.cursor + 1) };
                    if let Some(Row::Job { episode, queue: true, .. }) = next.and_then(|i| rows.get(i)) {
                        shared::send(ctx, DownloadCommand::Move { id: j.id, to: episode.job.position as usize });
                    }
                }
            }
            Action::Remove => {
                if let Some(Row::Rule(rule)) = current {
                    shared::deleted_rule(ctx, rule.id);
                } else if let Some(row) = current {
                    self.confirm(ctx, row.ids());
                }
            }
            Action::ContextMenu => {
                if let Some(row) = current {
                    self.menu(ctx, row);
                }
            }
            _ => return false,
        }
        true
    }
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        if result == OverlayResult::Confirmed(DELETE) {
            for id in std::mem::take(&mut self.delete) {
                shared::deleted(ctx, id);
            }
            return;
        }
        let OverlayResult::Chosen(Action::Digit(code)) = result else { return };
        let snap = shared::snapshot(ctx);
        let rows = self.rows(ctx, &snap);
        let Some(row) = rows.iter().find(|r| self.selected.as_ref() == Some(&r.key())) else { return };
        let Some(job) = row.job() else { return };
        match code {
            200 => self.activate(ctx, row),
            201 => Self::pause(ctx, job),
            202 => shared::send(ctx, DownloadCommand::Retry(vec![job.id])),
            203 => shared::send(ctx, DownloadCommand::Move { id: job.id, to: 0 }),
            204 => {
                if let Ok(media) = crate::downloads::planning::media(ctx.db, job) {
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
            205 => self.confirm(ctx, row.ids()),
            206 => {
                let jobs = self.page.as_ref().map_or(snap.jobs.as_slice(), |t| t.jobs.as_slice());
                self.confirm(ctx, jobs.iter().filter(|j| j.watched_at.is_some()).map(|j| j.id).collect());
            }
            207 => {
                if let Row::Job { episode, .. } = row {
                    self.confirm(ctx, episode.others.iter().filter(|j| j.state.playable()).map(|j| j.id).collect());
                }
            }
            208 => shared::send(ctx, DownloadCommand::Cancel(vec![job.id])),
            _ => {}
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let snap = shared::snapshot(ctx);
        let rows = self.rows(ctx, &snap);
        self.select(&rows);
        let mut back = false;
        ui.horizontal(|ui| {
            if self.page.is_some() && ui.button("← Downloads").clicked() {
                back = true;
            }
            ui.heading(self.page.as_ref().map_or("Downloads", |t| t.name.as_str()));
        });
        if back {
            self.leave();
            return;
        }
        if self.page.is_none() {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "{:.1} GB of {:.0} GB · {:.1} GB free",
                    snap.used_bytes as f64 / 1e9,
                    snap.cap_bytes as f64 / 1e9,
                    snap.free_bytes as f64 / 1e9
                ));
                if ui.button(if snap.paused { "Resume downloads" } else { "Pause downloads" }).clicked() {
                    shared::send(ctx, DownloadCommand::PauseAll(!snap.paused));
                }
            });
        }
        let mut activate = None;
        let mut menu = None;
        let mut remove = None;
        let mut move_to = None;
        egui::ScrollArea::vertical().id_salt(if self.page.is_some() { "download-episodes" } else { "download-titles" }).show(ui, |ui| {
            if rows.is_empty() {
                ui.label("Download a film or episode, or record a programme with Ctrl+D.");
            }
            let mut section = "";
            let mut season = None;
            for (i, row) in rows.iter().enumerate() {
                if self.page.is_none() && section != row.section() {
                    ui.separator();
                    ui.heading(row.section());
                    section = row.section();
                }
                if self.page.is_some()
                    && let Some(job) = row.job()
                {
                    let number = grouping::coordinates(job).0;
                    if season != Some(number) {
                        ui.heading(format!("Season {number}"));
                        season = Some(number);
                    }
                }
                let selected = self.cursor == i;
                let response = match row {
                    Row::Title(title) => egui::Frame::new()
                        .fill(if selected { ui.visuals().selection.bg_fill } else { egui::Color32::TRANSPARENT })
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                if ctx.layout != LayoutClass::Compact {
                                    let width = if ctx.layout == LayoutClass::Wide { 52.0 } else { 36.0 } * ctx.theme.rem();
                                    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, width * 1.5), egui::Sense::hover());
                                    crate::ui::widgets::poster::paint_poster_image(
                                        ui,
                                        ctx.theme,
                                        ctx.images,
                                        title.poster.as_deref(),
                                        &title.name,
                                        rect,
                                        crate::images::ImageKind::Poster,
                                    );
                                }
                                ui.vertical(|ui| {
                                    ui.strong(&title.name);
                                    ui.label(title.summary());
                                    if !title.series
                                        && let Some(job) = row.job()
                                    {
                                        ui.label(shared::state_text(job));
                                        retention(ui, ctx, job);
                                    }
                                });
                            });
                        })
                        .response
                        .interact(egui::Sense::click()),
                    Row::Rule(rule) => {
                        ui.horizontal(|ui| {
                            let r = ui.selectable_label(selected, format!("{} · keep {} unwatched", rule.title, rule.keep));
                            if ui.button("Delete").clicked() {
                                shared::deleted_rule(ctx, rule.id);
                            }
                            r
                        })
                        .inner
                    }
                    Row::Job { episode, title, queue } => {
                        let job = &episode.job;
                        ui.horizontal_wrapped(|ui| {
                            if *queue && ui.add(egui::Label::new("⠿").sense(egui::Sense::drag())).drag_started() {
                                self.drag = Some(job.id);
                            }
                            let label = if self.page.is_some() {
                                job.subtitle.as_deref().unwrap_or(title).to_owned()
                            } else if job.kind == crate::downloads::Kind::Episode {
                                let (s, e) = grouping::coordinates(job);
                                format!("{title} · S{s}E{e}")
                            } else {
                                title.clone()
                            };
                            let r = ui.selectable_label(selected, label);
                            ui.label(shared::state_text(job));
                            if *queue
                                && job.state.active()
                                && let Some(p) = job.progress()
                            {
                                ui.add(egui::ProgressBar::new(p as f32).desired_width(90.0));
                            }
                            if job.state.playable() {
                                ui.label(format!("{:.1} GB", job.bytes_done as f64 / 1e9));
                            }
                            if episode.watched {
                                ui.label("✓ Watched");
                                retention(ui, ctx, job);
                            }
                            ui.label(job.source_id.and_then(|s| ctx.lib.source(s)).map_or("Playlist removed", |s| s.name.as_str()));
                            if *queue {
                                if ui.small_button(if job.state == State::Paused { "Resume" } else { "Pause" }).clicked() {
                                    Self::pause(ctx, job);
                                }
                                if ui.small_button("Cancel").clicked() {
                                    shared::send(ctx, DownloadCommand::Cancel(vec![job.id]));
                                }
                                if job.state == State::Scheduled
                                    && let Some(at) = job.prog_start
                                {
                                    ui.label(crate::ui::widgets::time::day_time(at, ctx.config.interface.clock_24h));
                                    if snap.jobs.iter().any(|other| {
                                        other.id != job.id
                                            && other.source_id == job.source_id
                                            && (matches!(other.state, State::Running | State::Waiting)
                                                || (other.state == State::Scheduled
                                                    && other.start().zip(job.stop()).is_some_and(|(a, b)| a < b)
                                                    && other.stop().zip(job.start()).is_some_and(|(a, b)| a > b)))
                                    }) {
                                        ui.label("⚠ Clashes with downloads on this account");
                                    }
                                }
                                if job.kind.recording()
                                    && job.state == State::Running
                                    && let Some(stop) = job.stop()
                                {
                                    ui.label(format!("{} min left", (stop - ctx.now).max(0) / 60));
                                }
                                if ctx.layout == LayoutClass::Wide && job.state == State::Running {
                                    let speed = job.fp["download_speed_bps"].as_f64().unwrap_or(0.0);
                                    ui.label(format!("{:.1} MB/s", speed / 1e6));
                                    if speed > 0.0
                                        && let Some(left) = job.bytes_total.map(|n| n.saturating_sub(job.bytes_done))
                                    {
                                        ui.label(format!("~{} min", (left as f64 / speed / 60.0).ceil() as u64));
                                    }
                                }
                            } else {
                                if ui.small_button("Play").clicked() {
                                    activate = Some(i);
                                }
                                if ui.small_button("Delete").clicked() {
                                    remove = Some(i);
                                }
                            }
                            r
                        })
                        .inner
                    }
                };
                if selected && self.scroll {
                    response.scroll_to_me(Some(egui::Align::Center));
                }
                if response.clicked() || response.secondary_clicked() {
                    self.cursor = i;
                    self.selected = Some(row.key());
                    if response.clicked() {
                        activate = Some(i);
                    } else {
                        menu = Some(i);
                    }
                }
                if matches!(row, Row::Job { queue: true, .. })
                    && response.hovered()
                    && ui.input(|i| i.pointer.any_released())
                    && let Some(id) = self.drag.take()
                    && let Some(job) = row.job()
                {
                    move_to = Some((id, job.position as usize));
                }
                if let Some(job) = row.job()
                    && shared::clash_live(job, ctx.now)
                {
                    ui.horizontal_wrapped(|ui| {
                        for (label, record, alternate) in
                            [("Record (stops playback)", true, false), ("Use the other account", true, true), ("Skip this recording", false, false)]
                        {
                            if alternate && job.fp["download_alternate"].as_bool() != Some(true) {
                                continue;
                            }
                            if ui.button(label).clicked() {
                                shared::send(ctx, DownloadCommand::Clash { id: job.id, record, alternate });
                            }
                        }
                    });
                }
            }
            self.footer(ui, ctx, &snap);
        });
        self.scroll = false;
        if let Some(i) = activate {
            self.cursor = i;
            self.selected = Some(rows[i].key());
            self.activate(ctx, &rows[i]);
        }
        if let Some(i) = menu {
            self.menu(ctx, &rows[i]);
        }
        if let Some(i) = remove {
            self.confirm(ctx, rows[i].ids());
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

fn retention(ui: &mut egui::Ui, ctx: &UiCtx<'_>, job: &Job) {
    let days = ctx.config.downloads.delete_after_watched_days;
    if days > 0
        && let Some(watched) = job.watched_at
    {
        let left = (watched + i64::from(days) * 86400 - ctx.now).max(0);
        ui.label(format!("Deletes in {} days", (left + 86399) / 86400));
    }
}

#[cfg(test)]
#[path = "downloads_tests.rs"]
mod tests;
