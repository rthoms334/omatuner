//! Series detail (DESIGN §13.6): header (backdrop, title, plot) + season tabs (`[` / `]`,
//! Tab moves between buttons / seasons / episodes) + episode list opened at
//! `store::vod::next_episode`; Enter plays, Shift+Enter from the start, Ctrl+W marks watched,
//! bulk "mark season watched" in the context menu. Key context `[Detail]`. End of an episode:
//! the page follows along (cursor on the next episode). Owner: WP-12.
//!
//! Data is read off the UI thread (series, seasons, episodes, per-episode progress and the
//! episode to open at) and re-read on `DataScope::{VodDetails, Progress, Overrides}`; Xtream
//! series without loaded details submit `JobKind::LoadSeriesDetails` and show a skeleton;
//! playlist (M3U) series are complete at import and get the sparse header (DESIGN §27.7).
//! The "Next episode in 10 s" card itself is the fullscreen player's (WP-11, drawn over the
//! video in OSD colours, for an episode started anywhere); this page only moves its cursor.

use std::collections::HashMap;

use eframe::egui;

use super::home::kit::{self, Loader};
use super::movie_detail::{DetailHeader, NO_DESCRIPTION, detail_bar, fraction, resumable, resume_or_play, sparse_meta};
use crate::jobs::{JobEvent, JobKind};
use crate::keymap::{Action, KeyContext};
use crate::model::{Episode, EpisodeId, ItemKey, ItemKind, ItemRef, MediaRef, PlayOrigin, Series, SeriesId, SourceKind, VodKind, WatchProgress};
use crate::msg::{AppEvent, DataScope};
use crate::player::PlayerEvent;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::{RowState, empty_state, glyphs, rows, section};
use crate::ui::{OverlayRequest, Route, Screen, ScreenId, UiCtx};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    Buttons,
    Seasons,
    #[default]
    Episodes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Play,
    Favourite,
    SeasonWatched,
}

/// Context-menu choices (echoed back as `Action::Digit`).
const MENU_SEASON_WATCHED: u8 = 1;
const MENU_SEASON_UNWATCHED: u8 = 2;

#[derive(Debug, Clone)]
struct Data {
    series: Series,
    /// Season numbers in order (from the season rows and the episodes).
    seasons: Vec<u32>,
    episodes: Vec<Episode>,
    progress: HashMap<ItemKey, WatchProgress>,
    next: Option<EpisodeId>,
}

impl Data {
    fn season_episodes(&self, season: u32) -> Vec<&Episode> {
        self.episodes.iter().filter(|e| e.season == season).collect()
    }
}

fn load(c: &rusqlite::Connection, id: SeriesId) -> crate::error::Result<Data> {
    use crate::store::{user, vod};
    let series = vod::series(c, id)?;
    let mut seasons: Vec<u32> = vod::seasons(c, id)?.into_iter().map(|s| s.number).collect();
    let episodes = vod::episodes(c, id, None)?;
    for e in &episodes {
        if !seasons.contains(&e.season) {
            seasons.push(e.season);
        }
    }
    seasons.sort_unstable();
    seasons.dedup();
    let mut progress = HashMap::new();
    for e in &episodes {
        if let Some(p) = user::progress(c, &episode_ref(e))? {
            progress.insert(e.key.clone(), p);
        }
    }
    let next = vod::next_episode(c, id)?.map(|e| e.id);
    Ok(Data { series, seasons, episodes, progress, next })
}

fn episode_ref(e: &Episode) -> ItemRef {
    ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key.clone() }
}

/// "S01E03 · Pilot" ("S01E03 · Episode 3" for an untitled playlist episode).
fn episode_label(e: &Episode) -> String {
    format!("S{:02}E{:02} · {}", e.season, e.episode, rows::episode_title(e))
}

#[derive(Default)]
pub struct SeriesDetailScreen {
    id: Option<SeriesId>,
    data: Option<Data>,
    loader: Option<Loader<Data>>,
    missing: bool,
    details_requested: Option<SeriesId>,
    section: Section,
    btn: usize,
    season: usize,
    /// Cursor inside the current season's episodes.
    ep: usize,
    /// Place the cursor on the next episode when the data arrives (first load of a series).
    position_pending: bool,
    scroll_to_cursor: bool,
}

