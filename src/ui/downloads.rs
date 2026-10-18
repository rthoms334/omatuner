//! Shared download actions and guarded labels (DESIGN §43 §2, §6). Every surface uses the
//! service's snapshot; the add menus do their catalogue reads only when opened.

use crate::downloads::{DownloadCommand, DownloadsSnapshot, Job, State, Target};
use crate::keymap::{Action, KeyContext};
use crate::model::{DownloadId, ItemKind, MediaRef, PlayOrigin, SourceId, TileId};
use crate::msg::Toast;
use crate::player::{PlayRequest, PlayerCommand};
use crate::ui::{OverlayRequest, Route, UiCtx, item_menu};
use std::sync::Arc;

pub(crate) mod grouping;

#[derive(Debug, Clone, PartialEq)]
pub struct Busy {
    pub id: DownloadId,
    pub source: SourceId,
    pub tile: TileId,
    pub command: Box<PlayerCommand>,
}

pub fn snapshot(ctx: &UiCtx<'_>) -> Arc<DownloadsSnapshot> {
    let Some(handle) = ctx.services.agent.downloads() else { return Arc::default() };
    let raw = handle.snapshot();
    let mut shown = (*raw).clone();
    let hide = ctx.parental.hides_explicit(&ctx.config.parental);
    shown.jobs.retain(|j| visible(ctx.lib, hide, j));
    shown.used_bytes = shown.jobs.iter().map(|j| j.bytes_done).sum();
    shown.rules.retain(|r| !ctx.lib.conceals(&r.item) && (!hide || !ctx.lib.explicit.contains(&r.item)));
    let guard = crate::ui::widgets::spoiler::Spoilers::of(ctx);
    for job in &mut shown.jobs {
        if job.kind == crate::downloads::Kind::Episode {
            let norm = crate::spoiler::series_key(&job.title);
            let subject = crate::guard::Subject::Episode {
                series_norm: &norm,
                season: job.fp.get("download_season").and_then(serde_json::Value::as_i64).unwrap_or(0) as i32,
                episode: job.fp.get("download_episode").and_then(serde_json::Value::as_i64).unwrap_or(0) as i32,
            };
            if let Some(subtitle) = &job.subtitle {
                let (numbers, title) = subtitle.split_once(" · ").unwrap_or((subtitle, ""));
                job.subtitle = Some(
                    guard
                        .text(&subject, crate::guard::Field::EpisodeTitle, title)
                        .map_or_else(|| numbers.to_owned(), |title| format!("{numbers} · {title}")),
                );
            }
        } else if job.kind.recording() {
            job.title = ctx.services.agent.guard().outside_title(&job.title, ctx.now).into_owned();
        }
    }
    Arc::new(shown)
}

fn visible(lib: &crate::store::library::Library, hide: bool, job: &Job) -> bool {
    let mut items = job.item().into_iter().chain(grouping::title_item(job));
    (!hide || !job.explicit()) && items.all(|item| !lib.conceals(&item) && (!hide || !lib.explicit.contains(&item)))
}

pub fn send(ctx: &UiCtx<'_>, command: DownloadCommand) {
    if let Some(handle) = ctx.services.agent.downloads() {
        handle.send(command);
    }
}

