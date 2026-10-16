//! Download choices, size confirmation, and the account clash prompt (DESIGN §43 §2.3–§2.4).
use super::dialogs::{card_area, scrim_guarded};
use super::{Overlay, OverlayOutcome};
use crate::downloads::{DownloadCommand, SeriesScope, Target};
use crate::keymap::{Action, KeyContext};
use crate::model::{DownloadId, ItemKind, ItemRef, MediaRef};
use crate::ui::downloads as shared;
use crate::ui::widgets::card::CardKind;
use crate::ui::{Route, UiCtx, item_menu};
use eframe::egui;

#[derive(Clone)]
enum Choice {
    Add(Vec<Target>),
    Series(crate::model::ItemRef, SeriesScope),
    Play(DownloadId),
    Delete(DownloadId),
    Stop(DownloadId),
    Show,
    Pin(DownloadId, crate::model::ItemRef),
    Follow,
}

pub struct Picker {
    target: item_menu::Target,
    explicit: Option<Target>,
    choices: Vec<(String, Choice, u64)>,
    cursor: usize,
    ready: bool,
    fetch: crate::ui::copies::EpisodeFetch,
    minutes: u32,
    keep: u32,
    confirm: Option<DownloadId>,
    error: Option<String>,
}
impl Picker {
    pub fn new(target: item_menu::Target) -> Self {
        Self {
            target,
            explicit: None,
            choices: vec![],
            cursor: 0,
            ready: false,
            fetch: Default::default(),
            minutes: 30,
            keep: 3,
            confirm: None,
            error: None,
        }
    }
    pub fn for_recording(channel: crate::model::ChannelId, target: Target) -> Self {
        let mut picker = Self::new(item_menu::Target::Channel(channel));
        picker.explicit = Some(target);
        picker
    }
    fn init(&mut self, ctx: &UiCtx<'_>) {
        if self.ready || self.fetch.busy() {
            return;
        }
        self.ready = true;
        let Ok(target) = self.explicit.clone().map(Ok).unwrap_or_else(|| shared::target(ctx, &self.target)) else {
            self.error = Some("That item is no longer available".into());
            return;
        };
        let snap = shared::snapshot(ctx);
        if let Some(job) = snap.jobs.iter().find(|j| j.matches(&target.item) && j.prog_start == target.programme.as_ref().map(|p| p.start)) {
            if job.state.playable() {
                self.choices.push(("Play download".into(), Choice::Play(job.id), 0));
                self.choices.push(("Delete download…".into(), Choice::Delete(job.id), 0));
            } else if job.state.active() && job.kind.recording() {
                self.choices.push(("Stop recording".into(), Choice::Stop(job.id), 0));
            }
            for copy in crate::downloads::planning::alternatives(ctx.db, ctx.lib, &target.item).unwrap_or_default() {
                if copy.source == target.item.source {
                    continue;
                }
                let name = ctx.lib.source(copy.source).map_or("another account", |s| s.name.as_str());
                self.choices.push((format!("Download from {name}"), Choice::Pin(job.id, copy), crate::store::downloads::estimate(job)));
            }
            self.choices.push(("Show in Downloads".into(), Choice::Show, 0));
            return;
        }
        let estimate = |targets: &[Target]| -> u64 {
            targets
                .iter()
                .filter_map(|t| {
                    crate::downloads::planning::new_job(
                        ctx.db,
                        ctx.lib,
                        ctx.services.agent.parental(&ctx.config.parental),
                        ctx.config,
                        t,
                        false,
                        ctx.now,
                    )
                    .ok()
                })
                .map(|j| crate::store::downloads::estimate(&j))
                .sum()
        };
        if matches!(target.item.kind, ItemKind::Series | ItemKind::Episode) {
            if let Ok(sid) = crate::downloads::planning::series_id(ctx.db, &target.item) {
                if let Ok(unloaded) = crate::store::copies::unloaded_series(ctx.db, &[sid])
                    && !unloaded.is_empty()
                {
                    self.ready = false;
                    self.fetch.start(ctx, &unloaded);
                    return;
                }
                if target.item.kind == ItemKind::Episode {
                    self.choices.push(("This episode".into(), Choice::Add(vec![target.clone()]), estimate(std::slice::from_ref(&target))));
                }
                let episodes = crate::store::vod::episodes(ctx.db, sid, None).unwrap_or_default();
                if target.item.kind == ItemKind::Episode {
                    if let Some(ep) = episodes.iter().find(|e| e.key == target.item.key) {
                        let rest: Vec<_> = episodes
                            .iter()
                            .filter(|e| e.season == ep.season && e.episode >= ep.episode)
                            .map(|e| Target {
                                item: crate::model::ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key.clone() },
                                programme: None,
                                minutes: None,
                                any_copy: true,
                            })
                            .collect();
                        self.choices.push((
                            format!("Rest of season {} ({} episodes)", ep.season, rest.len()),
                            Choice::Add(rest.clone()),
                            estimate(&rest),
                        ));
                    }
                } else {
                    for (label, scope) in [("Next episode", SeriesScope::Next(1)), ("Next 3 unwatched", SeriesScope::Next(3))] {
                        let targets = crate::downloads::planning::series_targets(ctx.db, &target.item, scope).unwrap_or_default();
                        self.choices.push((label.into(), Choice::Series(target.item.clone(), scope), estimate(&targets)));
                    }
                    let mut seasons: Vec<_> = episodes.iter().map(|e| e.season).collect();
                    seasons.sort();
                    seasons.dedup();
                    for season in seasons {
                        let targets =
                            crate::downloads::planning::series_targets(ctx.db, &target.item, SeriesScope::Season(season)).unwrap_or_default();
                        self.choices.push((
                            format!("Season {season} ({} episodes)", targets.len()),
                            Choice::Series(target.item.clone(), SeriesScope::Season(season)),
                            estimate(&targets),
                        ));
                    }
                    let targets = crate::downloads::planning::series_targets(ctx.db, &target.item, SeriesScope::All).unwrap_or_default();
                    self.choices.push((
                        format!("All episodes ({})", targets.len()),
                        Choice::Series(target.item.clone(), SeriesScope::All),
                        estimate(&targets),
                    ));
                    self.choices.push((
                        "Keep the next unwatched downloaded".into(),
                        Choice::Series(target.item.clone(), SeriesScope::Keep(3)),
                        estimate(&crate::downloads::planning::series_targets(ctx.db, &target.item, SeriesScope::Keep(3)).unwrap_or_default()),
                    ));
                }
            }
        } else {
            let label = if target.item.kind == ItemKind::Channel {
                if target.programme.as_ref().is_some_and(|p| p.stop <= ctx.now) {
                    "Save catch-up"
                } else if target.programme.is_some() {
                    "Record this programme"
                } else {
                    "Record for a set time"
                }
            } else {
                "Download from the best copy"
            };
            self.choices.push((label.into(), Choice::Add(vec![target.clone()]), estimate(std::slice::from_ref(&target))));
            if target.item.kind == ItemKind::Channel && target.programme.as_ref().is_some_and(|p| p.start <= ctx.now && p.stop > ctx.now) {
                let timed = Target { programme: None, minutes: Some(30), any_copy: false, ..target.clone() };
                self.choices.push(("Record for a set time".into(), Choice::Add(vec![timed.clone()]), estimate(&[timed])));
            }
            if target.item.kind == ItemKind::Movie {
                for copy in crate::store::watchlist::title_copies(ctx.db, ctx.lib, &target.item).unwrap_or_default() {
                    let name = ctx.lib.source(copy.source).map_or("this account", |s| s.name.as_str());
                    let t = Target { item: copy, any_copy: false, ..target.clone() };
                    self.choices.push((format!("Download from {name}"), Choice::Add(vec![t.clone()]), estimate(&[t])));
                }
            } else if matches!(self.target, item_menu::Target::Programme { .. }) {
                self.choices.push(("Record every new episode…".into(), Choice::Follow, 0));
            }
        }
    }
    fn size(&self, choice: &Choice, bytes: u64) -> u64 {
        match choice {
            Choice::Series(_, SeriesScope::Keep(n)) => bytes.saturating_mul(self.keep as u64) / u64::from(*n).max(1),
            Choice::Add(ts) if ts.iter().any(|t| t.minutes.is_some()) => (self.minutes as f64 / 60.0 * 2_000_000_000.0).ceil() as u64,
            _ => bytes,
        }
    }
    fn choose(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if let Some(id) = self.confirm.take() {
            shared::deleted(ctx, id);
            return OverlayOutcome::Close;
        }
        let Some((_, choice, size)) = self.choices.get(self.cursor).cloned() else { return OverlayOutcome::Keep };
        let size = self.size(&choice, size);
        let snap = shared::snapshot(ctx);
        if size > 0
            && !crate::downloads::scheduler::space_allows(
                snap.used_bytes,
                snap.free_bytes,
                snap.cap_bytes,
                ctx.config.downloads.reserve_gb.saturating_mul(1_000_000_000),
                size,
            )
        {
            self.error = Some("There is not enough room for this download".into());
            return OverlayOutcome::Keep;
        }
        let command = match choice {
            Choice::Add(mut targets) => {
                for t in &mut targets {
                    if t.minutes.is_some() {
                        t.minutes = Some(self.minutes);
                    }
                }
                DownloadCommand::Add { targets, agent: false }
            }
            Choice::Series(item, scope) => DownloadCommand::Series {
                item,
                scope: if matches!(scope, SeriesScope::Keep(_)) { SeriesScope::Keep(self.keep) } else { scope },
                agent: false,
            },
            Choice::Play(id) => {
                shared::play(ctx, id);
                return OverlayOutcome::Close;
            }
            Choice::Delete(id) => {
                self.confirm = Some(id);
                return OverlayOutcome::Keep;
            }
            Choice::Stop(id) => DownloadCommand::StopRecording(id),
            Choice::Pin(id, item) => DownloadCommand::Pin { id, item },
            Choice::Show => {
                ctx.push(Route::Downloads);
                return OverlayOutcome::Close;
            }
            Choice::Follow => {
                if let item_menu::Target::Programme { programme, channel } = self.target {
                    ctx.open(crate::ui::OverlayRequest::FollowSheet(crate::ui::overlays::follow_sheet::FollowSheetSpec {
                        record: true,
                        programme: Some((programme, channel)),
                        rule: None,
                    }));
                }
                return OverlayOutcome::Close;
            }
        };
        if let Some(handle) = ctx.services.agent.downloads() {
            let bus = ctx.services.bus.clone();
            ctx.services.rt.spawn(async move {
                let r = handle.request(command).await;
                if let Some(error) = r.error {
                    bus.toast(crate::msg::Toast::error(error));
                } else if !r.refused.is_empty() {
                    bus.toast(crate::msg::Toast::info(format!("Added {} · {} could not be added", r.added.len(), r.refused.len())));
                } else {
                    bus.toast(crate::msg::Toast::info(if r.rule.is_some() {
                        "Download rule saved".into()
                    } else {
                        format!("Added {} to Downloads", r.added.len())
                    }));
                }
            });
        }
        OverlayOutcome::Close
    }
}
impl Overlay for Picker {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        self.init(ctx);
        match action {
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => self.cursor = (self.cursor + 1).min(self.choices.len().saturating_sub(1)),
            Action::Left | Action::Decrease => {
                self.keep = self.keep.saturating_sub(1).max(1);
                self.minutes = self.minutes.saturating_sub(5).max(1);
            }
            Action::Right | Action::Increase => {
                self.keep = (self.keep + 1).min(10);
                self.minutes = (self.minutes + 5).min(1440);
            }
            Action::Activate => return self.choose(ctx),
            Action::Back => return OverlayOutcome::Close,
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }
    fn ui(&mut self, egui: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.init(ctx);
        if self.fetch.poll(ctx, egui).is_some() {
            self.ready = false;
            self.init(ctx);
        }
        if scrim_guarded(egui, ctx.theme, "download-picker", false) {
            return OverlayOutcome::Close;
        }
        let mut choose = false;
        let mut close = false;
        let theme = ctx.theme;
        let snap = shared::snapshot(ctx);
        card_area(egui, theme, "download-picker", CardKind::Popup, 520.0 * theme.rem(), None, |ui| {
            ui.heading(if self.confirm.is_some() { "Delete this download?" } else { "Download / record" });
            if self.confirm.is_some() {
                ui.label("The file is deleted after 10 seconds. You can undo until then.");
            } else {
                ui.label(format!("{:.1} GB used · {:.1} GB free", snap.used_bytes as f64 / 1e9, snap.free_bytes as f64 / 1e9));
                if self.fetch.busy() {
                    ui.label("Loading episodes…");
                }
                for (i, (label, choice, size)) in self.choices.iter().enumerate() {
                    let size = self.size(choice, *size);
                    let label = if size > 0 { format!("{label} · ~{:.1} GB", size as f64 / 1e9) } else { label.clone() };
                    if ui.selectable_label(i == self.cursor, label).clicked() {
                        self.cursor = i;
                        choose = !matches!(choice, Choice::Series(_, SeriesScope::Keep(_)))
                            && !matches!(choice, Choice::Add(ts) if ts.iter().any(|t| t.minutes.is_some()));
                    }
                    if i == self.cursor && matches!(choice, Choice::Series(_, SeriesScope::Keep(_))) {
                        ui.horizontal(|ui| {
                            ui.label("Keep");
                            ui.add(egui::DragValue::new(&mut self.keep).range(1..=10));
                            ui.label("unwatched episodes");
                        });
                    }
                    if i == self.cursor && matches!(choice,Choice::Add(ts) if ts.iter().any(|t|t.minutes.is_some())) {
                        ui.horizontal(|ui| {
                            ui.label("Minutes");
                            ui.add(egui::DragValue::new(&mut self.minutes).range(1..=1440));
                        });
                    }
                }
            }
            if let Some(error) = &self.error {
                ui.label(error);
            }
            ui.horizontal(|ui| {
                if ui.button(if self.confirm.is_some() { "Delete" } else { "Add" }).clicked() {
                    choose = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close {
            OverlayOutcome::Close
        } else if choose {
            self.choose(ctx)
        } else {
            OverlayOutcome::Keep
        }
    }
}

pub struct Ask {
    busy: shared::Busy,
    cursor: usize,
    pending: bool,
    alternate: Option<(String, crate::model::MediaRef)>,
    checked: bool,
}
impl Ask {
    pub fn new(busy: shared::Busy) -> Self {
        Self { busy, cursor: 0, pending: false, alternate: None, checked: false }
    }
    fn init(&mut self, ctx: &UiCtx<'_>) {
        if self.checked {
            return;
        }
        self.checked = true;
        let req = match self.busy.command.as_ref() {
            crate::player::PlayerCommand::Play(r) => r,
            crate::player::PlayerCommand::PlayCopy(c) => &c.req,
            crate::player::PlayerCommand::CastTo { play: Some(r), .. } => r,
            _ => return,
        };
        let item = match &req.media {
            MediaRef::Movie(id) => {
                crate::store::vod::movie(ctx.db, *id).ok().map(|m| ItemRef { source: m.source_id, kind: ItemKind::Movie, key: m.key })
            }
            MediaRef::Episode(id) => {
                crate::store::vod::episode(ctx.db, *id).ok().map(|e| ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key })
            }
            MediaRef::Channel(id) | MediaRef::Catchup { channel: id, .. } => ctx.lib.live.get(*id).map(crate::ui::screens::live::channel_ref),
            _ => None,
        };
        let Some(item) = item else { return };
        for copy in crate::downloads::planning::alternatives(ctx.db, ctx.lib, &item).unwrap_or_default() {
            if copy.source == item.source || !ctx.services.tiles.budget().admits(self.busy.tile, copy.source) {
                continue;
            }
            let target = Target { item: copy.clone(), programme: None, minutes: Some(30), any_copy: false };
            let Ok(job) = crate::downloads::planning::new_job(
                ctx.db,
                ctx.lib,
                ctx.services.agent.parental(&ctx.config.parental),
                ctx.config,
                &target,
                !ctx.parental.is_unlocked(&ctx.config.parental),
                ctx.now,
            ) else {
                continue;
            };
            let Ok(mut media) = crate::downloads::planning::media(ctx.db, &job) else { continue };
            if let (crate::model::MediaRef::Catchup { start, stop, catchup_id, .. }, crate::model::MediaRef::Channel(channel)) = (&req.media, &media)
            {
                media = crate::model::MediaRef::Catchup { channel: *channel, start: *start, stop: *stop, catchup_id: catchup_id.clone() };
            }
            let name = ctx.lib.source(copy.source).map_or("another account", |s| s.name.as_str()).to_owned();
            self.alternate = Some((name, media));
            break;
        }
    }
    fn choose(&mut self, ctx: &UiCtx<'_>) -> OverlayOutcome {
        self.init(ctx);
        if self.cursor == 1
            && let Some((_, media)) = &self.alternate
        {
            let Some(player) = ctx.services.tiles.registry().get(self.busy.tile) else { return OverlayOutcome::Close };
            let mut command = (*self.busy.command).clone();
            match &mut command {
                crate::player::PlayerCommand::Play(r) | crate::player::PlayerCommand::CastTo { play: Some(r), .. } => r.media = media.clone(),
                crate::player::PlayerCommand::PlayCopy(c) => {
                    c.req.media = media.clone();
                    c.fallback = None;
                    c.elsewhere = None;
                }
                _ => {}
            }
            player.send(command);
            return OverlayOutcome::Close;
        }
        if self.cursor != 0 {
            return OverlayOutcome::Close;
        }
        let Some(handle) = ctx.services.agent.downloads() else { return OverlayOutcome::Close };
        let Some(player) = ctx.services.tiles.registry().get(self.busy.tile) else { return OverlayOutcome::Close };
        let source = self.busy.source;
        let command = (*self.busy.command).clone();
        let budget = ctx.services.tiles.budget().clone();
        let tile = self.busy.tile;
        let previous = budget.claim_of(tile);
        budget.claim(tile, Some(source));
        ctx.services.rt.spawn(async move {
            let reply = handle.request(DownloadCommand::PauseForPlay(source)).await;
            if reply.error.is_none() {
                player.send(command);
            } else {
                budget.claim(tile, previous);
            }
        });
        self.pending = true;
        OverlayOutcome::Close
    }
}
impl Overlay for Ask {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Up | Action::Down | Action::FocusNext | Action::FocusPrev => {
                self.init(ctx);
                let count = 2 + usize::from(self.alternate.is_some());
                self.cursor = (self.cursor + if matches!(action, Action::Up | Action::FocusPrev) { count - 1 } else { 1 }) % count;
                OverlayOutcome::Keep
            }
            Action::Activate => self.choose(ctx),
            Action::Back => OverlayOutcome::Close,
            _ => OverlayOutcome::Ignored,
        }
    }
    fn ui(&mut self, egui: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.init(ctx);
        if scrim_guarded(egui, ctx.theme, "download-busy", self.pending) {
            return OverlayOutcome::Close;
        }
        if self.alternate.is_some() && egui.input(|i| i.key_pressed(egui::Key::Num2)) {
            self.cursor = 1;
            return self.choose(ctx);
        }
        let snapshot = shared::snapshot(ctx);
        let job = snapshot.jobs.iter().find(|j| j.id == self.busy.id);
        let mut answer = None;
        card_area(egui, ctx.theme, "download-busy", CardKind::Popup, 500.0 * ctx.theme.rem(), None, |ui| {
            let account = ctx.lib.source(self.busy.source).map_or("This account", |s| s.name.as_str());
            ui.heading(format!("{account} is busy"));
            if let Some(job) = job {
                ui.label(&job.title);
                if let Some(subtitle) = &job.subtitle {
                    ui.label(subtitle);
                }
                ui.label(shared::state_text(job));
            }
            let label = if job.is_some_and(|j| j.kind.recording()) { "Stop recording and play · Enter" } else { "Pause it and play · Enter" };
            if ui.selectable_label(self.cursor == 0, label).clicked() {
                answer = Some(0);
            }
            if let Some((name, _)) = &self.alternate
                && ui.selectable_label(self.cursor == 1, format!("Play from {name} · 2")).clicked()
            {
                answer = Some(1);
            }
            let cancel = 1 + usize::from(self.alternate.is_some());
            if ui.selectable_label(self.cursor == cancel, "Cancel").clicked() {
                answer = Some(cancel);
            }
        });
        if let Some(answer) = answer {
            self.cursor = answer;
            self.choose(ctx)
        } else {
            OverlayOutcome::Keep
        }
    }
}

/// A forget action has already removed progress; removing its file is a separate choice.
pub struct DeleteFiles {
    ids: Vec<DownloadId>,
    confirm: super::dialogs::ConfirmOverlay,
}
impl DeleteFiles {
    pub fn new(ids: Vec<DownloadId>) -> Self {
        Self {
            ids,
            confirm: super::dialogs::ConfirmOverlay::new(super::ConfirmSpec {
                title: "Delete the download too?".into(),
                body: "Remove the saved file from this computer. Undo is available for 10 seconds.".into(),
                confirm_label: "Delete download".into(),
                destructive: true,
                token: 0,
            }),
        }
    }
    fn finish(&self, ctx: &mut UiCtx<'_>, outcome: OverlayOutcome) -> OverlayOutcome {
        match outcome {
            OverlayOutcome::CloseWith(super::OverlayResult::Confirmed(_)) => {
                for id in &self.ids {
                    shared::deleted(ctx, *id);
                }
                OverlayOutcome::Close
            }
            OverlayOutcome::CloseWith(super::OverlayResult::Cancelled(_)) => OverlayOutcome::Close,
            other => other,
        }
    }
}
impl Overlay for DeleteFiles {
    fn key_contexts(&self) -> Vec<KeyContext> {
        self.confirm.key_contexts()
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        let outcome = self.confirm.on_action(ctx, action);
        self.finish(ctx, outcome)
    }
    fn ui(&mut self, egui: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let outcome = self.confirm.ui(egui, ctx);
        self.finish(ctx, outcome)
    }
}