impl SeriesDetailScreen {
    fn reload(&mut self, ctx: &UiCtx<'_>) {
        let Some(id) = self.id else { return };
        self.loader = Some(Loader::start(ctx, move |c| load(c, id)));
    }

    fn season_number(&self) -> Option<u32> {
        self.data.as_ref().and_then(|d| d.seasons.get(self.season).copied())
    }

    fn current_episodes(&self) -> Vec<&Episode> {
        match (&self.data, self.season_number()) {
            (Some(d), Some(s)) => d.season_episodes(s),
            _ => Vec::new(),
        }
    }

    fn focused_episode(&self) -> Option<Episode> {
        self.current_episodes().get(self.ep).map(|e| (*e).clone())
    }

    /// The next episode to watch (button "Play S01E03"): the stored one, else the first.
    fn next_episode(&self) -> Option<Episode> {
        let d = self.data.as_ref()?;
        d.next.and_then(|id| d.episodes.iter().find(|e| e.id == id)).or(d.episodes.first()).cloned()
    }

    fn series_ref(&self) -> Option<ItemRef> {
        self.data.as_ref().map(|d| ItemRef { source: d.series.source_id, kind: ItemKind::Series, key: d.series.key.clone() })
    }

    fn locked(&self, ctx: &UiCtx<'_>) -> bool {
        let locked = self
            .data
            .as_ref()
            .map(|d| d.series.locked)
            .or_else(|| self.id.and_then(|id| ctx.lib.vod.find(VodKind::Series(id))).map(|v| v.locked))
            .unwrap_or(false);
        locked && !kit::unlocked(ctx)
    }

    /// Put the cursor on `id` (switching season).
    fn select_episode(&mut self, id: EpisodeId) {
        let Some(d) = &self.data else { return };
        let Some(e) = d.episodes.iter().find(|e| e.id == id) else { return };
        if let Some(si) = d.seasons.iter().position(|s| *s == e.season) {
            self.season = si;
            self.ep = d.season_episodes(e.season).iter().position(|x| x.id == id).unwrap_or(0);
            self.scroll_to_cursor = true;
        }
    }

    fn set_season(&mut self, idx: usize) {
        let n = self.data.as_ref().map_or(0, |d| d.seasons.len());
        if n == 0 {
            return;
        }
        let idx = idx.min(n - 1);
        if idx != self.season {
            self.season = idx;
            self.ep = 0;
            self.scroll_to_cursor = true;
        }
    }

    fn play(&mut self, ctx: &mut UiCtx<'_>, e: &Episode, from_start: bool, alt: bool) {
        if self.locked(ctx) {
            kit::ask_pin(ctx, if alt { Action::ActivateAlt } else { Action::Activate }, "This series is locked");
            return;
        }
        if from_start {
            kit::play_media(ctx, MediaRef::Episode(e.id), false, PlayOrigin::Fullscreen);
        } else {
            resume_or_play(ctx, MediaRef::Episode(e.id), &episode_ref(e), episode_label(e), PlayOrigin::Fullscreen);
        }
    }

    fn buttons(&self) -> [Btn; 3] {
        [Btn::Play, Btn::Favourite, Btn::SeasonWatched]
    }

    fn season_watched(&self) -> bool {
        let eps = self.current_episodes();
        let d = self.data.as_ref();
        !eps.is_empty() && eps.iter().all(|e| d.and_then(|d| d.progress.get(&e.key)).is_some_and(|p| p.finished))
    }

    fn mark(&mut self, ctx: &mut UiCtx<'_>, eps: Vec<Episode>, on: bool) {
        if eps.is_empty() {
            return;
        }
        let items: Vec<(ItemRef, String)> = eps.iter().map(|e| (episode_ref(e), episode_label(e))).collect();
        let now = ctx.now;
        let _ = ctx.services.store.writer().submit_notify(DataScope::Progress, move |c| crate::store::user::mark_watched(c, &items, on, now));
        // optimistic; the reload on DataScope::Progress confirms it
        if let Some(d) = &mut self.data {
            for e in &eps {
                if on {
                    let p = d.progress.entry(e.key.clone()).or_insert_with(|| WatchProgress {
                        item: episode_ref(e),
                        position_s: 0.0,
                        duration_s: None,
                        updated_at: now,
                        finished: true,
                        title: String::new(),
                        fingerprint: Default::default(),
                    });
                    p.finished = true;
                } else {
                    d.progress.remove(&e.key);
                }
            }
        }
    }