pub fn target(ctx: &UiCtx<'_>, target: &item_menu::Target) -> Result<Target, &'static str> {
    if let item_menu::Target::Channel(id) = target
        && crate::queue::ui::add_target(ctx, target).is_err()
    {
        let ch = ctx.lib.live.get(*id).ok_or("Channel not found")?;
        return Ok(Target { item: crate::ui::screens::live::channel_ref(ch), programme: None, minutes: Some(30), any_copy: false });
    }
    let t = crate::queue::ui::add_target(ctx, target)?;
    Ok(Target { item: t.item, programme: t.programme, minutes: None, any_copy: t.any_copy })
}

pub fn menu_items(_target: &item_menu::Target, _ctx: &UiCtx<'_>, keys: KeyContext) -> Vec<crate::ui::overlays::MenuItem> {
    if !crate::downloads::ENABLED {
        return vec![];
    }
    vec![item_menu::entry("⤓", "Download / record…", Action::Download, keys)]
}

pub fn on_action(ctx: &mut UiCtx<'_>, action: Action, target: &item_menu::Target) -> bool {
    if action != Action::Download || !crate::downloads::ENABLED {
        return false;
    }
    ctx.open(OverlayRequest::Download(*target));
    true
}

pub fn play(ctx: &mut UiCtx<'_>, id: DownloadId) {
    let snap = snapshot(ctx);
    let Some(job) = snap.jobs.iter().find(|j| j.id == id) else { return };
    if crate::downloads::planning::locked(ctx.db, ctx.lib, job) && !ctx.parental.is_unlocked(&ctx.config.parental) {
        ctx.open(OverlayRequest::Pin { then: Box::new(Action::PlayDownload(id)), reason: "This download is locked".into() });
        return;
    }
    let media = if job.state.playable() {
        MediaRef::Download(id)
    } else {
        match crate::downloads::planning::media(ctx.db, job) {
            Ok(media) => media,
            Err(_) => {
                ctx.toast(Toast::info("That item is no longer in your playlists"));
                return;
            }
        }
    };
    ctx.play(PlayerCommand::Play(PlayRequest { media, zap: None, origin: PlayOrigin::Fullscreen, resume: true, immediate: true }));
    ctx.push(Route::Player);
}

/// Forgetting a title offers to remove its saved file after a separate Confirm (§43 §3).
pub fn offer_cleanup(ctx: &mut UiCtx<'_>, items: &[crate::model::ItemRef]) {
    let mut items = items.to_vec();
    for series in items.clone().into_iter().filter(|i| i.kind == ItemKind::Series) {
        if let Ok(targets) = crate::downloads::planning::series_targets(ctx.db, &series, crate::downloads::SeriesScope::All) {
            items.extend(targets.into_iter().map(|t| t.item));
        }
    }
    let ids: Vec<_> = snapshot(ctx).jobs.iter().filter(|j| j.state.playable() && items.iter().any(|item| j.matches(item))).map(|j| j.id).collect();
    if !ids.is_empty() {
        ctx.open(OverlayRequest::DeleteDownloads(ids));
    }
}

pub fn deleted_rule(ctx: &mut UiCtx<'_>, id: i64) {
    send(ctx, DownloadCommand::DeleteRule(id));
    ctx.toast(Toast { action: Some(("Undo".into(), Action::UndoDownloadRule(id))), ttl_ms: 10_000, ..Toast::info("Keep rule removed") });
}

pub fn clash_live(job: &Job, now: i64) -> bool {
    matches!(job.state, State::Scheduled | State::Waiting)
        && job.reason.as_deref() == Some("clash")
        && job.fp.get("download_clash_at").and_then(|v| v.as_i64()).is_some_and(|at| now < at + 60)
}

pub fn deleted(ctx: &mut UiCtx<'_>, id: DownloadId) {
    send(ctx, DownloadCommand::Delete(vec![id]));
    ctx.toast(Toast { action: Some(("Undo".into(), Action::UndoDownload(id))), ttl_ms: 10_000, ..Toast::info("Download deleted") });
}

pub fn state_text(job: &Job) -> String {
    let mut text = state_label(job);
    if let Some(playlist) = job.fp.get("download_switched_to").and_then(|v| v.as_str()) {
        text.push_str(&format!(" · Switched to {playlist}"));
    }
    text
}

fn state_label(job: &Job) -> String {
    if job.state == State::Finishing && job.reason.as_deref() == Some("finishing failed") {
        return "Finishing failed; the recording is kept".into();
    }
    if job.state == State::Finishing {
        return format!("Finishing… {:.0} %", job.fp.get("download_finishing").and_then(serde_json::Value::as_f64).unwrap_or(0.0) * 100.0);
    }
    if let Some(reason) = job.reason.as_deref() {
        match reason {
            "space" if job.state == State::Waiting => return "Waiting for space".into(),
            "busy" => return "Waiting for this account".into(),
            "window" => return "Waiting for the download window".into(),
            "watching" if job.state == State::Missed => return "Missed (you were watching)".into(),
            "watching" => return "Waiting for catch-up; keeping your picture".into(),
            "clash" => return "Recording due; this account is in use".into(),
            "guide missing" => return "Not in the guide any more".into(),
            "finishing failed" => return "Finishing failed; the recording is kept".into(),
            _ => {}
        }
    }
    if matches!(job.state, State::Paused | State::Waiting)
        && job.bytes_done > 0
        && job.fp.get("download_validators").and_then(|v| v.get("accepts_ranges")).and_then(|v| v.as_bool()) == Some(false)
    {
        return "Waiting; can't resume on this playlist".into();
    }
    if job.state.playable() {
        if job.reason.as_deref() == Some("saved from catch-up") {
            return "Recorded from catch-up".into();
        }
        if let Some(gaps) = job.reason.as_deref().and_then(|s| s.strip_prefix("gaps:"))
            && let Some((count, seconds)) = gaps.split_once(':')
        {
            return format!("Recorded · {count} gaps, {seconds} s");
        }
    }
    let name = match job.state {
        State::Scheduled => "Scheduled",
        State::Waiting => "Waiting",
        State::Running => {
            if job.kind.recording() {
                "Recording"
            } else {
                "Downloading"
            }
        }
        State::Paused => "Paused",
        State::Done => "Downloaded",
        State::Partial => "Partial",
        State::Failed => "Failed",
        State::Missed => "Missed",
        State::Cancelled => "Cancelled",
        State::Finishing => unreachable!(),
    };
    if job.state == State::Running {
        job.progress().map_or_else(|| name.into(), |p| format!("{name} {:.0} %", p * 100.0))
    } else if let Some(reason) = job.reason.as_deref().filter(|_| matches!(job.state, State::Failed | State::Waiting | State::Missed)) {
        format!("{name} · {reason}")
    } else {
        name.into()
    }
}

/// Frame-local lookup for leaf widgets that do not hold a UiCtx. Built once from the watch
/// snapshot: painting a card or guide cell never queries SQLite (DESIGN §43 §2.1, §5).
#[derive(Clone, Default)]
struct Badges {
    items: std::collections::HashMap<crate::model::ItemRef, String>,
    programmes: std::collections::HashMap<(crate::model::ChannelId, i64), String>,
    recording: std::collections::HashSet<crate::model::ItemRef>,
}
const BADGES: &str = "downloads-badges";
pub fn publish_badges(egui: &eframe::egui::Context, ctx: &UiCtx<'_>) {
    let snap = snapshot(ctx);
    let mut badges = Badges::default();
    let mut series: std::collections::HashMap<crate::model::ItemRef, (usize, usize)> = Default::default();
    for j in &snap.jobs {
        let Some(item) = j.item() else { continue };
        if matches!(j.state, State::Cancelled | State::Failed | State::Missed) {
            continue;
        }
        let label = if j.kind.recording() && !j.state.playable() {
            "⏺".to_owned()
        } else if j.state == State::Running {
            j.progress().map_or_else(|| "⤓".into(), |p| format!("⤓ {:.0} %", p * 100.0))
        } else {
            "⤓".into()
        };
        if j.kind == crate::downloads::Kind::Live && j.state == State::Running {
            badges.recording.insert(item.clone());
        }
        if let Some(start) = j.prog_start
            && let Some(id) = ctx.lib.live.by_ref(&item)
        {
            badges.programmes.insert((id, start), label.clone());
        }
        badges.items.insert(item.clone(), label.clone());
        if let Some(copies) = j.fp.get("download_copies").and_then(|v| serde_json::from_value::<Vec<crate::model::ItemRef>>(v.clone()).ok()) {
            for copy in copies {
                if !ctx.lib.conceals(&copy) {
                    badges.items.insert(copy, label.clone());
                }
            }
        }
        if let Some(key) = j.fp.get("download_series_key").and_then(|v| v.as_str()) {
            let group = crate::model::ItemRef { source: item.source, kind: ItemKind::Series, key: crate::model::ItemKey(key.into()) };
            let n = series.entry(group).or_default();
            n.0 += usize::from(j.state == State::Done);
            n.1 += 1;
        }
    }
    for (item, (done, total)) in series {
        badges.items.insert(item, format!("⤓ {done}/{total}"));
    }
    egui.data_mut(|d| d.insert_temp(eframe::egui::Id::new(BADGES), Arc::new(badges)));
}
pub fn cached_badge(egui: &eframe::egui::Context, item: &crate::model::ItemRef) -> Option<String> {
    egui.data(|d| d.get_temp::<Arc<Badges>>(eframe::egui::Id::new(BADGES)))?.items.get(item).cloned()
}
pub fn programme_badge(egui: &eframe::egui::Context, channel: crate::model::ChannelId, start: i64) -> Option<String> {
    egui.data(|d| d.get_temp::<Arc<Badges>>(eframe::egui::Id::new(BADGES)))?.programmes.get(&(channel, start)).cloned()
}
pub fn recording(egui: &eframe::egui::Context, item: &crate::model::ItemRef) -> bool {
    egui.data(|d| d.get_temp::<Arc<Badges>>(eframe::egui::Id::new(BADGES))).is_some_and(|s| s.recording.contains(item))
}
pub fn status_chip(shared: &crate::shared::AppShared, cfg: &crate::config::ParentalConfig) -> Option<crate::ui::widgets::status_bar::StatusChip> {
    let snap = shared.downloads()?.snapshot();
    let hide = shared.parental(cfg).hides_explicit;
    let lib = shared.library();
    let n = snap.jobs.iter().filter(|j| j.state.active() && visible(&lib, hide, j)).count();
    if n == 0 {
        return None;
    }
    Some(crate::ui::widgets::status_bar::StatusChip {
        text: format!("⤓ {n} downloading · {:.1} MB/s", snap.speed_bps / 1e6),
        label: "Downloads".into(),
        action: Action::GoDownloads,
        warning: false,
        close: None,
    })
}
pub fn due_toasts(egui: &eframe::egui::Context, ctx: &UiCtx<'_>) {
    let snap = snapshot(ctx);
    let jobs: Vec<_> = snap.jobs.iter().filter(|j| clash_live(j, ctx.now)).collect();
    if jobs.is_empty() {
        return;
    }
    eframe::egui::Area::new(eframe::egui::Id::new("recording-due"))
        .anchor(eframe::egui::Align2::RIGHT_BOTTOM, [-24.0, -70.0])
        .order(eframe::egui::Order::Foreground)
        .show(egui, |ui| {
            eframe::egui::Frame::popup(ui.style()).show(ui, |ui| {
                for job in jobs {
                    let left = job.fp["download_clash_at"].as_i64().unwrap_or(ctx.now) + 60 - ctx.now;
                    let name = job.source_id.and_then(|s| ctx.lib.source(s)).map_or("this account", |s| s.name.as_str());
                    ui.label(format!("{} recording starts on {name}, which you're watching · {left} s", job.title));
                    ui.horizontal(|ui| {
                        for (label, record, alternate) in
                            [("Record (stops this)", true, false), ("Record from another account", true, true), ("Skip", false, false)]
                        {
                            if alternate && job.fp.get("download_alternate").and_then(|v| v.as_bool()) != Some(true) {
                                continue;
                            }
                            if ui.button(label).clicked() {
                                send(ctx, DownloadCommand::Clash { id: job.id, record, alternate });
                            }
                        }
                    });
                }
            });
        });
    egui.request_repaint_after(std::time::Duration::from_secs(1));
}