    fn press(&mut self, ctx: &mut UiCtx<'_>, b: Btn) {
        match b {
            Btn::Play => {
                if let Some(e) = self.next_episode() {
                    self.play(ctx, &e, false, false);
                }
            }
            Btn::Favourite => {
                if let Some(item) = self.series_ref() {
                    kit::toggle_favourite(ctx, &item);
                }
            }
            Btn::SeasonWatched => {
                let on = !self.season_watched();
                let eps = self.current_episodes().into_iter().cloned().collect();
                self.mark(ctx, eps, on);
            }
        }
    }

    fn move_vertical(&mut self, down: bool) {
        let n_eps = self.current_episodes().len();
        match (self.section, down) {
            (Section::Buttons, true) => self.section = Section::Seasons,
            (Section::Seasons, true) => self.section = Section::Episodes,
            (Section::Seasons, false) => self.section = Section::Buttons,
            (Section::Episodes, false) if self.ep == 0 => self.section = Section::Seasons,
            (Section::Episodes, false) => self.ep -= 1,
            (Section::Episodes, true) => self.ep = (self.ep + 1).min(n_eps.saturating_sub(1)),
            (Section::Buttons, false) => {}
        }
        self.scroll_to_cursor = true;
    }

    /// After an episode ended: the episode after `key` (next in season order), if any.
    fn after(&self, key: &ItemKey) -> Option<Episode> {
        let d = self.data.as_ref()?;
        let i = d.episodes.iter().position(|e| &e.key == key)?;
        d.episodes.get(i + 1).cloned()
    }
}

impl Screen for SeriesDetailScreen {
    fn id(&self) -> ScreenId {
        ScreenId::SeriesDetail
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Detail]
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let Route::SeriesDetail(id) = route else { return };
        if self.id != Some(*id) {
            self.data = None;
            self.section = Section::Episodes;
            self.season = 0;
            self.ep = 0;
            self.btn = 0;
            self.position_pending = true;
        }
        self.id = Some(*id);
        self.missing = false;
        self.reload(ctx);
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        match ev {
            AppEvent::Data(DataScope::VodDetails | DataScope::Progress | DataScope::Overrides | DataScope::Vod | DataScope::All) => self.reload(ctx),
            AppEvent::Job(JobEvent::Finished { kind: JobKind::LoadSeriesDetails(s), outcome: Err(msg), .. }) if Some(*s) == self.id => {
                ctx.toast(crate::msg::Toast::error(format!("Couldn't load the episodes: {}", crate::redact::redact(msg))));
            }
            AppEvent::Player(PlayerEvent::Ended { item: Some(item), .. }) if item.kind == ItemKind::Episode => {
                // this page shows the series: follow along
                let shown = self.data.as_ref().and_then(|d| d.episodes.iter().find(|e| e.key == item.key && e.source_id == item.source)).cloned();
                // (the "Next episode in 10 s" card is the fullscreen player's, DESIGN §13.6)
                if let Some(ended) = &shown {
                    self.reload(ctx);
                    if let Some(n) = self.after(&ended.key) {
                        self.select_episode(n.id);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        match action {
            Action::Up => self.move_vertical(false),
            Action::Down => self.move_vertical(true),
            Action::Left | Action::Right => {
                let d: isize = if action == Action::Left { -1 } else { 1 };
                match self.section {
                    Section::Buttons => self.btn = (self.btn as isize + d).clamp(0, self.buttons().len() as isize - 1) as usize,
                    Section::Seasons => self.set_season((self.season as isize + d).max(0) as usize),
                    Section::Episodes => {}
                }
            }
            Action::TabPrev => self.set_season(self.season.saturating_sub(1)),
            Action::TabNext => self.set_season(self.season + 1),
            Action::FocusNext | Action::FocusPrev => {
                let order = [Section::Buttons, Section::Seasons, Section::Episodes];
                let i = order.iter().position(|s| *s == self.section).unwrap_or(0);
                let next = if action == Action::FocusNext { (i + 1) % 3 } else { (i + 2) % 3 };
                self.section = order[next];
            }
            Action::PageUp | Action::Home if self.section == Section::Episodes => {
                self.ep = 0;
                self.scroll_to_cursor = true;
            }
            Action::PageDown | Action::End if self.section == Section::Episodes => {
                self.ep = self.current_episodes().len().saturating_sub(1);
                self.scroll_to_cursor = true;
            }
            Action::Activate => match self.section {
                Section::Buttons => {
                    let b = self.buttons()[self.btn.min(2)];
                    self.press(ctx, b);
                }
                Section::Seasons => self.section = Section::Episodes,
                Section::Episodes => {
                    if let Some(e) = self.focused_episode() {
                        self.play(ctx, &e, false, false);
                    }
                }
            },
            Action::ActivateAlt => {
                let e = if self.section == Section::Episodes { self.focused_episode() } else { self.next_episode() };
                if let Some(e) = e {
                    self.play(ctx, &e, true, true);
                }
            }
            Action::MarkWatched => {
                if let Some(e) = self.focused_episode() {
                    let on = !self.data.as_ref().and_then(|d| d.progress.get(&e.key)).is_some_and(|p| p.finished);
                    self.mark(ctx, vec![e], on);
                }
            }
            Action::ToggleFavourite => self.press(ctx, Btn::Favourite),
            Action::ToggleLock => {
                if let Some((item, d)) = self.series_ref().zip(self.data.as_ref()) {
                    let by_group = kit::any_group_locked(&ctx.lib.vod.series_groups, &d.series.group_ids);
                    kit::toggle_lock(ctx, &item, d.series.locked, by_group);
                }
            }
            Action::AddToList | Action::Rename | Action::ToggleHidden => {
                if let Some((item, d)) = self.series_ref().zip(self.data.as_ref()) {
                    let name = d.series.display_name().to_owned();
                    match action {
                        Action::AddToList => kit::list_picker(ctx, &item, &name),
                        Action::Rename => kit::rename_item(ctx, &item, &name),
                        _ => kit::hide_item(ctx, &item, &name),
                    }
                }
            }
            Action::ContextMenu => {
                let mi = |icon: &'static str, label: &str, action: Action, shortcut: Option<&str>| MenuItem {
                    icon,
                    label: label.into(),
                    action,
                    enabled: true,
                    shortcut: shortcut.map(str::to_owned),
                };
                let mut episode = mi(glyphs::CHECK, "Mark episode watched / unwatched", Action::MarkWatched, Some("Ctrl+W"));
                episode.enabled = self.focused_episode().is_some();
                let items = vec![
                    mi(glyphs::CHECK, "Mark season watched", Action::Digit(MENU_SEASON_WATCHED), None),
                    mi(glyphs::CLOSE, "Mark season unwatched", Action::Digit(MENU_SEASON_UNWATCHED), None),
                    episode,
                    mi(glyphs::FAVOURITE, "Add to / remove from Favourites", Action::ToggleFavourite, Some("Ctrl+F")),
                    mi(glyphs::FAV_LIST, "Favourites lists…", Action::AddToList, Some("Ctrl+Shift+F")),
                    mi(glyphs::TAG, "Rename…", Action::Rename, Some("F2")),
                    mi(glyphs::LOCK, "Lock / unlock", Action::ToggleLock, Some("Ctrl+L")),
                    mi(glyphs::HIDDEN, "Hide", Action::ToggleHidden, Some("Ctrl+H")),
                ];
                ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Series".into(), items }));
            }
            // this is the details page already
            Action::ShowDetails => {}
            Action::MoveUp | Action::MoveDown | Action::SortMenu | Action::OpenInGuide | Action::AssignEpg | Action::Remove => kit::not_here(ctx),
            _ => return false,
        }
        true
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Chosen(Action::Digit(MENU_SEASON_WATCHED)) => {
                let eps = self.current_episodes().into_iter().cloned().collect();
                self.mark(ctx, eps, true);
            }
            OverlayResult::Chosen(Action::Digit(MENU_SEASON_UNWATCHED)) => {
                let eps = self.current_episodes().into_iter().cloned().collect();
                self.mark(ctx, eps, false);
            }
            OverlayResult::Chosen(
                a @ (Action::MarkWatched | Action::ToggleFavourite | Action::AddToList | Action::Rename | Action::ToggleLock | Action::ToggleHidden),
            ) => {
                self.on_action(ctx, a);
            }
            OverlayResult::Unlocked(a @ (Action::Activate | Action::ActivateAlt | Action::ToggleLock)) => {
                self.on_action(ctx, a);
            }
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        match kit::poll(&mut self.loader) {
            Some(Ok(d)) => {
                let lazy = d.series.details_loaded_at.is_none() && ctx.lib.source(d.series.source_id).is_some_and(|s| s.kind == SourceKind::Xtream);
                if lazy && self.details_requested != Some(d.series.id) {
                    self.details_requested = Some(d.series.id);
                    ctx.services.jobs.submit(JobKind::LoadSeriesDetails(d.series.id));
                }
                let next = d.next;
                let had_episodes = !d.episodes.is_empty();
                self.data = Some(d);
                if self.position_pending && had_episodes {
                    self.position_pending = false;
                    if let Some(n) = next {
                        self.select_episode(n);
                    }
                }
                let n = self.data.as_ref().map_or(0, |d| d.seasons.len());
                self.season = self.season.min(n.saturating_sub(1));
                self.ep = self.ep.min(self.current_episodes().len().saturating_sub(1));
                self.missing = false;
            }
            Some(Err(crate::error::Error::NotFound(_))) => self.missing = true,
            Some(Err(e)) => kit::error_toast(ctx, "Couldn't read this series", &e),
            None => {}
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        if self.missing || self.id.is_none() {
            detail_bar(ui, ctx, false);
            empty_state::empty_state(ui, theme, glyphs::SERIES, "This series is no longer in your playlists", None, None);
            return;
        }
        let summary = self.id.and_then(|id| ctx.lib.vod.find(VodKind::Series(id))).cloned();
        // DESIGN §27.7: a playlist series is complete at import (no details to wait for)
        let source = self.data.as_ref().map(|d| d.series.source_id).or(summary.as_ref().map(|s| s.source_id));
        let sparse = !source.and_then(|s| ctx.lib.source(s)).is_some_and(|s| s.kind == SourceKind::Xtream);
        let Some(data) = self.data.clone() else {
            // list-level data while the page loads
            let title = summary.as_ref().map(|s| s.name.to_string()).unwrap_or_default();
            detail_bar(ui, ctx, false);
            let header = DetailHeader {
                title: &title,
                meta: "",
                text: &[],
                poster: summary.as_ref().and_then(|s| s.poster.as_deref()),
                backdrop: None,
                playing: false,
                locked: false,
                loading: !sparse,
            };
            header.show(ui, ctx, |_, _| {});
            return;
        };
        let s = &data.series;
        let mut meta = Vec::new();
        if let Some(y) = s.year {
            meta.push(y.to_string());
        }
        if let Some(g) = s.genre.clone().filter(|g| !g.is_empty()) {
            meta.push(g);
        }
        if let Some(r) = s.rating.filter(|r| *r > 0.0) {
            meta.push(format!("★ {r:.1}"));
        }
        let mut text = Vec::new();
        if let Some(p) = s.plot.as_deref().filter(|p| !p.trim().is_empty()) {
            text.push((p.to_owned(), false));
        }
        if let Some(c) = s.cast.as_deref().filter(|c| !c.trim().is_empty()) {
            text.push((format!("Cast: {c}"), true));
        }
        if sparse {
            meta.extend(sparse_meta(ctx, &s.group_ids, Some(s.source_id)));
            if text.is_empty() {
                text.push((NO_DESCRIPTION.to_owned(), true));
            }
        }
        let playing_ep = ctx.player.playable.as_ref().and_then(|p| match p.media {
            MediaRef::Episode(id) => Some(id),
            _ => None,
        });
        let playing = playing_ep.is_some_and(|id| data.episodes.iter().any(|e| e.id == id));
        let fav = self.series_ref().is_some_and(|i| ctx.favs.is_fav(&i));
        let next = self.next_episode();
        let resume_at =
            next.as_ref().and_then(|e| data.progress.get(&e.key)).filter(|p| resumable(p, ctx.config.player.resume_min_s)).map(|p| p.position_s);
        let season_watched = self.season_watched();
        let loading = data.episodes.is_empty() && (self.loader.is_some() || !sparse && s.details_loaded_at.is_none());
        let (section, btn_focus, season_idx, ep_idx) = (self.section, self.btn, self.season, self.ep);
        let scroll = std::mem::take(&mut self.scroll_to_cursor);
        let mut clicked_btn = None;
        let mut clicked_season = None;
        let mut clicked_ep: Option<(usize, bool)> = None;
        let mut menu = detail_bar(ui, ctx, true);
        let mut ep_menu = None;
        egui::ScrollArea::vertical().id_salt("series-detail").auto_shrink([false, false]).show(ui, |ui| {
            let header = DetailHeader {
                title: s.display_name(),
                meta: &meta.join(" · "),
                text: &text,
                poster: s.cover.as_deref(),
                backdrop: s.backdrop.as_deref(),
                playing,
                locked: s.locked,
                loading: false,
            };
            menu |= header.show(ui, ctx, |ui, ctx| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(theme.space(SpaceToken::Lg), theme.space(SpaceToken::Md));
                    for (i, b) in self.buttons().iter().enumerate() {
                        let (icon, label, primary, selected) = match b {
                            Btn::Play => {
                                let label = match (&next, resume_at) {
                                    (Some(e), Some(at)) => format!("Resume S{:02}E{:02} {}", e.season, e.episode, kit::clock(at)),
                                    (Some(e), None) => format!("Play S{:02}E{:02}", e.season, e.episode),
                                    (None, _) => "Play".into(),
                                };
                                (glyphs::PLAY, label, true, false)
                            }
                            Btn::Favourite => (
                                if fav { glyphs::FAVOURITE } else { glyphs::FAVOURITE_OFF },
                                if fav { "Favourite".into() } else { "Add to Favourites".into() },
                                false,
                                fav,
                            ),
                            Btn::SeasonWatched => (
                                glyphs::CHECK,
                                if season_watched { "Season watched".into() } else { "Mark season watched".into() },
                                false,
                                season_watched,
                            ),
                        };
                        let style = ButtonStyle {
                            primary,
                            selected,
                            bordered: true,
                            focused: section == Section::Buttons && i == btn_focus,
                            ..Default::default()
                        };
                        let enabled = !matches!(b, Btn::Play) || next.is_some();
                        if ui.add_enabled_ui(enabled, |ui| button(ui, ctx.theme, Some(icon), &label, style)).inner.clicked() {
                            clicked_btn = Some(i);
                        }
                    }
                });
            });
            ui.add_space(theme.space(SpaceToken::Xxl));
            if data.seasons.is_empty() {
                if loading {
                    section::section_header(ui, theme, "Episodes");
                    for _ in 0..4 {
                        let (r, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), rows::row_height(theme, ctx.config.interface.density, false) - 4.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(r, t.rounding, t.fill_field);
                        ui.add_space(4.0);
                    }
                } else {
                    empty_state::empty_state(
                        ui,
                        theme,
                        glyphs::SERIES,
                        "No episodes yet",
                        Some("The provider lists no episodes for this series."),
                        None,
                    );
                }
                return;
            }
            // season tabs
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Xs);
                for (i, n) in data.seasons.iter().enumerate() {
                    let style =
                        ButtonStyle { selected: i == season_idx, focused: section == Section::Seasons && i == season_idx, ..Default::default() };
                    let label = if *n == 0 { "Specials".to_owned() } else { format!("Season {n}") };
                    if button(ui, theme, None, &label, style).clicked() {
                        clicked_season = Some(i);
                    }
                }
                ui.label(egui::RichText::new("[ ]").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            });
            ui.add_space(theme.space(SpaceToken::Md));
            let season = data.seasons.get(season_idx).copied().unwrap_or(0);
            for (i, e) in data.season_episodes(season).into_iter().enumerate() {
                let p = data.progress.get(&e.key);
                let watched = p.is_some_and(|p| p.finished);
                let cursor = section == Section::Episodes && i == ep_idx;
                let selected = section != Section::Episodes && i == ep_idx;
                let resp =
                    rows::episode_row(ui, theme, e, p.and_then(fraction), watched, playing_ep == Some(e.id), RowState::from_flags(cursor, selected));
                if (cursor || selected) && scroll {
                    ui.scroll_to_rect(resp.rect, None);
                }
                if resp.double_clicked() {
                    clicked_ep = Some((i, true));
                } else if resp.secondary_clicked() {
                    ep_menu = Some(i);
                } else if resp.clicked() {
                    clicked_ep = Some((i, false));
                }
            }
        });
        if let Some(i) = clicked_btn {
            self.section = Section::Buttons;
            self.btn = i;
            let b = self.buttons()[i];
            self.press(ctx, b);
        }
        if let Some(i) = clicked_season {
            self.section = Section::Seasons;
            self.set_season(i);
        }
        if let Some((i, play)) = clicked_ep {
            self.section = Section::Episodes;
            self.ep = i;
            if play && let Some(e) = self.focused_episode() {
                self.play(ctx, &e, false, false);
            }
        }
        // right-click on an episode: the menu key on it (its watched item applies to it)
        if let Some(i) = ep_menu {
            self.section = Section::Episodes;
            self.ep = i;
            menu = true;
        }
        if menu {
            self.on_action(ctx, Action::ContextMenu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SourceId, StreamLocator};

    fn ep(id: i64, season: u32, n: u32) -> Episode {
        Episode {
            id: EpisodeId(id),
            series_id: SeriesId(1),
            source_id: SourceId(1),
            key: ItemKey(format!("e{id}")),
            provider_id: None,
            season,
            episode: n,
            title: format!("Episode {n}"),
            plot: None,
            duration_s: Some(2700),
            still: None,
            air_date: None,
            rating: None,
            stream: StreamLocator::Url { url: "http://x".into() },
        }
    }

    fn screen() -> SeriesDetailScreen {
        let series = Series {
            id: SeriesId(1),
            source_id: SourceId(1),
            key: ItemKey("s".into()),
            provider_id: None,
            name: "Show".into(),
            clean_name: "Show".into(),
            rename: None,
            year: None,
            group_ids: vec![],
            cover: None,
            backdrop: None,
            rating: None,
            plot: None,
            genre: None,
            cast: None,
            director: None,
            release_date: None,
            last_modified: None,
            added_at: None,
            provider_order: 0,
            hidden: false,
            locked: false,
            details_loaded_at: Some(1),
        };
        let episodes = vec![ep(1, 1, 1), ep(2, 1, 2), ep(3, 2, 1), ep(4, 2, 2), ep(5, 2, 3)];
        SeriesDetailScreen {
            id: Some(SeriesId(1)),
            data: Some(Data { series, seasons: vec![1, 2], episodes, progress: HashMap::new(), next: Some(EpisodeId(4)) }),
            ..Default::default()
        }
    }

    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{self, harness, has_label, rig, seed, until};

    fn ep_progress(t: &crate::ui::testing::TestUi, key: &str) -> Option<WatchProgress> {
        let item = ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey(key.into()) };
        crate::store::user::progress(&t.services().store.reader().unwrap(), &item).unwrap()
    }

    #[test]
    fn page_opens_at_the_next_episode_seasons_marks_and_follows_an_ended_episode() {
        let (mut t, one) = rig();
        let ids = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ids = testkit::series(c, s, 1, 2, 3)?;
            let p = |key: &str, pos: f64, finished: bool, at: i64| WatchProgress {
                item: ItemRef { source: s, kind: ItemKind::Episode, key: ItemKey(key.into()) },
                position_s: pos,
                duration_s: Some(2700.0),
                updated_at: at,
                finished,
                title: String::new(),
                fingerprint: Default::default(),
            };
            crate::store::user::save_progress(c, &p("m3u:ep0-1-1", 2600.0, true, 5))?;
            crate::store::user::save_progress(c, &p("m3u:ep0-1-2", 600.0, false, 6))?;
            Ok(ids)
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1200.0, 900.0));
        h.state_mut().enter(Route::SeriesDetail(ids[0]));
        until(&mut h, "the episodes", |h| has_label(h, "Resume S01E02 10:00"));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Detail]);
        assert!(has_label(&h, "Season 1") && has_label(&h, "Season 2"));
        assert_eq!((h.state().s.season, h.state().s.ep), (0, 1), "opens at the episode in progress");
        // Enter on it: the resume prompt
        h.state_mut().act(Action::Activate);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Resume(r))] if r.position_s == 600.0), "{nav:?}");
        // ] next season, [ back; Ctrl+W marks the focused episode
        h.state_mut().act(Action::TabNext);
        assert_eq!((h.state().s.season, h.state().s.ep), (1, 0));
        h.state_mut().act(Action::MarkWatched);
        until(&mut h, "S2E1 watched", |h| ep_progress(&h.state().t, "m3u:ep0-2-1").is_some_and(|p| p.finished));
        // the context menu marks the whole season
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items[0].label == "Mark season watched"), "{nav:?}");
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_SEASON_WATCHED)));
        until(&mut h, "season 2 watched", |h| (1..=3).all(|e| ep_progress(&h.state().t, &format!("m3u:ep0-2-{e}")).is_some_and(|p| p.finished)));
        h.state_mut().act(Action::TabPrev);
        assert_eq!(h.state().s.season, 0);
        // Shift+Enter on S1E3 plays from the start, and the page remembers it started it
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::ActivateAlt);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
        let s13 = h.state().s.focused_episode().unwrap();
        assert_eq!((s13.season, s13.episode), (1, 3));
        // it ends (watched) in the fullscreen player: the page follows along to S2E1 and opens
        // nothing itself (the "Next episode in 10 s" card is the player's, tests/guide.rs)
        crate::ui::screens::movie_detail::tests::fullscreen_placement(&mut h.state_mut().t);
        h.state().t.services().bus.send(AppEvent::Player(PlayerEvent::Ended { item: Some(episode_ref(&s13)), finished: true }));
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![], "no second up-next card over the player's");
        let next = h.state().s.data.as_ref().unwrap().episodes.iter().find(|e| e.season == 2 && e.episode == 1).unwrap().id;
        assert_eq!(h.state().s.focused_episode().map(|e| e.id), Some(next));
    }

    #[test]
    fn opens_at_the_next_episode_and_walks_sections() {
        let mut s = screen();
        s.select_episode(EpisodeId(4));
        assert_eq!((s.season, s.ep), (1, 1));
        assert_eq!(s.next_episode().map(|e| e.id), Some(EpisodeId(4)));
        s.move_vertical(true);
        s.move_vertical(true);
        assert_eq!(s.ep, 2);
        s.move_vertical(true);
        assert_eq!(s.ep, 2, "clamped at the last episode");
        s.ep = 0;
        s.move_vertical(false);
        assert_eq!(s.section, Section::Seasons, "↑ from the first episode reaches the season tabs");
        s.move_vertical(false);
        assert_eq!(s.section, Section::Buttons);
        s.set_season(0);
        assert_eq!((s.season, s.ep), (0, 0));
        s.set_season(9);
        assert_eq!(s.season, 1, "clamped to the last season");
        assert_eq!(s.after(&ItemKey("e2".into())).map(|e| e.id), Some(EpisodeId(3)), "the next episode crosses seasons");
        assert!(s.after(&ItemKey("e5".into())).is_none());
    }

    #[test]
    fn season_watched_follows_progress() {
        let mut s = screen();
        s.set_season(0);
        assert!(!s.season_watched());
        let d = s.data.as_mut().unwrap();
        for e in d.episodes.clone().iter().filter(|e| e.season == 1) {
            d.progress.insert(
                e.key.clone(),
                WatchProgress {
                    item: episode_ref(e),
                    position_s: 0.0,
                    duration_s: None,
                    updated_at: 0,
                    finished: true,
                    title: String::new(),
                    fingerprint: Default::default(),
                },
            );
        }
        assert!(s.season_watched());
        assert_eq!(episode_label(&ep(7, 1, 3)), "S01E03 · Episode 3");
        // a playlist episode without a title (DESIGN §27.7)
        assert_eq!(episode_label(&Episode { title: String::new(), ..ep(8, 2, 4) }), "S02E04 · Episode 4");
    }
}
